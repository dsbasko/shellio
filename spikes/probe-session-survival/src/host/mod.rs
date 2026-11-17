//! Rust-хост пробы (план P0.4, «Rust-хост»), задача 11: жизненный цикл, сокет и живой поток без
//! снимка. Процесс на сессию вне дерева запустившего: запускающий процесс выходит сразу, его
//! потомок делает `setsid` и запускает хост ещё одним `fork`, поэтому хост не лидер сессии и не
//! получает управляющий терминал. Хост публикует сокет до запуска нагрузки (проигравший
//! конкурентный `ensure` нагрузку не запускает), запускает нагрузку во втором PTY, пишет запись
//! реестра и отчитывается одной строкой [`Launch`] в stdout.
//!
//! Главный поток хоста разбирает одну ограниченную очередь событий: куски PTY, соединения,
//! сообщения и выход нагрузки. Чтение PTY не ждёт клиентов; клиенту кадры идут через его
//! ограниченную очередь, переполнение разрывает клиента с причиной в журнале.

pub mod attach;
pub mod pty;
pub mod registry;
pub mod socket;

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::backend::{HostRecord, Lead, Role, SessionId};
use crate::observer;
use crate::proto::{self, Hello, Message, Purpose, Status, Welcome};
use crate::pty::Winsize;
use socket::Point;

pub const HOST_NAME: &str = "probe-session-host";
/// Сокет точек остановки окна публикации: только проба.
pub const BREAK_ENV: &str = "PSS_HOST_BREAK";
/// Событий в очереди главного потока.
pub const EVENT_QUEUE: usize = 256;
/// Кадров в очереди одного клиента.
pub const CLIENT_QUEUE: usize = 1024;
/// Срок записи кадра клиенту: остановленный клиент разрывается.
pub const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// Срок от `SIGHUP` нагрузке до `SIGKILL` оставшимся процессам сессии при `Stop`.
pub const STOP_GRACE: Duration = Duration::from_secs(1);

/// Аргументы `probe-session-host serve`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    pub dir: PathBuf,
    pub session: SessionId,
    pub size: Winsize,
    /// Метка запуска в `argv`: по ней стенд находит оставшиеся процессы.
    pub tag: String,
    pub program: PathBuf,
    pub args: Vec<String>,
}

impl Options {
    pub fn to_args(&self) -> Vec<String> {
        let mut args = vec![
            "serve".to_owned(),
            "--dir".to_owned(),
            self.dir.display().to_string(),
            "--session".to_owned(),
            self.session.to_string(),
            "--size".to_owned(),
            format!(
                "{}x{}x{}x{}",
                self.size.rows, self.size.cols, self.size.xpixel, self.size.ypixel
            ),
            "--tag".to_owned(),
            self.tag.clone(),
            "--".to_owned(),
            self.program.display().to_string(),
        ];
        args.extend(self.args.iter().cloned());
        args
    }

    pub fn parse(args: &[String]) -> Result<Options, String> {
        let mut rest = args.iter();
        if rest.next().map(String::as_str) != Some("serve") {
            return Err("ожидается подкоманда serve".to_owned());
        }
        let (mut dir, mut session, mut size, mut tag) = (None, None, None, None);
        loop {
            let flag = rest.next().ok_or("нет -- и нагрузки")?;
            if flag == "--" {
                break;
            }
            let value = rest.next().ok_or_else(|| format!("{flag}: нет значения"))?;
            match flag.as_str() {
                "--dir" => dir = Some(PathBuf::from(value)),
                "--session" => session = Some(SessionId::new(value)?),
                "--size" => size = Some(parse_size(value)?),
                "--tag" => tag = Some(value.clone()),
                other => return Err(format!("неизвестный флаг {other}")),
            }
        }
        let program = PathBuf::from(rest.next().ok_or("нет программы нагрузки")?);
        Ok(Options {
            dir: dir.ok_or("нет --dir")?,
            session: session.ok_or("нет --session")?,
            size: size.ok_or("нет --size")?,
            tag: tag.ok_or("нет --tag")?,
            program,
            args: rest.cloned().collect(),
        })
    }
}

fn parse_size(text: &str) -> Result<Winsize, String> {
    let parts: Vec<u16> = text
        .split('x')
        .map(|part| part.parse::<u16>())
        .collect::<Result<_, _>>()
        .map_err(|err| format!("--size {text}: {err}"))?;
    match parts[..] {
        [rows, cols, xpixel, ypixel] if rows > 0 && cols > 0 => Ok(Winsize {
            rows,
            cols,
            xpixel,
            ypixel,
        }),
        _ => Err(format!(
            "--size {text}: ожидается строки x колонки x пиксели x пиксели"
        )),
    }
}

/// Строка отчёта хоста в stdout запускающего.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "launch", content = "detail", rename_all = "kebab-case")]
pub enum Launch {
    Ready(HostRecord),
    /// Имя сессии занято другим хостом; нагрузка не запускалась.
    Busy(String),
    Failed(String),
}

/// Вход `probe-session-host`.
pub fn main(args: &[String]) -> i32 {
    let options = match Options::parse(args) {
        Ok(options) => options,
        Err(err) => {
            eprintln!("{HOST_NAME}: {err}");
            return 2;
        }
    };
    // SAFETY: потоков ещё нет; после `fork` в потомке вызываются только `setsid`, `fork` и
    // `_exit` до возврата в обычный код единственного потока.
    unsafe {
        match libc::fork() {
            -1 => {
                eprintln!("{HOST_NAME}: fork: {}", io::Error::last_os_error());
                return 1;
            }
            0 => {}
            _ => libc::_exit(0),
        }
        if libc::setsid() < 0 {
            report(&Launch::Failed(format!(
                "setsid: {}",
                io::Error::last_os_error()
            )));
            libc::_exit(1);
        }
        match libc::fork() {
            -1 => {
                report(&Launch::Failed(format!(
                    "второй fork: {}",
                    io::Error::last_os_error()
                )));
                libc::_exit(1);
            }
            0 => {}
            _ => libc::_exit(0),
        }
    }
    serve(&options)
}

fn report(launch: &Launch) {
    let line = serde_json::to_string(launch).unwrap_or_else(|_| "{}".to_owned());
    let mut stdout = io::stdout().lock();
    let _ = writeln!(stdout, "{line}");
    let _ = stdout.flush();
}

/// stdin и stdout на `/dev/null`: запускающий дочитывает stdout до конца.
fn detach_stdio() {
    if let Ok(null) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")
    {
        use std::os::fd::AsRawFd;
        // SAFETY: дескрипторы открыты; `dup2` заменяет стандартные потоки.
        unsafe {
            libc::dup2(null.as_raw_fd(), libc::STDIN_FILENO);
            libc::dup2(null.as_raw_fd(), libc::STDOUT_FILENO);
        }
    }
}

fn pause_at(point: Point) {
    let Ok(path) = std::env::var(BREAK_ENV) else {
        return;
    };
    let Ok(mut stream) = UnixStream::connect(&path) else {
        eprintln!("{HOST_NAME}: точка {}: нет сокета {path}", point.name());
        return;
    };
    let _ = writeln!(stream, "{}", point.name());
    let mut go = [0u8; 1];
    let _ = io::Read::read(&mut stream, &mut go);
}

fn serve(options: &Options) -> i32 {
    let pid = std::process::id() as i32;
    let published =
        match socket::publish(&options.dir, options.session.as_str(), pid, &mut pause_at) {
            Ok(published) => published,
            Err(socket::PublishError::Busy { path }) => {
                report(&Launch::Busy(path.display().to_string()));
                return 0;
            }
            Err(err) => {
                report(&Launch::Failed(err.to_string()));
                return 1;
            }
        };
    let env: Vec<(String, String)> = std::env::vars()
        .filter(|(name, _)| name != BREAK_ENV)
        .collect();
    let workload = match pty::spawn(&options.program, &options.args, &env, options.size) {
        Ok(workload) => workload,
        Err(err) => {
            let _ = published.unpublish();
            report(&Launch::Failed(format!("запуск нагрузки: {err}")));
            return 1;
        }
    };
    let record = match describe(options, &workload) {
        Ok(record) => record,
        Err(err) => {
            let _ = published.unpublish();
            report(&Launch::Failed(err));
            return 1;
        }
    };
    if let Err(err) = registry::write(&options.dir, &record) {
        let _ = published.unpublish();
        report(&Launch::Failed(format!("запись реестра: {err}")));
        return 1;
    }
    report(&Launch::Ready(record.clone()));
    detach_stdio();
    let code = match Hub::start(options, record, workload, &published) {
        Ok(hub) => hub.run(),
        Err(err) => {
            eprintln!("{HOST_NAME}: {err}");
            1
        }
    };
    let _ = published.unpublish();
    let _ = std::fs::remove_file(registry::record_path(
        &options.dir,
        options.session.as_str(),
    ));
    code
}

fn describe(options: &Options, workload: &pty::Workload) -> Result<HostRecord, String> {
    let host = observer::current()?;
    let info = observer::inspect(workload.pid())?
        .ok_or_else(|| format!("нагрузка {} вышла до записи", workload.pid()))?;
    Ok(HostRecord {
        session: options.session.clone(),
        host: host.key,
        workload: info.key,
        workload_pgid: info.pgid,
        workload_sid: info.sid,
    })
}

// --- главный поток ---

type ConnId = u64;

enum Event {
    Pty(Vec<u8>),
    PtyEnd(String),
    Exited(ExitStatus),
    Conn(ConnId, UnixStream, Hello),
    Msg(ConnId, Message),
    Closed(ConnId),
    StopDeadline,
}

struct Conn {
    purpose: Purpose,
    frames: SyncSender<Vec<u8>>,
    stream: UnixStream,
}

struct Hub<'a> {
    options: &'a Options,
    record: HostRecord,
    terminal: pty::Terminal,
    input: pty::InputQueue,
    events: Receiver<Event>,
    sender: SyncSender<Event>,
    conns: BTreeMap<ConnId, Conn>,
    leader: Option<ConnId>,
    generation: u32,
    offset: u64,
    reads: u64,
    size: Winsize,
    stopping: bool,
}

fn on_read(bytes: Vec<u8>) -> Event {
    Event::Pty(bytes)
}

fn on_end(reason: String) -> Event {
    Event::PtyEnd(reason)
}

impl<'a> Hub<'a> {
    fn start(
        options: &'a Options,
        record: HostRecord,
        workload: pty::Workload,
        published: &socket::Published,
    ) -> Result<Hub<'a>, String> {
        let (sender, events) = mpsc::sync_channel(EVENT_QUEUE);
        workload
            .terminal
            .start_reader(sender.clone(), on_read, on_end)
            .map_err(|err| format!("поток чтения PTY: {err}"))?;
        let input = workload
            .terminal
            .start_writer()
            .map_err(|err| format!("поток записи PTY: {err}"))?;
        let pty::Workload {
            mut child,
            terminal,
        } = workload;
        let exits = sender.clone();
        thread::Builder::new()
            .name("workload-wait".to_owned())
            .spawn(move || {
                if let Ok(status) = child.wait() {
                    let _ = exits.send(Event::Exited(status));
                }
            })
            .map_err(|err| format!("поток ожидания нагрузки: {err}"))?;
        let listener = published
            .listener
            .try_clone()
            .map_err(|err| format!("слушатель: {err}"))?;
        let accepts = sender.clone();
        thread::Builder::new()
            .name("accept".to_owned())
            .spawn(move || accept_loop(listener, accepts))
            .map_err(|err| format!("поток приёма: {err}"))?;
        let size = terminal.size().unwrap_or(options.size);
        Ok(Hub {
            options,
            record,
            terminal,
            input,
            events,
            sender,
            conns: BTreeMap::new(),
            leader: None,
            generation: 0,
            offset: 0,
            reads: 0,
            size,
            stopping: false,
        })
    }

    fn run(mut self) -> i32 {
        while let Ok(event) = self.events.recv() {
            match event {
                Event::Pty(bytes) => self.output(bytes),
                Event::PtyEnd(reason) => eprintln!("{HOST_NAME}: конец PTY: {reason}"),
                Event::Exited(status) => {
                    self.exited(status);
                    return 0;
                }
                Event::Conn(id, stream, hello) => self.connect(id, stream, hello),
                Event::Msg(id, message) => self.message(id, message),
                Event::Closed(id) => self.drop_conn(id, None),
                Event::StopDeadline => self.kill_session(),
            }
        }
        1
    }

    fn send(&mut self, id: ConnId, message: &Message) {
        let Some(conn) = self.conns.get(&id) else {
            return;
        };
        match conn.frames.try_send(proto::encode(message)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => self.drop_conn(
                id,
                Some(format!("очередь клиента заполнена ({CLIENT_QUEUE} кадров)")),
            ),
            Err(TrySendError::Disconnected(_)) => self.drop_conn(id, None),
        }
    }

    fn drop_conn(&mut self, id: ConnId, reason: Option<String>) {
        let Some(conn) = self.conns.remove(&id) else {
            return;
        };
        let _ = conn.stream.shutdown(std::net::Shutdown::Both);
        if let Some(reason) = reason {
            eprintln!("{HOST_NAME}: клиент {id} разорван: {reason}");
        }
        if self.leader == Some(id) {
            self.leader = None;
        }
    }

    fn attach_ids(&self) -> Vec<ConnId> {
        self.conns
            .iter()
            .filter(|(_, conn)| conn.purpose == Purpose::Attach)
            .map(|(id, _)| *id)
            .collect()
    }

    fn output(&mut self, bytes: Vec<u8>) {
        let message = Message::Output {
            offset: self.offset,
            bytes,
        };
        if let Message::Output { bytes, .. } = &message {
            self.offset += bytes.len() as u64;
        }
        self.reads += 1;
        for id in self.attach_ids() {
            self.send(id, &message);
        }
    }

    fn resize(&mut self, size: Winsize) {
        if size == self.size {
            return;
        }
        match self.terminal.resize(size) {
            Ok(()) => self.size = size,
            Err(err) => eprintln!("{HOST_NAME}: ресайз: {err}"),
        }
    }

    fn connect(&mut self, id: ConnId, stream: UnixStream, hello: Hello) {
        let writer = match stream.try_clone() {
            Ok(writer) => writer,
            Err(err) => {
                eprintln!("{HOST_NAME}: соединение {id}: {err}");
                return;
            }
        };
        let _ = writer.set_write_timeout(Some(CLIENT_WRITE_TIMEOUT));
        let (frames, queue) = mpsc::sync_channel::<Vec<u8>>(CLIENT_QUEUE);
        let closed = self.sender.clone();
        let spawned = thread::Builder::new()
            .name(format!("client-{id}"))
            .spawn(move || {
                let mut writer = writer;
                for frame in queue {
                    if writer.write_all(&frame).is_err() {
                        let _ = closed.send(Event::Closed(id));
                        return;
                    }
                }
            });
        if spawned.is_err() {
            return;
        }
        self.conns.insert(
            id,
            Conn {
                purpose: hello.purpose,
                frames,
                stream,
            },
        );
        if hello.version != proto::VERSION {
            self.send(
                id,
                &Message::Refused(format!(
                    "версия протокола клиента {}, у хоста {}",
                    hello.version,
                    proto::VERSION
                )),
            );
            self.drop_conn(id, None);
            return;
        }
        let role = match hello.purpose {
            Purpose::Control => Role::Unowned,
            Purpose::Attach => {
                let previous = self.leader;
                let leads = match hello.lead {
                    Lead::Claim => true,
                    Lead::IfFree => previous.is_none(),
                };
                if leads {
                    self.leader = Some(id);
                    self.generation += 1;
                    if let Some(previous) = previous {
                        let generation = self.generation;
                        self.send(
                            previous,
                            &Message::Role {
                                role: Role::Follower,
                                generation,
                            },
                        );
                    }
                    self.resize(hello.size);
                    Role::Leader
                } else {
                    Role::Follower
                }
            }
        };
        let welcome = Message::Welcome(Welcome {
            version: proto::VERSION,
            host: format!("{HOST_NAME} {}", env!("CARGO_PKG_VERSION")),
            vt: "нет: до задачи 12".to_owned(),
            session: self.options.session.to_string(),
            role,
            generation: self.generation,
            offset: self.offset,
        });
        self.send(id, &welcome);
    }

    fn message(&mut self, id: ConnId, message: Message) {
        let purpose = match self.conns.get(&id) {
            Some(conn) => conn.purpose,
            None => return,
        };
        let control = purpose == Purpose::Control;
        match message {
            Message::Input(bytes) => {
                let pushed = self.input.push(bytes);
                if control {
                    let reply = match pushed {
                        Ok(()) => Message::Done,
                        Err(reason) => Message::Refused(reason),
                    };
                    self.send(id, &reply);
                } else if let Err(reason) = pushed {
                    eprintln!("{HOST_NAME}: ввод клиента {id} отвергнут: {reason}");
                }
            }
            Message::Resize(size) => {
                if control || self.leader == Some(id) {
                    self.resize(size);
                }
                if control {
                    self.send(id, &Message::Done);
                }
            }
            Message::StatusRequest => {
                let status = Message::Status(Status {
                    host: self.record.host,
                    workload: self.record.workload,
                    workload_pgid: self.record.workload_pgid,
                    workload_sid: self.record.workload_sid,
                    clients: self.attach_ids().len() as u32,
                    leader: self.leader.is_some(),
                    reads: self.reads,
                    read_bytes: self.offset,
                    size: self.size,
                });
                self.send(id, &status);
            }
            Message::Stop => {
                self.send(id, &Message::Done);
                self.stop();
            }
            Message::Detach => {
                self.send(id, &Message::Done);
                self.drop_conn(id, None);
            }
            other => {
                self.send(
                    id,
                    &Message::Refused(format!("сообщение не для хоста: {other:?}")),
                );
            }
        }
    }

    fn stop(&mut self) {
        if self.stopping {
            return;
        }
        self.stopping = true;
        let pgid = self.record.workload_pgid;
        let _ = crate::backend::signal(-pgid, libc::SIGHUP);
        let _ = crate::backend::signal(-pgid, libc::SIGCONT);
        let deadline = self.sender.clone();
        let _ = thread::Builder::new()
            .name("stop-deadline".to_owned())
            .spawn(move || {
                let (_keep, wait) = mpsc::channel::<()>();
                let _ = wait.recv_timeout(STOP_GRACE);
                let _ = deadline.send(Event::StopDeadline);
            });
    }

    /// `SIGKILL` всем живым процессам сессии нагрузки, кроме самого хоста.
    fn kill_session(&self) {
        let processes = registry::all_processes().unwrap_or_default();
        for key in registry::remaining(&self.record, &processes) {
            if key != self.record.host {
                let _ = crate::backend::signal(key.pid, libc::SIGKILL);
            }
        }
    }

    fn exited(&mut self, status: ExitStatus) {
        use std::os::unix::process::ExitStatusExt;
        if self.stopping {
            self.kill_session();
        }
        let message = Message::Exited {
            code: status.code(),
            signal: status.signal(),
        };
        for id in self.attach_ids() {
            self.send(id, &message);
        }
        for (_, conn) in std::mem::take(&mut self.conns) {
            drop(conn.frames);
            let _ = conn.stream.shutdown(std::net::Shutdown::Read);
        }
    }
}

fn accept_loop(listener: std::os::unix::net::UnixListener, events: SyncSender<Event>) {
    let mut next: ConnId = 1;
    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            continue;
        };
        let id = next;
        next += 1;
        let events = events.clone();
        let _ = thread::Builder::new()
            .name(format!("conn-{id}"))
            .spawn(move || conn_loop(id, stream, events));
    }
}

fn conn_loop(id: ConnId, stream: UnixStream, events: SyncSender<Event>) {
    let mut reader = match stream.try_clone() {
        Ok(reader) => reader,
        Err(_) => return,
    };
    let hello = match proto::read_message(&mut reader) {
        Ok(Some(Message::Hello(hello))) => hello,
        _ => return,
    };
    if events.send(Event::Conn(id, stream, hello)).is_err() {
        return;
    }
    loop {
        match proto::read_message(&mut reader) {
            Ok(Some(message)) => {
                if events.send(Event::Msg(id, message)).is_err() {
                    return;
                }
            }
            Ok(None) | Err(_) => {
                let _ = events.send(Event::Closed(id));
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_round_trip_through_argv() {
        let options = Options {
            dir: PathBuf::from("/tmp/pss-n/r"),
            session: SessionId::new("sn-a1k").unwrap(),
            size: Winsize {
                rows: 24,
                cols: 80,
                xpixel: 640,
                ypixel: 384,
            },
            tag: "pss-n".to_owned(),
            program: PathBuf::from("/bin/fixture"),
            args: vec!["--mode".to_owned(), "emitter".to_owned(), "--".to_owned()],
        };
        assert_eq!(Options::parse(&options.to_args()), Ok(options));
        assert!(Options::parse(&["serve".to_owned(), "--dir".to_owned()]).is_err());
        assert!(parse_size("0x80x0x0").is_err());
    }

    #[test]
    fn launch_lines_are_tagged() {
        let line = serde_json::to_string(&Launch::Busy("/tmp/x".to_owned())).unwrap();
        assert_eq!(line, r#"{"launch":"busy","detail":"/tmp/x"}"#);
        assert_eq!(
            serde_json::from_str::<Launch>(&line).unwrap(),
            Launch::Busy("/tmp/x".to_owned())
        );
    }
}
