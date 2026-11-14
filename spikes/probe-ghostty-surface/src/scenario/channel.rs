//! Сокет фикстур на стороне пробы: слушатель в `--out`, рабочие потоки приёма и чтения и разобранные
//! квитанции, которые главный поток забирает сам. Один слушатель на процесс; соединение узнаётся
//! по nonce из квитанции готовности. Команды фикстуре этот шаг ещё не шлёт.
//!
//! Путь в `--out` длиннее `sun_path`, поэтому сокет привязывается по имени из своего каталога со
//! сменой рабочего каталога. Это действие на весь процесс, поэтому [`Hub::bind`] вызывается, пока
//! потоков ещё нет, а потоки приёма запускает [`Hub::start`].

use std::io::{self, Read};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use crate::fixture::{self, ChannelRoute, Event, FrameReader, Ready, Receipt};

/// Имя сокета в `--out`.
pub const SOCKET: &str = "fixture.sock";

enum HubEvent {
    Connected { conn: u64 },
    Receipt { conn: u64, receipt: Box<Receipt> },
    Corrupt { conn: u64, error: String },
    Closed { conn: u64, error: Option<String> },
}

/// Соединение одной фикстуры.
pub struct Conn {
    pub id: u64,
    pub receipts: Vec<Receipt>,
    pub ready: Option<Ready>,
    /// Причина и код выхода из квитанции `exit`.
    pub exit: Option<(String, u8)>,
    pub closed: bool,
    pub errors: Vec<String>,
}

pub struct Hub {
    path: PathBuf,
    listener: Option<UnixListener>,
    receiver: Option<Receiver<HubEvent>>,
    conns: Vec<Conn>,
}

impl Hub {
    /// Привязывает сокет `SOCKET` в `dir`. Вызывается до первого рабочего потока процесса.
    pub fn bind(dir: &Path) -> Result<Hub, String> {
        let path = dir.join(SOCKET);
        let _ = std::fs::remove_file(&path);
        let listener = match fixture::channel_route(&path)? {
            ChannelRoute::Direct => UnixListener::bind(&path).map_err(|err| err.to_string()),
            ChannelRoute::ViaParent { dir, name } => {
                let back =
                    std::env::current_dir().map_err(|err| format!("рабочий каталог: {err}"))?;
                std::env::set_current_dir(&dir)
                    .map_err(|err| format!("{}: {err}", dir.display()))?;
                let bound = UnixListener::bind(&name).map_err(|err| err.to_string());
                std::env::set_current_dir(&back)
                    .map_err(|err| format!("возврат в {}: {err}", back.display()))?;
                bound
            }
        }
        .map_err(|err| format!("сокет фикстур {}: {err}", path.display()))?;
        Ok(Hub {
            path,
            listener: Some(listener),
            receiver: None,
            conns: Vec::new(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Запускает поток приёма соединений. Каждое соединение читает свой поток.
    pub fn start(&mut self) -> Result<(), String> {
        let listener = self.listener.take().ok_or("приём фикстур уже запущен")?;
        let (sender, receiver) = mpsc::channel();
        thread::Builder::new()
            .name("probe-fixture-accept".to_owned())
            .spawn(move || accept_loop(listener, sender))
            .map_err(|err| format!("поток приёма фикстур не запущен: {err}"))?;
        self.receiver = Some(receiver);
        Ok(())
    }

    /// Забирает разобранные квитанции. Не блокирует.
    pub fn poll(&mut self) {
        let Some(receiver) = &self.receiver else {
            return;
        };
        let mut events = Vec::new();
        while let Ok(event) = receiver.try_recv() {
            events.push(event);
        }
        for event in events {
            self.apply(event);
        }
    }

    /// Блокирующее ожидание следующего события для тестов без окна.
    #[cfg(test)]
    fn poll_for(&mut self, timeout: std::time::Duration) -> bool {
        let Some(receiver) = &self.receiver else {
            return false;
        };
        match receiver.recv_timeout(timeout) {
            Ok(event) => {
                self.apply(event);
                self.poll();
                true
            }
            Err(_) => false,
        }
    }

    fn apply(&mut self, event: HubEvent) {
        match event {
            HubEvent::Connected { conn } => self.conns.push(Conn {
                id: conn,
                receipts: Vec::new(),
                ready: None,
                exit: None,
                closed: false,
                errors: Vec::new(),
            }),
            HubEvent::Receipt { conn, receipt } => {
                if let Some(conn) = self.conn_mut(conn) {
                    match &receipt.event {
                        Event::Ready(ready) => conn.ready = Some(ready.clone()),
                        Event::Exit { reason, code, .. } => {
                            conn.exit = Some((reason.clone(), *code));
                        }
                        _ => {}
                    }
                    conn.receipts.push(*receipt);
                }
            }
            HubEvent::Corrupt { conn, error } => {
                if let Some(conn) = self.conn_mut(conn) {
                    conn.errors.push(error);
                }
            }
            HubEvent::Closed { conn, error } => {
                if let Some(conn) = self.conn_mut(conn) {
                    conn.closed = true;
                    conn.errors.extend(error);
                }
            }
        }
    }

    fn conn_mut(&mut self, id: u64) -> Option<&mut Conn> {
        self.conns.iter_mut().find(|conn| conn.id == id)
    }

    /// Соединение фикстуры с этим nonce готовности.
    pub fn by_nonce(&mut self, nonce: &str) -> Option<&mut Conn> {
        self.conns.iter_mut().find(|conn| {
            conn.ready
                .as_ref()
                .is_some_and(|ready| ready.nonce == nonce)
        })
    }

    /// Краткое состояние для сообщения о тайм-ауте.
    pub fn describe(&self) -> String {
        let conns: Vec<String> = self
            .conns
            .iter()
            .map(|conn| {
                format!(
                    "#{} ready={:?} receipts={} closed={} errors={:?}",
                    conn.id,
                    conn.ready.as_ref().map(|ready| &ready.nonce),
                    conn.receipts.len(),
                    conn.closed,
                    conn.errors
                )
            })
            .collect();
        format!("соединений фикстур {}: [{}]", conns.len(), conns.join("; "))
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn accept_loop(listener: UnixListener, sender: Sender<HubEvent>) {
    let mut next = 0u64;
    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            continue;
        };
        next += 1;
        let conn = next;
        if sender.send(HubEvent::Connected { conn }).is_err() {
            return;
        }
        let reader = sender.clone();
        let spawned = thread::Builder::new()
            .name(format!("probe-fixture-read-{conn}"))
            .spawn(move || read_loop(conn, stream, reader));
        if let Err(err) = spawned {
            let _ = sender.send(HubEvent::Closed {
                conn,
                error: Some(format!("поток чтения не запущен: {err}")),
            });
        }
    }
}

fn read_loop(conn: u64, mut stream: UnixStream, sender: Sender<HubEvent>) {
    let mut frames = FrameReader::default();
    let mut buf = [0u8; 8192];
    let error = loop {
        match stream.read(&mut buf) {
            Ok(0) => break frames.finish().err().map(|err| err.to_string()),
            Ok(read) => {
                frames.push(&buf[..read]);
                while let Some(frame) = frames.next_frame() {
                    let event = match frame.and_then(|frame| fixture::decode::<Receipt>(&frame)) {
                        Ok(receipt) => HubEvent::Receipt {
                            conn,
                            receipt: Box::new(receipt),
                        },
                        Err(err) => HubEvent::Corrupt {
                            conn,
                            error: err.to_string(),
                        },
                    };
                    if sender.send(event).is_err() {
                        return;
                    }
                }
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => break Some(format!("чтение сокета: {err}")),
        }
    };
    let _ = sender.send(HubEvent::Closed { conn, error });
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::time::Duration;

    use super::*;

    const WAIT: Duration = Duration::from_secs(5);

    fn ready(nonce: &str) -> Ready {
        Ready {
            nonce: nonce.to_owned(),
            env_nonce: Some("env".to_owned()),
            argv0: "-probe".to_owned(),
            pid: 10,
            ppid: 9,
            pgid: 9,
            sid: 8,
            foreground_pgid: Some(9),
            tty: Some("/dev/ttys001".to_owned()),
            term: Some("xterm-ghostty".to_owned()),
            terminfo: Some("/x/terminfo".to_owned()),
            terminfo_entry: Some("/x/terminfo/78/xterm-ghostty".to_owned()),
            lang: Some("en_US.UTF-8".to_owned()),
            env_names: vec!["TERM".to_owned()],
            size: None,
        }
    }

    #[test]
    fn hub_routes_receipts_by_ready_nonce() {
        let dir = std::env::temp_dir().join(format!("pgs-hub-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut hub = Hub::bind(&dir).unwrap();
        hub.start().unwrap();
        let mut client = UnixStream::connect(hub.path()).unwrap();
        let receipt = Receipt {
            seq: 1,
            t_ns: 5,
            event: Event::Ready(ready("run-s1")),
        };
        client
            .write_all(&fixture::encode(&receipt).unwrap())
            .unwrap();
        client.write_all(b"{broken\n").unwrap();
        while hub.by_nonce("run-s1").is_none() || hub.conns[0].errors.is_empty() {
            assert!(hub.poll_for(WAIT), "нет события: {}", hub.describe());
        }
        assert!(hub.by_nonce("other").is_none());
        let conn = hub.by_nonce("run-s1").unwrap();
        assert_eq!(conn.ready.as_ref().unwrap().pid, 10);

        let exit = Receipt {
            seq: 2,
            t_ns: 6,
            event: Event::Exit {
                reason: "hangup".to_owned(),
                detail: None,
                code: 0,
            },
        };
        client.write_all(&fixture::encode(&exit).unwrap()).unwrap();
        drop(client);
        while !hub.conns[0].closed {
            assert!(hub.poll_for(WAIT), "нет закрытия: {}", hub.describe());
        }
        assert_eq!(hub.conns[0].exit, Some(("hangup".to_owned(), 0)));
        assert_eq!(hub.conns[0].receipts.len(), 2);
        let socket = hub.path().to_path_buf();
        drop(hub);
        assert!(!socket.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
