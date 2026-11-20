//! Квитанции фикстур и кадры канала (план P0.8, «Solution Overview», «Фикстуры»).
//!
//! Канал — сокет Unix по пути: фикстура подключается к сокету стенда (`--channel`) и шлёт
//! события, стенд по тому же соединению шлёт команды. Кадр в обе стороны — одна строка JSON и
//! `\n`. Первый кадр фикстуры — `hello`, после подготовки — `ready` с квитанцией. Каждое событие
//! фикстура сначала записывает в свой файл в каталоге процесса (временный файл и переименование),
//! затем отправляет в канал: файл, прочитанный после кадра, уже содержит это событие.
//!
//! Квитанция несёт идентичность в двух видах (ключ списка `(pid, p_start)` из собственного
//! `proc_pidinfo` и `ri_proc_start_abstime` собственного `proc_pid_rusage`), родителя, группу и
//! сессию, `getrusage(RUSAGE_SELF)` всего процесса и собственное чтение `proc_pid_rusage` между
//! двумя отметками `CLOCK_UPTIME_RAW`, выделенные байты и число рабочих потоков нагрузки.
//! Кадры и файл фикстуры формата источника P0.4 (`spikes/probe-session-survival/src/fixture/
//! channel.rs`) не повторяют: здесь нет PTY, и состав событий свой.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::attrib::OwnerRecord;
use crate::proc::{ReadError, Usage};
use crate::snapshot::Key;

/// Предел кадра в байтах без `\n`.
pub const MAX_FRAME: usize = 1 << 20;
/// `sockaddr_un.sun_path` на macOS — 104 байта вместе с нулём.
pub const SUN_PATH_MAX: usize = 103;
/// Каталог файлов фикстур внутри каталога процесса сценария.
pub const LOG_DIR: &str = "fixtures";
pub const LOG_SCHEMA: u32 = 1;

/// Режим фикстуры.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// Долгоживущий процесс без нагрузки: attach, посторонний, лист дерева, сирота.
    Idle,
    /// Нагрузка CPU с фиксированным темпом.
    Load,
    /// Память с касанием страниц.
    Memory,
    /// Дерево детей и внуков; с `--double-fork` ещё и уход из дерева.
    Tree,
    /// Квитанция готовности и выход.
    QuickExit,
    /// Промежуточный процесс двойного `fork`: запускает сироту и сразу выходит.
    DoubleFork,
    /// Хост с записью владения: программа — лидер своей сессии и группы.
    Host,
    /// Ребёнок `login`, запущенный командной строкой поверхности Ghostty.
    LoginChild,
}

impl Mode {
    pub const ALL: [Mode; 8] = [
        Mode::Idle,
        Mode::Load,
        Mode::Memory,
        Mode::Tree,
        Mode::QuickExit,
        Mode::DoubleFork,
        Mode::Host,
        Mode::LoginChild,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Mode::Idle => "idle",
            Mode::Load => "load",
            Mode::Memory => "memory",
            Mode::Tree => "tree",
            Mode::QuickExit => "quick-exit",
            Mode::DoubleFork => "double-fork",
            Mode::Host => "host",
            Mode::LoginChild => "login-child",
        }
    }

    pub fn from_name(name: &str) -> Option<Mode> {
        Mode::ALL.into_iter().find(|mode| mode.name() == name)
    }

    /// Фикстура ждёт команд до выхода; `quick-exit` и `double-fork` выходят сами.
    pub fn long_lived(self) -> bool {
        !matches!(self, Mode::QuickExit | Mode::DoubleFork)
    }
}

/// `getrusage(RUSAGE_SELF)` всего процесса, нс.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rusage {
    pub user_ns: u64,
    pub system_ns: u64,
}

impl Rusage {
    pub fn cpu_ns(self) -> u64 {
        self.user_ns.saturating_add(self.system_ns)
    }
}

/// Процесс фикстуры на момент квитанции: родитель меняется при уходе из дерева.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Process {
    pub pid: i32,
    pub ppid: i32,
    pub pgid: i32,
    pub sid: Result<i32, ReadError>,
    pub uid: u32,
    pub euid: u32,
    /// Ключ списка: `p_start` из собственного `proc_pidinfo(PROC_PIDTBSDINFO)`.
    pub key: Result<Key, ReadError>,
    /// `argv[0]`: у ребёнка `login` он начинается с `-` (`exec -l`).
    pub argv0: String,
}

/// Согласованная граница: `getrusage`, затем собственный `proc_pid_rusage` (тики Mach,
/// `ri_phys_footprint`, `ri_proc_start_abstime`) между двумя отметками `CLOCK_UPTIME_RAW`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Boundary {
    pub t_begin_ns: u64,
    pub rusage: Rusage,
    pub own: Result<Usage, ReadError>,
    pub t_end_ns: u64,
}

impl Boundary {
    /// Середина отметок: время границы для темпа CPU.
    pub fn t_mid_ns(&self) -> u64 {
        self.t_begin_ns + self.t_end_ns.saturating_sub(self.t_begin_ns) / 2
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    /// Сквозной номер квитанции фикстуры с единицы.
    pub seq: u64,
    pub name: String,
    pub mode: Mode,
    /// `ready`, `before` (память до выделения) или метка команды стенда.
    pub label: String,
    pub process: Process,
    pub boundary: Boundary,
    /// Выделенные и тронутые байты режима `memory`.
    pub allocated_bytes: u64,
    /// Рабочие потоки нагрузки: один до 100% одного ядра, два выше.
    pub workers: u32,
}

impl Receipt {
    /// `ri_proc_start_abstime` собственного чтения.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "квитанции сверяют сценарии задачи 6")
    )]
    pub fn start_abstime(&self) -> Option<u64> {
        self.boundary
            .own
            .as_ref()
            .ok()
            .map(|usage| usage.start_abstime)
    }
}

/// Роль ребёнка, запущенного фикстурой.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    /// Ребёнок узла дерева.
    Child,
    /// Промежуточный процесс двойного `fork`.
    Middle,
    /// Сирота двойного `fork`: его родитель выходит сразу.
    Orphan,
    /// Программа хоста: фикстура `tree`, лидер своей сессии и группы.
    Program,
    /// Настоящий `/usr/bin/login` как программа хоста.
    Login,
}

/// Откуда взят ключ ребёнка. Процесс root (`login` с setuid) `proc_pidinfo` не читается
/// (`EPERM`), его `p_start` даёт только список `sysctl(KERN_PROC_ALL)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum KeySource {
    PidInfo,
    List { pidinfo: ReadError },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FoundKey {
    pub key: Key,
    pub source: KeySource,
}

/// Ребёнок фикстуры: ключ прочитан после `spawn` и до `waitpid`, пока PID держит он.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spawned {
    /// Имя фикстуры ребёнка; у `login` — имя, под которым записан он сам.
    pub name: String,
    pub role: Role,
    pub pid: i32,
    pub key: Result<FoundKey, String>,
    pub argv: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitStatus {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    /// Команда стенда.
    Command,
    /// Канал закрыт: стенд вышел или закрыл соединение.
    ChannelClosed,
    /// Истёк собственный срок фикстуры.
    Lifetime,
    /// Режим без ожидания команд закончил работу.
    Done,
    Error,
}

/// Событие фикстуры.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Event {
    Hello {
        name: String,
        mode: Mode,
        pid: i32,
    },
    Ready {
        receipt: Receipt,
    },
    /// Квитанция по команде `id` или по собственной границе режима (`id` пуст).
    Receipt {
        id: Option<u64>,
        receipt: Receipt,
    },
    Spawned {
        child: Spawned,
    },
    ChildExit {
        pid: i32,
        status: WaitStatus,
    },
    /// Запись владения хоста.
    Owner {
        record: OwnerRecord,
    },
    Error {
        id: Option<u64>,
        message: String,
    },
    Exit {
        reason: ExitReason,
    },
}

/// Команда стенда.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub command: Command,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Command {
    /// Квитанция на границе с меткой.
    Receipt {
        label: String,
    },
    Exit,
}

/// Файл фикстуры: все её события по порядку.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Log {
    pub schema: u32,
    pub name: String,
    pub mode: Mode,
    pub events: Vec<Event>,
}

/// Кадр: JSON и `\n`.
pub fn encode(value: &impl Serialize) -> Result<Vec<u8>, String> {
    let mut bytes =
        serde_json::to_vec(value).map_err(|err| format!("сериализация кадра: {err}"))?;
    if bytes.len() > MAX_FRAME {
        return Err(format!(
            "кадр {} байт больше предела {MAX_FRAME}",
            bytes.len()
        ));
    }
    bytes.push(b'\n');
    Ok(bytes)
}

fn decode<T: serde::de::DeserializeOwned>(line: &str, what: &str) -> Result<T, String> {
    let line = line.strip_suffix('\n').unwrap_or(line);
    if line.len() > MAX_FRAME {
        return Err(format!(
            "{what}: кадр {} байт больше предела {MAX_FRAME}",
            line.len()
        ));
    }
    serde_json::from_str(line).map_err(|err| format!("{what}: {err}"))
}

pub fn decode_request(line: &str) -> Result<Request, String> {
    decode(line, "команда стенда")
}

pub fn decode_event(line: &str) -> Result<Event, String> {
    decode(line, "событие фикстуры")
}

/// Имя файла фикстуры: имя с `/` → `.`, прочие символы вне `[A-Za-z0-9._-]` → `_`.
pub fn log_file_name(name: &str) -> String {
    let safe: String = name
        .chars()
        .map(|c| match c {
            '/' => '.',
            c if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') => c,
            _ => '_',
        })
        .collect();
    format!("fixture-{safe}.json")
}

/// Читает файл фикстуры и проверяет его.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "файлы фикстур читают сценарии задачи 6")
)]
pub fn read_log(out: &Path, name: &str) -> Result<Log, String> {
    let path = out.join(LOG_DIR).join(log_file_name(name));
    let bytes = std::fs::read(&path).map_err(|err| format!("{}: {err}", path.display()))?;
    let log: Log =
        serde_json::from_slice(&bytes).map_err(|err| format!("{}: {err}", path.display()))?;
    if log.schema != LOG_SCHEMA {
        return Err(format!(
            "{}: схема {}, ожидалась {LOG_SCHEMA}",
            path.display(),
            log.schema
        ));
    }
    if log.name != name {
        return Err(format!(
            "{}: имя «{}», ожидалось «{name}»",
            path.display(),
            log.name
        ));
    }
    check_log(&log).map_err(|err| format!("{}: {err}", path.display()))?;
    Ok(log)
}

/// Проверка порядка: первый `hello`, номера квитанций подряд с единицы, имя и режим квитанций
/// совпадают с файлом, `ready` один и ни одного события после `exit`.
pub fn check_log(log: &Log) -> Result<(), String> {
    match log.events.first() {
        Some(Event::Hello { name, mode, .. }) if *name == log.name && *mode == log.mode => {}
        Some(Event::Hello { name, mode, .. }) => {
            return Err(format!(
                "hello от «{name}» ({}), файл «{}» ({})",
                mode.name(),
                log.name,
                log.mode.name()
            ));
        }
        _ => return Err("первое событие — не hello".to_owned()),
    }
    let mut seq = 0;
    let mut ready = 0;
    let mut exited = false;
    for (index, event) in log.events.iter().enumerate().skip(1) {
        if exited {
            return Err(format!("событие {index} после exit"));
        }
        let receipt = match event {
            Event::Hello { .. } => return Err(format!("повторный hello в событии {index}")),
            Event::Ready { receipt } => {
                ready += 1;
                Some(receipt)
            }
            Event::Receipt { receipt, .. } => Some(receipt),
            Event::Exit { .. } => {
                exited = true;
                None
            }
            Event::Spawned { .. }
            | Event::ChildExit { .. }
            | Event::Owner { .. }
            | Event::Error { .. } => None,
        };
        if let Some(receipt) = receipt {
            seq += 1;
            if receipt.seq != seq {
                return Err(format!(
                    "квитанция {} в событии {index}, ожидалась {seq}",
                    receipt.seq
                ));
            }
            if receipt.name != log.name || receipt.mode != log.mode {
                return Err(format!(
                    "квитанция {} от «{}» ({}) в файле «{}»",
                    receipt.seq,
                    receipt.name,
                    receipt.mode.name(),
                    log.name
                ));
            }
        }
    }
    if ready > 1 {
        return Err(format!("ready {ready} раз"));
    }
    Ok(())
}

/// CPU% процесса по квитанциям: `100 × Δ(user + system) / Δt` между серединами отметок.
/// Убывший счётчик или неположительный интервал — нет значения.
pub fn cpu_pct(earlier: &Receipt, later: &Receipt) -> Option<f64> {
    let cpu = later
        .boundary
        .rusage
        .cpu_ns()
        .checked_sub(earlier.boundary.rusage.cpu_ns())?;
    let wall = later
        .boundary
        .t_mid_ns()
        .checked_sub(earlier.boundary.t_mid_ns())?;
    if wall == 0 {
        return None;
    }
    Some(100.0 * cpu as f64 / wall as f64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proc::StartTime;

    fn key(pid: i32, sec: u64) -> Key {
        Key {
            pid,
            start: StartTime { sec, usec: 7 },
        }
    }

    fn receipt(seq: u64, label: &str, t_ns: u64, cpu_ns: u64) -> Receipt {
        Receipt {
            seq,
            name: "load-50".to_owned(),
            mode: Mode::Load,
            label: label.to_owned(),
            process: Process {
                pid: 4242,
                ppid: 4000,
                pgid: 4000,
                sid: Ok(3900),
                uid: 501,
                euid: 501,
                key: Ok(key(4242, 1_700_000_000)),
                argv0: "/x/probe-resources".to_owned(),
            },
            boundary: Boundary {
                t_begin_ns: t_ns,
                rusage: Rusage {
                    user_ns: cpu_ns,
                    system_ns: 0,
                },
                own: Ok(Usage {
                    start_abstime: 99,
                    user_mach: cpu_ns,
                    system_mach: 0,
                    phys_footprint: 3 << 20,
                }),
                t_end_ns: t_ns + 2_000,
            },
            allocated_bytes: 0,
            workers: 1,
        }
    }

    fn log(events: Vec<Event>) -> Log {
        Log {
            schema: LOG_SCHEMA,
            name: "load-50".to_owned(),
            mode: Mode::Load,
            events,
        }
    }

    fn hello() -> Event {
        Event::Hello {
            name: "load-50".to_owned(),
            mode: Mode::Load,
            pid: 4242,
        }
    }

    #[test]
    fn mode_names_round_trip() {
        for mode in Mode::ALL {
            assert_eq!(Mode::from_name(mode.name()), Some(mode));
            let json = serde_json::to_string(&mode).unwrap();
            assert_eq!(json, format!("\"{}\"", mode.name()));
        }
        assert_eq!(Mode::from_name("loader"), None);
        assert!(!Mode::QuickExit.long_lived());
        assert!(!Mode::DoubleFork.long_lived());
        assert!(Mode::LoginChild.long_lived());
    }

    #[test]
    fn events_round_trip_through_frames() {
        let events = vec![
            hello(),
            Event::Ready {
                receipt: receipt(1, "ready", 1_000, 0),
            },
            Event::Receipt {
                id: Some(3),
                receipt: receipt(2, "t0", 2_000_000, 500_000),
            },
            Event::Spawned {
                child: Spawned {
                    name: "host-a/p".to_owned(),
                    role: Role::Login,
                    pid: 4300,
                    key: Ok(FoundKey {
                        key: key(4300, 1_700_000_001),
                        source: KeySource::List {
                            pidinfo: ReadError::Denied,
                        },
                    }),
                    argv: vec!["/usr/bin/login".to_owned(), "-flp".to_owned()],
                },
            },
            Event::Owner {
                record: OwnerRecord {
                    host: key(4242, 1),
                    programs: vec![key(4300, 2)],
                    group: key(4300, 2),
                    session: key(4300, 2),
                },
            },
            Event::ChildExit {
                pid: 4300,
                status: WaitStatus {
                    code: None,
                    signal: Some(libc::SIGTERM),
                },
            },
            Event::Error {
                id: None,
                message: "строка с \"кавычками\"\nи переводом".to_owned(),
            },
            Event::Exit {
                reason: ExitReason::ChannelClosed,
            },
        ];
        for event in events {
            let frame = encode(&event).unwrap();
            assert_eq!(frame.last(), Some(&b'\n'));
            assert_eq!(frame.iter().filter(|b| **b == b'\n').count(), 1);
            let line = std::str::from_utf8(&frame).unwrap();
            assert_eq!(decode_event(line).unwrap(), event);
        }
    }

    #[test]
    fn receipt_keeps_failed_reads_visible() {
        let mut denied = receipt(1, "ready", 0, 0);
        denied.process.sid = Err(ReadError::Exited);
        denied.process.key = Err(ReadError::Denied);
        denied.boundary.own = Err(ReadError::Other { errno: 5 });
        let line = String::from_utf8(
            encode(&Event::Ready {
                receipt: denied.clone(),
            })
            .unwrap(),
        )
        .unwrap();
        assert!(line.contains("\"denied\""), "{line}");
        assert!(line.contains("\"errno\":5"), "{line}");
        let Event::Ready { receipt } = decode_event(&line).unwrap() else {
            panic!("не ready");
        };
        assert_eq!(receipt, denied);
        assert_eq!(receipt.start_abstime(), None);
        assert_eq!(
            super::tests::receipt(1, "x", 0, 0).start_abstime(),
            Some(99)
        );
    }

    #[test]
    fn requests_decode_and_reject_garbage() {
        let request = Request {
            id: 7,
            command: Command::Receipt {
                label: "t1".to_owned(),
            },
        };
        let frame = String::from_utf8(encode(&request).unwrap()).unwrap();
        assert_eq!(
            frame,
            "{\"id\":7,\"command\":{\"kind\":\"receipt\",\"label\":\"t1\"}}\n"
        );
        assert_eq!(decode_request(&frame).unwrap(), request);
        assert_eq!(
            decode_request("{\"id\":8,\"command\":{\"kind\":\"exit\"}}").unwrap(),
            Request {
                id: 8,
                command: Command::Exit
            }
        );
        assert!(decode_request("{\"id\":8,\"command\":{\"kind\":\"halt\"}}").is_err());
        assert!(decode_request("не json").is_err());
        assert!(decode_event("{\"kind\":\"ready\"}").is_err());
        let huge = format!("\"{}\"", "x".repeat(MAX_FRAME));
        assert!(decode_event(&huge).unwrap_err().contains("предела"));
        let message = Event::Error {
            id: None,
            message: "x".repeat(MAX_FRAME),
        };
        assert!(encode(&message).unwrap_err().contains("предела"));
    }

    #[test]
    fn log_order_is_checked() {
        let good = log(vec![
            hello(),
            Event::Receipt {
                id: None,
                receipt: receipt(1, "before", 0, 0),
            },
            Event::Ready {
                receipt: receipt(2, "ready", 10, 0),
            },
            Event::Receipt {
                id: Some(1),
                receipt: receipt(3, "t0", 20, 5),
            },
            Event::Exit {
                reason: ExitReason::Command,
            },
        ]);
        assert_eq!(check_log(&good), Ok(()));

        let mut gap = good.clone();
        gap.events[3] = Event::Receipt {
            id: Some(1),
            receipt: receipt(4, "t0", 20, 5),
        };
        assert!(check_log(&gap).unwrap_err().contains("ожидалась 3"));

        let mut no_hello = good.clone();
        no_hello.events.remove(0);
        assert!(check_log(&no_hello).unwrap_err().contains("hello"));

        let mut after_exit = good.clone();
        after_exit.events.push(Event::ChildExit {
            pid: 1,
            status: WaitStatus {
                code: Some(0),
                signal: None,
            },
        });
        assert!(check_log(&after_exit).unwrap_err().contains("после exit"));

        let mut twice = good.clone();
        twice.events.insert(
            4,
            Event::Ready {
                receipt: receipt(4, "ready", 30, 5),
            },
        );
        assert!(check_log(&twice).unwrap_err().contains("ready 2"));

        let mut foreign = good.clone();
        let mut other = receipt(3, "t0", 20, 5);
        other.name = "load-10".to_owned();
        foreign.events[3] = Event::Receipt {
            id: Some(1),
            receipt: other,
        };
        assert!(check_log(&foreign).unwrap_err().contains("load-10"));

        let mut wrong_hello = good;
        wrong_hello.events[0] = Event::Hello {
            name: "load-50".to_owned(),
            mode: Mode::Idle,
            pid: 1,
        };
        assert!(check_log(&wrong_hello).unwrap_err().contains("idle"));
    }

    #[test]
    fn log_file_is_read_and_validated() {
        let dir =
            std::env::temp_dir().join(format!("probe-resources-receipts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(LOG_DIR)).unwrap();
        let written = log(vec![
            hello(),
            Event::Ready {
                receipt: receipt(1, "ready", 0, 0),
            },
        ]);
        std::fs::write(
            dir.join(LOG_DIR).join(log_file_name("load-50")),
            serde_json::to_vec(&written).unwrap(),
        )
        .unwrap();
        assert_eq!(read_log(&dir, "load-50").unwrap(), written);
        assert!(read_log(&dir, "load-10").is_err());

        let mut stale = written;
        stale.schema = LOG_SCHEMA + 1;
        std::fs::write(
            dir.join(LOG_DIR).join(log_file_name("load-50")),
            serde_json::to_vec(&stale).unwrap(),
        )
        .unwrap();
        assert!(read_log(&dir, "load-50").unwrap_err().contains("схема"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn log_file_names_are_flat_and_safe() {
        assert_eq!(log_file_name("host-a/p/c"), "fixture-host-a.p.c.json");
        assert_eq!(log_file_name("a b'$"), "fixture-a_b__.json");
    }

    #[test]
    fn cpu_pct_uses_boundary_midpoints() {
        let t0 = receipt(1, "t0", 1_000_000_000, 100_000_000);
        let t1 = receipt(2, "t1", 3_000_000_000, 1_100_000_000);
        let pct = cpu_pct(&t0, &t1).unwrap();
        assert!((pct - 50.0).abs() < 1e-9, "{pct}");

        let mut two_threads = t1.clone();
        two_threads.boundary.rusage.system_ns = 2_000_000_000;
        assert!((cpu_pct(&t0, &two_threads).unwrap() - 150.0).abs() < 1e-9);

        assert_eq!(cpu_pct(&t1, &t0), None);
        assert_eq!(cpu_pct(&t0, &t0), None);
    }
}
