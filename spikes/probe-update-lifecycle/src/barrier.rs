//! Барьеры пробы (задача 8; нужны задачам 10 и 11): процесс на именованной границе протокола
//! замены ждёт, пока стенд не снимет файл-барьер. Это средство стенда пробы, в продукт оно не
//! переносится.
//!
//! Барьер — файл `<state>/barriers/<роль>.<точка>`. Роль процесса задаётся один раз при старте
//! ([`set_role`]): GUI без номера попытки — `gui`, GUI с `--update-attempt` — `next`, помощник —
//! `helper`; восстановление идёт под ролью процесса, который его выполняет. Без каталога барьеров
//! или без роли барьер ничего не делает: `make check` и сценарии, которые барьеров не ставят, их не
//! замечают. Процесс, дошедший до поставленного барьера, пишет `<роль>.<точка>.reached` (ключ
//! процесса и время) и ждёт удаления файла барьера через `kqueue` на каталог; наблюдение
//! регистрируется до проверки файла. Барьер, не снятый за [`WAIT`], завершает процесс с кодом
//! [`EXIT_BARRIER`]: это ошибка стенда, а не протокола.
//!
//! Каталог границ ([`B2`], [`C1`]) переписан из раздела «Рубежи и клетки» плана P0.5 строка в
//! строку; тест сверяет, что у каждой границы есть барьер в коде.
//!
//! Отметки (задача 10): процесс с ролью пишет [`note`] — файл в `<state>/barriers/notes/` о шаге,
//! который стенд иначе не увидит (ответ владельца второму запуску, запрос, дождавшийся готовности
//! владельца). Без каталога отметок они ничего не делают и ничего не ждут.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::journal::State;
use crate::observer::{self, ProcessKey};
use crate::{kq, say, sys};

pub const BARRIERS_DIR: &str = "barriers";
pub const REACHED_SUFFIX: &str = ".reached";
/// Сколько процесс ждёт снятия барьера.
pub const WAIT: Duration = Duration::from_secs(120);
/// Код выхода процесса, барьер которого не сняли.
pub const EXIT_BARRIER: i32 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    /// GUI без номера попытки: GUI N, обычный запуск, восстановление при взятии блокировки.
    Gui,
    /// GUI с `--update-attempt`: N+1, запущенный помощником.
    Next,
    Helper,
}

impl Role {
    pub fn name(self) -> &'static str {
        match self {
            Role::Gui => "gui",
            Role::Next => "next",
            Role::Helper => "helper",
        }
    }
}

/// Мутация восстановления, кроме записи журнала (у неё свои точки).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mutation {
    /// Удаление каталога кандидата попытки.
    Remove,
    /// Обратный `RENAME_SWAP`.
    Swap,
    /// Переименование отказавшего N+1 в `.shellio-failed-<attempt>`.
    Retire,
}

impl Mutation {
    pub const ALL: [Mutation; 3] = [Mutation::Remove, Mutation::Swap, Mutation::Retire];

    pub fn name(self) -> &'static str {
        match self {
            Mutation::Remove => "remove",
            Mutation::Swap => "swap",
            Mutation::Retire => "retire",
        }
    }
}

/// Именованная граница.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "point", rename_all = "kebab-case")]
pub enum Point {
    /// Запись журнала в `state`: до (`renamed: false`) или после `rename` временного файла.
    Journal { state: State, renamed: bool },
    /// Мутация диска восстановлением: до или после.
    Step { mutation: Mutation, done: bool },
    /// Посреди удаления каталога кандидата: часть уже удалена.
    RemovePartial,
    /// GUI N: помощник запущен, GUI N ещё не вышел и держит блокировку и сокет.
    HelperSpawned,
    /// Помощник: GUI N вышел, блокировка не взята.
    GuiExited,
    /// Помощник: блокировка взята, обмена не было.
    Locked,
    /// Помощник: `renamex_np` выполнен, `swapped` не записан.
    Exchanged,
    /// Помощник или взявший роль помощника: N+1 запущен и опознан, `launched` не записан.
    Opened,
    /// Помощник: `launched` записан, блокировка отпущена.
    Released,
    /// N+1: о запуске сообщено, блокировка не бралась.
    Started,
    /// N+1: владелец, сокет не опубликован.
    BeforePublish,
    /// N+1: сокет опубликован, `committed` не записан.
    Published,
}

impl Point {
    pub fn name(self) -> String {
        let side = |after: bool, yes: &str, no: &str| if after { yes } else { no }.to_owned();
        match self {
            Point::Journal { state, renamed } => {
                format!(
                    "journal-{}-{}",
                    state.name(),
                    side(renamed, "after", "before")
                )
            }
            Point::Step { mutation, done } => {
                format!("step-{}-{}", mutation.name(), side(done, "after", "before"))
            }
            Point::RemovePartial => "remove-partial".to_owned(),
            Point::HelperSpawned => "helper-spawned".to_owned(),
            Point::GuiExited => "gui-exited".to_owned(),
            Point::Locked => "locked".to_owned(),
            Point::Exchanged => "exchanged".to_owned(),
            Point::Opened => "opened".to_owned(),
            Point::Released => "released".to_owned(),
            Point::Started => "started".to_owned(),
            Point::BeforePublish => "before-publish".to_owned(),
            Point::Published => "published".to_owned(),
        }
    }

    /// Точки, которые ставит код одного места (не запись журнала и не мутация восстановления).
    pub const SITES: [Point; 10] = [
        Point::RemovePartial,
        Point::HelperSpawned,
        Point::GuiExited,
        Point::Locked,
        Point::Exchanged,
        Point::Opened,
        Point::Released,
        Point::Started,
        Point::BeforePublish,
        Point::Published,
    ];
}

/// Барьер: роль и точка.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Barrier {
    pub role: Role,
    pub point: Point,
}

impl Barrier {
    pub const fn new(role: Role, point: Point) -> Barrier {
        Barrier { role, point }
    }

    pub fn key(self) -> String {
        format!("{}.{}", self.role.name(), self.point.name())
    }

    pub fn path(self, state: &Path) -> PathBuf {
        dir(state).join(self.key())
    }

    pub fn reached_path(self, state: &Path) -> PathBuf {
        dir(state).join(format!("{}{REACHED_SUFFIX}", self.key()))
    }
}

pub fn dir(state: &Path) -> PathBuf {
    state.join(BARRIERS_DIR)
}

static ROLE: OnceLock<Role> = OnceLock::new();

/// Роль процесса для барьеров; задаётся один раз при старте.
pub fn set_role(role: Role) {
    let _ = ROLE.set(role);
}

/// Что пишет процесс, дошедший до барьера.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reached {
    pub key: String,
    pub process: ProcessKey,
    pub uptime_ns: u64,
}

/// Граница `point` в процессе с ролью: если барьер поставлен, ждать его снятия.
pub fn reach(state: &Path, point: Point) {
    let Some(role) = ROLE.get().copied() else {
        return;
    };
    let barrier = Barrier::new(role, point);
    if let Err(reason) = hold(state, barrier) {
        say(&format!("барьер {}: {reason}", barrier.key()));
        std::process::exit(EXIT_BARRIER);
    }
}

fn hold(state: &Path, barrier: Barrier) -> Result<(), String> {
    let dir = dir(state);
    if !dir.is_dir() {
        return Ok(());
    }
    let queue = kq::Queue::on_dir(&dir)?;
    let armed = barrier.path(state);
    if !armed.exists() {
        return Ok(());
    }
    let process = observer::current()?.key;
    let reached = Reached {
        key: barrier.key(),
        process,
        uptime_ns: sys::uptime_ns(),
    };
    let text = serde_json::to_vec(&reached).map_err(|err| err.to_string())?;
    let path = barrier.reached_path(state);
    let tmp = dir.join(format!(".{}.{}", barrier.key(), process.pid));
    fs::write(&tmp, text)
        .and_then(|()| fs::rename(&tmp, &path))
        .map_err(|err| format!("{}: {err}", path.display()))?;
    say(&format!(
        "барьер {}: процесс {} ждёт снятия",
        barrier.key(),
        process.pid
    ));
    let deadline = Instant::now() + WAIT;
    while armed.exists() {
        if queue.wait(deadline)? == kq::Event::TimedOut {
            return Err(format!("не снят за {} с", WAIT.as_secs()));
        }
    }
    Ok(())
}

// --- стенд ---

/// Поставить барьер.
pub fn arm(state: &Path, barrier: Barrier) -> Result<(), String> {
    let dir = dir(state);
    fs::create_dir_all(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    let path = barrier.path(state);
    fs::write(&path, b"").map_err(|err| format!("{}: {err}", path.display()))
}

/// Снять барьер; снятый раньше — не ошибка.
pub fn release(state: &Path, barrier: Barrier) -> Result<(), String> {
    match fs::remove_file(barrier.path(state)) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(format!("{}: {err}", barrier.path(state).display())),
    }
}

/// Ждёт, пока процесс дойдёт до барьера, не позже `deadline`.
pub fn wait_reached(state: &Path, barrier: Barrier, deadline: Instant) -> Result<Reached, String> {
    let dir = dir(state);
    let queue = kq::Queue::on_dir(&dir)?;
    let path = barrier.reached_path(state);
    loop {
        match fs::read(&path) {
            Ok(bytes) => {
                return serde_json::from_slice(&bytes)
                    .map_err(|err| format!("{}: {err}", path.display()));
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(format!("{}: {err}", path.display())),
        }
        if queue.wait(deadline)? == kq::Event::TimedOut {
            return Err(format!("барьер {} не достигнут к сроку", barrier.key()));
        }
    }
}

// --- отметки ---

pub const NOTES_DIR: &str = "notes";

pub fn notes_dir(state: &Path) -> PathBuf {
    dir(state).join(NOTES_DIR)
}

/// Отметка процесса с ролью.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Note {
    pub event: String,
    pub role: Role,
    pub process: ProcessKey,
    /// Номер отметки в процессе, с 1.
    pub seq: u64,
    pub uptime_ns: u64,
    pub detail: serde_json::Value,
}

static NOTE_SEQ: AtomicU64 = AtomicU64::new(0);
static NOTE_KEY: OnceLock<ProcessKey> = OnceLock::new();

/// Отметка `event` в `<state>/barriers/notes/<роль>.<pid>.<номер>.json`; без роли или без
/// каталога отметок — ничего. Ошибка записи отметки процесс не останавливает.
pub fn note(state: &Path, event: &str, detail: serde_json::Value) {
    let Some(role) = ROLE.get().copied() else {
        return;
    };
    let dir = notes_dir(state);
    if !dir.is_dir() {
        return;
    }
    let process = match NOTE_KEY.get() {
        Some(key) => *key,
        None => match observer::current() {
            Ok(info) => *NOTE_KEY.get_or_init(|| info.key),
            Err(_) => return,
        },
    };
    let seq = NOTE_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    let note = Note {
        event: event.to_owned(),
        role,
        process,
        seq,
        uptime_ns: sys::uptime_ns(),
        detail,
    };
    let Ok(text) = serde_json::to_vec(&note) else {
        return;
    };
    let name = format!("{}.{}.{seq}.json", role.name(), process.pid);
    let tmp = dir.join(format!(".{name}"));
    if fs::write(&tmp, text)
        .and_then(|()| fs::rename(&tmp, dir.join(&name)))
        .is_err()
    {
        let _ = fs::remove_file(&tmp);
    }
}

/// Все отметки по времени.
pub fn read_notes(state: &Path) -> Result<Vec<Note>, String> {
    let dir = notes_dir(state);
    let mut notes = Vec::new();
    for entry in fs::read_dir(&dir)
        .map_err(|err| format!("{}: {err}", dir.display()))?
        .filter_map(Result::ok)
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || !name.ends_with(".json") {
            continue;
        }
        let bytes = fs::read(entry.path()).map_err(|err| format!("{name}: {err}"))?;
        notes.push(serde_json::from_slice(&bytes).map_err(|err| format!("{name}: {err}"))?);
    }
    notes.sort_by_key(|note: &Note| (note.uptime_ns, note.process.pid, note.seq));
    Ok(notes)
}

/// Ждёт, пока отметки не удовлетворят `ready`, не позже `deadline`.
pub fn wait_notes(
    state: &Path,
    deadline: Instant,
    ready: impl Fn(&[Note]) -> bool,
) -> Result<Vec<Note>, String> {
    let queue = kq::Queue::on_dir(&notes_dir(state))?;
    loop {
        let notes = read_notes(state)?;
        if ready(&notes) {
            return Ok(notes);
        }
        if queue.wait(deadline)? == kq::Event::TimedOut {
            return Err(format!(
                "отметки к сроку не появились, есть {}",
                notes.len()
            ));
        }
    }
}

// --- каталог границ ---

/// Граница из раздела «Рубежи и клетки» плана и барьеры, которыми стенд её достигает.
#[derive(Clone, Copy, Debug)]
pub struct Boundary {
    pub actor: &'static str,
    pub boundary: &'static str,
    pub barriers: &'static [Barrier],
}

const fn b(role: Role, point: Point) -> Barrier {
    Barrier::new(role, point)
}

const fn journal(role: Role, state: State, renamed: bool) -> Barrier {
    b(role, Point::Journal { state, renamed })
}

const fn step(role: Role, mutation: Mutation, done: bool) -> Barrier {
    b(role, Point::Step { mutation, done })
}

/// Таблица B2, строка в строку.
pub const B2: [Boundary; 20] = [
    Boundary {
        actor: "GUI N",
        boundary: "после staged",
        barriers: &[journal(Role::Gui, State::Staged, true)],
    },
    Boundary {
        actor: "GUI N",
        boundary: "после intent, до запуска помощника",
        barriers: &[journal(Role::Gui, State::Intent, true)],
    },
    Boundary {
        actor: "помощник",
        boundary: "после выхода GUI N, до взятия блокировки",
        barriers: &[b(Role::Helper, Point::GuiExited)],
    },
    Boundary {
        actor: "помощник",
        boundary: "после взятия блокировки, до обмена",
        barriers: &[b(Role::Helper, Point::Locked)],
    },
    Boundary {
        actor: "помощник",
        boundary: "между renamex_np и записью swapped",
        barriers: &[b(Role::Helper, Point::Exchanged)],
    },
    Boundary {
        actor: "помощник",
        boundary: "после swapped, до open",
        barriers: &[journal(Role::Helper, State::Swapped, true)],
    },
    Boundary {
        actor: "помощник",
        boundary: "после open, до записи launched",
        barriers: &[b(Role::Helper, Point::Opened)],
    },
    Boundary {
        actor: "помощник",
        boundary: "после launched и освобождения блокировки, N+1 здоров",
        barriers: &[b(Role::Helper, Point::Released)],
    },
    Boundary {
        actor: "помощник",
        boundary: "после launched, N+1 завис",
        barriers: &[
            b(Role::Helper, Point::Released),
            b(Role::Next, Point::Published),
        ],
    },
    Boundary {
        actor: "GUI N+1",
        boundary: "до публикации сокета",
        barriers: &[b(Role::Next, Point::BeforePublish)],
    },
    Boundary {
        actor: "GUI N+1",
        boundary: "после публикации сокета, до committed",
        barriers: &[b(Role::Next, Point::Published)],
    },
    Boundary {
        actor: "GUI N+1",
        boundary: "после committed",
        barriers: &[journal(Role::Next, State::Committed, true)],
    },
    Boundary {
        actor: "GUI N+1",
        boundary: "во время cleaning",
        barriers: &[b(Role::Next, Point::RemovePartial)],
    },
    Boundary {
        actor: "GUI N+1",
        boundary: "живой, но не подтверждается до срока",
        barriers: &[b(Role::Next, Point::Published)],
    },
    Boundary {
        actor: "помощник",
        boundary: "после rollback-intent, до обратного обмена",
        barriers: &[journal(Role::Helper, State::RollbackIntent, true)],
    },
    Boundary {
        actor: "помощник",
        boundary: "между обратным обменом и rolled-back",
        barriers: &[step(Role::Helper, Mutation::Swap, true)],
    },
    Boundary {
        actor: "восстановление",
        boundary: "посреди удаления кандидата в aborting",
        barriers: &[b(Role::Gui, Point::RemovePartial)],
    },
    Boundary {
        actor: "восстановление или помощник",
        boundary: "между переименованием в .shellio-failed-<attempt>/ и записью retired",
        barriers: &[
            step(Role::Gui, Mutation::Retire, true),
            step(Role::Helper, Mutation::Retire, true),
        ],
    },
    Boundary {
        actor: "GUI N+1 с ключом владельца",
        boundary: "вышел штатно до срока без подтверждения",
        barriers: &[b(Role::Next, Point::Published)],
    },
    Boundary {
        actor: "восстановление",
        boundary: "на каждой остальной своей мутации (до и после каждой записи и обмена)",
        barriers: &[
            step(Role::Gui, Mutation::Remove, false),
            step(Role::Gui, Mutation::Remove, true),
            step(Role::Gui, Mutation::Swap, false),
            step(Role::Gui, Mutation::Swap, true),
            step(Role::Gui, Mutation::Retire, false),
            journal(Role::Gui, State::Aborting, false),
            journal(Role::Gui, State::Aborting, true),
            journal(Role::Gui, State::Swapped, false),
            journal(Role::Gui, State::Swapped, true),
            journal(Role::Gui, State::RollbackIntent, false),
            journal(Role::Gui, State::RollbackIntent, true),
            journal(Role::Gui, State::RolledBack, false),
            journal(Role::Gui, State::RolledBack, true),
            journal(Role::Gui, State::Retired, false),
            journal(Role::Gui, State::Retired, true),
            journal(Role::Gui, State::Cleaning, false),
            journal(Role::Gui, State::Cleaning, true),
            journal(Role::Gui, State::Idle, false),
            journal(Role::Gui, State::Idle, true),
        ],
    },
];

/// Барьеры клетки C1. «После смерти владельца» — событие (`SIGKILL` владельца), а не точка в
/// коде: барьера у него нет.
pub const C1: [Boundary; 5] = [
    Boundary {
        actor: "GUI N",
        boundary: "до выхода N",
        barriers: &[b(Role::Gui, Point::HelperSpawned)],
    },
    Boundary {
        actor: "помощник",
        boundary: "при блокировке у помощника",
        barriers: &[b(Role::Helper, Point::Locked)],
    },
    Boundary {
        actor: "GUI N+1",
        boundary: "после launched до взятия блокировки N+1",
        barriers: &[b(Role::Next, Point::Started)],
    },
    Boundary {
        actor: "GUI N+1",
        boundary: "после bind N+1 до готовности",
        barriers: &[b(Role::Next, Point::Published)],
    },
    Boundary {
        actor: "владелец",
        boundary: "после смерти владельца",
        barriers: &[],
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::Store;
    use crate::journal::tests::{ATTEMPT, journal_in, put, temp_dir};
    use std::collections::HashSet;
    use std::sync::mpsc;
    use std::thread;

    /// Исходники, где стоят барьеры, без тестов.
    fn sources() -> Vec<(&'static str, &'static str)> {
        let strip = |text: &'static str| text.split("#[cfg(test)]").next().unwrap_or(text);
        vec![
            ("owner.rs", strip(include_str!("owner.rs"))),
            ("update.rs", strip(include_str!("update.rs"))),
            ("journal.rs", strip(include_str!("journal.rs"))),
            ("recover.rs", strip(include_str!("recover.rs"))),
            ("compat.rs", strip(include_str!("compat.rs"))),
        ]
    }

    fn variant(point: Point) -> String {
        format!("Point::{point:?}")
    }

    #[test]
    fn every_b2_row_and_every_c1_barrier_has_a_barrier() {
        for row in B2 {
            assert!(
                !row.barriers.is_empty(),
                "B2 «{} {}» без барьера",
                row.actor,
                row.boundary
            );
        }
        let events: Vec<_> = C1.iter().filter(|row| row.barriers.is_empty()).collect();
        assert_eq!(events.len(), 1, "без барьера только событие");
        assert_eq!(events[0].boundary, "после смерти владельца");
        // Каждая точка одного места стоит в коде ровно одним вызовом `reach`.
        let sources = sources();
        for point in Point::SITES {
            let needle = variant(point);
            let sites: Vec<&str> = sources
                .iter()
                .filter(|(_, text)| text.contains("reach(") && text.contains(&needle))
                .map(|(name, _)| *name)
                .collect();
            let count: usize = sources
                .iter()
                .map(|(_, text)| text.matches(&needle).count())
                .sum();
            assert!(!sites.is_empty(), "{needle}: барьера в коде нет");
            assert_eq!(count, 1, "{needle}: мест {count}, ждали одно");
        }
        // Точки записи журнала и мутаций восстановления — в общих местах.
        let journal = sources
            .iter()
            .find(|(name, _)| *name == "journal.rs")
            .unwrap()
            .1;
        assert_eq!(
            journal.matches("Point::Journal {").count(),
            2,
            "до и после rename"
        );
        let recover = sources
            .iter()
            .find(|(name, _)| *name == "recover.rs")
            .unwrap()
            .1;
        assert_eq!(
            recover.matches("Point::Step {").count(),
            1,
            "одно место: до и после каждой мутации"
        );
        // Каждый барьер каталога — из перечня точек.
        let mut keys = HashSet::new();
        for row in B2.iter().chain(C1.iter()) {
            for barrier in row.barriers {
                match barrier.point {
                    Point::Journal { .. } | Point::Step { .. } => {}
                    point => assert!(Point::SITES.contains(&point), "{point:?}"),
                }
                keys.insert(barrier.key());
            }
        }
        assert!(keys.contains("helper.journal-swapped-after"));
        assert!(keys.contains("next.started"));
    }

    #[test]
    fn keys_are_distinct_file_names() {
        let mut all = HashSet::new();
        let mut points: Vec<Point> = Point::SITES.to_vec();
        for state in State::ALL {
            for renamed in [false, true] {
                points.push(Point::Journal { state, renamed });
            }
        }
        for mutation in Mutation::ALL {
            for done in [false, true] {
                points.push(Point::Step { mutation, done });
            }
        }
        for role in [Role::Gui, Role::Next, Role::Helper] {
            for point in &points {
                let key = Barrier::new(role, *point).key();
                assert!(
                    key.bytes()
                        .all(|b| b.is_ascii_lowercase() || b == b'-' || b == b'.'),
                    "{key}"
                );
                assert!(all.insert(key.clone()), "{key} повторяется");
            }
        }
    }

    #[test]
    fn notes_are_written_only_into_an_existing_notes_dir_and_read_in_order() {
        set_role(Role::Gui);
        let dir = temp_dir("notes");
        let state = dir.join("state");
        fs::create_dir_all(&state).unwrap();
        note(&state, "lost", serde_json::json!({}));
        assert!(!notes_dir(&state).exists(), "без каталога отметок — ничего");
        fs::create_dir_all(notes_dir(&state)).unwrap();
        note(&state, "first", serde_json::json!({ "n": 1 }));
        note(&state, "second", serde_json::json!({ "n": 2 }));
        let notes = read_notes(&state).unwrap();
        let events: Vec<&str> = notes.iter().map(|note| note.event.as_str()).collect();
        assert_eq!(events, ["first", "second"]);
        assert!(notes[0].seq < notes[1].seq);
        assert_eq!(notes[0].role, Role::Gui);
        assert_eq!(notes[0].process.pid, std::process::id() as i32);
        let deadline = Instant::now() + Duration::from_secs(5);
        let waited = wait_notes(&state, deadline, |notes| notes.len() == 2).unwrap();
        assert_eq!(waited, notes);
        let missing = wait_notes(&state, Instant::now(), |notes| notes.len() == 3);
        assert!(missing.is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Запись журнала останавливается на обеих своих границах: до `rename` на диске прежний
    /// журнал, после — новый; без барьера запись идёт насквозь.
    #[test]
    fn journal_write_stops_before_and_after_rename() {
        set_role(Role::Gui);
        let dir = temp_dir("barrier");
        let state = dir.join("state");
        let store = Store::new(&state);
        let installed = dir.join("install").join("probe-update.app");
        put(&store, &journal_in(State::Staged, &installed));
        let before = Barrier::new(
            Role::Gui,
            Point::Journal {
                state: State::Aborting,
                renamed: false,
            },
        );
        let after = Barrier::new(
            Role::Gui,
            Point::Journal {
                state: State::Aborting,
                renamed: true,
            },
        );
        arm(&state, before).unwrap();
        arm(&state, after).unwrap();
        let (sender, receiver) = mpsc::channel();
        let writer = {
            let store = store.clone();
            thread::spawn(move || {
                let written = store.advance(ATTEMPT, &[State::Staged], |journal| {
                    journal.state = State::Aborting;
                });
                sender.send(()).unwrap();
                written
            })
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        let reached = wait_reached(&state, before, deadline).unwrap();
        assert_eq!(reached.key, "gui.journal-aborting-before");
        assert_eq!(store.read().unwrap().unwrap().state, State::Staged);
        release(&state, before).unwrap();
        wait_reached(&state, after, deadline).unwrap();
        assert_eq!(store.read().unwrap().unwrap().state, State::Aborting);
        assert!(
            receiver.try_recv().is_err(),
            "запись ещё стоит после rename"
        );
        release(&state, after).unwrap();
        writer.join().unwrap().unwrap();
        // Без барьера граница проходится сразу.
        store
            .advance(ATTEMPT, &[State::Aborting], |journal| {
                journal.state = State::Idle;
                journal.outcome = Some(crate::journal::Outcome::Aborted);
            })
            .unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }
}
