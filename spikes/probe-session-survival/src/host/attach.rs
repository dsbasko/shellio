//! Attach-клиент Rust-хоста (`probe-session-attach`): протокол и байты без VT. Состояние сессии
//! приходит от хоста VT-байтами (`Restore`) до живого потока, клиент передаёт их терминалу как
//! есть. Клиент подключается только к опубликованному сокету и сессию не создаёт. Терминал
//! клиента переводится в raw-режим; `Ctrl-\` отключает клиента, конец терминала (hangup) завершает
//! его. Роль клиент сообщает своему терминалу заголовком `<префикс><роль>:<поколение>`, как
//! управляемый клиент zmx.

use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::thread;

use crate::backend::{Lead, Role};
use crate::proto::{self, Hello, Message, Purpose};
use crate::pty::{self, Winsize};

pub const ATTACH_NAME: &str = "probe-session-attach";
/// `Ctrl-\`.
pub const DETACH_KEY: u8 = 0x1c;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    pub socket: PathBuf,
    pub lead: Lead,
    pub role_prefix: Option<String>,
}

impl Options {
    pub fn to_args(&self) -> Vec<String> {
        let mut args = vec![
            "--socket".to_owned(),
            self.socket.display().to_string(),
            "--lead".to_owned(),
            match self.lead {
                Lead::IfFree => "if-free",
                Lead::Claim => "claim",
            }
            .to_owned(),
        ];
        if let Some(prefix) = &self.role_prefix {
            args.extend(["--role-prefix".to_owned(), prefix.clone()]);
        }
        args
    }

    pub fn parse(args: &[String]) -> Result<Options, String> {
        let (mut socket, mut lead, mut role_prefix) = (None, Lead::IfFree, None);
        let mut rest = args.iter();
        while let Some(flag) = rest.next() {
            let value = rest.next().ok_or_else(|| format!("{flag}: нет значения"))?;
            match flag.as_str() {
                "--socket" => socket = Some(PathBuf::from(value)),
                "--lead" => {
                    lead = match value.as_str() {
                        "if-free" => Lead::IfFree,
                        "claim" => Lead::Claim,
                        other => return Err(format!("--lead {other}")),
                    }
                }
                "--role-prefix" => role_prefix = Some(value.clone()),
                other => return Err(format!("неизвестный флаг {other}")),
            }
        }
        Ok(Options {
            socket: socket.ok_or("нет --socket")?,
            lead,
            role_prefix,
        })
    }
}

/// Заголовок роли для терминала клиента.
pub fn role_title(prefix: &str, role: Role, generation: u32) -> Vec<u8> {
    format!("\x1b]2;{prefix}{}:{generation}\x07", role.name()).into_bytes()
}

/// Ввод терминала до клавиши отключения и признак, что она нажата.
pub fn split_detach(bytes: &[u8]) -> (&[u8], bool) {
    match bytes.iter().position(|byte| *byte == DETACH_KEY) {
        Some(at) => (&bytes[..at], true),
        None => (bytes, false),
    }
}

pub fn main(args: &[String]) -> i32 {
    let options = match Options::parse(args) {
        Ok(options) => options,
        Err(err) => {
            eprintln!("{ATTACH_NAME}: {err}");
            return 2;
        }
    };
    match run(&options) {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("{ATTACH_NAME}: {err}");
            1
        }
    }
}

fn run(options: &Options) -> Result<(), String> {
    let stdin = io::stdin();
    let size = pty::winsize(stdin.as_fd()).unwrap_or(Winsize::cells(24, 80));
    let mut stream = UnixStream::connect(&options.socket)
        .map_err(|err| format!("нет сессии {}: {err}", options.socket.display()))?;
    proto::write_message(
        &mut stream,
        &Message::Hello(Hello {
            version: proto::VERSION,
            purpose: Purpose::Attach,
            lead: options.lead,
            client: format!("{ATTACH_NAME} {}", env!("CARGO_PKG_VERSION")),
            size,
        }),
    )
    .map_err(|err| format!("hello: {err}"))?;
    let welcome = match proto::read_message(&mut stream) {
        Ok(Some(Message::Welcome(welcome))) => welcome,
        Ok(Some(Message::Refused(reason))) => return Err(format!("хост отказал: {reason}")),
        Ok(other) => return Err(format!("вместо welcome: {other:?}")),
        Err(err) => return Err(format!("welcome: {err}")),
    };
    let saved = pty::get_termios(stdin.as_fd()).ok();
    if let Some(termios) = &saved {
        let _ = pty::set_termios(stdin.as_fd(), &pty::raw_mode(termios));
    }
    pty::block_for_sigwait(libc::SIGWINCH).map_err(|err| format!("маска SIGWINCH: {err}"))?;
    let mut out = io::stdout().lock();
    if let Some(prefix) = &options.role_prefix {
        let _ = out.write_all(&role_title(prefix, welcome.role, welcome.generation));
        let _ = out.flush();
    }
    drop(out);
    let mut reader = stream.try_clone().map_err(|err| format!("сокет: {err}"))?;
    let prefix = options.role_prefix.clone();
    thread::Builder::new()
        .name("from-host".to_owned())
        .spawn(move || from_host(&mut reader, prefix.as_deref()))
        .map_err(|err| format!("поток вывода: {err}"))?;
    let resizer = stream.try_clone().map_err(|err| format!("сокет: {err}"))?;
    thread::Builder::new()
        .name("winch".to_owned())
        .spawn(move || winch_loop(resizer))
        .map_err(|err| format!("поток SIGWINCH: {err}"))?;
    let result = to_host(&mut stream);
    if let Some(termios) = &saved {
        let _ = pty::set_termios(io::stdin().as_fd(), termios);
    }
    result
}

/// Вывод хоста в терминал; выход нагрузки или конец соединения завершают клиента.
fn from_host(reader: &mut UnixStream, prefix: Option<&str>) {
    let mut out = io::stdout();
    loop {
        match proto::read_message(reader) {
            Ok(Some(Message::Output { bytes, .. } | Message::Restore(bytes))) => {
                if out.write_all(&bytes).and_then(|()| out.flush()).is_err() {
                    std::process::exit(1);
                }
            }
            Ok(Some(Message::Role { role, generation })) => {
                if let Some(prefix) = prefix {
                    let _ = out.write_all(&role_title(prefix, role, generation));
                    let _ = out.flush();
                }
            }
            Ok(Some(Message::Exited { .. })) | Ok(None) => std::process::exit(0),
            Ok(Some(_)) => {}
            Err(_) => std::process::exit(1),
        }
    }
}

/// Ввод терминала в хост до `Ctrl-\` или конца терминала.
fn to_host(stream: &mut UnixStream) -> Result<(), String> {
    let mut stdin = io::stdin().lock();
    let mut buf = [0u8; 4096];
    loop {
        let read = match stdin.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(read) => read,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            // Конец терминала: GUI закрыл мастер PTY.
            Err(_) => return Ok(()),
        };
        let (bytes, detach) = split_detach(&buf[..read]);
        if !bytes.is_empty() {
            proto::write_message(stream, &Message::Input(bytes.to_vec()))
                .map_err(|err| format!("ввод: {err}"))?;
        }
        if detach {
            let _ = proto::write_message(stream, &Message::Detach);
            let _ = stream.shutdown(std::net::Shutdown::Write);
            return Ok(());
        }
    }
}

fn winch_loop(mut stream: UnixStream) {
    // SAFETY: набор сигналов на стеке, `sigwait` пишет номер в локальную переменную.
    let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGWINCH);
    }
    loop {
        let mut sig: libc::c_int = 0;
        // SAFETY: см. выше.
        if unsafe { libc::sigwait(&set, &mut sig) } != 0 {
            return;
        }
        let Ok(size) = pty::winsize(io::stdin().as_fd()) else {
            continue;
        };
        if proto::write_message(&mut stream, &Message::Resize(size)).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::parse_role;

    #[test]
    fn options_round_trip() {
        let options = Options {
            socket: PathBuf::from("/tmp/pss-n/r/s1"),
            lead: Lead::Claim,
            role_prefix: Some("pss-role;t:".to_owned()),
        };
        assert_eq!(Options::parse(&options.to_args()), Ok(options));
        assert!(Options::parse(&["--lead".to_owned(), "x".to_owned()]).is_err());
    }

    #[test]
    fn role_title_is_parsed_back() {
        let title = role_title("pss-role;t:", Role::Leader, 3);
        let text = String::from_utf8(title).unwrap();
        let report = text
            .strip_prefix("\x1b]2;pss-role;t:")
            .and_then(|rest| rest.strip_suffix('\x07'))
            .and_then(parse_role)
            .unwrap();
        assert_eq!(report.role, Role::Leader);
        assert_eq!(report.generation, 3);
    }

    #[test]
    fn detach_key_cuts_input() {
        assert_eq!(split_detach(b"ab\x1ccd"), (&b"ab"[..], true));
        assert_eq!(split_detach(b"ab"), (&b"ab"[..], false));
    }
}
