//! Health N+1 (задача 8; план P0.5, раздел «Health»). N+1 подтверждается, только когда выполнено
//! всё ([`judge`], чистая функция фактов):
//!
//! 1. исполняемый файл GUI по `proc_pidpath` (канонический путь) и CDHash процесса из ядра равны
//!    ожидаемым для N+1;
//! 2. N+1 — единственный владелец блокировки (взятие с другого открытого файла получает
//!    `EWOULDBLOCK`), его сокет отвечает на `ping` с номером поколения;
//! 3. каждая сессия снимка `intent` найдена в реестре с теми же процессами, attach N+1 согласовал
//!    версию с её хостом, получил хвост без пропуска по номерам и квитанцию ввода от нагрузки;
//!    сессий вне снимка нет;
//! 4. PID и время старта хостов и нагрузок снимка прежние и процессы живы.
//!
//! «Процесс жив» само по себе health не считается. Attach health — обычный attach нулевого
//! размера: лидерство переходит к N+1, размер сессии не меняется. Номер ввода — время старта
//! процесса N+1 в микросекундах: квитанции стенда и прежних health с ним не совпадают.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::attach::{self, Attached, Connected};
use crate::generation::Part;
use crate::journal::{Journal, SessionKeys};
use crate::observer::{self, ProcessKey};
use crate::owner::{self, Answer, Lock, Reply, Request};
use crate::proto::{ATTACH_VERSIONS, Ack, Purpose};
use crate::pty::Winsize;
use crate::sys;

/// Срок одного шага health: attach, хвост и квитанция одной сессии, `ping` своего сокета.
pub const STEP: Duration = Duration::from_secs(10);
/// Команда нагрузки, на которую она отвечает квитанцией.
pub const COMMAND: &str = "ping";

/// Что health увидел в одной сессии.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFacts {
    pub expected: SessionKeys,
    /// Запись реестра с теми же процессами.
    pub found: bool,
    /// Хост и нагрузка живы с прежними PID и временем старта.
    pub same_processes: bool,
    /// Согласованная attach версия; `None` — отказ или нет соединения.
    pub version: Option<u16>,
    /// Строк хвоста и пропуски по номерам.
    pub lines: usize,
    pub gaps: Vec<String>,
    /// Квитанция нагрузки на ввод health и ответ хоста.
    pub receipt: bool,
    pub error: Option<String>,
}

/// Факты health.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Facts {
    pub generation: u32,
    pub executable: Option<PathBuf>,
    pub expected_executable: PathBuf,
    pub cdhash: Option<String>,
    pub expected_cdhash: String,
    pub lock_held: bool,
    pub ping_generation: Option<u32>,
    pub sessions: Vec<SessionFacts>,
    /// Сессии реестра вне снимка `intent`.
    pub unexpected: Vec<String>,
    pub input_seq: u64,
}

/// Нарушения правила health; пусто — N+1 здоров.
pub fn judge(facts: &Facts) -> Vec<String> {
    let mut violations = Vec::new();
    if facts.executable.as_deref() != Some(facts.expected_executable.as_path()) {
        violations.push(format!(
            "исполняемый файл {:?}, ожидался {}",
            facts.executable,
            facts.expected_executable.display()
        ));
    }
    if facts.cdhash.as_deref() != Some(facts.expected_cdhash.as_str()) {
        violations.push(format!(
            "CDHash процесса {:?}, ожидался {}",
            facts.cdhash, facts.expected_cdhash
        ));
    }
    if !facts.lock_held {
        violations.push("блокировка не у этого процесса".to_owned());
    }
    if facts.ping_generation != Some(facts.generation) {
        violations.push(format!(
            "ping своего сокета: {:?}, ожидалось поколение {}",
            facts.ping_generation, facts.generation
        ));
    }
    for session in &facts.sessions {
        let name = &session.expected.session;
        if !session.found {
            violations.push(format!("{name}: нет в реестре с прежними процессами"));
        }
        if !session.same_processes {
            violations.push(format!("{name}: хост или нагрузка сменились или не живы"));
        }
        if session.version.is_none() {
            violations.push(format!("{name}: attach не согласовал версию"));
        }
        if session.lines == 0 || !session.gaps.is_empty() {
            violations.push(format!(
                "{name}: хвост {} строк, пропуски {:?}",
                session.lines, session.gaps
            ));
        }
        if !session.receipt {
            violations.push(format!("{name}: нет квитанции ввода"));
        }
        if let Some(error) = &session.error {
            violations.push(format!("{name}: {error}"));
        }
    }
    if !facts.unexpected.is_empty() {
        violations.push(format!("сессии вне снимка intent: {:?}", facts.unexpected));
    }
    violations
}

fn alive(key: ProcessKey) -> bool {
    observer::identify(key).is_ok_and(|found| found.is_alive())
}

/// Attach к сессии: хвост с начала, ввод health и квитанция.
fn probe_session(socket: &Path, seq: u64, facts: &mut SessionFacts) {
    let hello = attach::hello(Purpose::Attach, ATTACH_VERSIONS, 0, Winsize::cells(0, 0));
    let client = match attach::connect(socket, &hello, STEP) {
        Ok(Connected::Welcome(client)) => client,
        Ok(Connected::Refused(reason)) => {
            facts.error = Some(format!("attach отвергнут: {reason}"));
            return;
        }
        Err(reason) => {
            facts.error = Some(format!("attach: {reason}"));
            return;
        }
    };
    facts.version = Some(client.welcome.version);
    let mut attached = Attached::new(client, None);
    let deadline = Instant::now() + STEP;
    let exchanged = attached
        .pump_until(deadline, |transcript| !transcript.lines.is_empty())
        .and_then(|()| attached.exchange(seq, COMMAND, deadline));
    let _ = attached.detach(Instant::now() + STEP);
    facts.lines = attached.transcript.lines.len();
    facts.gaps = attached.transcript.problems.clone();
    facts.receipt = attached.transcript.receipt(seq, COMMAND).is_some()
        && attached.acks.contains(&(seq, Ack::Accepted));
    if let Err(reason) = exchanged {
        facts.error = Some(reason);
    }
}

/// Сбор фактов health процессом `me` под его блокировкой.
pub fn gather(
    state: &Path,
    installed: &Path,
    journal: &Journal,
    me: ProcessKey,
    generation: u32,
) -> Facts {
    let canonical = |path: &Path| fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let expected_executable = canonical(&Part::App.path(installed));
    let executable = sys::process_path(me.pid).ok().map(|path| canonical(&path));
    let cdhash = sys::process_cdhash(me.pid).ok();
    let lock_held = matches!(Lock::try_acquire(&owner::lock_path(state)), Ok(None));
    let ping_generation = match owner::request(state, &Request::Ping { wait_ms: 0 }, STEP) {
        Ok(Answer::Reply(Reply::Pong(info))) if info.key == me => Some(info.generation),
        _ => None,
    };
    let seq = u64::try_from(me.start_us).unwrap_or(u64::MAX);
    let (entries, problems) = owner::registry(state);
    let mut sessions = Vec::new();
    for expected in &journal.sessions {
        let mut facts = SessionFacts {
            expected: expected.clone(),
            found: false,
            same_processes: alive(expected.host) && alive(expected.workload),
            version: None,
            lines: 0,
            gaps: Vec::new(),
            receipt: false,
            error: None,
        };
        let entry = entries.iter().find(|entry| {
            entry.record.session == expected.session
                && entry.record.host == expected.host
                && entry.record.workload == expected.workload
        });
        if let Some(entry) = entry {
            facts.found = true;
            probe_session(Path::new(&entry.record.socket), seq, &mut facts);
        }
        sessions.push(facts);
    }
    let mut unexpected: Vec<String> = entries
        .iter()
        .filter(|entry| entry.host_alive)
        .filter(|entry| {
            !journal
                .sessions
                .iter()
                .any(|expected| expected.session == entry.record.session)
        })
        .map(|entry| entry.record.session.clone())
        .collect();
    unexpected.extend(
        problems
            .into_iter()
            .map(|problem| format!("запись: {problem}")),
    );
    Facts {
        generation,
        executable,
        expected_executable,
        cdhash,
        expected_cdhash: journal.cdhash_to.clone(),
        lock_held,
        ping_generation,
        sessions,
        unexpected,
        input_seq: seq,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(pid: i32) -> ProcessKey {
        ProcessKey { pid, start_us: 1 }
    }

    fn healthy() -> Facts {
        Facts {
            generation: 2,
            executable: Some(PathBuf::from(
                "/i/probe-update.app/Contents/MacOS/probe-update-app",
            )),
            expected_executable: PathBuf::from(
                "/i/probe-update.app/Contents/MacOS/probe-update-app",
            ),
            cdhash: Some("bb".to_owned()),
            expected_cdhash: "bb".to_owned(),
            lock_held: true,
            ping_generation: Some(2),
            sessions: vec![SessionFacts {
                expected: SessionKeys {
                    session: "h1".to_owned(),
                    host: key(10),
                    workload: key(11),
                },
                found: true,
                same_processes: true,
                version: Some(1),
                lines: 5,
                gaps: Vec::new(),
                receipt: true,
                error: None,
            }],
            unexpected: Vec::new(),
            input_seq: 77,
        }
    }

    #[test]
    fn healthy_facts_pass_and_each_condition_alone_fails() {
        assert_eq!(judge(&healthy()), Vec::<String>::new());
        type Case = (&'static str, fn(&mut Facts));
        let cases: Vec<Case> = vec![
            ("путь", |f| {
                f.executable = Some(PathBuf::from(
                    "/i/.shellio-update-a/Contents/MacOS/probe-update-app",
                ))
            }),
            ("путь не прочитан", |f| f.executable = None),
            ("CDHash", |f| f.cdhash = Some("aa".to_owned())),
            ("блокировка", |f| f.lock_held = false),
            ("ping другого поколения", |f| {
                f.ping_generation = Some(1)
            }),
            ("ping без ответа", |f| f.ping_generation = None),
            ("сессии нет", |f| f.sessions[0].found = false),
            ("версия", |f| f.sessions[0].version = None),
            ("хвост пуст", |f| f.sessions[0].lines = 0),
            ("пропуск", |f| {
                f.sessions[0].gaps = vec!["строка 3 после 1".to_owned()]
            }),
            ("квитанция", |f| f.sessions[0].receipt = false),
            ("процессы", |f| f.sessions[0].same_processes = false),
            ("лишняя сессия", |f| {
                f.unexpected = vec!["h9".to_owned()]
            }),
        ];
        for (name, change) in cases {
            let mut facts = healthy();
            change(&mut facts);
            let violations = judge(&facts);
            assert_eq!(violations.len(), 1, "{name}: {violations:?}");
        }
    }

    #[test]
    fn no_sessions_is_healthy_when_the_process_itself_is() {
        let mut facts = healthy();
        facts.sessions.clear();
        assert!(judge(&facts).is_empty());
    }
}
