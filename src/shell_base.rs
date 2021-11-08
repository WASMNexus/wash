use std::env;
use std::fs;
use std::fs::OpenOptions;
use std::io;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::exit;
use std::thread;
use std::time::Duration;
use substring::Substring;

use conch_parser::lexer::Lexer;
use conch_parser::parse::DefaultParser;
use iterm2;

use crate::interpreter::interpret;

use std::collections::HashMap;

pub const EXIT_SUCCESS: i32 = 0;
pub const EXIT_FAILURE: i32 = 1;
pub const EXIT_CRITICAL_FAILURE: i32 = 2;
pub const EXIT_CMD_NOT_FOUND: i32 = 127;

pub struct SyscallResult {
    pub exit_status: i32,
    pub output: String,
}

// communicate with the worker thread
pub fn syscall(
    command: &str,
    args: &[&str],
    envs: &HashMap<String, String>,
    background: bool,
    #[allow(unused_variables)] redirects: &[(u16, String, String)],
) -> Result<SyscallResult, Box<dyn std::error::Error>> {
    #[cfg(target_os = "wasi")]
    let result = {
        let result = fs::read_link(format!(
            "/!{}",
            [
                command,
                &args.join("\x1b"),
                &envs
                    .iter()
                    .map(|(key, val)| format!("{}={}", key, val))
                    .collect::<Vec<_>>()
                    .join("\x1b"),
                &format!("{}", background),
                &redirects
                    .iter()
                    .map(|(fd, filename, operation)| format!("{} {} {}", fd, filename, operation))
                    .collect::<Vec<_>>()
                    .join("\x1b"),
            ]
            .join("\x1b\x1b")
        ))?
        .to_str()
        .unwrap()
        .trim_matches(char::from(0))
        .to_string();
        if !background {
            let (exit_status, output) = result.split_once("\x1b").unwrap();
            let exit_status = exit_status.parse::<i32>().unwrap();
            SyscallResult {
                exit_status,
                output: output.to_string(),
            }
        } else {
            SyscallResult {
                exit_status: 0,
                output: "".to_string(),
            }
        }
    };
    #[cfg(not(target_os = "wasi"))]
    let result = {
        if command == "spawn" {
            let mut spawned = std::process::Command::new(args[0])
                .args(&args[1..])
                .envs(envs)
                .spawn()?;
            // TODO: add redirects
            // TODO: return exit status from function
            if !background {
                let exit_status = spawned.wait()?.code().unwrap();
                SyscallResult {
                    exit_status,
                    output: "".to_string(),
                }
            } else {
                SyscallResult {
                    exit_status: 0,
                    output: "".to_string(),
                }
            }
        } else {
            SyscallResult {
                exit_status: 0,
                output: "".to_string(),
            }
        }
    };
    Ok(result)
}

pub struct Shell {
    pub pwd: String,
    pub history: Vec<String>,
    pub vars: HashMap<String, String>,
    pub should_echo: bool,
    pub last_exit_status: i32,
}

impl Shell {
    pub fn new(should_echo: bool, pwd: &str) -> Self {
        Shell {
            should_echo,
            pwd: pwd.to_string(),
            history: Vec::new(),
            vars: HashMap::new(),
            last_exit_status: 0,
        }
    }

    fn parse_prompt_string(&self) -> String {
        env::var("PS1")
            .unwrap_or_else(|_| "\\u@\\h:\\w$ ".to_string())
            .replace(
                "\\u",
                &env::var("USER").unwrap_or_else(|_| "user".to_string()),
            )
            .replace(
                "\\h",
                &env::var("HOSTNAME").unwrap_or_else(|_| "hostname".to_string()),
            )
            // FIXME: should only replace if it starts with HOME
            .replace("\\w", &self.pwd.replace(&env::var("HOME").unwrap(), "~"))
    }

    fn echo(&self, output: &str) {
        if self.should_echo {
            print!("{}", output);
        }
    }

    pub fn run_command(&mut self, command: &str) -> Result<(), Box<dyn std::error::Error>> {
        self.handle_input(command)
    }

    pub fn run_script(
        &mut self,
        script_name: impl Into<PathBuf>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.handle_input(&fs::read_to_string(script_name.into()).unwrap())
    }

    pub fn run_interpreter(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        // disable echoing on hterm side (ignore Error that will arise on platforms other than web
        let _ = syscall("set_echo", &["0"], &HashMap::new(), false, &[]);

        // TODO: see https://github.com/WebAssembly/wasi-filesystem/issues/24
        env::set_current_dir(env::var("PWD").unwrap()).unwrap();

        let history_path = {
            if PathBuf::from(env::var("HOME").unwrap()).exists() {
                format!("{}/.shell_history", env::var("HOME").unwrap())
            } else {
                format!("{}/.shell_history", env::var("PWD").unwrap())
            }
        };
        if PathBuf::from(&history_path).exists() {
            self.history = fs::read_to_string(&history_path)
                .unwrap()
                .lines()
                .map(str::to_string)
                .collect();
        }
        let mut shell_history = match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&history_path)
        {
            Ok(file) => Some(file),
            Err(error) => {
                eprintln!("Unable to open file for storing shell history: {}", error);
                None
            }
        };

        let shellrc_path = {
            if PathBuf::from(env::var("HOME").unwrap()).exists() {
                format!("{}/.shellrc", env::var("HOME").unwrap())
            } else {
                format!("{}/.shellrc", env::var("PWD").unwrap())
            }
        };
        if PathBuf::from(&shellrc_path).exists() {
            self.run_script(shellrc_path).unwrap();
        }

        let mut cursor_position = 0;

        let motd_path = PathBuf::from("/etc/motd");
        if motd_path.exists() {
            println!("{}", fs::read_to_string(motd_path).unwrap());
        }

        loop {
            let mut input = String::new();
            let mut input_stash = String::new();
            print!("{}", self.parse_prompt_string());
            io::stdout().flush().unwrap();

            let mut c1 = [0];
            let mut escaped = false;
            let mut history_entry_to_display: i32 = -1;
            // read line
            loop {
                // this is to handle EOF when piping to shell
                match io::stdin().read_exact(&mut c1) {
                    Ok(()) => {}
                    Err(_) => exit(0),
                }
                if escaped {
                    match c1[0] {
                        0x5b => {
                            let mut c2 = [0];
                            io::stdin().read_exact(&mut c2).unwrap();
                            match c2[0] {
                                0x32 | 0x33 | 0x35 | 0x36 => {
                                    let mut c3 = [0];
                                    io::stdin().read_exact(&mut c3).unwrap();
                                    match [c2[0], c3[0]] {
                                        [0x35, 0x7e] => {
                                            println!("TODO: PAGE UP");
                                            escaped = false;
                                        }
                                        [0x36, 0x7e] => {
                                            println!("TODO: PAGE DOWN");
                                            escaped = false;
                                        }
                                        [0x32, 0x7e] => {
                                            println!("TODO: INSERT");
                                            escaped = false;
                                        }
                                        // delete key
                                        [0x33, 0x7e] => {
                                            if input.len() - cursor_position > 0 {
                                                self.echo(
                                                    &" ".repeat(input.len() - cursor_position + 1),
                                                );
                                                input.remove(cursor_position);
                                                self.echo(
                                                    &format!("{}", 8 as char)
                                                        .repeat(input.len() - cursor_position + 2),
                                                );
                                                self.echo(
                                                    &input
                                                        .chars()
                                                        .skip(cursor_position)
                                                        .collect::<String>(),
                                                );
                                                self.echo(
                                                    &format!("{}", 8 as char)
                                                        .repeat(input.len() - cursor_position),
                                                );
                                            }
                                            escaped = false;
                                        }
                                        [0x33, 0x3b] => {
                                            println!("TODO: SHIFT + DELETE");
                                            let mut c4 = [0];
                                            // TWO MORE! TODO: improve!
                                            io::stdin().read_exact(&mut c4).unwrap();
                                            io::stdin().read_exact(&mut c4).unwrap();
                                            escaped = false;
                                        }
                                        _ => {
                                            println!(
                                                "TODO: [ + 0x{:02x} + 0x{:02x}",
                                                c2[0] as u8, c3[0] as u8
                                            );
                                            escaped = false;
                                        }
                                    }
                                }
                                // up arrow
                                0x41 => {
                                    if !self.history.is_empty() && history_entry_to_display != 0 {
                                        if history_entry_to_display == -1 {
                                            history_entry_to_display =
                                                (self.history.len() - 1) as i32;
                                            input_stash = input.clone();
                                        } else if history_entry_to_display > 0 {
                                            history_entry_to_display -= 1;
                                        }
                                        // bring cursor to the end so that clearing later starts from
                                        // proper position
                                        self.echo(
                                            &input
                                                .chars()
                                                .skip(cursor_position)
                                                .collect::<String>(),
                                        );
                                        for _ in 0..input.len() {
                                            self.echo(&format!("{} {}", 8 as char, 8 as char));
                                        }
                                        input =
                                            self.history[history_entry_to_display as usize].clone();
                                        cursor_position = input.len();
                                        self.echo(&input);
                                    }
                                    escaped = false;
                                }
                                // down arrow
                                0x42 => {
                                    if history_entry_to_display != -1 {
                                        // bring cursor to the end so that clearing later starts from
                                        // proper position
                                        self.echo(
                                            &input
                                                .chars()
                                                .skip(cursor_position)
                                                .collect::<String>(),
                                        );
                                        for _ in 0..input.len() {
                                            self.echo(&format!("{} {}", 8 as char, 8 as char));
                                            // '\b \b', clear left of cursor
                                        }
                                        if self.history.len() - 1
                                            > (history_entry_to_display as usize)
                                        {
                                            history_entry_to_display += 1;
                                            input = self.history[history_entry_to_display as usize]
                                                .clone();
                                        } else {
                                            input = input_stash.clone();
                                            history_entry_to_display = -1;
                                        }
                                        cursor_position = input.len();
                                        self.echo(&input);
                                    }
                                    escaped = false;
                                }
                                // right arrow
                                0x43 => {
                                    if cursor_position < input.len() {
                                        self.echo(
                                            &input
                                                .chars()
                                                .nth(cursor_position)
                                                .unwrap()
                                                .to_string(),
                                        );
                                        cursor_position += 1;
                                    }
                                    escaped = false;
                                }
                                // left arrow
                                0x44 => {
                                    if cursor_position > 0 {
                                        self.echo(&format!("{}", 8 as char));
                                        cursor_position -= 1;
                                    }
                                    escaped = false;
                                }
                                // end key
                                0x46 => {
                                    self.echo(
                                        &input.chars().skip(cursor_position).collect::<String>(),
                                    );
                                    cursor_position = input.len();
                                    escaped = false;
                                }
                                // home key
                                0x48 => {
                                    self.echo(&format!("{}", 8 as char).repeat(cursor_position));
                                    cursor_position = 0;
                                    escaped = false;
                                }
                                _ => {
                                    println!("WE HAVE UNKNOWN CONTROL CODE '[' + {}", c2[0] as u8);
                                    escaped = false;
                                }
                            }
                        }
                        _ => {
                            escaped = false;
                        }
                    }
                } else {
                    if c1[0] != 0x1b {
                        history_entry_to_display = -1;
                    }
                    match c1[0] {
                        // enter
                        10 => {
                            input = input.trim().to_string();
                            self.echo("\n");
                            cursor_position = 0;
                            break;
                        }
                        // backspace
                        127 => {
                            if !input.is_empty() && cursor_position > 0 {
                                self.echo(&format!("{}", 8 as char));
                                self.echo(&" ".repeat(input.len() - cursor_position + 1));
                                input.remove(cursor_position - 1);
                                cursor_position -= 1;
                                self.echo(
                                    &format!("{}", 8 as char)
                                        .repeat(input.len() - cursor_position + 1),
                                );
                                self.echo(&input.chars().skip(cursor_position).collect::<String>());
                                self.echo(
                                    &format!("{}", 8 as char).repeat(input.len() - cursor_position),
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
                            input.insert(cursor_position, c1[0] as char);
                            // echo
                            self.echo(&format!(
                                "{}{}",
                                input.chars().skip(cursor_position).collect::<String>(),
                                format!("{}", 8 as char).repeat(input.len() - cursor_position - 1),
                            ));
                            cursor_position += 1;
                        }
                    }
                }
                io::stdout().flush().unwrap();
            }

            // handle line

            // TODO: incorporate this into interpreter of parsed input

            if input.replace(" ", "").is_empty() {
                continue;
            }

            // handle '!' history
            if input.starts_with('!') {
                let sbstr = input
                    .split_whitespace()
                    .next()
                    .unwrap()
                    .substring(1, 64)
                    .split_whitespace()
                    .next()
                    .unwrap();
                let history_entry_id: usize = sbstr.parse().unwrap_or_else(|_| {
                    if sbstr.is_empty() {
                        return 0;
                    }
                    let mut j = 0;
                    let mut found = 0;
                    for entry in &self.history {
                        j += 1;
                        if entry.substring(0, sbstr.len()) == sbstr {
                            found = j;
                            break;
                        }
                    }
                    found
                });
                if history_entry_id == 0 || self.history.len() < history_entry_id {
                    if sbstr.is_empty() {
                        println!("!{}: event not found", sbstr);
                    }
                    input.clear();
                    continue;
                } else {
                    let input = format!(
                        "{}{}",
                        self.history[history_entry_id - 1],
                        input.strip_prefix(&format!("!{}", sbstr)).unwrap()
                    );
                    cursor_position = input.len();
                }
            }

            // only write to file if it was successfully created
            if let Some(ref mut shell_history) = shell_history {
                // don't push !commands and duplicates of last command
                if input.substring(0, 1) != "!" && Some(&input) != self.history.last() {
                    self.history.push(input.clone());
                    writeln!(shell_history, "{}", &input).unwrap();
                }
            }

            if let Err(error) = self.handle_input(&input) {
                println!("{:#?}", error);
            };
        }
    }

    fn handle_input(&mut self, input: &str) -> Result<(), Box<dyn std::error::Error>> {
        let lex = Lexer::new(input.chars());
        let parser = DefaultParser::new(lex);
        for cmd in parser {
            match cmd {
                Ok(cmd) => interpret(self, &cmd),
                Err(e) => {
                    println!("{:?}", e);
                }
            }
        }
        Ok(())
    }

    // TODO: return exit status code
    pub fn execute_command(
        &mut self,
        command: &str,
        args: &mut Vec<String>,
        env: &HashMap<String, String>,
        background: bool,
        redirects: &[(u16, String, String)],
    ) -> Result<i32, Box<dyn std::error::Error>> {
        let result: Result<i32, Box<dyn std::error::Error>> = match command {
            // built in commands
            "clear" => {
                print!("\x1b[2J\x1b[H");
                Ok(EXIT_SUCCESS)
            }
            "exit" => {
                let exit_code: i32 = {
                    if args.is_empty() {
                        0
                    } else {
                        args[0].parse().unwrap()
                    }
                };
                exit(exit_code);
            }
            "pwd" => {
                println!("{}", env::current_dir().unwrap().display());
                Ok(EXIT_SUCCESS)
            }
            "cd" => {
                let path = if args.is_empty() {
                    PathBuf::from(env::var("HOME").unwrap())
                } else if args[0] == "-" {
                    PathBuf::from(env::var("OLDPWD").unwrap())
                } else if args[0].starts_with('/') {
                    PathBuf::from(&args[0])
                } else {
                    PathBuf::from(&self.pwd).join(&args[0])
                };

                // simply including this in source breaks shell
                if !Path::new(&path).exists() {
                    println!("cd: {}: No such file or directory", path.display());
                    Ok(EXIT_FAILURE)
                } else {
                    let metadata = fs::metadata(&path).unwrap();
                    if metadata.is_file() {
                        println!("cd: {}: Not a directory", path.display());
                        Ok(EXIT_FAILURE)
                    } else {
                        // TODO: for both targets, chain the commands and exit early if previous
                        // step fails
                        #[cfg(target_os = "wasi")]
                        {
                            syscall(
                                "set_env",
                                &["OLDPWD", env::current_dir().unwrap().to_str().unwrap()],
                                env,
                                background,
                                &[],
                            )
                            .unwrap();
                            let pwd =
                                syscall("chdir", &[path.to_str().unwrap()], env, background, &[])
                                    .unwrap()
                                    .output;
                            syscall("set_env", &["PWD", &pwd], env, background, &[]).unwrap();
                            self.pwd = PathBuf::from(&pwd).display().to_string();
                            Ok(EXIT_SUCCESS)
                        }
                        #[cfg(not(target_os = "wasi"))]
                        {
                            env::set_var("OLDPWD", env::current_dir().unwrap().to_str().unwrap());
                            let pwd_path = fs::canonicalize(path).unwrap();
                            self.pwd = String::from(pwd_path.to_str().unwrap());
                            env::set_var("PWD", &self.pwd);
                            env::set_current_dir(&pwd_path).unwrap();
                            Ok(EXIT_SUCCESS)
                        }
                    }
                }
            }
            "history" => {
                for (i, history_entry) in self.history.iter().enumerate() {
                    println!("{}: {}", i + 1, history_entry);
                }
                Ok(EXIT_SUCCESS)
            }
            "unset" => {
                if args.is_empty() {
                    println!("unset: help: unset <VAR> [<VAR>] ...");
                    return Ok(EXIT_FAILURE);
                }
                for arg in args {
                    if arg == "PWD" || arg == "HOME" {
                        println!("unset: cannot unset {}", &arg);
                    } else {
                        self.vars.remove(arg);
                        if env::var(&arg).is_ok() {
                            env::remove_var(&arg);
                            syscall("set_env", &[arg], env, background, &[]).unwrap();
                        }
                    }
                }
                Ok(EXIT_SUCCESS)
            }
            "declare" => {
                if args.is_empty() {
                    // TODO: we should join and sort the variables!
                    for (key, value) in self.vars.iter() {
                        println!("{}={}", key, value);
                    }
                    for (key, value) in env::vars() {
                        println!("{}={}", key, value);
                    }
                } else if args[0] == "-x" || args[0] == "+x" {
                    // if -x is provided declare works as export
                    // if +x then makes global var local
                    for arg in args.iter().skip(1) {
                        if args[0] == "-x" {
                            if let Some((key, value)) = arg.split_once("=") {
                                syscall("set_env", &[key, value], env, background, &[]).unwrap();
                            }
                        } else if let Some((key, value)) = arg.split_once("=") {
                            syscall("set_env", &[key], env, background, &[]).unwrap();
                            self.vars.insert(key.to_string(), value.to_string());
                        } else {
                            let value = env::var(arg).unwrap();
                            syscall("set_env", &[arg], env, background, &[]).unwrap();
                            self.vars.insert(arg.clone(), value.clone());
                        }
                    }
                } else {
                    for arg in args {
                        if let Some((key, value)) = arg.split_once("=") {
                            self.vars.insert(key.to_string(), value.to_string());
                        }
                    }
                }
                Ok(EXIT_SUCCESS)
            }
            "export" => {
                // export creates an env value if A=B notation is used, or just
                // copies a local var to env if no "=" is used.
                // export on unexisting local var exports empty variable.
                if args.is_empty() {
                    println!("export: help: export <VAR>[=<VALUE>] [<VAR>[=<VALUE>]] ...");
                    return Ok(EXIT_FAILURE);
                }
                for arg in args {
                    if let Some((key, value)) = arg.split_once("=") {
                        self.vars.remove(key);
                        env::set_var(&key, &value);
                        syscall("set_env", &[key, value], env, background, &[]).unwrap();
                    } else if let Some(value) = self.vars.remove(arg) {
                        env::set_var(&arg, &value);
                        syscall("set_env", &[arg, &value], env, background, &[]).unwrap();
                    } else {
                        env::set_var(&arg, "");
                        syscall("set_env", &[arg, ""], env, background, &[]).unwrap();
                    }
                }
                Ok(EXIT_SUCCESS)
            }
            "source" => {
                if let Some(filename) = args.get(0) {
                    self.run_script(filename).unwrap();
                    Ok(EXIT_SUCCESS)
                } else {
                    println!("source: help: source <filename>");
                    Ok(EXIT_FAILURE)
                }
            }
            "write" => {
                if args.len() < 2 {
                    println!("write: help: write <filename> <contents>");
                    Ok(EXIT_FAILURE)
                } else {
                    match fs::write(args.remove(0), args.join(" ")) {
                        Ok(_) => Ok(EXIT_SUCCESS),
                        Err(error) => {
                            println!("write: failed to write to file: {}", error);
                            Ok(EXIT_FAILURE)
                        }
                    }
                }
            }
            "imgcat" => {
                if args.is_empty() {
                    println!("usage: imgcat <IMAGE>");
                    Ok(EXIT_FAILURE)
                } else {
                    // TODO: find out why it breaks the order of prompt
                    iterm2::File::read(&args[0])
                        .unwrap()
                        .width(iterm2::Dimension::Auto)
                        .height(iterm2::Dimension::Auto)
                        .preserve_aspect_ratio(true)
                        .show()
                        .unwrap();
                    Ok(EXIT_SUCCESS)
                }
            }
            "unzip" => {
                if let Some(filepath) = &args.get(0) {
                    let file = fs::File::open(&PathBuf::from(filepath)).unwrap();
                    let mut archive = zip::ZipArchive::new(file).unwrap();
                    for i in 0..archive.len() {
                        let mut file = archive.by_index(i).unwrap();
                        let outpath = file.enclosed_name().to_owned().unwrap();
                        if file.name().ends_with('/') {
                            println!("creating dir {}", outpath.display());
                            fs::create_dir_all(&outpath).unwrap();
                            continue;
                        }
                        if let Some(parent) = outpath.parent() {
                            if !parent.exists() {
                                println!("creating dir {}", parent.display());
                                fs::create_dir_all(&parent).unwrap();
                            }
                        }
                        println!("decompressing {}", file.enclosed_name().unwrap().display());
                        let mut outfile = fs::File::create(&outpath).unwrap();
                        io::copy(&mut file, &mut outfile).unwrap();
                        println!(
                            "decompressing {} done.",
                            file.enclosed_name().unwrap().display()
                        );
                    }
                    Ok(EXIT_SUCCESS)
                } else {
                    println!("unzip: missing operand");
                    Ok(EXIT_FAILURE)
                }
            }
            "sleep" => {
                // TODO: requires poll_oneoff implementation
                if let Some(sec_str) = &args.get(0) {
                    if let Ok(sec) = sec_str.parse() {
                        thread::sleep(Duration::new(sec, 0));
                        Ok(EXIT_SUCCESS)
                    } else {
                        println!("sleep: invalid time interval `{}`", sec_str);
                        Ok(EXIT_FAILURE)
                    }
                } else {
                    println!("sleep: missing operand");
                    Ok(EXIT_FAILURE)
                }
            }
            "hexdump" => {
                if args.is_empty() {
                    println!("hexdump: help: hexump <filename>");
                    Ok(EXIT_FAILURE)
                } else {
                    let contents = fs::read(args.remove(0)).unwrap_or_else(|_| {
                        println!("hexdump: error: file not found.");
                        return vec![];
                    });
                    let len = contents.len();
                    let mut v = ['.'; 16];
                    for j in 0..len {
                        let c = contents[j] as char;
                        v[j % 16] = c;
                        if (j % 16) == 0 {
                            print!("{:08x} ", j);
                        }
                        if (j % 8) == 0 {
                            print!(" ");
                        }
                        print!("{:02x} ", c as u8);
                        if (j + 1) == len || (j % 16) == 15 {
                            let mut count = 16;
                            if (j + 1) == len {
                                count = len % 16;
                                for _ in 0..(16 - (len % 16)) {
                                    print!("   ");
                                }
                                if count < 8 {
                                    print!(" ");
                                }
                            }
                            print!(" |");
                            for c in v.iter_mut().take(count) {
                                if (0x20..0x7e).contains(&(*c as u8)) {
                                    print!("{}", *c as char);
                                    *c = '.';
                                } else {
                                    print!(".");
                                }
                            }
                            println!("|");
                        }
                    }
                    Ok(EXIT_SUCCESS)
                }
            }
            "mkdir" | "rmdir" | "touch" | "rm" | "mv" | "cp" | "echo" | "date" | "ls"
            | "printf" | "env" | "cat" | "realpath" | "ln" | "printenv" | "md5sum" | "wc" => {
                args.insert(0, command.to_string());
                #[cfg(target_os = "wasi")]
                args.insert(0, String::from("/usr/bin/coreutils"));
                #[cfg(not(target_os = "wasi"))]
                args.insert(0, String::from("/bin/busybox"));
                let args_: Vec<&str> = args.iter().map(|s| &**s).collect();
                Ok(syscall("spawn", &args_[..], env, background, redirects)
                    .unwrap()
                    .exit_status)
            }
            // external commands or command not found
            _ => {
                let fullpath = if command.starts_with('/') {
                    let fullpath = PathBuf::from(command);
                    if fullpath.is_file() {
                        Ok(fullpath)
                    } else {
                        Err(format!(
                            "shell: no such file or directory: {}",
                            fullpath.display()
                        ))
                    }
                } else if command.starts_with('.') {
                    let path = PathBuf::from(&self.pwd);
                    let fullpath = path.join(command);
                    if fullpath.is_file() {
                        Ok(fullpath)
                    } else {
                        Err(format!(
                            "shell: no such file or directory: {}",
                            fullpath.display()
                        ))
                    }
                } else {
                    let mut found = false;
                    let mut fullpath = PathBuf::new();
                    // get PATH env variable, split it and look for binaries in each directory
                    for bin_dir in env::var("PATH").unwrap_or_default().split(':') {
                        let bin_dir = PathBuf::from(bin_dir);
                        fullpath = bin_dir.join(&command);
                        if fullpath.is_file() {
                            found = true;
                            break;
                        }
                    }
                    if found {
                        Ok(fullpath)
                    } else {
                        Err(format!("command not found: {}", command))
                    }
                };

                match fullpath {
                    Ok(path) => {
                        args.insert(0, path.display().to_string());
                        let args_: Vec<&str> = args.iter().map(|s| &**s).collect();
                        Ok(syscall("spawn", &args_[..], env, background, redirects)
                            .unwrap()
                            .exit_status)
                    }
                    Err(reason) => {
                        println!("{}", reason);
                        Ok(EXIT_FAILURE)
                    }
                }
            }
        };
        self.last_exit_status = if let Ok(exit_status) = result {
            exit_status
        } else {
            EXIT_CRITICAL_FAILURE
        };
        Ok(self.last_exit_status)
    }
}
