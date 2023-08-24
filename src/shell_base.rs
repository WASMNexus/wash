/*
 * Copyright (c) 2022-2023 Antmicro <www.antmicro.com>
 *
 * SPDX-License-Identifier: Apache-2.0
 */

use color_eyre::Report;
use lazy_static::lazy_static;
#[cfg(not(target_os = "wasi"))]
use nix;
use regex::Regex;
use std::collections::{HashMap, VecDeque};
use std::env;
use std::fs;
use std::fs::{File, OpenOptions};
use std::io;
use std::io::ErrorKind;
use std::io::{BufRead, BufReader, Read, Write};
#[cfg(target_os = "wasi")]
use std::mem;
#[cfg(not(target_os = "wasi"))]
use std::os::fd::IntoRawFd;
#[cfg(target_os = "wasi")]
use std::os::wasi::io::{AsRawFd, FromRawFd};
use std::path::{Path, PathBuf};
#[cfg(target_os = "wasi")]
use wasi;

use crate::internals::INTERNALS_MAP;
use crate::interpreter::InputInterpreter;
use crate::output_device::OutputDevice;

#[cfg(target_os = "wasi")]
pub type Fd = wasi::Fd;
#[cfg(not(target_os = "wasi"))]
pub type Fd = std::os::fd::RawFd;

pub const EXIT_SUCCESS: i32 = 0;
pub const EXIT_FAILURE: i32 = 1;
pub const EXIT_CRITICAL_FAILURE: i32 = 2;
pub const EXIT_CMD_NOT_FOUND: i32 = 127;
pub const EXIT_INTERRUPTED: i32 = 130;

pub const STDIN: Fd = 0;
pub const STDOUT: Fd = 1;
pub const STDERR: Fd = 2;
pub const CLEAR_ESCAPE_CODE: &str = "\x1b[2J\x1b[H";

enum HistoryExpansion {
    Expanded(String),
    EventNotFound(String),
    Unchanged,
}

#[cfg(target_os = "wasi")]
pub type Redirect = wasi_ext_lib::Redirect;

#[cfg(not(target_os = "wasi"))]
#[derive(Debug)]
pub enum Redirect {
    Read(Fd, String),
    Write(Fd, String),
    Append(Fd, String),
    ReadWrite(Fd, String),
    PipeIn(Fd),
    PipeOut(Fd),
    Duplicate { fd_src: Fd, fd_dst: Fd },
    Close(Fd),
}

pub fn preprocess_redirects<'a>(
    redirects: &'a [Redirect],
    output_device: &mut OutputDevice<'a>,
) -> Result<(), Report> {
    enum DescriptorState<'a> {
        Redirect(&'a Redirect),
        Opened,
        Closed,
    }

    let mut red_map: HashMap<Fd, DescriptorState> = HashMap::new();

    for redirect in redirects.iter() {
        let affected_fd = match redirect {
            Redirect::Read(fd, path) => {
                // Check file exist
                let file_path = Path::new(path);
                if !file_path.exists() {
                    return Err(Report::msg(format!("{}: No such file or directory", path)));
                }
                red_map.insert(*fd, DescriptorState::Redirect(redirect));
                *fd
            }
            Redirect::Write(fd, path)
            | Redirect::Append(fd, path)
            | Redirect::ReadWrite(fd, path) => {
                let file_path = Path::new(path);
                if file_path.is_dir() {
                    return Err(Report::msg(format!("{}: Is a directory", path)));
                }
                red_map.insert(*fd, DescriptorState::Redirect(redirect));
                *fd
            }
            // In redirect array first should be pipes and then other redirects.
            // We assume that pipes fds are opened by wash correctly.
            Redirect::PipeIn(_) => {
                red_map.insert(STDIN, DescriptorState::Redirect(redirect));
                STDIN
            }
            Redirect::PipeOut(_) => {
                red_map.insert(STDOUT, DescriptorState::Redirect(redirect));
                STDOUT
            }
            Redirect::Duplicate { fd_src, fd_dst } => {
                let redirect = match red_map.get(fd_src) {
                    Some(DescriptorState::Redirect(redirected)) => *redirected,
                    Some(DescriptorState::Opened) => redirect,
                    Some(DescriptorState::Closed) => {
                        return Err(Report::msg(format!("{}: Bad file descriptor", fd_src)));
                    }
                    None => {
                        // check fd_src is opened
                        let fd_res = {
                            #[cfg(target_os = "wasi")]
                            unsafe {
                                wasi::fd_fdstat_get(*fd_src)
                            }
                            #[cfg(not(target_os = "wasi"))]
                            nix::fcntl::fcntl(*fd_src, nix::fcntl::F_GETFD)
                        };
                        if fd_res.is_ok() {
                            red_map.insert(*fd_src, DescriptorState::Opened);
                            redirect
                        } else {
                            return Err(Report::msg(format!("{}: Bad file descriptor", fd_src)));
                        }
                    }
                };
                red_map.insert(*fd_dst, DescriptorState::Redirect(redirect));
                *fd_dst
            }
            Redirect::Close(fd) => {
                match red_map.get(fd) {
                    Some(DescriptorState::Opened) | Some(DescriptorState::Redirect(_)) => {
                        red_map.remove(fd);
                    }
                    Some(DescriptorState::Closed) => {
                        return Err(Report::msg(format!("{}: Bad file descriptor", fd)));
                    }
                    None => {
                        let fd_res = {
                            #[cfg(target_os = "wasi")]
                            unsafe {
                                wasi::fd_fdstat_get(*fd)
                            }
                            #[cfg(not(target_os = "wasi"))]
                            nix::fcntl::fcntl(*fd, nix::fcntl::F_GETFD)
                        };
                        if fd_res.is_err() {
                            return Err(Report::msg(format!("{}: Bad file descriptor", fd)));
                        }
                    }
                }

                red_map.insert(*fd, DescriptorState::Closed);
                *fd
            }
        };

        if affected_fd == STDOUT {
            output_device.set_redirect_out(redirect);
        } else if affected_fd == STDERR {
            output_device.set_redirect_err(redirect);
        }
    }

    Ok(())
}

#[cfg(not(target_os = "wasi"))]
pub fn apply_redirects(redirects: &[Redirect]) -> io::Result<()> {
    for redirect in redirects.iter() {
        let (fd_src, fd_dst): (Fd, Fd) = match redirect {
            Redirect::Read(fd, path)
            | Redirect::Write(fd, path)
            | Redirect::Append(fd, path)
            | Redirect::ReadWrite(fd, path) => {
                let mut open_options = OpenOptions::new();
                match redirect {
                    Redirect::Read(_, _) => {
                        open_options.read(true);
                    }
                    Redirect::Write(_, _) => {
                        open_options.write(true).truncate(true).create(true);
                    }
                    Redirect::Append(_, _) => {
                        open_options.write(true).append(true).create(true);
                    }
                    Redirect::ReadWrite(_, _) => {
                        open_options.read(true).write(true).create(true);
                    }
                    _ => unreachable!(),
                };

                // After this line, user is responsible for closing fd
                let opened_fd = open_options.open(path)?.into_raw_fd();

                (opened_fd, *fd)
            }
            Redirect::PipeIn(fd) => (*fd, STDIN),
            Redirect::PipeOut(fd) => (*fd, STDOUT),
            Redirect::Duplicate { fd_src, fd_dst } => (*fd_src, *fd_dst),
            Redirect::Close(fd) => {
                nix::unistd::close(*fd)?;
                continue;
            }
        };

        nix::unistd::dup2(fd_src, fd_dst)?;

        // TODO: set cloexec instead of closing fds
        if let Redirect::Duplicate {
            fd_src: _,
            fd_dst: _,
        } = redirect
        {
            // Do not close fd_src
            continue;
        }

        nix::unistd::close(fd_src)?;
    }

    Ok(())
}

pub fn spawn(
    path: &str,
    args: &[&str],
    env: &HashMap<String, String>,
    background: bool,
    redirects: &[Redirect],
) -> Result<(i32, i32), i32> {
    #[cfg(target_os = "wasi")]
    {
        wasi_ext_lib::spawn(path, args, env, background, redirects)
    }
    #[cfg(not(target_os = "wasi"))]
    match unsafe { nix::unistd::fork() } {
        Ok(nix::unistd::ForkResult::Parent { child }) => {
            if !background {
                Ok((wait_for_child(child), i32::from(child)))
            } else {
                Ok((EXIT_SUCCESS, i32::from(child)))
            }
        }
        Ok(nix::unistd::ForkResult::Child) => {
            use std::ffi::CString;

            fn env_fmt<T: std::fmt::Display>((key, val): (T, T)) -> CString {
                CString::new(format!("{}={}", key, val)).unwrap()
            }

            // Apply all redirects
            if let Err(err) = apply_redirects(redirects) {
                eprintln!("{}: {}", env!("CARGO_PKG_NAME"), err);
                std::process::exit(EXIT_FAILURE);
            }

            let prog_name = if let Some(idx) = path.rfind('/') {
                &path[(idx + 1)..]
            } else {
                path
            };

            let cpath = CString::new(path).unwrap();
            let cargs: Vec<CString> = [prog_name]
                .iter()
                .chain(args.iter())
                .map(|arg: &&str| CString::new(*arg).unwrap())
                .collect();

            let cenv: Vec<CString> = std::env::vars()
                .map(env_fmt)
                .chain(env.iter().map(env_fmt))
                .collect();

            if let Err(err) =
                nix::unistd::execve(cpath.as_c_str(), cargs.as_slice(), cenv.as_slice())
            {
                eprintln!("{}: {}", env!("CARGO_PKG_NAME"), err);
                std::process::exit(EXIT_FAILURE);
            }
            unreachable!()
        }
        Err(err) => {
            eprintln!("{}: {}", env!("CARGO_PKG_NAME"), err);
            Err(EXIT_FAILURE)
        }
    }
}

#[cfg(not(target_os = "wasi"))]
pub fn wait_for_child(child_pid: nix::unistd::Pid) -> i32 {
    loop {
        let wait_status = nix::sys::wait::waitpid(child_pid, None)
            .unwrap_or_else(|err| panic!("{}: waitpid error: {}", env!("CARGO_PKG_NAME"), err));
        match wait_status {
            nix::sys::wait::WaitStatus::Exited(waited_pid, exit_code)
                if waited_pid == child_pid =>
            {
                return exit_code;
            }
            nix::sys::wait::WaitStatus::Exited(_, _) => {
                continue;
            }
            _ => unreachable!(),
        }
    }
}

pub fn path_exists(path: &str) -> io::Result<bool> {
    fs::metadata(path).map(|_| true).or_else(|error| {
        if error.kind() == ErrorKind::NotFound {
            Ok(false)
        } else {
            Err(error)
        }
    })
}

#[cfg(target_os = "wasi")]
struct InternalEventSource {
    subs: [wasi::Subscription; 2],
    events: [wasi::Event; 2],
    tty_input: File,
    event_src: File,
}

#[cfg(target_os = "wasi")]
impl InternalEventSource {
    const TTY_TOKEN: u64 = 1;
    const SIGINT_TOKEN: u64 = 2;

    pub fn read_byte(&mut self) -> Result<Option<u8>, Report> {
        // subscribe and wait
        let mut byte: [u8; 1] = [0];
        let result = unsafe {
            wasi::poll_oneoff(
                self.subs.as_ptr(),
                self.events.as_mut_ptr(),
                self.subs.len(),
            )
        };

        let events_count = match result {
            Ok(n) => n,
            Err(e) => {
                return Err(Report::msg(format!(
                    "Poll_oneoff returned non zero code = {e}!"
                )));
            }
        };

        for event in self.events[0..events_count].iter() {
            let errno = event.error.raw();
            if errno > 0 {
                return Err(Report::msg("Poll_oneoff returned non zero code for event!"));
            }
        }

        for event in self.events[0..events_count].iter() {
            match (event.userdata, event.type_) {
                (Self::TTY_TOKEN, wasi::EVENTTYPE_FD_READ) => {
                    self.tty_input.read_exact(&mut byte)?
                }
                (Self::SIGINT_TOKEN, wasi::EVENTTYPE_FD_READ) => {
                    let mut read_buff: [u8; wasi_ext_lib::WASI_EVENTS_MASK_SIZE] =
                        [0u8; wasi_ext_lib::WASI_EVENTS_MASK_SIZE];

                    self.event_src.read_exact(&mut read_buff)?;

                    let events = u32::from_le_bytes(read_buff) as wasi_ext_lib::WasiEvents;

                    if events & wasi_ext_lib::WASI_EVENT_SIGINT != 0 {
                        return Ok(None);
                    } else {
                        return Err(Report::msg(
                            "Event_source did not return subsribed SigInt event!",
                        ));
                    }
                }
                _ => unreachable!(),
            }
        }

        Ok(Some(byte[0]))
    }
}

#[cfg(target_os = "wasi")]
impl Default for InternalEventSource {
    fn default() -> Self {
        let input_fd = STDIN as i32;

        if !wasi_ext_lib::isatty(input_fd).unwrap() {
            panic!("Input is not TTY!");
        }

        let event_source_fd = match wasi_ext_lib::event_source_fd(wasi_ext_lib::WASI_EVENT_SIGINT) {
            Ok(fd) => fd,
            Err(err) => {
                panic!("Cannot obtain event_source_fd, error code: {}", err);
            }
        };

        if let Err(err) = wasi_ext_lib::fcntl(
            event_source_fd as Fd,
            wasi_ext_lib::FcntlCommand::F_SETFD {
                flags: wasi_ext_lib::WASI_EXT_FDFLAG_CLOEXEC,
            },
        ) {
            panic!(
                "Cannot set cloexec flag for event_source_fd, error code: {}",
                err
            );
        }

        InternalEventSource {
            subs: [
                wasi::Subscription {
                    userdata: Self::TTY_TOKEN,
                    u: wasi::SubscriptionU {
                        tag: wasi::EVENTTYPE_FD_READ.raw(),
                        u: wasi::SubscriptionUU {
                            fd_read: wasi::SubscriptionFdReadwrite {
                                file_descriptor: input_fd as u32,
                            },
                        },
                    },
                },
                wasi::Subscription {
                    userdata: Self::SIGINT_TOKEN,
                    u: wasi::SubscriptionU {
                        tag: wasi::EVENTTYPE_FD_READ.raw(),
                        u: wasi::SubscriptionUU {
                            fd_read: wasi::SubscriptionFdReadwrite {
                                file_descriptor: event_source_fd as u32,
                            },
                        },
                    },
                },
            ],
            events: unsafe { mem::zeroed() },
            tty_input: unsafe { File::from_raw_fd(input_fd) },
            event_src: unsafe { File::from_raw_fd(event_source_fd) },
        }
    }
}

enum InternalReader {
    #[cfg(target_os = "wasi")]
    StdinWithSigInt(InternalEventSource),
    OnlyStdin,
}

impl InternalReader {
    fn read_byte(&mut self) -> Result<Option<u8>, Report> {
        match self {
            #[cfg(target_os = "wasi")]
            InternalReader::StdinWithSigInt(reader) => reader.read_byte(),
            InternalReader::OnlyStdin => {
                let mut buffer: [u8; 1] = [0];
                io::stdin().read_exact(&mut buffer)?;

                Ok(Some(buffer[0]))
            }
        }
    }
}

pub struct Shell {
    pub pwd: PathBuf,
    pub vars: HashMap<String, String>,
    pub args: VecDeque<String>,
    pub last_exit_status: i32,
    pub last_job_pid: Option<u32>,
    pub history: Vec<String>,

    history_path: PathBuf,
    should_echo: bool,
    cursor_position: usize,
    insert_mode: bool,

    reader: InternalReader,
}

impl Shell {
    pub fn new(should_echo: bool, pwd: &str, args: VecDeque<String>) -> Self {
        Shell {
            should_echo,
            pwd: PathBuf::from(pwd),
            args,
            history: Vec::new(),
            history_path: PathBuf::from(if PathBuf::from(env::var("HOME").unwrap()).exists() {
                format!(
                    "{}/.{}_history",
                    env::var("HOME").unwrap(),
                    env!("CARGO_PKG_NAME")
                )
            } else {
                format!(
                    "{}/.{}_history",
                    env::var("PWD").unwrap(),
                    env!("CARGO_PKG_NAME")
                )
            }),
            vars: HashMap::new(),
            last_exit_status: EXIT_SUCCESS,
            last_job_pid: None,
            cursor_position: 0,
            insert_mode: false,

            reader: InternalReader::OnlyStdin,
        }
    }

    fn print_prompt(&mut self, input: &str) {
        print!("{}{}", self.parse_prompt_string(), input);
        io::stdout().flush().unwrap();
        self.cursor_position = input.len();
    }

    fn parse_prompt_string(&self) -> String {
        fn get_hostname() -> String {
            #[cfg(not(target_os = "wasi"))]
            {
                if let Ok(name) = nix::sys::utsname::uname() {
                    return unsafe {
                        String::from_utf8_lossy(std::mem::transmute(name.nodename())).into_owned()
                    };
                }
            }
            env::var("HOSTNAME").unwrap_or_else(|_| "hostname".to_string())
        }

        env::var("PS1")
            .unwrap_or_else(|_| "\x1b[1;34m\\u@\\h \x1b[1;33m\\w$\x1b[0m ".to_string())
            .replace(
                "\\u",
                &env::var("USER").unwrap_or_else(|_| "user".to_string()),
            )
            .replace("\\h", &get_hostname())
            // FIXME: should only replace if it starts with HOME
            .replace(
                "\\w",
                &self
                    .pwd
                    .display()
                    .to_string()
                    .replace(&env::var("HOME").unwrap(), "~"),
            )
    }

    fn echo(&self, output: &str) {
        if self.should_echo {
            // TODO: should this maybe use OutputDevice too?
            print!("{output}");
        }
    }

    pub fn run_command(&mut self, command: &str) -> Result<i32, Report> {
        self.handle_input(command)
    }

    pub fn run_script(&mut self, script_name: impl Into<PathBuf>) -> Result<i32, Report> {
        self.handle_input(&fs::read_to_string(script_name.into()).unwrap())
    }

    fn get_cursor_to_end(&mut self, input: &String) {
        self.echo(&input.chars().skip(self.cursor_position).collect::<String>());
        for _ in 0..input.len() {
            self.echo(&format!("{} {}", 8 as char, 8 as char));
        }
    }

    /// Builds a line from standard input.
    // TODO: maybe wrap in one more loop and only return when non-empty line is produced?
    // returns Ok(false) when SigInt occurred
    fn get_line(&mut self, input: &mut String) -> Result<bool, Report> {
        let mut input_stash = String::new();

        let mut c1;
        let mut escaped = false;
        let mut history_entry_to_display: i32 = -1;
        if self.insert_mode {
            self.insert_mode = false;
        }

        loop {
            // this is to handle EOF when piping to shell
            match self.reader.read_byte()? {
                Some(byte) => c1 = byte,
                None => return Ok(false),
            }
            if escaped {
                match c1 {
                    0x5b => {
                        let c2 = match self.reader.read_byte()? {
                            Some(byte) => byte,
                            None => return Ok(false),
                        };

                        match c2 {
                            0x32 | 0x33 | 0x35 | 0x36 => {
                                let c3 = match self.reader.read_byte()? {
                                    Some(byte) => byte,
                                    None => return Ok(false),
                                };
                                match [c2, c3] {
                                    // PageUp
                                    [0x35, 0x7e] => {
                                        if !self.history.is_empty() && history_entry_to_display != 0
                                        {
                                            if history_entry_to_display == -1 {
                                                input_stash = input.clone();
                                            }
                                            history_entry_to_display = 0;
                                            // bring cursor to the end so that clearing later starts from
                                            // proper position
                                            self.get_cursor_to_end(input);
                                            *input = self.history[0].clone();
                                            self.cursor_position = input.len();
                                            self.echo(input);
                                        }
                                        escaped = false;
                                    }
                                    // PageDown
                                    [0x36, 0x7e] => {
                                        if history_entry_to_display != -1 {
                                            // bring cursor to the end so that clearing later starts from
                                            // proper position
                                            self.get_cursor_to_end(input);
                                            *input = input_stash.clone();
                                            history_entry_to_display = -1;
                                            self.cursor_position = input.len();
                                            self.echo(input);
                                        }
                                        escaped = false;
                                    }
                                    // Insert
                                    [0x32, 0x7e] => {
                                        self.insert_mode = !self.insert_mode;
                                        escaped = false;
                                    }
                                    // delete key
                                    [0x33, 0x7e] => {
                                        if input.len() - self.cursor_position > 0 {
                                            self.echo(
                                                &" ".repeat(input.len() - self.cursor_position + 1),
                                            );
                                            input.remove(self.cursor_position);
                                            self.echo(
                                                &format!("{}", 8 as char)
                                                    .repeat(input.len() - self.cursor_position + 2),
                                            );
                                            self.echo(
                                                &input
                                                    .chars()
                                                    .skip(self.cursor_position)
                                                    .collect::<String>(),
                                            );
                                            self.echo(
                                                &format!("{}", 8 as char)
                                                    .repeat(input.len() - self.cursor_position),
                                            );
                                        }
                                        escaped = false;
                                    }
                                    [0x33, 0x3b] => {
                                        println!("TODO: SHIFT + DELETE");
                                        let mut c4 = [0; 2];
                                        // TWO MORE! TODO: improve!
                                        io::stdin().read_exact(&mut c4).unwrap();
                                        escaped = false;
                                    }
                                    _ => {
                                        println!("TODO: [ + 0x{:02x} + 0x{:02x}", c2, c3);
                                        escaped = false;
                                    }
                                }
                            }
                            // up arrow
                            0x41 => {
                                if !self.history.is_empty() && history_entry_to_display != 0 {
                                    if history_entry_to_display == -1 {
                                        history_entry_to_display = (self.history.len() - 1) as i32;
                                        input_stash = input.clone();
                                    } else if history_entry_to_display > 0 {
                                        history_entry_to_display -= 1;
                                    }
                                    // bring cursor to the end so that clearing later starts from
                                    // proper position
                                    self.get_cursor_to_end(input);
                                    *input =
                                        self.history[history_entry_to_display as usize].clone();
                                    self.cursor_position = input.len();
                                    self.echo(input);
                                }
                                escaped = false;
                            }
                            // down arrow
                            0x42 => {
                                if history_entry_to_display != -1 {
                                    // bring cursor to the end so that clearing later starts from
                                    // proper position
                                    self.get_cursor_to_end(input);
                                    if self.history.len() - 1 > (history_entry_to_display as usize)
                                    {
                                        history_entry_to_display += 1;
                                        *input =
                                            self.history[history_entry_to_display as usize].clone();
                                    } else {
                                        *input = input_stash.clone();
                                        history_entry_to_display = -1;
                                    }
                                    self.cursor_position = input.len();
                                    self.echo(input);
                                }
                                escaped = false;
                            }
                            // right arrow
                            0x43 => {
                                if self.cursor_position < input.len() {
                                    self.echo(
                                        &input
                                            .chars()
                                            .nth(self.cursor_position)
                                            .unwrap()
                                            .to_string(),
                                    );
                                    self.cursor_position += 1;
                                }
                                escaped = false;
                            }
                            // left arrow
                            0x44 => {
                                if self.cursor_position > 0 {
                                    self.echo(&format!("{}", 8 as char));
                                    self.cursor_position -= 1;
                                }
                                escaped = false;
                            }
                            // end key
                            0x46 => {
                                self.echo(
                                    &input.chars().skip(self.cursor_position).collect::<String>(),
                                );
                                self.cursor_position = input.len();
                                escaped = false;
                            }
                            // home key
                            0x48 => {
                                self.echo(&format!("{}", 8 as char).repeat(self.cursor_position));
                                self.cursor_position = 0;
                                escaped = false;
                            }
                            _ => {
                                println!("WE HAVE UNKNOWN CONTROL CODE '[' + {}", c2);
                                escaped = false;
                            }
                        }
                    }
                    _ => {
                        escaped = false;
                    }
                }
            } else {
                if c1 != 0x1b {
                    history_entry_to_display = -1;
                }
                match c1 {
                    // enter
                    10 => {
                        self.echo("\n");
                        self.cursor_position = 0;
                        *input = input.trim().to_string();
                        return Ok(true);
                    }
                    // backspace
                    127 => {
                        if !input.is_empty() && self.cursor_position > 0 {
                            self.echo(&format!("{}", 8 as char));
                            self.echo(&" ".repeat(input.len() - self.cursor_position + 1));
                            input.remove(self.cursor_position - 1);
                            self.cursor_position -= 1;
                            self.echo(
                                &format!("{}", 8 as char)
                                    .repeat(input.len() - self.cursor_position + 1),
                            );
                            self.echo(
                                &input.chars().skip(self.cursor_position).collect::<String>(),
                            );
                            self.echo(
                                &format!("{}", 8 as char)
                                    .repeat(input.len() - self.cursor_position),
                            );
                        }
                    }
                    // control codes
                    code if code < 32 => {
                        if code == 0x1b {
                            escaped = true;
                        }
                        // ignore rest for now
                    }
                    // regular characters
                    _ => {
                        if !self.insert_mode {
                            input.insert(self.cursor_position, c1 as char);
                        } else {
                            // in insert mode, when cursor is in the middle, chars are replaced
                            // instead of being put in the middle while moving next characters further
                            if self.cursor_position != input.len() {
                                input.replace_range(
                                    self.cursor_position..self.cursor_position + 1,
                                    std::str::from_utf8(&[c1]).unwrap(),
                                );
                            } else {
                                // if cursor is at the end, chars are input regularly
                                input.push(c1 as char);
                            }
                        }
                        // echo
                        self.echo(&format!(
                            "{}{}",
                            input.chars().skip(self.cursor_position).collect::<String>(),
                            format!("{}", 8 as char).repeat(input.len() - self.cursor_position - 1),
                        ));
                        self.cursor_position += 1;
                    }
                }
            }
            io::stdout().flush().unwrap();
        }
    }

    /// Expands input line with history expansion.
    fn history_expansion(&mut self, input: &str) -> HistoryExpansion {
        let mut processed = input.to_string();
        if let Some(last_command) = self.history.last() {
            processed = processed.replace("!!", last_command);
        }
        // for eg. "!12", "!-2"
        lazy_static! {
            static ref NUMBER_RE: Regex = Regex::new(r"(?:^|[^\[])!(-?\d+)").unwrap();
        }
        // for each match
        for captures in NUMBER_RE.captures_iter(input) {
            // get matched number
            let full_match = captures.get(0).unwrap().as_str();
            let group_match = captures.get(1).unwrap().as_str();
            let history_number = group_match.parse::<i32>().unwrap();
            let history_number = if history_number < 0 {
                (self.history.len() as i32 + history_number) as usize
            } else {
                (history_number - 1) as usize
            };
            // get that entry from history (if it exists)
            if let Some(history_cmd) = self.history.get(history_number) {
                // replace the match with the entry from history
                processed = processed.replace(full_match, history_cmd);
            } else {
                return HistoryExpansion::EventNotFound(full_match.into());
            }
        }

        // $ for eg. "!ls"
        lazy_static! {
            static ref STRING_RE: Regex = Regex::new(r"(?:^|[^\[])!(\w+)").unwrap();
        }
        // for each match
        // TODO: Clippy warns about redundant clone here, removing it produces errors
        // find out if there is a better solution that would satisfy Clippy
        #[allow(clippy::redundant_clone)]
        for captures in STRING_RE.captures_iter(&processed.clone()) {
            let full_match = captures.get(0).unwrap().as_str();
            let group_match = captures.get(1).unwrap().as_str();

            // find history entry starting with the match
            if let Some(history_cmd) = self
                .history
                .iter()
                .rev()
                .find(|entry| entry.starts_with(group_match))
            {
                // replace the match with the entry from history
                processed = processed.replace(full_match, history_cmd);
            } else {
                return HistoryExpansion::EventNotFound(full_match.into());
            }
        }

        if input == processed {
            HistoryExpansion::Unchanged
        } else {
            HistoryExpansion::Expanded(processed)
        }
    }

    pub fn run_interpreter(&mut self) -> Result<i32, Report> {
        if cfg!(target_os = "wasi") && self.should_echo {
            // disable echoing on hterm side (ignore Error that will arise on wasi runtimes other
            // than ours browser implementation (i. e. wasmer/wasmtime)
            #[cfg(target_os = "wasi")]
            let _ = wasi_ext_lib::set_echo(true);
        }

        #[cfg(target_os = "wasi")]
        {
            // TODO: see https://github.com/WebAssembly/wasi-filesystem/issues/24
            _ = wasi_ext_lib::chdir(&if let Ok(p) = wasi_ext_lib::getcwd() {
                p
            } else {
                String::from("/")
            });
        }

        if PathBuf::from(&self.history_path).exists() {
            self.history = fs::read_to_string(&self.history_path)
                .unwrap()
                .lines()
                .map(str::to_string)
                .collect();
        }

        let washrc_path = {
            if PathBuf::from(env::var("HOME").unwrap()).exists() {
                format!(
                    "{}/.{}rc",
                    env::var("HOME").unwrap(),
                    env!("CARGO_PKG_NAME")
                )
            } else {
                format!("{}/.{}rc", env::var("PWD").unwrap(), env!("CARGO_PKG_NAME"))
            }
        };
        if PathBuf::from(&washrc_path).exists() {
            self.run_script(washrc_path).unwrap();
        }

        let motd_path = PathBuf::from("/etc/motd");
        if motd_path.exists() {
            println!("{}", fs::read_to_string(motd_path).unwrap());
        }

        let mut input = String::new();
        // line loop
        loop {
            self.print_prompt(&input);
            if !self.get_line(&mut input)? {
                self.last_exit_status = EXIT_INTERRUPTED;
                input.clear();
                println!();
            }

            if input.is_empty() {
                continue;
            }

            match self.history_expansion(&input) {
                HistoryExpansion::Expanded(expanded) => {
                    input = expanded;
                    continue;
                }
                HistoryExpansion::EventNotFound(event) => {
                    eprintln!("{event}: event not found");
                }
                HistoryExpansion::Unchanged => {
                    if let Err(error) = self.handle_input(&input) {
                        eprintln!("{error:#?}");
                    };
                }
            }
            match OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.history_path)
            {
                Ok(mut file) => {
                    if Some(&input) != self.history.last() {
                        self.history.push(input.clone());
                        writeln!(file, "{}", &input).unwrap();
                    }
                }
                Err(error) => {
                    eprintln!(
                        "Unable to open file for storing {} history: {}",
                        env!("CARGO_PKG_NAME"),
                        error
                    );
                }
            };
            input.clear();
        }
    }

    fn handle_input(&mut self, input: &str) -> Result<i32, Report> {
        // TODO: define and use constructor
        let mut interpreter = InputInterpreter::from_input(input);
        Ok(interpreter.interpret(self))
    }

    pub fn execute_command(
        &mut self,
        command: &str,
        args: &mut Vec<String>,
        env: &HashMap<String, String>,
        background: bool,
        redirects: &[Redirect],
    ) -> Result<i32, Report> {
        let mut output_device = OutputDevice::new();
        if let Err(err) = preprocess_redirects(redirects, &mut output_device) {
            output_device.eprintln(format!("{}: {}", env!("CARGO_PKG_NAME"), err).as_str());
            output_device.flush()?;
            return Ok(EXIT_FAILURE);
        }

        let result: Result<i32, Report> = if let Some(internal) = INTERNALS_MAP.get(command) {
            internal(self, args, &mut output_device)
        } else {
            let full_path = if command.starts_with('/') {
                let full_path = PathBuf::from(command);
                if path_exists(full_path.to_str().unwrap())? {
                    Ok(full_path)
                } else {
                    Err(format!(
                        "{}: no such file or directory",
                        full_path.display()
                    ))
                }
            } else if command.starts_with('.') {
                let path = PathBuf::from(&self.pwd);
                let full_path = path.join(command);
                if path_exists(full_path.to_str().unwrap())? {
                    Ok(full_path)
                } else {
                    Err(format!(
                        "{}: no such file or directory",
                        full_path.display()
                    ))
                }
            } else {
                let mut found = false;
                let mut full_path = PathBuf::new();
                // get PATH env variable, split it and look for binaries in each directory
                for bin_dir in env::var("PATH").unwrap_or_default().split(':') {
                    let bin_dir = PathBuf::from(bin_dir);
                    full_path = bin_dir.join(command);
                    // see https://internals.rust-lang.org/t/the-api-of-path-exists-encourages-broken-code/13817/3
                    if path_exists(full_path.to_str().unwrap())? {
                        found = true;
                        break;
                    }
                }
                if found {
                    Ok(full_path)
                } else {
                    Err(format!("{command}: command not found"))
                }
            };

            match full_path {
                Ok(path) => {
                    let file = File::open(&path).unwrap();

                    let mut lines = BufReader::new(file).lines();
                    let reader_result = lines.next();
                    // close opened file to prevent fd leak
                    drop(lines);

                    if let Some(Ok(line)) = reader_result {
                        // file starts with valid UTF-8, most likely a script
                        let binary_path = if let Some(path) = line.strip_prefix("#!") {
                            path.trim().to_string()
                        } else {
                            env::var("SHELL").unwrap()
                        };
                        args.insert(0, binary_path);
                        args.insert(1, path.into_os_string().into_string().unwrap());
                        let args_: Vec<&str> = args.iter().map(|s| &**s).collect();
                        // TODO: we should not unwrap here
                        let (exit_status, child_pid) =
                            spawn(args_[0], &args_[1..], env, background, redirects).unwrap();

                        if background {
                            self.last_job_pid = Some(child_pid as u32);
                        }

                        Ok(exit_status)
                    } else {
                        // most likely WASM binary
                        args.insert(0, path.into_os_string().into_string().unwrap());
                        let args_: Vec<&str> = args.iter().map(|s| &**s).collect();
                        match spawn(args_[0], &args_[1..], env, background, redirects) {
                            // nonempty output message means that binary couldn't be executed
                            Err(e) => {
                                output_device.eprintln(&format!(
                                    "{}: could not execute binary (os error {})",
                                    env!("CARGO_PKG_NAME"),
                                    e
                                ));
                                Ok(EXIT_FAILURE)
                            }
                            Ok((exit_status, child_pid)) => {
                                if background {
                                    self.last_job_pid = Some(child_pid as u32);
                                }
                                Ok(exit_status)
                            }
                        }
                    }
                }
                Err(reason) => {
                    output_device.eprintln(&format!("{}: {}", env!("CARGO_PKG_NAME"), &reason));
                    Ok(EXIT_FAILURE)
                }
            }
        };

        output_device.flush()?;

        self.last_exit_status = if let Ok(exit_status) = result {
            exit_status
        } else {
            EXIT_CRITICAL_FAILURE
        };
        Ok(self.last_exit_status)
    }

    #[cfg(target_os = "wasi")]
    pub fn register_sigint(&mut self) -> Result<(), Report> {
        let event_source = InternalEventSource::default();
        if let Err(e) = wasi_ext_lib::attach_sigint(event_source.event_src.as_raw_fd()) {
            Err(Report::msg(format!(
                "Cannot attach SigInt event descriptor, error code = {e}!"
            )))
        } else {
            self.reader = InternalReader::StdinWithSigInt(event_source);
            Ok(())
        }
    }
}
