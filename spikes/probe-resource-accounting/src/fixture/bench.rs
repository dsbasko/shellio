//! Сторона стенда: сокет канала, запуск фикстур, ожидание готовности и квитанций.
//!
//! Каждое соединение читает свой поток и передаёт события в одну очередь, стенд разбирает её
//! в своём потоке. Ожидание — срок по монотонным часам на `recv_timeout`, без опроса. При
//! завершении стенд закрывает все соединения: фикстуры, ушедшие из его группы процессов, выходят
//! по закрытию канала.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command as Process, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use super::Spec;
use super::receipts::{self, Command, Event, Receipt, Request, SUN_PATH_MAX};

/// Путь сокета стенда в `dir`; длиннее `sun_path` — ошибка исполнения.
pub fn socket_path(dir: &Path, pid: u32) -> Result<PathBuf, String> {
    let path = dir.join(format!("fx-{pid}.sock"));
    let len = path.as_os_str().len();
    if len > SUN_PATH_MAX {
        return Err(format!(
            "путь сокета {} — {len} байт, предел sun_path {SUN_PATH_MAX}",
            path.display()
        ));
    }
    Ok(path)
}

enum Incoming {
    Hello { name: String, writer: UnixStream },
    Event { name: String, event: Box<Event> },
    Bad { name: Option<String>, error: String },
    Closed { name: String },
}

/// Состояние одной фикстуры по её событиям.
#[derive(Debug, Default)]
pub struct Seen {
    pub ready: Option<Receipt>,
    pub events: Vec<Event>,
    pub closed: bool,
}

pub struct Bench {
    exe: PathBuf,
    socket: PathBuf,
    out: PathBuf,
    lifetime: Duration,
    rx: Receiver<Incoming>,
    links: BTreeMap<String, UnixStream>,
    seen: BTreeMap<String, Seen>,
    /// Отказы канала: кадр не разобран или соединение без `hello`.
    pub faults: Vec<String>,
    children: Vec<Child>,
    next_id: u64,
}

impl Bench {
    /// `exe` — исполняемый файл с режимом `--fixture`, `socket_dir` — короткий каталог сокета
    /// (`SHELLIO_STATE_DIR` процесса), `out` — каталог процесса сценария.
    pub fn start(
        exe: PathBuf,
        socket_dir: &Path,
        out: &Path,
        lifetime: Duration,
    ) -> Result<Bench, String> {
        let socket = socket_path(socket_dir, std::process::id())?;
        let _ = std::fs::remove_file(&socket);
        let listener =
            UnixListener::bind(&socket).map_err(|err| format!("{}: {err}", socket.display()))?;
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || accept(listener, tx));
        Ok(Bench {
            exe,
            socket,
            out: out.to_path_buf(),
            lifetime,
            rx,
            links: BTreeMap::new(),
            seen: BTreeMap::new(),
            faults: Vec::new(),
            children: Vec::new(),
            next_id: 0,
        })
    }

    pub fn out(&self) -> &Path {
        &self.out
    }

    /// Запускает фикстуру ребёнком стенда; возвращает PID.
    pub fn spawn(&mut self, spec: &Spec) -> Result<i32, String> {
        spec.validate()?;
        let channel = path_str(&self.socket)?;
        let out = path_str(&self.out)?;
        let child = Process::new(&self.exe)
            .args(spec.argv(channel, out, self.lifetime))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|err| format!("запуск фикстуры {}: {err}", spec.name))?;
        let pid = child.id() as i32;
        self.children.push(child);
        Ok(pid)
    }

    pub fn seen(&self, name: &str) -> Option<&Seen> {
        self.seen.get(name)
    }

    /// Одно событие очереди до срока; `false` — срок вышел.
    fn pump(&mut self, deadline: Instant) -> bool {
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        match self.rx.recv_timeout(deadline - now) {
            Ok(incoming) => self.handle(incoming),
            Err(RecvTimeoutError::Timeout) => false,
            Err(RecvTimeoutError::Disconnected) => {
                self.faults
                    .push("поток приёма соединений завершился".to_owned());
                false
            }
        }
    }

    /// Разбирает уже пришедшие события без ожидания.
    fn drain(&mut self) {
        while let Ok(incoming) = self.rx.try_recv() {
            self.handle(incoming);
        }
    }

    fn handle(&mut self, incoming: Incoming) -> bool {
        match incoming {
            Incoming::Hello { name, writer } => {
                if self.links.insert(name.clone(), writer).is_some() {
                    self.faults.push(format!("повторное соединение «{name}»"));
                }
                self.seen.entry(name).or_default();
            }
            Incoming::Event { name, event } => {
                let event = *event;
                let seen = self.seen.entry(name).or_default();
                if let Event::Ready { receipt } = &event {
                    seen.ready = Some(receipt.clone());
                }
                seen.events.push(event);
            }
            Incoming::Bad { name, error } => self.faults.push(match name {
                Some(name) => format!("«{name}»: {error}"),
                None => error,
            }),
            Incoming::Closed { name } => {
                self.seen.entry(name.clone()).or_default().closed = true;
                self.links.remove(&name);
            }
        }
        true
    }

    /// Фикстура вышла: прислала `exit` или закрыла канал.
    fn gone(&self, name: &str) -> bool {
        self.seen.get(name).is_some_and(|seen| {
            seen.closed
                || seen
                    .events
                    .iter()
                    .any(|event| matches!(event, Event::Exit { .. }))
        })
    }

    /// Ошибка или выход фикстуры до ожидаемого события.
    fn failure(&self, name: &str, id: Option<u64>) -> Option<String> {
        let seen = self.seen.get(name)?;
        for event in &seen.events {
            match event {
                Event::Error {
                    id: error_id,
                    message,
                } if id.is_none() || *error_id == id || error_id.is_none() => {
                    return Some(format!("«{name}»: {message}"));
                }
                Event::Exit { reason } => {
                    return Some(format!("«{name}» вышла: {reason:?}"));
                }
                _ => {}
            }
        }
        if seen.closed {
            return Some(format!("«{name}»: канал закрыт"));
        }
        None
    }

    /// Ждёт `ready` от всех имён.
    pub fn wait_ready(
        &mut self,
        names: &[String],
        timeout: Duration,
    ) -> Result<BTreeMap<String, Receipt>, String> {
        let deadline = Instant::now() + timeout;
        loop {
            let mut missing = Vec::new();
            for name in names {
                match self.seen.get(name).and_then(|seen| seen.ready.as_ref()) {
                    Some(_) => {}
                    None => {
                        if let Some(failure) = self.failure(name, None) {
                            return Err(failure);
                        }
                        missing.push(name.as_str());
                    }
                }
            }
            if missing.is_empty() {
                return Ok(names
                    .iter()
                    .filter_map(|name| {
                        let ready = self.seen.get(name)?.ready.clone()?;
                        Some((name.clone(), ready))
                    })
                    .collect());
            }
            if !self.pump(deadline) {
                return Err(format!(
                    "ready не пришёл за {} с: {}",
                    timeout.as_secs(),
                    missing.join(", ")
                ));
            }
        }
    }

    fn send(&mut self, name: &str, command: Command) -> Result<u64, String> {
        self.next_id += 1;
        let id = self.next_id;
        let frame = receipts::encode(&Request { id, command })?;
        let link = self
            .links
            .get_mut(name)
            .ok_or_else(|| format!("нет соединения с «{name}»"))?;
        link.write_all(&frame)
            .map_err(|err| format!("команда «{name}»: {err}"))?;
        Ok(id)
    }

    /// Квитанция фикстуры на границе с меткой.
    pub fn receipt(
        &mut self,
        name: &str,
        label: &str,
        timeout: Duration,
    ) -> Result<Receipt, String> {
        let id = self.send(
            name,
            Command::Receipt {
                label: label.to_owned(),
            },
        )?;
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(seen) = self.seen.get(name) {
                for event in &seen.events {
                    if let Event::Receipt {
                        id: Some(got),
                        receipt,
                    } = event
                        && *got == id
                    {
                        return Ok(receipt.clone());
                    }
                }
            }
            if let Some(failure) = self.failure(name, Some(id)) {
                return Err(failure);
            }
            if !self.pump(deadline) {
                return Err(format!(
                    "квитанция «{label}» от «{name}» не пришла за {} с",
                    timeout.as_secs()
                ));
            }
        }
    }

    /// Команда выхода; ожидание — [`Bench::wait_exit`].
    pub fn exit(&mut self, name: &str) -> Result<(), String> {
        self.send(name, Command::Exit).map(|_| ())
    }

    /// Ждёт `exit` или закрытия канала от всех имён.
    pub fn wait_exit(&mut self, names: &[String], timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        loop {
            let missing: Vec<&str> = names
                .iter()
                .filter(|name| !self.gone(name))
                .map(String::as_str)
                .collect();
            if missing.is_empty() {
                return Ok(());
            }
            let missing = missing.join(", ");
            if !self.pump(deadline) {
                return Err(format!(
                    "выход не подтверждён за {} с: {missing}",
                    timeout.as_secs()
                ));
            }
        }
    }

    /// Выход всех соединённых фикстур и сбор детей стенда. Фикстуры, вышедшие сами
    /// (`quick-exit`, промежуточный процесс двойного `fork`), команду не получают.
    pub fn finish(&mut self, timeout: Duration) -> Result<(), String> {
        self.drain();
        let names: Vec<String> = self
            .links
            .keys()
            .filter(|name| !self.gone(name))
            .cloned()
            .collect();
        for name in &names {
            // Отказ записи — фикстура вышла после разбора очереди; выход подтвердит wait_exit.
            let _ = self.exit(name);
        }
        self.wait_exit(&names, timeout)?;
        let deadline = Instant::now() + timeout;
        for mut child in std::mem::take(&mut self.children) {
            // Выход подтверждён событием; сбор не ждёт дольше срока: остаток убивается.
            if Instant::now() >= deadline {
                let _ = child.kill();
            }
            child
                .wait()
                .map_err(|err| format!("сбор фикстуры {}: {err}", child.id()))?;
        }
        Ok(())
    }
}

impl Drop for Bench {
    fn drop(&mut self) {
        for link in self.links.values() {
            let _ = link.shutdown(Shutdown::Both);
        }
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_file(&self.socket);
    }
}

fn path_str(path: &Path) -> Result<&str, String> {
    path.to_str()
        .ok_or_else(|| format!("путь {} не UTF-8", path.display()))
}

fn accept(listener: UnixListener, tx: Sender<Incoming>) {
    for stream in listener.incoming() {
        let tx = tx.clone();
        match stream {
            Ok(stream) => {
                thread::spawn(move || connection(stream, tx));
            }
            Err(err) => {
                if tx
                    .send(Incoming::Bad {
                        name: None,
                        error: format!("accept: {err}"),
                    })
                    .is_err()
                {
                    return;
                }
            }
        }
    }
}

fn connection(stream: UnixStream, tx: Sender<Incoming>) {
    let writer = match stream.try_clone() {
        Ok(writer) => writer,
        Err(err) => {
            let _ = tx.send(Incoming::Bad {
                name: None,
                error: format!("соединение: {err}"),
            });
            return;
        }
    };
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let mut name: Option<String> = None;
    let mut writer = Some(writer);
    loop {
        line.clear();
        let got = reader.read_line(&mut line);
        let incoming = match (got, &name) {
            (Ok(0) | Err(_), Some(name)) => Incoming::Closed { name: name.clone() },
            (Ok(0) | Err(_), None) => Incoming::Bad {
                name: None,
                error: "соединение закрыто до hello".to_owned(),
            },
            (Ok(_), _) => match (receipts::decode_event(&line), &name) {
                (Ok(Event::Hello { name: hello, .. }), None) => {
                    name = Some(hello.clone());
                    match writer.take() {
                        Some(writer) => Incoming::Hello {
                            name: hello.clone(),
                            writer,
                        },
                        None => continue,
                    }
                }
                (Ok(event), Some(name)) => Incoming::Event {
                    name: name.clone(),
                    event: Box::new(event),
                },
                (Ok(_), None) => Incoming::Bad {
                    name: None,
                    error: "первый кадр соединения — не hello".to_owned(),
                },
                (Err(error), name) => Incoming::Bad {
                    name: name.clone(),
                    error,
                },
            },
        };
        let stop = matches!(
            incoming,
            Incoming::Closed { .. } | Incoming::Bad { name: None, .. }
        );
        if tx.send(incoming).is_err() || stop {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_path_respects_sun_path() {
        let short = socket_path(Path::new("/tmp/shellio-probe-resources.ab12cd"), 4242).unwrap();
        assert_eq!(
            short,
            Path::new("/tmp/shellio-probe-resources.ab12cd/fx-4242.sock")
        );
        let long_dir = format!("/tmp/{}", "d".repeat(100));
        let err = socket_path(Path::new(&long_dir), 4242).unwrap_err();
        assert!(err.contains("sun_path"), "{err}");
    }

    #[test]
    fn finish_tolerates_a_fixture_that_exited_by_itself() {
        let base = std::env::temp_dir().join(format!("pfb-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let mut bench = Bench::start(
            PathBuf::from("/nonexistent"),
            &base,
            &base,
            Duration::from_secs(5),
        )
        .unwrap();
        let mut client = UnixStream::connect(&bench.socket).unwrap();
        for event in [
            Event::Hello {
                name: "quick".to_owned(),
                mode: receipts::Mode::QuickExit,
                pid: 1,
            },
            Event::Exit {
                reason: receipts::ExitReason::Done,
            },
        ] {
            client
                .write_all(&receipts::encode(&event).unwrap())
                .unwrap();
        }
        drop(client);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !bench.links.contains_key("quick") {
            assert!(bench.pump(deadline), "hello не пришёл");
        }
        bench.finish(Duration::from_secs(10)).unwrap();
        assert!(bench.gone("quick"));
        assert!(bench.faults.is_empty(), "{:?}", bench.faults);
        drop(bench);
        std::fs::remove_dir_all(&base).unwrap();
    }
}
