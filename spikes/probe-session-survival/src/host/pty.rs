//! PTY нагрузки Rust-хоста: запуск во втором PTY, постоянное чтение в ограниченную очередь
//! событий хоста и запись ввода отдельным потоком с очередью, ограниченной в байтах
//! ([`io::INPUT_QUEUE_BYTES`]).

use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;
use std::process::Child;
use std::sync::Arc;
use std::thread;

use crate::host::io::{self as host_io, ByteQueue, Room};
use crate::pty::{self, Spawn, Winsize};

/// Кусок одного чтения PTY.
pub const READ_CHUNK: usize = 64 << 10;

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

    /// Поток чтения: каждый прочитанный кусок — `on_read` (`false` — читать дальше некуда), конец
    /// PTY — `on_end`. Чтение не ждёт клиентов.
    pub fn start_reader(
        &self,
        mut on_read: impl FnMut(Vec<u8>) -> bool + Send + 'static,
        on_end: impl FnOnce(String) + Send + 'static,
    ) -> io::Result<thread::JoinHandle<()>> {
        let mut file = File::from(self.master.try_clone()?);
        thread::Builder::new()
            .name("pty-read".to_owned())
            .spawn(move || {
                let mut buf = vec![0u8; READ_CHUNK];
                loop {
                    match file.read(&mut buf) {
                        Ok(0) => {
                            on_end("EOF".to_owned());
                            return;
                        }
                        Ok(read) => {
                            if !on_read(buf[..read].to_vec()) {
                                return;
                            }
                        }
                        Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                        Err(err) => {
                            on_end(err.to_string());
                            return;
                        }
                    }
                }
            })
    }

    /// Поток записи ввода в PTY: частичная запись продолжается, байты считаются до её конца.
    pub fn start_writer(&self) -> io::Result<InputQueue> {
        let mut file = File::from(self.master.try_clone()?);
        let queue = Arc::new(ByteQueue::new(host_io::INPUT_QUEUE_BYTES));
        let writer = Arc::clone(&queue);
        thread::Builder::new()
            .name("pty-write".to_owned())
            .spawn(move || {
                let _ = host_io::drain(&writer, &mut file);
            })?;
        Ok(InputQueue { queue })
    }
}

#[derive(Clone)]
pub struct InputQueue {
    queue: Arc<ByteQueue>,
}

impl InputQueue {
    /// Ставит ввод целиком; переполнение — явный отказ, а не потеря части.
    pub fn push(&self, bytes: Vec<u8>) -> Result<(), String> {
        let len = bytes.len();
        self.queue
            .push(bytes, Room::Limit)
            .map_err(|refusal| format!("ввод {len} байт отвергнут: {refusal}"))
    }

    /// Ответ хоста на запрос нагрузки: сверх предела ввода есть запас.
    pub fn reply(&self, bytes: Vec<u8>) -> Result<(), String> {
        let len = bytes.len();
        self.queue
            .push(bytes, Room::Reserve(host_io::REPLY_RESERVE))
            .map_err(|refusal| format!("ответ {len} байт отвергнут: {refusal}"))
    }

    /// Байты в очереди и в записи.
    pub fn queued(&self) -> usize {
        self.queue.counted()
    }
}
