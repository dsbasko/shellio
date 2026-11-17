//! PTY нагрузки Rust-хоста: запуск во втором PTY, постоянное чтение в ограниченную очередь
//! событий хоста и запись ввода отдельным потоком с ограниченной очередью.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;
use std::process::Child;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread;

use crate::pty::{self, Spawn, Winsize};

/// Кусок одного чтения PTY.
pub const READ_CHUNK: usize = 64 << 10;
/// Кусков ввода в очереди записи в PTY.
pub const INPUT_QUEUE: usize = 256;

pub struct Workload {
    pub child: Child,
    pub terminal: Terminal,
}

/// Мастер PTY нагрузки.
pub struct Terminal {
    master: OwnedFd,
}

pub fn spawn(
    program: &Path,
    args: &[String],
    env: &[(String, String)],
    size: Winsize,
) -> io::Result<Workload> {
    let spawned = pty::spawn(&Spawn {
        program,
        args,
        env,
        size,
    })?;
    Ok(Workload {
        child: spawned.child,
        terminal: Terminal {
            master: spawned.master,
        },
    })
}

impl Workload {
    pub fn pid(&self) -> i32 {
        self.child.id() as i32
    }
}

impl Terminal {
    pub fn resize(&self, size: Winsize) -> io::Result<()> {
        pty::set_winsize(self.master.as_fd(), size)
    }

    pub fn size(&self) -> io::Result<Winsize> {
        pty::winsize(self.master.as_fd())
    }

    /// Поток чтения: каждый прочитанный кусок — `on_read`, конец PTY — `on_end`. Чтение не ждёт
    /// клиентов: очередь событий хоста одна, её разбирает главный поток хоста.
    pub fn start_reader<E: Send + 'static>(
        &self,
        events: SyncSender<E>,
        on_read: fn(Vec<u8>) -> E,
        on_end: fn(String) -> E,
    ) -> io::Result<thread::JoinHandle<()>> {
        let mut file = File::from(self.master.try_clone()?);
        thread::Builder::new()
            .name("pty-read".to_owned())
            .spawn(move || {
                let mut buf = vec![0u8; READ_CHUNK];
                loop {
                    match file.read(&mut buf) {
                        Ok(0) => {
                            let _ = events.send(on_end("EOF".to_owned()));
                            return;
                        }
                        Ok(read) => {
                            if events.send(on_read(buf[..read].to_vec())).is_err() {
                                return;
                            }
                        }
                        Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                        Err(err) => {
                            let _ = events.send(on_end(err.to_string()));
                            return;
                        }
                    }
                }
            })
    }

    /// Поток записи ввода в PTY с очередью [`INPUT_QUEUE`].
    pub fn start_writer(&self) -> io::Result<InputQueue> {
        let mut file = File::from(self.master.try_clone()?);
        let (tx, rx): (SyncSender<Vec<u8>>, Receiver<Vec<u8>>) = mpsc::sync_channel(INPUT_QUEUE);
        thread::Builder::new()
            .name("pty-write".to_owned())
            .spawn(move || {
                for bytes in rx {
                    if file.write_all(&bytes).is_err() {
                        return;
                    }
                }
            })?;
        Ok(InputQueue { tx })
    }
}

pub struct InputQueue {
    tx: SyncSender<Vec<u8>>,
}

impl InputQueue {
    /// Ставит ввод в очередь; переполнение — явный отказ, а не потеря.
    pub fn push(&self, bytes: Vec<u8>) -> Result<(), String> {
        match self.tx.try_send(bytes) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(format!(
                "очередь ввода в PTY заполнена ({INPUT_QUEUE} кусков)"
            )),
            Err(TrySendError::Disconnected(_)) => Err("запись в PTY закрыта".to_owned()),
        }
    }
}
