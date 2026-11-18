//! Журнал попытки обновления `<state>/update/journal.json` по таблице переходов плана P0.5
//! (раздел «Протокол замены»). Запись атомарна: временный файл, `fsync`, `rename`, `fsync`
//! каталога. Перед каждой записью писатель перечитывает файл и сверяет номер попытки и
//! состояние-предусловие с диском, а не со своей копией. Пишет только владелец `shellio.lock`;
//! блокировку берёт вызывающий (задача 6).

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::observer::ProcessKey;
use crate::sys;

pub const UPDATE_DIR: &str = "update";
pub const JOURNAL_FILE: &str = "journal.json";
/// Каталог кандидата попытки рядом с установленным бандлом: `.shellio-update-<attempt>`.
pub const STAGED_PREFIX: &str = ".shellio-update-";
/// Отказавший N+1 попытки: `.shellio-failed-<attempt>`.
pub const FAILED_PREFIX: &str = ".shellio-failed-";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    Idle,
    StagedDeferred,
    Staged,
    Intent,
    Aborting,
    Swapped,
    Launched,
    Committed,
    Cleaning,
    RollbackIntent,
    RolledBack,
    Retired,
}

impl State {
    pub const ALL: [State; 12] = [
        State::Idle,
        State::StagedDeferred,
        State::Staged,
        State::Intent,
        State::Aborting,
        State::Swapped,
        State::Launched,
        State::Committed,
        State::Cleaning,
        State::RollbackIntent,
        State::RolledBack,
        State::Retired,
    ];

    pub fn name(self) -> &'static str {
        match self {
            State::Idle => "idle",
            State::StagedDeferred => "staged-deferred",
            State::Staged => "staged",
            State::Intent => "intent",
            State::Aborting => "aborting",
            State::Swapped => "swapped",
            State::Launched => "launched",
            State::Committed => "committed",
            State::Cleaning => "cleaning",
            State::RollbackIntent => "rollback-intent",
            State::RolledBack => "rolled-back",
            State::Retired => "retired",
        }
    }

    /// Состояния, в которых срок health уже задан: от `intent` до конца попытки.
    fn has_deadline(self) -> bool {
        matches!(
            self,
            State::Intent
                | State::Swapped
                | State::Launched
                | State::Committed
                | State::Cleaning
                | State::RollbackIntent
                | State::RolledBack
                | State::Retired
        )
    }
}

/// Итог законченной попытки в `idle`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    /// «прервана»
    Aborted,
    /// «подтверждена»
    Committed,
    /// «откачена»
    RolledBack,
}

/// Таблица переходов плана, строка в строку.
pub const TRANSITIONS: [(State, State); 16] = [
    (State::Idle, State::StagedDeferred),
    (State::Idle, State::Staged),
    (State::Staged, State::Intent),
    (State::StagedDeferred, State::Staged),
    (State::Staged, State::Aborting),
    (State::Intent, State::Aborting),
    (State::Aborting, State::Idle),
    (State::Intent, State::Swapped),
    (State::Swapped, State::Launched),
    (State::Launched, State::Committed),
    (State::Committed, State::Cleaning),
    (State::Cleaning, State::Idle),
    (State::Launched, State::RollbackIntent),
    (State::RollbackIntent, State::RolledBack),
    (State::RolledBack, State::Retired),
    (State::Retired, State::Idle),
];

pub fn allowed(from: State, to: State) -> bool {
    TRANSITIONS.contains(&(from, to))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Journal {
    /// Номер попытки: случайный идентификатор, `[0-9a-f]`.
    pub attempt: String,
    pub state: State,
    pub from: u32,
    pub to: u32,
    /// Ожидаемые CDHash бандлов N и N+1.
    pub cdhash_from: String,
    pub cdhash_to: String,
    /// Установленный путь бандла; каталоги попытки лежат рядом с ним.
    pub installed: PathBuf,
    /// Срок health, мс от эпохи Unix. Задаётся один раз, в `staged → intent`.
    pub deadline_ms: Option<u64>,
    pub gui_from: Option<ProcessKey>,
    pub helper: Option<ProcessKey>,
    /// Ключ владельца — процесс N+1, записанный в `launched`.
    pub owner: Option<ProcessKey>,
    pub outcome: Option<Outcome>,
    /// Причина отсрочки или прерывания.
    pub reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JournalError {
    Io(String),
    /// Файл есть, но не разбирается или нарушает инварианты; за `idle` не принимается.
    Corrupt(String),
    /// Номер попытки или состояние на диске не те, что ждал писатель; ничего не записано.
    Stale {
        expected: String,
        found: String,
    },
    /// Запись нарушила бы таблицу переходов или неизменные поля попытки.
    Rejected(String),
}

impl std::fmt::Display for JournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JournalError::Io(reason) => write!(f, "журнал: {reason}"),
            JournalError::Corrupt(reason) => write!(f, "журнал повреждён: {reason}"),
            JournalError::Stale { expected, found } => {
                write!(f, "журнал не тот: ждали {expected}, на диске {found}")
            }
            JournalError::Rejected(reason) => write!(f, "запись журнала отвергнута: {reason}"),
        }
    }
}

pub fn is_attempt(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 64
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl Journal {
    /// Новая попытка в `staged` или `staged-deferred`.
    pub fn begin(
        attempt: &str,
        state: State,
        (from, cdhash_from): (u32, &str),
        (to, cdhash_to): (u32, &str),
        installed: &Path,
    ) -> Journal {
        Journal {
            attempt: attempt.to_owned(),
            state,
            from,
            to,
            cdhash_from: cdhash_from.to_owned(),
            cdhash_to: cdhash_to.to_owned(),
            installed: installed.to_path_buf(),
            deadline_ms: None,
            gui_from: None,
            helper: None,
            owner: None,
            outcome: None,
            reason: None,
        }
    }

    pub fn parse(text: &str) -> Result<Journal, JournalError> {
        let journal: Journal =
            serde_json::from_str(text).map_err(|err| JournalError::Corrupt(err.to_string()))?;
        journal.validate().map_err(JournalError::Corrupt)?;
        Ok(journal)
    }

    fn validate(&self) -> Result<(), String> {
        if !is_attempt(&self.attempt) {
            return Err(format!("номер попытки {:?}", self.attempt));
        }
        if self.from == self.to || self.cdhash_from == self.cdhash_to {
            return Err("поколения «из» и «в» совпадают".to_owned());
        }
        if self.installed.file_name().is_none() || self.installed.parent().is_none() {
            return Err(format!("установленный путь {}", self.installed.display()));
        }
        if self.state.has_deadline() && self.deadline_ms.is_none() {
            return Err(format!("{}: нет срока health", self.state.name()));
        }
        if matches!(self.state, State::Staged | State::StagedDeferred) && self.deadline_ms.is_some()
        {
            return Err(format!("{}: срок до intent", self.state.name()));
        }
        if self.state == State::Launched && self.owner.is_none() {
            return Err("launched: нет ключа владельца".to_owned());
        }
        if (self.state == State::Idle) != self.outcome.is_some() {
            return Err(format!("{}: итог попытки", self.state.name()));
        }
        Ok(())
    }

    fn install_dir(&self) -> &Path {
        self.installed.parent().unwrap_or(Path::new("/"))
    }

    pub fn staged_dir(&self) -> PathBuf {
        self.install_dir()
            .join(format!("{STAGED_PREFIX}{}", self.attempt))
    }

    pub fn failed_dir(&self) -> PathBuf {
        self.install_dir()
            .join(format!("{FAILED_PREFIX}{}", self.attempt))
    }

    fn label(&self) -> String {
        format!("{} {}", self.attempt, self.state.name())
    }
}

/// Неизменные поля попытки и правила полей перехода `old → new`.
fn check_change(old: &Journal, new: &Journal) -> Result<(), String> {
    if !allowed(old.state, new.state) {
        return Err(format!(
            "переход {} → {} не из таблицы",
            old.state.name(),
            new.state.name()
        ));
    }
    if (
        &old.attempt,
        old.from,
        old.to,
        &old.cdhash_from,
        &old.cdhash_to,
        &old.installed,
    ) != (
        &new.attempt,
        new.from,
        new.to,
        &new.cdhash_from,
        &new.cdhash_to,
        &new.installed,
    ) {
        return Err("изменены поля попытки".to_owned());
    }
    let sets_deadline = old.state == State::Staged && new.state == State::Intent;
    if old.deadline_ms != new.deadline_ms && !(sets_deadline && old.deadline_ms.is_none()) {
        return Err("срок health задаётся один раз в intent и не продлевается".to_owned());
    }
    if old.owner != new.owner && !(new.state == State::Launched && old.owner.is_none()) {
        return Err("ключ владельца задаётся один раз в launched".to_owned());
    }
    new.validate()
}

/// Журнал в каталоге `<state>/update`.
#[derive(Clone, Debug)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(state_dir: &Path) -> Store {
        Store {
            dir: state_dir.join(UPDATE_DIR),
        }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join(JOURNAL_FILE)
    }

    /// `None` — журнала нет, это `idle` без попыток. Пустой или битый файл — ошибка.
    pub fn read(&self) -> Result<Option<Journal>, JournalError> {
        match fs::read_to_string(self.path()) {
            Ok(text) => Journal::parse(&text).map(Some),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(JournalError::Io(format!(
                "{}: {err}",
                self.path().display()
            ))),
        }
    }

    /// Начало попытки: на диске `idle` или журнала нет, новая запись — `staged` или
    /// `staged-deferred` с другим номером попытки.
    pub fn begin(&self, next: &Journal) -> Result<(), JournalError> {
        let current = self.read()?;
        if let Some(current) = &current
            && current.state != State::Idle
        {
            return Err(JournalError::Stale {
                expected: "idle".to_owned(),
                found: current.label(),
            });
        }
        if !matches!(next.state, State::Staged | State::StagedDeferred) {
            return Err(JournalError::Rejected(format!(
                "попытка начинается с {}",
                next.state.name()
            )));
        }
        if current.is_some_and(|current| current.attempt == next.attempt) {
            return Err(JournalError::Rejected("номер попытки повторён".to_owned()));
        }
        next.validate().map_err(JournalError::Rejected)?;
        self.write(next)
    }

    /// Переход попытки `attempt` из одного из состояний `from`: журнал перечитывается, при
    /// расхождении ничего не пишется.
    pub fn advance(
        &self,
        attempt: &str,
        from: &[State],
        change: impl FnOnce(&mut Journal),
    ) -> Result<Journal, JournalError> {
        let expected = || {
            let states: Vec<_> = from.iter().map(|state| state.name()).collect();
            format!("{attempt} {}", states.join("|"))
        };
        let Some(current) = self.read()? else {
            return Err(JournalError::Stale {
                expected: expected(),
                found: "нет журнала".to_owned(),
            });
        };
        if current.attempt != attempt || !from.contains(&current.state) {
            return Err(JournalError::Stale {
                expected: expected(),
                found: current.label(),
            });
        }
        let mut next = current.clone();
        change(&mut next);
        check_change(&current, &next).map_err(JournalError::Rejected)?;
        self.write(&next)?;
        Ok(next)
    }

    /// Временный файл, `fsync`, `rename`, `fsync` каталога. При отказе временный файл
    /// удаляется, если можно, и прежний журнал остаётся как был.
    fn write(&self, journal: &Journal) -> Result<(), JournalError> {
        let io = |what: &str, err: io::Error| JournalError::Io(format!("{what}: {err}"));
        fs::create_dir_all(&self.dir).map_err(|err| io("каталог журнала", err))?;
        let tmp = self
            .dir
            .join(format!(".{JOURNAL_FILE}.tmp-{}", std::process::id()));
        let mut text =
            serde_json::to_vec_pretty(journal).map_err(|err| JournalError::Io(err.to_string()))?;
        text.push(b'\n');
        let written = (|| {
            let mut file = fs::File::create(&tmp).map_err(|err| io("временный файл", err))?;
            file.write_all(&text)
                .map_err(|err| io("запись временного файла", err))?;
            file.sync_all()
                .map_err(|err| io("fsync временного файла", err))?;
            fs::rename(&tmp, self.path()).map_err(|err| io("rename журнала", err))
        })();
        if let Err(err) = written {
            let _ = fs::remove_file(&tmp);
            return Err(err);
        }
        sys::sync_dir(&self.dir).map_err(|err| io("fsync каталога журнала", err))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    pub const ATTEMPT: &str = "a1b2c3";
    pub const CD_FROM: &str = "1111111111111111111111111111111111111111";
    pub const CD_TO: &str = "2222222222222222222222222222222222222222";
    pub const DEADLINE: u64 = 1_000_000;
    pub const OWNER: ProcessKey = ProcessKey {
        pid: 4242,
        start_us: 77,
    };

    pub fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "psu-journal-{name}-{}-{}",
            std::process::id(),
            sys::uptime_ns()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Журнал попытки в состоянии `state` с полями, которых это состояние требует.
    pub fn journal_in(state: State, installed: &Path) -> Journal {
        let mut journal =
            Journal::begin(ATTEMPT, State::Staged, (1, CD_FROM), (2, CD_TO), installed);
        journal.state = state;
        if state.has_deadline() {
            journal.deadline_ms = Some(DEADLINE);
        }
        if matches!(
            state,
            State::Launched
                | State::Committed
                | State::Cleaning
                | State::RollbackIntent
                | State::RolledBack
                | State::Retired
        ) {
            journal.owner = Some(OWNER);
        }
        if state == State::Idle {
            journal.outcome = Some(Outcome::Aborted);
        }
        journal
    }

    pub fn put(store: &Store, journal: &Journal) {
        store.write(journal).unwrap();
    }

    fn installed(dir: &Path) -> PathBuf {
        dir.join("install").join("probe-update.app")
    }

    #[test]
    fn transition_table_is_exactly_the_plan() {
        let mut count = 0;
        for from in State::ALL {
            for to in State::ALL {
                if allowed(from, to) {
                    count += 1;
                }
            }
        }
        assert_eq!(count, TRANSITIONS.len());
        assert!(!allowed(State::Committed, State::Idle));
        assert!(!allowed(State::Launched, State::Idle));
        assert!(!allowed(State::Swapped, State::Aborting));
    }

    /// Каждая строка таблицы переходов проходит через `advance` с полями, которые она задаёт.
    #[test]
    fn every_transition_row_writes() {
        let dir = temp_dir("rows");
        let store = Store::new(&dir.join("state"));
        for (from, to) in TRANSITIONS {
            if from == State::Idle {
                put(&store, &journal_in(State::Idle, &installed(&dir)));
                let mut next = journal_in(to, &installed(&dir));
                next.attempt = "b0".to_owned();
                store.begin(&next).unwrap();
                assert_eq!(store.read().unwrap(), Some(next));
                continue;
            }
            let mut start = journal_in(from, &installed(&dir));
            if from == State::Swapped {
                start.owner = None;
            }
            put(&store, &start);
            let written = store
                .advance(ATTEMPT, &[from], |journal| {
                    journal.state = to;
                    if to == State::Intent {
                        journal.deadline_ms = Some(DEADLINE);
                    }
                    if to == State::Launched {
                        journal.owner = Some(OWNER);
                    }
                    if to == State::Idle {
                        journal.outcome = Some(Outcome::Committed);
                    }
                })
                .unwrap_or_else(|err| panic!("{} → {}: {err}", from.name(), to.name()));
            assert_eq!(written.state, to);
            assert_eq!(store.read().unwrap(), Some(written));
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn writer_checks_attempt_and_state_on_disk() {
        let dir = temp_dir("stale");
        let store = Store::new(&dir.join("state"));
        put(&store, &journal_in(State::Launched, &installed(&dir)));
        let before = fs::read(store.path()).unwrap();
        // Запоздалый писатель: то же состояние, чужой номер попытки.
        let late = store.advance("ffff", &[State::Launched], |journal| {
            journal.state = State::RollbackIntent;
        });
        assert!(matches!(late, Err(JournalError::Stale { .. })));
        // Своя попытка, но предусловие уже другое.
        let wrong = store.advance(ATTEMPT, &[State::Swapped], |journal| {
            journal.state = State::Launched;
        });
        assert!(matches!(wrong, Err(JournalError::Stale { .. })));
        // Переход не из таблицы.
        let skip = store.advance(ATTEMPT, &[State::Launched], |journal| {
            journal.state = State::Idle;
            journal.outcome = Some(Outcome::Committed);
        });
        assert!(matches!(skip, Err(JournalError::Rejected(_))));
        // Срок не продлевается.
        let extend = store.advance(ATTEMPT, &[State::Launched], |journal| {
            journal.state = State::Committed;
            journal.deadline_ms = Some(DEADLINE + 1);
        });
        assert!(matches!(extend, Err(JournalError::Rejected(_))));
        // Новая попытка поверх незаконченной.
        let mut other = journal_in(State::Staged, &installed(&dir));
        other.attempt = "bb".to_owned();
        assert!(matches!(
            store.begin(&other),
            Err(JournalError::Stale { .. })
        ));
        assert_eq!(fs::read(store.path()).unwrap(), before);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_is_idle_but_damaged_is_an_error() {
        let dir = temp_dir("damaged");
        let store = Store::new(&dir.join("state"));
        assert_eq!(store.read(), Ok(None));
        fs::create_dir_all(dir.join("state").join(UPDATE_DIR)).unwrap();
        for text in ["", "{", "{\"attempt\":\"a1\"}", "null"] {
            fs::write(store.path(), text).unwrap();
            assert!(
                matches!(store.read(), Err(JournalError::Corrupt(_))),
                "{text:?}"
            );
        }
        // Разбирается, но нарушает инварианты: launched без ключа владельца.
        let mut broken = journal_in(State::Launched, &installed(&dir));
        broken.owner = None;
        fs::write(store.path(), serde_json::to_string(&broken).unwrap()).unwrap();
        assert!(matches!(store.read(), Err(JournalError::Corrupt(_))));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn failed_write_keeps_previous_journal_and_no_temp_file() {
        let dir = temp_dir("enospc");
        let store = Store::new(&dir.join("state"));
        put(&store, &journal_in(State::Staged, &installed(&dir)));
        let before = fs::read(store.path()).unwrap();
        let update = dir.join("state").join(UPDATE_DIR);
        fs::set_permissions(&update, fs::Permissions::from_mode(0o500)).unwrap();
        let result = store.advance(ATTEMPT, &[State::Staged], |journal| {
            journal.state = State::Aborting;
        });
        fs::set_permissions(&update, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(matches!(result, Err(JournalError::Io(_))));
        assert_eq!(fs::read(store.path()).unwrap(), before);
        let names: Vec<_> = fs::read_dir(&update)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, [JOURNAL_FILE]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn attempt_ids_and_paths() {
        assert!(is_attempt(&sys::random_id()));
        assert!(!is_attempt(""));
        assert!(!is_attempt("../x"));
        assert!(!is_attempt("ABC"));
        let journal = journal_in(State::Staged, Path::new("/i/probe-update.app"));
        assert_eq!(journal.staged_dir(), Path::new("/i/.shellio-update-a1b2c3"));
        assert_eq!(journal.failed_dir(), Path::new("/i/.shellio-failed-a1b2c3"));
    }
}
