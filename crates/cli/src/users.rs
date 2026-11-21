//! `rxh user`: edits the users file of the admin API directly, without the
//! daemon. The way back in when every password is lost.

use std::fs;
use std::io::{self, BufRead, IsTerminal, Read};
use std::path::Path;
use std::process;

use rxh_core::config::Admin;
use rxh_server::access::{self, ADMIN_ROLE, Store};

use crate::cli::{Cli, UserCommand};
use crate::commands;

/// Runs one `rxh user` command on `[admin] users` of the configuration file.
pub(crate) fn execute(cli: &Cli, command: UserCommand) -> Result<(), String> {
    let path = commands::config_path(cli);
    let store = Store::open(access::users_file(Some(&path), &users_setting(&path)?));
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

/// `[admin] users` of the configuration file at `path`, which only the main
/// file can set. Nothing else of the file is checked and no placeholder is
/// replaced: the secrets a configuration takes from the environment of the
/// service are not in the shell of whoever lost a password.
fn users_setting(path: &Path) -> Result<String, String> {
    let invalid = |reason: &dyn std::fmt::Display| format!("{}: {reason}", path.display());
    let text = fs::read_to_string(path).map_err(|err| invalid(&err))?;
    let document: toml::Table = text.parse().map_err(|err| invalid(&err))?;
    let users = match document.get("admin").and_then(|admin| admin.get("users")) {
        None => return Ok(Admin::default().users),
        Some(toml::Value::String(users)) if !users.is_empty() => users,
        Some(_) => return Err(invalid(&"[admin] users must be the path of a file")),
    };
    if users.contains("{env.") || users.contains("{file.") {
        return Err(invalid(
            &"[admin] users holds a placeholder, which rxh user does not replace; \
              write the path of the users file there",
        ));
    }
    Ok(users.clone())
}

/// The new password: asked twice without echo on a terminal, else the first
/// line of standard input.
fn read_password() -> Result<String, String> {
    let cannot_read = |err: io::Error| format!("cannot read the password: {err}");
    if !io::stdin().is_terminal() {
        let mut line = String::new();
        io::stdin()
            .lock()
            .read_line(&mut line)
            .map_err(cannot_read)?;
        let password = line.trim_end_matches(['\r', '\n']);
        if password.is_empty() {
            return Err(String::from("no password on standard input"));
        }
        return Ok(password.to_owned());
    }
    eprint!("Password: ");
    let password = read_hidden().map_err(cannot_read)?;
    eprint!("Password again: ");
    if read_hidden().map_err(cannot_read)? != password {
        return Err(String::from("the passwords differ"));
    }
    Ok(password)
}

/// One line typed on the terminal of standard input, not shown. The
/// interrupt key (Ctrl-C) puts the terminal back and exits with status 130,
/// as the signal would have, which would have left the echo off.
#[cfg(unix)]
fn read_hidden() -> io::Result<String> {
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
    let keys = Keys {
        interrupt: saved.c_cc[libc::VINTR],
        erase: saved.c_cc[libc::VERASE],
        end_of_file: saved.c_cc[libc::VEOF],
    };
    // Key by key, with the special keys read as bytes rather than acted on.
    termios.c_lflag &= !(libc::ECHO | libc::ICANON | libc::ISIG);
    termios.c_cc[libc::VMIN] = 1;
    termios.c_cc[libc::VTIME] = 0;
    // SAFETY: as above; `termios` is the current state with flags changed.
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &termios) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let typed = typed(&mut io::stdin().lock(), &keys);
    // SAFETY: as above; `saved` is the state `tcgetattr` returned.
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &saved) } != 0 {
        let err = io::Error::last_os_error();
        eprintln!("\nrxh: cannot turn the echo of the terminal back on ({err}): run `stty sane`");
    }
    // The Enter key was not shown either.
    eprintln!();
    match typed? {
        Typed::Interrupted => process::exit(130),
        Typed::Line(bytes) => String::from_utf8(bytes)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "the password is not UTF-8")),
    }
}

/// Elsewhere the password shows while it is typed.
#[cfg(not(unix))]
fn read_hidden() -> io::Result<String> {
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}

/// The special keys of a terminal, from its settings.
#[cfg(unix)]
struct Keys {
    interrupt: u8,
    erase: u8,
    end_of_file: u8,
}

/// What was typed at a hidden prompt.
#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
enum Typed {
    Line(Vec<u8>),
    Interrupted,
}

/// Reads keys from `input` until Enter, end of file or the interrupt key.
/// The erase key (and Backspace either way) removes the last character.
#[cfg(unix)]
fn typed(input: &mut impl Read, keys: &Keys) -> io::Result<Typed> {
    let mut line: Vec<u8> = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        if input.read(&mut byte)? == 0 {
            return Ok(Typed::Line(line));
        }
        match byte[0] {
            b'\n' | b'\r' => return Ok(Typed::Line(line)),
            key if key == keys.interrupt => return Ok(Typed::Interrupted),
            key if key == keys.end_of_file => return Ok(Typed::Line(line)),
            key if key == keys.erase || key == 0x7f || key == 0x08 => {
                // A whole character: its UTF-8 continuation bytes, then its
                // first byte.
                while line.pop().is_some_and(|last| last & 0xc0 == 0x80) {}
            }
            key => line.push(key),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    const KEYS: Keys = Keys {
        interrupt: 0x03,
        erase: 0x7f,
        end_of_file: 0x04,
    };

    fn typed_from(input: &[u8]) -> Typed {
        typed(&mut &input[..], &KEYS).expect("a slice reads without error")
    }

    #[test]
    fn keys_edit_the_hidden_line() {
        assert_eq!(typed_from(b"secret\nrest"), Typed::Line(b"secret".to_vec()));
        assert_eq!(
            typed_from(b"secrex\x7ft\r"),
            Typed::Line(b"secret".to_vec())
        );
        assert_eq!(
            typed_from(b"secrex\x08t\r"),
            Typed::Line(b"secret".to_vec())
        );
        // Backspace removes the two bytes of an `é`, and nothing on an empty line.
        assert_eq!(
            typed_from("\x7fcaf\u{e9}\x7fe\n".as_bytes()),
            Typed::Line(b"cafe".to_vec())
        );
        assert_eq!(typed_from(b"sec\x03ret\n"), Typed::Interrupted);
        assert_eq!(typed_from(b"secret\x04"), Typed::Line(b"secret".to_vec()));
        assert_eq!(typed_from(b"secret"), Typed::Line(b"secret".to_vec()));
    }
}
