use std::fs::File;
use std::io;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::exit;

fn main() {
    let mut pwd = PathBuf::from("/");
    let mut input = String::new();

    loop {
        // prompt for input
        print!("$ ");
        io::stdout().flush().unwrap();

        let mut c = [0];
        // read line
        loop {
            io::stdin().read_exact(&mut c).unwrap();
            match c[0] {
                // enter
                10 => {
                    println!();
                    break;
                }
                // backspace
                127 => {
                    if !input.is_empty() {
                        input.remove(input.len() - 1);
                        print!("{} {}", 8 as char, 8 as char); // '\b \b', clear left of cursor
                    }
                }
                // control codes
                code if code < 32 => {
                    // ignore for now
                }
                // regular characters
                _ => {
                    input.push(c[0] as char);
                    // echo
                    print!("{}", c[0] as char);
                }
            }
            io::stdout().flush().unwrap();
        }

        // handle line
        let mut words = input.split_whitespace();
        let command = words.next().unwrap_or_default();
        let args: Vec<_> = words.collect();

        match command {
            // built in commands
            "echo" => println!("{}", args.join(" ")),
            "cd" => {
                if args.is_empty() {
                    pwd = PathBuf::from("/");
                } else {
                    let path = args[0];

                    let new_path = if path.starts_with("/") {
                        PathBuf::from(path)
                    } else {
                        pwd.join(path)
                    };

                    // // simply including this in source breaks shell
                    // if !Path::new(&new_pwd).exists() {
                    //     println!("cd: no such file or directory: {}", new_pwd);
                    // } else {
                    //     pwd = new_pwd;
                    // }
                    pwd = new_path;
                }
            }
            "pwd" => println!("{}", pwd.display()),
            "exit" => exit(0),
            // external commands
            "duk" | "main" | "shell" => {
                File::open(format!("!{}", command));
            }
            // edge cases
            "" => {}
            _ => println!("command not found: {}", command),
        }
        input.clear();
    }
}
