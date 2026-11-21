//! `rxh user`: edits the users file of the admin API directly, without the
//! daemon. The way back in when every password is lost.

use std::io::{self, BufRead, IsTerminal};
use std::path::PathBuf;

use rxh_server::access::{ADMIN_ROLE, Store};

use crate::cli::{Cli, UserCommand};
use crate::commands;

/// Runs one `rxh user` command on `[admin] users` of the configuration file.
pub(crate) fn execute(cli: &Cli, command: UserCommand) -> Result<(), String> {
    let config =
        rxh_server::load_config(&commands::config_path(cli)).map_err(|err| err.to_string())?;
    // Relative to the working directory, as the daemon opens it.
    let store = Store::open(PathBuf::from(&config.admin.users));
    match command {
        UserCommand::List => {
            for user in store.read().user {
                if user.roles.is_empty() {
                    println!("{}", user.name);
                } else {
                    println!("{} {}", user.name, user.roles.join(","));
                }
            }
            Ok(())
        }
        UserCommand::Add { name, roles } => {
            let password = read_password()?;
            store.change(|access| {
                let roles = if roles.is_empty() && access.user.is_empty() {
                    vec![ADMIN_ROLE.to_owned()]
                } else {
                    roles
                };
                access.add_user(&name, &password, &roles)
            })
        }
        UserCommand::Passwd { name } => {
            let password = read_password()?;
            store.change(|access| access.set_password(&name, &password))
        }
        UserCommand::Remove { name } => store.change(|access| access.remove_user(&name)),
    }
}

/// The new password: asked twice without echo on a terminal, else the first
/// line of standard input.
fn read_password() -> Result<String, String> {
    if !io::stdin().is_terminal() {
        let password = read_line(/* echo= */ true)?;
        if password.is_empty() {
            return Err(String::from("no password on standard input"));
        }
        return Ok(password);
    }
    eprint!("Password: ");
    let password = read_line(/* echo= */ false)?;
    eprint!("Password again: ");
    if read_line(/* echo= */ false)? != password {
        return Err(String::from("the passwords differ"));
    }
    Ok(password)
}

/// One line of standard input without its line ending. Without `echo`, the
/// terminal does not show what is typed.
fn read_line(echo: bool) -> Result<String, String> {
    let read = || {
        let mut line = String::new();
        io::stdin().lock().read_line(&mut line).map(|_| line)
    };
    let line = if echo {
        read()
    } else {
        let line = without_echo(read);
        // The Enter key was not shown either.
        eprintln!();
        line
    };
    let line = line.map_err(|err| format!("cannot read the password: {err}"))?;
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}

/// Runs `read` with the echo of the terminal on standard input turned off.
// shortcut: Ctrl-C while reading leaves the echo off until `reset`, add a
// signal handler if users hit it.
#[cfg(unix)]
fn without_echo(read: impl FnOnce() -> io::Result<String>) -> io::Result<String> {
    use std::os::fd::AsRawFd;

    let fd = io::stdin().as_raw_fd();
    // SAFETY: `termios` is plain data, for which zeroes are a valid value;
    // `tcgetattr` fills it in.
    let mut termios: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: `fd` is standard input, open for the whole process, and
    // `termios` is a valid, writable struct.
    if unsafe { libc::tcgetattr(fd, &mut termios) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let saved = termios;
    termios.c_lflag &= !libc::ECHO;
    // SAFETY: as above; `termios` is the current state with one flag off.
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &termios) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let line = read();
    // SAFETY: as above; `saved` is the state `tcgetattr` returned.
    unsafe { libc::tcsetattr(fd, libc::TCSANOW, &saved) };
    line
}

/// Elsewhere the password shows while it is typed.
#[cfg(not(unix))]
fn without_echo(read: impl FnOnce() -> io::Result<String>) -> io::Result<String> {
    read()
}
