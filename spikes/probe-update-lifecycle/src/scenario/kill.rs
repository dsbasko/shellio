//! Сценарий `kill` (задача 11): клетка B2 «Убийства на границах». Каждая часть — свежий стенд:
//! GUI N поколения 1 с живой `h1`, барьеры части, `update` поколением 2 и `activate`; на барьере
//! стенд посылает `SIGKILL` дошедшему процессу, затем — отдельный запуск восстановления (прямой
//! запуск исполняемого файла по установленному пути) до готового владельца и общий оракул:
//! установленный вариант, журнал, каталоги попытки, `h1` с хвостом и квитанцией, история
//! журнала, второе восстановление без изменений. Шаги и факты — в `kill/steps.json`, файлы
//! процессов, отметки и файлы помощника — в `kill/<часть>/`.
//!
//! Каталог частей ([`CASES`]): каждая строка таблицы B2 (`barrier::B2`) — часть или несколько
//! частей с единственным ожидаемым итогом; дополнительные части перечня закрывают записи журнала
//! до и после `rename`, которых нет в строках B2; гонки — четыре управляемые гонки раздела B2.

use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{Value, json};

use crate::attach::{self, Attached, Connected};
use crate::barrier::{self, Barrier, Mutation, Point, Role};
use crate::compat::Staging;
use crate::generation::Layout;
use crate::host::SessionRecord;
use crate::journal::{Journal, Outcome, State};
use crate::observer::{self, ProcessKey};
use crate::owner::{self, AppOptions, AppReport, Lock, OwnerInfo, Reply, Request};
use crate::proto::{ATTACH_VERSIONS, Purpose};
use crate::pty::Winsize;
use crate::recover::{self, Seen};
use crate::results::{Cell, Diagnostics, Environment, Report, Requirement};
use crate::{kq, launch, spawn, sys, update};

use super::Verdict;
use super::owner_basic::{Guard, ask, clear_state, reports, start_app};
use super::update_flow::{self, STEP, Stand, keep_helper_files};

/// Ожидаемый итог части после отдельного запуска восстановления.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expect {
    /// Установлен N, попытка закончена с итогом (прервана или откачена).
    Before(Outcome),
    /// Установлен и подтверждён N+1.
    After,
}

/// Откуда часть.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// Строка таблицы B2 с номером в `barrier::B2`.
    Row(usize),
    /// Дополнительная часть перечня записей журнала.
    Extra,
    /// Управляемая гонка раздела B2.
    Race,
}

/// Ход части после стимула.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    /// Убийства по `kills`: барьеры из `armed` — процессы протокола, остальные — запуски
    /// восстановления, каждый со своим барьером.
    Kill,
    /// Помощник убит на `open`, N+1 стоит на `started`: запуск восстановления видит `swapped` и
    /// живой N+1, затем N+1 отпускается.
    Opened,
    /// N+1 завис на `published`: второй запуск, затем `SIGKILL` N+1 и истечение срока.
    Hung,
    /// N+1 удержан на `published`, сторож снимает его в срок.
    WatchdogKill,
    /// N+1 на `published` получает `quit` и выходит без подтверждения.
    Quit,
    /// N+1 стоит до отчёта о запуске, помощник откатывает по сроку запуска.
    NotStarted,
    /// `committed` перед `rename` удержан до 2 с до срока.
    CommitFirst,
    /// `committed` перед `rename` удержан за срок.
    WatchdogFirst,
    /// Помощник убит, N+1 удержан на `published` за срок и отпущен.
    NextChecks,
    /// Второй запуск кода N в окне `helper.gui-exited`.
    HandoffGui,
    /// Второй запуск кода N ждёт блокировку у помощника.
    HandoffHelper,
    /// Второй `open -n` поколения 2 при N+1 на `started`.
    TwoNext,
    /// Помощник удержан после `launched`, пока N+1 не подтвердится.
    ShortCommitted,
}

/// Часть сценария.
#[derive(Clone, Copy, Debug)]
pub struct Case {
    pub name: &'static str,
    pub flow: Flow,
    pub origin: Origin,
    /// Барьеры, которые стенд ставит до стимула.
    pub armed: &'static [Barrier],
    /// Барьеры, на которых стенд посылает `SIGKILL` дошедшему процессу, по порядку: у цепочки
    /// восстановления — по звену на барьер.
    pub kills: &'static [Barrier],
    pub expect: Expect,
    /// Переходы журнала, которые часть проходит (сверяются по истории журнала).
    pub transitions: &'static [(State, State)],
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

use State::{
    Aborting, Cleaning, Committed, Idle, Intent, Launched, Retired, RollbackIntent, RolledBack,
    Staged, Swapped,
};

const ABORT_FROM_STAGED: &[(State, State)] =
    &[(Idle, Staged), (Staged, Aborting), (Aborting, Idle)];
const ABORT_FROM_INTENT: &[(State, State)] = &[
    (Idle, Staged),
    (Staged, Intent),
    (Intent, Aborting),
    (Aborting, Idle),
];
const COMMIT: &[(State, State)] = &[
    (Idle, Staged),
    (Staged, Intent),
    (Intent, Swapped),
    (Swapped, Launched),
    (Launched, Committed),
    (Committed, Cleaning),
    (Cleaning, Idle),
];
const ROLLBACK: &[(State, State)] = &[
    (Idle, Staged),
    (Staged, Intent),
    (Intent, Swapped),
    (Swapped, Launched),
    (Launched, RollbackIntent),
    (RollbackIntent, RolledBack),
    (RolledBack, Retired),
    (Retired, Idle),
];
const LAUNCH_TIMEOUT: &[(State, State)] = &[
    (Idle, Staged),
    (Staged, Intent),
    (Intent, Swapped),
    (Swapped, RollbackIntent),
    (RollbackIntent, RolledBack),
    (RolledBack, Retired),
    (Retired, Idle),
];

const ABORTED: Expect = Expect::Before(Outcome::Aborted);
const ROLLED_BACK: Expect = Expect::Before(Outcome::RolledBack);

/// Части сценария: строки B2 по порядку, дополнительные части перечня, гонки.
pub const CASES: &[Case] = &[
    Case {
        name: "gui-after-staged",
        flow: Flow::Kill,
        origin: Origin::Row(0),
        armed: &[journal(Role::Gui, Staged, true)],
        kills: &[journal(Role::Gui, Staged, true)],
        expect: ABORTED,
        transitions: ABORT_FROM_STAGED,
    },
    Case {
        name: "gui-after-intent",
        flow: Flow::Kill,
        origin: Origin::Row(1),
        armed: &[journal(Role::Gui, Intent, true)],
        kills: &[journal(Role::Gui, Intent, true)],
        expect: ABORTED,
        transitions: ABORT_FROM_INTENT,
    },
    Case {
        name: "helper-gui-exited",
        flow: Flow::Kill,
        origin: Origin::Row(2),
        armed: &[b(Role::Helper, Point::GuiExited)],
        kills: &[b(Role::Helper, Point::GuiExited)],
        expect: ABORTED,
        transitions: ABORT_FROM_INTENT,
    },
    Case {
        name: "helper-locked",
        flow: Flow::Kill,
        origin: Origin::Row(3),
        armed: &[b(Role::Helper, Point::Locked)],
        kills: &[b(Role::Helper, Point::Locked)],
        expect: ABORTED,
        transitions: ABORT_FROM_INTENT,
    },
    Case {
        name: "helper-exchanged",
        flow: Flow::Kill,
        origin: Origin::Row(4),
        armed: &[b(Role::Helper, Point::Exchanged)],
        kills: &[b(Role::Helper, Point::Exchanged)],
        expect: Expect::After,
        transitions: COMMIT,
    },
    Case {
        name: "helper-after-swapped",
        flow: Flow::Kill,
        origin: Origin::Row(5),
        armed: &[journal(Role::Helper, Swapped, true)],
        kills: &[journal(Role::Helper, Swapped, true)],
        expect: Expect::After,
        transitions: COMMIT,
    },
    Case {
        name: "helper-opened",
        flow: Flow::Opened,
        origin: Origin::Row(6),
        armed: &[
            b(Role::Helper, Point::Opened),
            b(Role::Next, Point::Started),
        ],
        kills: &[b(Role::Helper, Point::Opened)],
        expect: Expect::After,
        transitions: COMMIT,
    },
    Case {
        name: "helper-released",
        flow: Flow::Kill,
        origin: Origin::Row(7),
        armed: &[b(Role::Helper, Point::Released)],
        kills: &[b(Role::Helper, Point::Released)],
        expect: Expect::After,
        transitions: COMMIT,
    },
    Case {
        name: "helper-released-next-hung",
        flow: Flow::Hung,
        origin: Origin::Row(8),
        armed: &[
            b(Role::Helper, Point::Released),
            b(Role::Next, Point::Published),
        ],
        kills: &[
            b(Role::Helper, Point::Released),
            b(Role::Next, Point::Published),
        ],
        expect: ROLLED_BACK,
        transitions: ROLLBACK,
    },
    Case {
        name: "next-before-publish",
        flow: Flow::Kill,
        origin: Origin::Row(9),
        armed: &[b(Role::Next, Point::BeforePublish)],
        kills: &[b(Role::Next, Point::BeforePublish)],
        expect: ROLLED_BACK,
        transitions: ROLLBACK,
    },
    Case {
        name: "next-published",
        flow: Flow::Kill,
        origin: Origin::Row(10),
        armed: &[b(Role::Next, Point::Published)],
        kills: &[b(Role::Next, Point::Published)],
        expect: ROLLED_BACK,
        transitions: ROLLBACK,
    },
    Case {
        name: "next-after-committed",
        flow: Flow::Kill,
        origin: Origin::Row(11),
        armed: &[journal(Role::Next, Committed, true)],
        kills: &[journal(Role::Next, Committed, true)],
        expect: Expect::After,
        transitions: COMMIT,
    },
    Case {
        name: "next-cleaning",
        flow: Flow::Kill,
        origin: Origin::Row(12),
        armed: &[b(Role::Next, Point::RemovePartial)],
        kills: &[b(Role::Next, Point::RemovePartial)],
        expect: Expect::After,
        transitions: COMMIT,
    },
    Case {
        name: "next-unconfirmed",
        flow: Flow::WatchdogKill,
        origin: Origin::Row(13),
        armed: &[b(Role::Next, Point::Published)],
        kills: &[],
        expect: ROLLED_BACK,
        transitions: ROLLBACK,
    },
    Case {
        name: "helper-after-rollback-intent",
        flow: Flow::Kill,
        origin: Origin::Row(14),
        armed: &[
            b(Role::Next, Point::BeforePublish),
            journal(Role::Helper, RollbackIntent, true),
        ],
        kills: &[
            b(Role::Next, Point::BeforePublish),
            journal(Role::Helper, RollbackIntent, true),
        ],
        expect: ROLLED_BACK,
        transitions: ROLLBACK,
    },
    Case {
        name: "helper-reverse-swapped",
        flow: Flow::Kill,
        origin: Origin::Row(15),
        armed: &[
            b(Role::Next, Point::BeforePublish),
            step(Role::Helper, Mutation::Swap, true),
        ],
        kills: &[
            b(Role::Next, Point::BeforePublish),
            step(Role::Helper, Mutation::Swap, true),
        ],
        expect: ROLLED_BACK,
        transitions: ROLLBACK,
    },
    Case {
        name: "recovery-remove-partial",
        flow: Flow::Kill,
        origin: Origin::Row(16),
        armed: &[journal(Role::Gui, Intent, true)],
        kills: &[
            journal(Role::Gui, Intent, true),
            b(Role::Gui, Point::RemovePartial),
        ],
        expect: ABORTED,
        transitions: ABORT_FROM_INTENT,
    },
    Case {
        name: "recovery-retired-gap",
        flow: Flow::Kill,
        origin: Origin::Row(17),
        armed: &[
            b(Role::Helper, Point::Released),
            b(Role::Next, Point::Published),
        ],
        kills: &[
            b(Role::Helper, Point::Released),
            b(Role::Next, Point::Published),
            step(Role::Gui, Mutation::Retire, true),
        ],
        expect: ROLLED_BACK,
        transitions: ROLLBACK,
    },
    Case {
        name: "helper-retired-gap",
        flow: Flow::Kill,
        origin: Origin::Row(17),
        armed: &[
            b(Role::Next, Point::BeforePublish),
            step(Role::Helper, Mutation::Retire, true),
        ],
        kills: &[
            b(Role::Next, Point::BeforePublish),
            step(Role::Helper, Mutation::Retire, true),
        ],
        expect: ROLLED_BACK,
        transitions: ROLLBACK,
    },
    Case {
        name: "next-quit-unconfirmed",
        flow: Flow::Quit,
        origin: Origin::Row(18),
        armed: &[b(Role::Next, Point::Published)],
        kills: &[],
        expect: ROLLED_BACK,
        transitions: ROLLBACK,
    },
    Case {
        name: "chain-abort",
        flow: Flow::Kill,
        origin: Origin::Row(19),
        armed: &[journal(Role::Gui, Intent, true)],
        kills: &[
            journal(Role::Gui, Intent, true),
            journal(Role::Gui, Aborting, false),
            journal(Role::Gui, Aborting, true),
            step(Role::Gui, Mutation::Remove, false),
            step(Role::Gui, Mutation::Remove, true),
            journal(Role::Gui, Idle, false),
            journal(Role::Gui, Idle, true),
        ],
        expect: ABORTED,
        transitions: ABORT_FROM_INTENT,
    },
    Case {
        name: "chain-rollback",
        flow: Flow::Kill,
        origin: Origin::Row(19),
        armed: &[
            b(Role::Helper, Point::Released),
            b(Role::Next, Point::Published),
        ],
        kills: &[
            b(Role::Helper, Point::Released),
            b(Role::Next, Point::Published),
            journal(Role::Gui, RollbackIntent, false),
            journal(Role::Gui, RollbackIntent, true),
            step(Role::Gui, Mutation::Swap, false),
            step(Role::Gui, Mutation::Swap, true),
            journal(Role::Gui, RolledBack, false),
            journal(Role::Gui, RolledBack, true),
            step(Role::Gui, Mutation::Retire, false),
            journal(Role::Gui, Retired, false),
            journal(Role::Gui, Retired, true),
        ],
        expect: ROLLED_BACK,
        transitions: ROLLBACK,
    },
    Case {
        name: "chain-swapped",
        flow: Flow::Kill,
        origin: Origin::Row(19),
        armed: &[b(Role::Helper, Point::Exchanged)],
        kills: &[
            b(Role::Helper, Point::Exchanged),
            journal(Role::Gui, Swapped, false),
            journal(Role::Gui, Swapped, true),
        ],
        expect: Expect::After,
        transitions: COMMIT,
    },
    Case {
        name: "chain-cleaning",
        flow: Flow::Kill,
        origin: Origin::Row(19),
        armed: &[journal(Role::Next, Committed, true)],
        kills: &[
            journal(Role::Next, Committed, true),
            journal(Role::Gui, Cleaning, false),
            journal(Role::Gui, Cleaning, true),
        ],
        expect: Expect::After,
        transitions: COMMIT,
    },
    Case {
        name: "gui-before-staged",
        flow: Flow::Kill,
        origin: Origin::Extra,
        armed: &[journal(Role::Gui, Staged, false)],
        kills: &[journal(Role::Gui, Staged, false)],
        expect: ABORTED,
        transitions: &[],
    },
    Case {
        name: "gui-before-intent",
        flow: Flow::Kill,
        origin: Origin::Extra,
        armed: &[journal(Role::Gui, Intent, false)],
        kills: &[journal(Role::Gui, Intent, false)],
        expect: ABORTED,
        transitions: ABORT_FROM_STAGED,
    },
    Case {
        name: "helper-before-launched",
        flow: Flow::Kill,
        origin: Origin::Extra,
        armed: &[journal(Role::Helper, Launched, false)],
        kills: &[journal(Role::Helper, Launched, false)],
        expect: Expect::After,
        transitions: COMMIT,
    },
    Case {
        name: "helper-after-launched",
        flow: Flow::Kill,
        origin: Origin::Extra,
        armed: &[journal(Role::Helper, Launched, true)],
        kills: &[journal(Role::Helper, Launched, true)],
        expect: Expect::After,
        transitions: COMMIT,
    },
    Case {
        name: "next-before-committed",
        flow: Flow::Kill,
        origin: Origin::Extra,
        armed: &[journal(Role::Next, Committed, false)],
        kills: &[journal(Role::Next, Committed, false)],
        expect: ROLLED_BACK,
        transitions: ROLLBACK,
    },
    Case {
        name: "next-not-started",
        flow: Flow::NotStarted,
        origin: Origin::Extra,
        armed: &[b(Role::Next, Point::Launching)],
        kills: &[],
        expect: ROLLED_BACK,
        transitions: LAUNCH_TIMEOUT,
    },
    Case {
        name: "race-deadline-commit-first",
        flow: Flow::CommitFirst,
        origin: Origin::Race,
        armed: &[journal(Role::Next, Committed, false)],
        kills: &[],
        expect: Expect::After,
        transitions: COMMIT,
    },
    Case {
        name: "race-deadline-watchdog-first",
        flow: Flow::WatchdogFirst,
        origin: Origin::Race,
        armed: &[journal(Role::Next, Committed, false)],
        kills: &[],
        expect: ROLLED_BACK,
        transitions: ROLLBACK,
    },
    Case {
        name: "race-deadline-next-checks",
        flow: Flow::NextChecks,
        origin: Origin::Race,
        armed: &[
            b(Role::Helper, Point::Released),
            b(Role::Next, Point::Published),
        ],
        kills: &[b(Role::Helper, Point::Released)],
        expect: ROLLED_BACK,
        transitions: ROLLBACK,
    },
    Case {
        name: "race-handoff-gui-to-helper",
        flow: Flow::HandoffGui,
        origin: Origin::Race,
        armed: &[b(Role::Helper, Point::GuiExited)],
        kills: &[],
        expect: ABORTED,
        transitions: ABORT_FROM_INTENT,
    },
    Case {
        name: "race-handoff-helper-to-next",
        flow: Flow::HandoffHelper,
        origin: Origin::Race,
        armed: &[b(Role::Helper, Point::Locked)],
        kills: &[],
        expect: Expect::After,
        transitions: COMMIT,
    },
    Case {
        name: "race-two-next",
        flow: Flow::TwoNext,
        origin: Origin::Race,
        armed: &[b(Role::Next, Point::Started)],
        kills: &[],
        expect: Expect::After,
        transitions: COMMIT,
    },
    Case {
        name: "race-short-committed",
        flow: Flow::ShortCommitted,
        origin: Origin::Race,
        armed: &[b(Role::Helper, Point::Released)],
        kills: &[],
        expect: Expect::After,
        transitions: COMMIT,
    },
];

// --- стенд ---

pub const DEADLINE: Duration = Duration::from_secs(1500);
pub const DIR: &str = "kill";
pub const STEPS_ARTIFACT: &str = "kill/steps.json";
pub const CELL: &str = "B2";
const COMPLETION: &str = "барьер, выход процесса, готовность владельца или запись журнала в срок шага (30 с), срок health (60 с от intent) и срок запуска N+1 (20 с) у частей со сроком; иначе ошибка исполнения";
/// Насколько раньше срока снимается барьер `committed` в гонке «подтверждение первым».
const COMMIT_MARGIN_MS: u64 = 2000;
/// Насколько позже срока отпускается N+1 в гонке «N+1 сам видит срок».
const LATE_MARGIN_MS: u64 = 1000;
/// Сколько раз стенд повторяет запуск восстановления, если владелец не появился (например,
/// `open` поглотила LaunchServices).
const RECOVERY_RUNS: usize = 3;
/// Срок ожидания помощника, который ждёт срок health.
const HELPER_WAIT: Duration = Duration::from_secs(120);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

fn alive(key: ProcessKey) -> bool {
    observer::identify(key).is_ok_and(|found| found.is_alive())
}

/// Ждёт до момента `at_ms` по часам Unix без сна: `kevent` без событий со сроком.
fn wait_until_ms(at_ms: u64) -> Result<(), String> {
    let queue = kq::Queue::new()?;
    loop {
        let now = now_ms();
        if now >= at_ms {
            return Ok(());
        }
        queue.wait(Instant::now() + Duration::from_millis(at_ms - now))?;
    }
}

fn json_lines(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .map(|text| {
            text.lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Что стенд увидел на убийстве.
#[derive(Clone, Debug, Serialize)]
pub struct Killed {
    pub barrier: String,
    pub process: ProcessKey,
    pub journal: Option<State>,
    pub journal_present: bool,
    pub installed: Seen,
    pub dirs: Vec<String>,
    pub uptime_ns: u64,
}

/// Живая часть.
struct Live<'a> {
    case: &'a Case,
    stand: Stand,
    out: PathBuf,
    attempt: Option<String>,
    helper: Option<ProcessKey>,
    gui: ProcessKey,
    h1: SessionRecord,
    killed: Vec<Killed>,
    steps: Vec<Value>,
    runs: usize,
    /// События помощника и строки N+1, сохранённые до уборки каталога состояния.
    kept: Option<(Vec<Value>, Vec<AppReport>)>,
}

impl Live<'_> {
    fn state(&self) -> &Path {
        &self.stand.state
    }

    fn step(&mut self, name: &str, facts: Value) {
        self.steps.push(json!({ "name": name, "facts": facts }));
    }

    fn journal(&self) -> Result<Option<Journal>, String> {
        self.stand.store.read().map_err(|err| err.to_string())
    }

    /// `SIGKILL` процессу, дошедшему до барьера; барьер снимается, `.reached` удаляется.
    fn kill_at(&mut self, barrier: Barrier, deadline: Instant) -> Result<ProcessKey, String> {
        let reached = barrier::wait_reached(self.state(), barrier, deadline)?;
        let journal = self.journal()?;
        let installed = self.stand.check(&self.stand.installed)?;
        let killed = Killed {
            barrier: barrier.key(),
            process: reached.process,
            journal: journal.as_ref().map(|journal| journal.state),
            journal_present: journal.is_some(),
            installed,
            dirs: self.stand.attempt_dirs(),
            uptime_ns: sys::uptime_ns(),
        };
        if alive(reached.process) {
            // SAFETY: сигнал процессу, узнанному по PID и времени старта на барьере.
            unsafe { libc::kill(reached.process.pid, libc::SIGKILL) };
        }
        if !launch::wait_exit(&[reached.process.pid], STEP).is_empty() && alive(reached.process) {
            return Err(format!(
                "стенд: {} не вышел после SIGKILL",
                reached.process.pid
            ));
        }
        barrier::release(self.state(), barrier)?;
        let _ = fs::remove_file(barrier.reached_path(self.state()));
        if self.attempt.is_none() {
            self.attempt = journal.map(|journal| journal.attempt);
        }
        self.killed.push(killed);
        Ok(reached.process)
    }

    /// Снять барьер и удалить `.reached`.
    fn release(&self, barrier: Barrier) -> Result<(), String> {
        barrier::release(self.state(), barrier)?;
        let _ = fs::remove_file(barrier.reached_path(self.state()));
        Ok(())
    }

    /// Ждёт, пока журнал не удовлетворит `ready`.
    fn wait_journal(
        &self,
        deadline: Instant,
        ready: impl Fn(Option<&Journal>) -> bool,
    ) -> Result<Option<Journal>, String> {
        let dir = self.state().join(crate::journal::UPDATE_DIR);
        fs::create_dir_all(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
        let queue = kq::Queue::on_dir(&dir)?;
        loop {
            let journal = self.journal()?;
            if ready(journal.as_ref()) {
                return Ok(journal);
            }
            if queue.wait(deadline)? == kq::Event::TimedOut {
                return Err(format!("стенд: журнал к сроку не дошёл: {journal:?}"));
            }
        }
    }

    /// Прямой запуск исполняемого файла по установленному пути.
    fn start_run(
        &mut self,
        guard: &mut Guard,
        name: &str,
        forward: Vec<String>,
    ) -> Result<i32, String> {
        let options = AppOptions {
            forward,
            ..self.stand.options.clone()
        };
        let child = start_app(&self.stand.app_bin, &options, &self.out, name)?;
        let pid = child.id() as i32;
        guard.apps.push(child);
        Ok(pid)
    }

    /// Запуски восстановления до готового владельца.
    fn recover_until_owner(&mut self, guard: &mut Guard) -> Result<OwnerInfo, String> {
        let mut tried = Vec::new();
        for _ in 0..RECOVERY_RUNS {
            self.runs += 1;
            let name = format!("recovery-{}", self.runs);
            let pid = self.start_run(guard, &name, Vec::new())?;
            match owner::wait_for_owner(self.state(), Instant::now() + STEP) {
                Ok(info) => {
                    self.step(
                        "recovery",
                        json!({ "run": name, "pid": pid, "owner": info, "tried": tried }),
                    );
                    return Ok(info);
                }
                Err(reason) => {
                    let exited = launch::wait_exit(&[pid], Duration::ZERO).is_empty();
                    tried.push(
                        json!({ "run": name, "pid": pid, "reason": reason, "exited": exited }),
                    );
                    if !exited {
                        return Err(format!("стенд: владельца нет, {name} жив: {reason}"));
                    }
                }
            }
        }
        Err(format!(
            "стенд: владельца нет после {RECOVERY_RUNS} запусков: {tried:?}"
        ))
    }

    /// Запуск восстановления, убитый на барьере.
    fn recovery_killed(&mut self, guard: &mut Guard, barrier: Barrier) -> Result<(), String> {
        barrier::arm(self.state(), barrier)?;
        self.runs += 1;
        let name = format!("recovery-{}", self.runs);
        let pid = self.start_run(guard, &name, Vec::new())?;
        let key = self.kill_at(barrier, Instant::now() + STEP)?;
        if key.pid != pid {
            return Err(format!(
                "стенд: на барьере {} стоял {}, запущен {pid}",
                barrier.key(),
                key.pid
            ));
        }
        Ok(())
    }

    fn wait_helper(&self, limit: Duration) -> Result<(), String> {
        if let Some(helper) = self.helper
            && !launch::wait_exit(&[helper.pid], limit).is_empty()
            && alive(helper)
        {
            return Err(format!("стенд: помощник {} не вышел", helper.pid));
        }
        Ok(())
    }

    fn helper_events(&self) -> Vec<Value> {
        if let Some((events, _)) = &self.kept {
            return events.clone();
        }
        match &self.attempt {
            Some(attempt) => {
                json_lines(&update::helper_dir(self.state(), attempt).join(update::HELPER_STDOUT))
            }
            None => Vec::new(),
        }
    }

    fn next_reports(&self) -> Vec<AppReport> {
        if let Some((_, reports)) = &self.kept {
            return reports.clone();
        }
        match &self.attempt {
            Some(attempt) => {
                json_lines(&update::helper_dir(self.state(), attempt).join(update::NEXT_STDOUT))
                    .into_iter()
                    .filter(|line| line.get("event").is_none())
                    .filter_map(|line| serde_json::from_value(line).ok())
                    .collect()
            }
            None => Vec::new(),
        }
    }
}

fn has_event(events: &[Value], name: &str) -> bool {
    events
        .iter()
        .any(|event| event.get("event").and_then(Value::as_str) == Some(name))
}

/// Ключи процессов `started.*.json` попытки.
fn started_keys(state: &Path, attempt: &str) -> Vec<ProcessKey> {
    let dir = update::helper_dir(state, attempt);
    let mut keys: Vec<ProcessKey> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|entry| {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    name.starts_with(update::STARTED_PREFIX) && name.ends_with(".json")
                })
                .filter_map(|entry| fs::read(entry.path()).ok())
                .filter_map(|bytes| serde_json::from_slice::<update::Started>(&bytes).ok())
                .map(|started| started.key)
                .collect()
        })
        .unwrap_or_default();
    keys.sort_by_key(|key| key.start_us);
    keys
}

pub fn run(out: &Path, environment: &Environment) -> Result<Report, String> {
    let (state, nonce, layout) = super::stand_inputs(environment)?;
    let dir = out.join(DIR);
    fs::create_dir(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    let filter: Option<Vec<String>> = std::env::var(launch::KILL_PARTS)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.split_whitespace().map(str::to_owned).collect());
    let mut guard = Guard {
        state: state.clone(),
        apps: Vec::new(),
        sessions: Vec::new(),
        nonce: Some(nonce.clone()),
        armed: true,
    };
    let mut steps = Vec::new();
    let mut cells = Vec::new();
    let mut failure = None;
    for case in CASES {
        if filter
            .as_ref()
            .is_some_and(|names| !names.iter().any(|name| name == case.name))
        {
            continue;
        }
        let case_out = dir.join(case.name);
        if let Err(err) = fs::create_dir(&case_out) {
            failure = Some(format!("{}: {err}", case_out.display()));
            break;
        }
        let started = Instant::now();
        match run_case(case, &state, &nonce, &layout, &case_out, &mut guard) {
            Ok((cell, facts)) => {
                steps.push(json!({ "case": case.name, "seconds": started.elapsed().as_secs_f64(), "facts": facts }));
                cells.push(cell);
            }
            Err((reason, facts)) => {
                steps.push(json!({ "case": case.name, "error": reason, "facts": facts }));
                failure = Some(format!("{}: {reason}", case.name));
                break;
            }
        }
    }
    guard.armed = failure.is_some();
    drop(guard);
    let written = crate::results::write_json(&dir, "steps.json", &steps);
    if let Some(reason) = failure {
        return Err(reason);
    }
    written?;
    let mut notes = vec![format!(
        "частей {}, срок health {} с от intent, срок запуска N+1 {} с, срок шага стенда {} с; барьер committed в гонке «подтверждение первым» снят за {} мс до срока, N+1 в гонке «N+1 сам видит срок» отпущен через {} мс после срока",
        cells.len(),
        update::HEALTH_WINDOW.as_secs(),
        update::LAUNCH_WAIT.as_secs(),
        STEP.as_secs(),
        COMMIT_MARGIN_MS,
        LATE_MARGIN_MS
    )];
    let partial = filter.map(|names| {
        format!(
            "отладочный прогон с фильтром {}: {}; рубежом не считается",
            launch::KILL_PARTS,
            names.join(" ")
        )
    });
    notes.extend(partial.clone());
    Ok(Report {
        cells,
        diagnostics: Diagnostics { notes, partial },
    })
}

type CaseResult = Result<(Cell, Value), (String, Value)>;

fn run_case(
    case: &Case,
    state: &Path,
    nonce: &str,
    layout: &Layout,
    out: &Path,
    guard: &mut Guard,
) -> CaseResult {
    let mut live = None;
    let result = drive(case, state, nonce, layout, out, guard, &mut live);
    let facts = live.as_ref().map_or(Value::Null, facts_of);
    if let Some(live) = &live
        && live.kept.is_none()
    {
        let _ = copy_dir(&barrier::notes_dir(&live.stand.state), &out.join("notes"));
        if let Some(attempt) = &live.attempt {
            let _ = keep_helper_files(&live.stand.state, attempt, out);
        }
    }
    match result {
        Ok(verdict) => Ok((part(case, verdict), facts)),
        Err(reason) => Err((reason, facts)),
    }
}

fn facts_of(live: &Live) -> Value {
    json!({
        "attempt": live.attempt,
        "helper": live.helper,
        "gui": live.gui,
        "h1": live.h1,
        "killed": live.killed,
        "steps": live.steps,
        "helper_events": live.helper_events(),
    })
}

fn copy_dir(source: &Path, dest: &Path) -> Result<(), String> {
    fs::create_dir_all(dest).map_err(|err| format!("{}: {err}", dest.display()))?;
    for entry in fs::read_dir(source)
        .map_err(|err| format!("{}: {err}", source.display()))?
        .filter_map(Result::ok)
    {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with('.') {
            fs::copy(entry.path(), dest.join(&name))
                .map_err(|err| format!("{}: {err}", entry.path().display()))?;
        }
    }
    Ok(())
}

fn part(case: &Case, verdict: Verdict) -> Cell {
    Cell {
        id: CELL.to_owned(),
        part: Some(case.name.to_owned()),
        requirement: Requirement::Required,
        status: verdict.status,
        stimulus: stimulus(case),
        oracle: oracle(case),
        completion: COMPLETION.to_owned(),
        reason: verdict.reason,
        artifacts: vec![STEPS_ARTIFACT.to_owned(), format!("{DIR}/{}", case.name)],
    }
}

fn keys(barriers: &[Barrier]) -> String {
    barriers
        .iter()
        .map(|barrier| barrier.key())
        .collect::<Vec<_>>()
        .join(", ")
}

fn stimulus(case: &Case) -> String {
    let origin = match case.origin {
        Origin::Row(index) => format!(
            "строка B2 «{} {}»",
            crate::barrier::B2[index].actor,
            crate::barrier::B2[index].boundary
        ),
        Origin::Extra => "дополнительная часть перечня записей журнала".to_owned(),
        Origin::Race => "управляемая гонка B2".to_owned(),
    };
    format!(
        "{origin}: барьеры {}; SIGKILL на {}; ход {:?}",
        keys(case.armed),
        if case.kills.is_empty() {
            "—".to_owned()
        } else {
            keys(case.kills)
        },
        case.flow
    )
}

fn oracle(case: &Case) -> String {
    let outcome = match case.expect {
        Expect::After => "установлен N+1 (CDHash поколения 2, codesign), журнал idle «подтверждена», каталога попытки нет".to_owned(),
        Expect::Before(Outcome::Aborted) if case.name == "gui-before-staged" => {
            "установлен N (CDHash поколения 1), журнала нет, каталог кандидата без записи журнала остаётся".to_owned()
        }
        Expect::Before(Outcome::Aborted) => {
            "установлен N (CDHash поколения 1), журнал idle «прервана», каталога попытки нет".to_owned()
        }
        Expect::Before(_) => "установлен N (CDHash поколения 1), журнал idle «откачена», отказавший N+1 в .shellio-failed-<attempt> проходит проверку как поколение 2".to_owned(),
    };
    format!(
        "{outcome}; владелец единственный, его поколение — установленное; h1 с прежними процессами, хвост без пропуска, квитанция ack 2 ping; история попытки проходит переходы части; звенья: журнал до rename прежний, после — новый; второе восстановление под блокировкой не меняет журнал, историю и раскладку; ход части — раздел отчёта «Ожидания B2»"
    )
}

/// Барьер записи журнала роли GUI в состоянии `state`.
fn gui_journal_blocks(case: &Case, state: State) -> bool {
    case.armed.iter().any(|barrier| {
        barrier.role == Role::Gui
            && matches!(barrier.point, Point::Journal { state: s, .. } if s == state)
    })
}

fn find(case: &Case, role: Role, point: Point) -> Result<Barrier, String> {
    case.armed
        .iter()
        .copied()
        .find(|barrier| barrier.role == role && barrier.point == point)
        .ok_or_else(|| {
            format!(
                "стенд: у части {} нет барьера {role:?} {point:?}",
                case.name
            )
        })
}

fn drive<'a>(
    case: &'a Case,
    state: &Path,
    nonce: &str,
    layout: &Layout,
    out: &Path,
    guard: &mut Guard,
    slot: &mut Option<Live<'a>>,
) -> Result<Verdict, String> {
    let stand = Stand::at(
        state,
        nonce,
        layout,
        "gen-1",
        &format!("install-{}", case.name),
    )?;
    let notes = barrier::notes_dir(state);
    fs::create_dir_all(&notes).map_err(|err| format!("{}: {err}", notes.display()))?;
    let (_, info) = stand.start_owner(guard, out, "gui-n")?;
    let h1 = stand.create_session(guard, "h1")?;
    let hello = attach::hello(Purpose::Attach, ATTACH_VERSIONS, 0, Winsize::cells(0, 0));
    let mut first = match attach::connect(Path::new(&h1.socket), &hello, STEP)? {
        Connected::Welcome(client) => Attached::new(client, Some(1)),
        Connected::Refused(reason) => return Err(format!("стенд: attach h1: {reason}")),
    };
    let deadline = Instant::now() + STEP;
    first.pump_until(deadline, |transcript| !transcript.lines.is_empty())?;
    first.exchange(1, "ping", deadline)?;
    first.detach(Instant::now() + STEP)?;
    *slot = Some(Live {
        case,
        stand,
        out: out.to_path_buf(),
        attempt: None,
        helper: None,
        gui: info.key,
        h1,
        killed: Vec::new(),
        steps: Vec::new(),
        runs: 0,
        kept: None,
    });
    let live = slot.as_mut().ok_or("стенд: часть")?;
    for barrier in case.armed {
        barrier::arm(state, *barrier)?;
    }
    let candidate = layout.get("gen-2")?.app.display().to_string();
    let update_request = Request::Update { candidate };
    let mut pending = None;
    if gui_journal_blocks(case, State::Staged) {
        let state = state.to_path_buf();
        pending = Some(thread::spawn(move || {
            owner::request(&state, &update_request, STEP)
        }));
    } else {
        match ask(state, &update_request)? {
            Reply::Staging(staging) => match *staging {
                Staging::Staged { attempt, .. } => live.attempt = Some(attempt),
                other => return Err(format!("стенд: update: {other:?}")),
            },
            other => return Err(format!("стенд: update: {other:?}")),
        }
        if gui_journal_blocks(case, State::Intent) {
            let state = state.to_path_buf();
            pending = Some(thread::spawn(move || {
                owner::request(&state, &Request::Activate, STEP)
            }));
        } else {
            let (attempt, helper) = update_flow::activate(state)?;
            live.attempt = Some(attempt);
            live.helper = Some(helper);
        }
    }

    let protocol: Vec<Barrier> = case
        .kills
        .iter()
        .copied()
        .filter(|barrier| case.armed.contains(barrier))
        .collect();
    let chain: Vec<Barrier> = case
        .kills
        .iter()
        .copied()
        .filter(|barrier| !case.armed.contains(barrier))
        .collect();
    let mut flow = json!({});
    match case.flow {
        Flow::Kill => {
            for barrier in &protocol {
                live.kill_at(*barrier, Instant::now() + STEP)?;
            }
            live.wait_helper(STEP)?;
        }
        Flow::Opened => {
            live.kill_at(protocol[0], Instant::now() + STEP)?;
            let started = find(case, Role::Next, Point::Started)?;
            let parked = barrier::wait_reached(state, started, Instant::now() + STEP)?;
            live.runs += 1;
            let name = format!("recovery-{}", live.runs);
            let pid = live.start_run(guard, &name, Vec::new())?;
            let attempt = live.attempt.clone().unwrap_or_default();
            let claimed = live.wait_journal(Instant::now() + STEP, |journal| {
                journal.is_some_and(|journal| {
                    journal.attempt == attempt && journal.state == State::Launched
                })
            })?;
            live.release(started)?;
            flow = json!({ "parked": parked, "run": pid, "claimed": claimed, "started": started_keys(state, &attempt) });
        }
        Flow::Hung => {
            live.kill_at(protocol[0], Instant::now() + STEP)?;
            let published = find(case, Role::Next, Point::Published)?;
            let parked = barrier::wait_reached(state, published, Instant::now() + STEP)?;
            let before = (
                live.journal()?,
                live.stand.check(&live.stand.installed)?,
                live.stand.attempt_dirs(),
            );
            let pid = live.start_run(guard, "second", vec![format!("b2-hung-{nonce}")])?;
            if !launch::wait_exit(&[pid], owner::SOCKET_WAIT + STEP).is_empty() {
                return Err("стенд: второй запуск при зависшем N+1 не вышел".to_owned());
            }
            let second = reports(out, "second")?;
            let after = (
                live.journal()?,
                live.stand.check(&live.stand.installed)?,
                live.stand.attempt_dirs(),
            );
            live.kill_at(published, Instant::now() + STEP)?;
            let deadline_ms = before
                .0
                .as_ref()
                .and_then(|journal| journal.deadline_ms)
                .ok_or("стенд: нет срока")?;
            wait_until_ms(deadline_ms)?;
            flow = json!({
                "parked": parked,
                "second": second,
                "untouched": before.0 == after.0 && before.1 == after.1 && before.2 == after.2,
                "before": { "journal": before.0, "installed": before.1, "dirs": before.2 },
                "deadline_ms": deadline_ms,
                "waited_to_ms": now_ms(),
            });
        }
        Flow::WatchdogKill | Flow::WatchdogFirst => {
            let barrier = case.armed[0];
            let parked = barrier::wait_reached(state, barrier, Instant::now() + STEP)?;
            live.wait_helper(HELPER_WAIT)?;
            let next_alive = alive(parked.process);
            live.release(barrier)?;
            flow = json!({ "parked": parked, "next_alive_after_helper": next_alive });
        }
        Flow::Quit => {
            let barrier = case.armed[0];
            let parked = barrier::wait_reached(state, barrier, Instant::now() + STEP)?;
            let quit = ask(state, &Request::Quit)?;
            live.release(barrier)?;
            let gone = launch::wait_exit(&[parked.process.pid], STEP).is_empty();
            live.wait_helper(STEP)?;
            flow = json!({ "parked": parked, "quit": format!("{quit:?}"), "next_exited": gone, "now_ms": now_ms() });
        }
        Flow::NotStarted => {
            let barrier = case.armed[0];
            let parked = barrier::wait_reached(state, barrier, Instant::now() + STEP)?;
            live.wait_helper(update::LAUNCH_WAIT + STEP)?;
            live.kill_at(barrier, Instant::now() + STEP)?;
            flow = json!({ "parked": parked });
        }
        Flow::CommitFirst => {
            let barrier = case.armed[0];
            let parked = barrier::wait_reached(state, barrier, Instant::now() + STEP)?;
            let deadline_ms = live
                .journal()?
                .and_then(|journal| journal.deadline_ms)
                .ok_or("стенд: нет срока")?;
            wait_until_ms(deadline_ms.saturating_sub(COMMIT_MARGIN_MS))?;
            let released_ms = now_ms();
            live.release(barrier)?;
            let owner = owner::wait_for_owner(state, Instant::now() + STEP)?;
            live.wait_helper(STEP)?;
            flow = json!({ "parked": parked, "deadline_ms": deadline_ms, "released_ms": released_ms, "owner": owner });
        }
        Flow::NextChecks => {
            live.kill_at(protocol[0], Instant::now() + STEP)?;
            let published = find(case, Role::Next, Point::Published)?;
            let parked = barrier::wait_reached(state, published, Instant::now() + STEP)?;
            let deadline_ms = live
                .journal()?
                .and_then(|journal| journal.deadline_ms)
                .ok_or("стенд: нет срока")?;
            wait_until_ms(deadline_ms + LATE_MARGIN_MS)?;
            let released_ms = now_ms();
            live.release(published)?;
            let gone = launch::wait_exit(&[parked.process.pid], STEP).is_empty();
            flow = json!({ "parked": parked, "deadline_ms": deadline_ms, "released_ms": released_ms, "next_exited": gone });
        }
        Flow::HandoffGui => {
            let barrier = case.armed[0];
            let parked = barrier::wait_reached(state, barrier, Instant::now() + STEP)?;
            let pid = live.start_run(guard, "second", vec![format!("b2-handoff-gui-{nonce}")])?;
            let owner = owner::wait_for_owner(state, Instant::now() + STEP)?;
            live.release(barrier)?;
            flow = json!({ "parked": parked, "second": pid, "owner": owner });
        }
        Flow::HandoffHelper => {
            let barrier = case.armed[0];
            let parked = barrier::wait_reached(state, barrier, Instant::now() + STEP)?;
            let pid =
                live.start_run(guard, "second", vec![format!("b2-handoff-helper-{nonce}")])?;
            barrier::wait_notes(state, Instant::now() + STEP, |notes| {
                notes
                    .iter()
                    .any(|note| note.event == "lock" && note.process.pid == pid)
            })?;
            live.release(barrier)?;
            let owner = owner::wait_for_owner(state, Instant::now() + STEP)?;
            if !launch::wait_exit(&[pid], owner::SOCKET_WAIT + STEP).is_empty() {
                return Err("стенд: второй запуск не вышел".to_owned());
            }
            flow = json!({ "parked": parked, "second": pid, "second_reports": reports(out, "second")?, "owner": owner });
        }
        Flow::TwoNext => {
            let barrier = case.armed[0];
            let parked = barrier::wait_reached(state, barrier, Instant::now() + STEP)?;
            let attempt = live.attempt.clone().unwrap_or_default();
            live.wait_journal(Instant::now() + STEP, |journal| {
                journal.is_some_and(|journal| {
                    journal.attempt == attempt && journal.state == State::Launched
                })
            })?;
            let options = AppOptions {
                forward: vec![format!("b2-two-next-{nonce}")],
                ..live.stand.options.clone()
            };
            let opened = open_new(&live.stand, &options, out, "second")?;
            let other = barrier::wait_notes(state, Instant::now() + STEP, |notes| {
                notes.iter().any(|note| {
                    note.event == "role"
                        && note.process != parked.process
                        && note.process != live.gui
                })
            })?;
            live.release(barrier)?;
            let owner = owner::wait_for_owner(state, Instant::now() + STEP)?;
            let second_key = other
                .iter()
                .find(|note| {
                    note.event == "role"
                        && note.process != parked.process
                        && note.process != live.gui
                })
                .map(|note| note.process);
            if let Some(key) = second_key
                && !launch::wait_exit(&[key.pid], owner::SOCKET_WAIT + STEP).is_empty()
            {
                return Err("стенд: второй N+1 не вышел".to_owned());
            }
            flow = json!({
                "parked": parked,
                "open": opened,
                "second": second_key,
                "second_reports": reports(out, "second").unwrap_or_default(),
                "owner": owner,
            });
        }
        Flow::ShortCommitted => {
            let barrier = case.armed[0];
            let parked = barrier::wait_reached(state, barrier, Instant::now() + STEP)?;
            let owner = owner::wait_for_owner(state, Instant::now() + STEP)?;
            let journal = live.journal()?;
            live.release(barrier)?;
            live.wait_helper(STEP)?;
            flow = json!({ "parked": parked, "owner": owner, "journal_at_release": journal });
        }
    }
    if let Some(pending) = pending {
        let answer = pending
            .join()
            .map_err(|_| "стенд: поток запроса".to_owned())?;
        live.step(
            "blocked-request",
            json!({ "answer": format!("{answer:?}") }),
        );
    }
    live.step("flow", flow);
    for barrier in &chain {
        live.recovery_killed(guard, *barrier)?;
    }
    let owner = live.recover_until_owner(guard)?;
    let checked = observe(live, &owner, layout)?;
    let closed = close(live, guard, &owner)?;
    live.step("closed", closed.clone());
    Ok(judge(live, &owner, &checked, &closed))
}

/// `open -n` установленного бандла с аргументами роли GUI; stdout и stderr — в `out/<name>.*`.
fn open_new(stand: &Stand, options: &AppOptions, out: &Path, name: &str) -> Result<Value, String> {
    let mut args = vec![
        "-n".to_owned(),
        "--env".to_owned(),
        format!("{}={}", launch::STATE_DIR, stand.state.display()),
        "--stdout".to_owned(),
        out.join(format!("{name}.stdout")).display().to_string(),
        "--stderr".to_owned(),
        out.join(format!("{name}.stderr")).display().to_string(),
        stand.installed.display().to_string(),
        "--args".to_owned(),
    ];
    args.extend(options.to_args());
    let output =
        spawn::output(Path::new("/usr/bin/open"), &args).map_err(|err| format!("open: {err}"))?;
    Ok(json!({
        "code": output.exit.code,
        "stderr": String::from_utf8_lossy(&output.stderr).trim(),
    }))
}

/// Что стенд видит после готового владельца.
#[derive(Clone, Debug, Serialize)]
struct Checked {
    installed: Seen,
    journal: Option<Journal>,
    dirs: Vec<String>,
    failed: Seen,
    history: Vec<State>,
    h1_same: bool,
    tail_problems: Vec<String>,
    tail_lines: usize,
    receipt: bool,
    gen1: String,
    gen2: String,
    started: Vec<ProcessKey>,
}

fn observe(live: &mut Live, owner: &OwnerInfo, layout: &Layout) -> Result<Checked, String> {
    let journal = live.journal()?;
    let attempt = live
        .attempt
        .clone()
        .or_else(|| journal.as_ref().map(|journal| journal.attempt.clone()));
    live.attempt = attempt.clone();
    let failed = match &journal {
        Some(journal) => live.stand.check(&journal.failed_dir())?,
        None => Seen::Absent,
    };
    let history: Vec<State> = live
        .stand
        .store
        .history()?
        .into_iter()
        .filter(|entry| Some(&entry.journal.attempt) == attempt.as_ref())
        .map(|entry| entry.journal.state)
        .collect();
    let h1_same = update_flow::same_processes(&live.h1)?;
    let hello = attach::hello(Purpose::Attach, ATTACH_VERSIONS, 0, Winsize::cells(0, 0));
    let (tail_problems, tail_lines, receipt) =
        match attach::connect(Path::new(&live.h1.socket), &hello, STEP)? {
            Connected::Welcome(client) => {
                let mut tail = Attached::new(client, Some(1));
                let deadline = Instant::now() + STEP;
                let receipt = tail.exchange(2, "ping", deadline).is_ok();
                let _ = tail.detach(Instant::now() + STEP);
                (
                    tail.transcript.problems.clone(),
                    tail.transcript.lines.len(),
                    receipt,
                )
            }
            Connected::Refused(reason) => (vec![format!("attach h1: {reason}")], 0, false),
        };
    let checked = Checked {
        installed: live.stand.check(&live.stand.installed)?,
        journal,
        dirs: live.stand.attempt_dirs(),
        failed,
        history,
        h1_same,
        tail_problems,
        tail_lines,
        receipt,
        gen1: layout.get("gen-1")?.cdhash.clone(),
        gen2: layout.get("gen-2")?.cdhash.clone(),
        started: attempt
            .as_deref()
            .map(|attempt| started_keys(&live.stand.state, attempt))
            .unwrap_or_default(),
    };
    live.step("checked", json!({ "owner": owner, "checked": checked }));
    Ok(checked)
}

/// `quit` владельца, второе восстановление стендом под блокировкой, остановка сессии, уборка.
fn close(live: &mut Live, guard: &mut Guard, owner: &OwnerInfo) -> Result<Value, String> {
    let state = live.stand.state.clone();
    let quit = ask(&state, &Request::Quit)?;
    if !launch::wait_exit(&[owner.key.pid], STEP).is_empty() && alive(owner.key) {
        return Err(format!(
            "стенд: владелец {} не вышел по quit",
            owner.key.pid
        ));
    }
    live.wait_helper(STEP)?;
    for app in &mut guard.apps {
        let pid = app.id() as i32;
        if !launch::wait_exit(&[pid], STEP).is_empty() {
            return Err(format!("стенд: процесс {pid} не вышел"));
        }
        let _ = app.wait();
    }
    guard.apps.clear();
    let journal_path = live.stand.store.path();
    let before = (
        fs::read(&journal_path).ok(),
        live.stand.store.history()?.len(),
        live.stand.attempt_dirs(),
        live.stand.check(&live.stand.installed)?,
    );
    let me = observer::current()?.key;
    let lock =
        Lock::acquire(&owner::lock_path(&state)).map_err(|err| format!("блокировка: {err}"))?;
    let is_alive = |key: &ProcessKey| alive(*key);
    let ctx = recover::Context {
        now_ms: now_ms(),
        actor: recover::Actor {
            key: me,
            launched_for: None,
        },
        alive: &is_alive,
    };
    let second = recover::recover(&live.stand.store, &live.stand.checker, &ctx)
        .map_err(|err| err.to_string());
    drop(lock);
    let after = (
        fs::read(&journal_path).ok(),
        live.stand.store.history()?.len(),
        live.stand.attempt_dirs(),
        live.stand.check(&live.stand.installed)?,
    );
    let unchanged = before == after;
    let steps = match &second {
        Ok(report) => format!("{:?}", report.steps),
        Err(reason) => reason.clone(),
    };
    let empty = second.as_ref().is_ok_and(|report| report.steps.is_empty());
    update_flow::stop(&live.h1)?;
    guard.sessions.clear();
    let left = launch::find_by_token(&guard.nonce.clone().unwrap_or_default());
    if !left.is_empty() {
        return Err(format!("стенд: остались процессы стенда {left:?}"));
    }
    live.kept = Some((live.helper_events(), live.next_reports()));
    copy_dir(&barrier::notes_dir(&state), &live.out.join("notes"))?;
    if let Some(attempt) = &live.attempt {
        keep_helper_files(&state, attempt, &live.out)?;
    }
    clear_state(&state);
    Ok(json!({
        "quit": format!("{quit:?}"),
        "second_recovery": steps,
        "second_recovery_empty": empty,
        "unchanged": unchanged,
        "history_len": before.1,
    }))
}

fn violation(violations: &mut Vec<String>, ok: bool, what: impl FnOnce() -> String) {
    if !ok {
        violations.push(what());
    }
}

fn judge(live: &Live, owner: &OwnerInfo, checked: &Checked, closed: &Value) -> Verdict {
    let case = live.case;
    let mut v = Vec::new();
    let (generation, cdhash) = match case.expect {
        Expect::After => (2, &checked.gen2),
        Expect::Before(_) => (1, &checked.gen1),
    };
    violation(
        &mut v,
        matches!(&checked.installed, Seen::Valid { cdhash: got } if got == cdhash),
        || {
            format!(
                "установлен {:?}, ждали поколение {generation}",
                checked.installed
            )
        },
    );
    violation(&mut v, owner.generation == generation, || {
        format!("владелец поколения {}", owner.generation)
    });
    let attempt = live.attempt.clone().unwrap_or_default();
    let orphan = case.name == "gui-before-staged";
    if orphan {
        violation(&mut v, checked.journal.is_none(), || {
            format!("журнал {:?}", checked.journal)
        });
        violation(
            &mut v,
            checked
                .dirs
                .iter()
                .any(|name| name.starts_with(crate::journal::STAGED_PREFIX)),
            || format!("каталога кандидата без журнала нет: {:?}", checked.dirs),
        );
    } else {
        let outcome = match case.expect {
            Expect::After => Outcome::Committed,
            Expect::Before(outcome) => outcome,
        };
        violation(
            &mut v,
            checked.journal.as_ref().is_some_and(|journal| {
                journal.attempt == attempt
                    && journal.state == State::Idle
                    && journal.outcome == Some(outcome)
            }),
            || format!("журнал {:?}, ждали idle {outcome:?}", checked.journal),
        );
        violation(
            &mut v,
            !checked
                .dirs
                .iter()
                .any(|name| name == &format!("{}{attempt}", crate::journal::STAGED_PREFIX)),
            || format!("каталог попытки остался: {:?}", checked.dirs),
        );
        if case.expect == Expect::Before(Outcome::RolledBack) {
            violation(
                &mut v,
                matches!(&checked.failed, Seen::Valid { cdhash } if cdhash == &checked.gen2),
                || format!("отказавший N+1: {:?}", checked.failed),
            );
        }
        let mut covered: Vec<(State, State)> = Vec::new();
        let mut prev = State::Idle;
        for state in &checked.history {
            if *state != prev {
                covered.push((prev, *state));
                prev = *state;
            }
        }
        for transition in case.transitions {
            violation(&mut v, covered.contains(transition), || {
                format!(
                    "перехода {transition:?} нет в истории {:?}",
                    checked.history
                )
            });
        }
    }
    violation(&mut v, checked.h1_same, || {
        "h1: процессы сменились".to_owned()
    });
    violation(
        &mut v,
        checked.tail_problems.is_empty() && checked.tail_lines > 0,
        || {
            format!(
                "хвост h1: {} строк, {:?}",
                checked.tail_lines, checked.tail_problems
            )
        },
    );
    violation(&mut v, checked.receipt, || {
        "нет квитанции ack 2 ping".to_owned()
    });
    violation(
        &mut v,
        closed["second_recovery_empty"] == json!(true) && closed["unchanged"] == json!(true),
        || {
            format!(
                "второе восстановление: {} , без изменений {}",
                closed["second_recovery"], closed["unchanged"]
            )
        },
    );
    for killed in &live.killed {
        if let Some(Point::Journal { state, renamed }) = parse_journal_point(&killed.barrier) {
            let on_disk = killed.journal;
            let ok = if renamed {
                on_disk == Some(state)
            } else {
                on_disk != Some(state)
            };
            violation(&mut v, ok, || {
                format!("на {} журнал на диске {on_disk:?}", killed.barrier)
            });
        }
    }
    flow_checks(live, owner, checked, &mut v);
    Verdict::of(
        v,
        format!(
            "{}: убито {}, запусков восстановления {}, владелец {} поколения {}, история {:?}, h1 прежняя ({} строк, ack 2), второе восстановление без шагов",
            case.name,
            live.killed
                .iter()
                .map(|killed| format!("{}@{}", killed.process.pid, killed.barrier))
                .collect::<Vec<_>>()
                .join(", "),
            live.runs,
            owner.key.pid,
            owner.generation,
            checked
                .history
                .iter()
                .map(|state| state.name())
                .collect::<Vec<_>>(),
            checked.tail_lines
        ),
    )
}

fn parse_journal_point(key: &str) -> Option<Point> {
    let rest = key.split_once('.')?.1.strip_prefix("journal-")?;
    let (name, side) = rest.rsplit_once('-')?;
    let state = State::ALL.into_iter().find(|state| state.name() == name)?;
    Some(Point::Journal {
        state,
        renamed: side == "after",
    })
}

fn step_facts<'a>(live: &'a Live, name: &str) -> Option<&'a Value> {
    live.steps
        .iter()
        .find(|step| step["name"] == json!(name))
        .map(|step| &step["facts"])
}

/// Оракулы хода части сверх общего.
fn flow_checks(live: &Live, owner: &OwnerInfo, checked: &Checked, v: &mut Vec<String>) {
    let events = live.helper_events();
    let flow = step_facts(live, "flow").cloned().unwrap_or(Value::Null);
    let parked: Option<ProcessKey> = serde_json::from_value(flow["parked"]["process"].clone()).ok();
    match live.case.flow {
        Flow::Kill => {}
        Flow::Opened => {
            violation(v, checked.started.len() == 1, || {
                format!("отчётов N+1: {:?}", checked.started)
            });
            violation(v, parked == Some(owner.key), || {
                format!("владелец {:?}, на started стоял {parked:?}", owner.key)
            });
        }
        Flow::Hung => {
            let refused = flow["second"].as_array().is_some_and(|reports| {
                reports.iter().any(|report| {
                    report["role"] == json!("refused") && report["retry"] == json!(true)
                })
            });
            violation(v, refused, || format!("второй запуск: {}", flow["second"]));
            violation(v, flow["untouched"] == json!(true), || {
                format!("при зависшем N+1 переставлено: {}", flow["before"])
            });
            violation(
                v,
                flow["waited_to_ms"].as_u64() >= flow["deadline_ms"].as_u64(),
                || "срок не истёк".to_owned(),
            );
        }
        Flow::WatchdogKill | Flow::WatchdogFirst => {
            let killed = events.iter().any(|event| {
                event["event"] == json!("watch-kill")
                    && serde_json::from_value::<ProcessKey>(event["detail"]["owner"].clone()).ok()
                        == parked
            });
            violation(v, killed, || "сторож не снял ключ владельца".to_owned());
            violation(v, flow["next_alive_after_helper"] == json!(false), || {
                "N+1 жив после сторожа".to_owned()
            });
            violation(v, !checked.history.contains(&State::Committed), || {
                "committed записан".to_owned()
            });
        }
        Flow::Quit => {
            violation(v, has_event(&events, "watch-owner-exited"), || {
                "помощник не увидел выход владельца".to_owned()
            });
            violation(v, !has_event(&events, "watch-kill"), || {
                "сторож посылал сигнал".to_owned()
            });
            let unconfirmed = live
                .next_reports()
                .iter()
                .any(|report| matches!(report, AppReport::Unconfirmed { .. }));
            violation(v, unconfirmed, || {
                format!("строки N+1: {:?}", live.next_reports())
            });
        }
        Flow::NotStarted => {
            violation(
                v,
                checked
                    .history
                    .windows(2)
                    .any(|pair| pair == [State::Swapped, State::RollbackIntent]),
                || format!("нет swapped → rollback-intent: {:?}", checked.history),
            );
        }
        Flow::CommitFirst => {
            let ok = matches!((flow["released_ms"].as_u64(), flow["deadline_ms"].as_u64()), (Some(r), Some(d)) if r < d);
            violation(v, ok, || format!("барьер снят не до срока: {flow}"));
            violation(v, !has_event(&events, "watch-kill"), || {
                "сторож посылал сигнал".to_owned()
            });
            violation(v, has_event(&events, "watch-finished"), || {
                "сторож не вышел по окончании попытки".to_owned()
            });
        }
        Flow::NextChecks => {
            let expired = live.next_reports().iter().any(|report| {
                matches!(report, AppReport::Unconfirmed { reason, .. } if reason.contains("срок health истёк"))
            });
            violation(v, expired, || {
                format!("строки N+1: {:?}", live.next_reports())
            });
            violation(v, !checked.history.contains(&State::Committed), || {
                "committed записан".to_owned()
            });
        }
        Flow::HandoffGui => {
            violation(v, has_event(&events, "foreign-state"), || {
                "помощник не увидел чужое состояние".to_owned()
            });
            violation(v, !has_event(&events, "exchanged"), || {
                "помощник менял бандлы".to_owned()
            });
        }
        Flow::HandoffHelper => {
            let forwarded = flow["second_reports"].as_array().is_some_and(|reports| {
                reports.iter().any(|report| {
                    report["role"] == json!("forwarded")
                        && serde_json::from_value::<ProcessKey>(report["owner"].clone()).ok()
                            == Some(owner.key)
                })
            });
            violation(v, forwarded, || {
                format!("второй запуск: {}", flow["second_reports"])
            });
        }
        Flow::TwoNext => {
            violation(v, parked == Some(owner.key), || {
                format!("владелец {:?}, ключ {parked:?}", owner.key)
            });
            let forwarded = flow["second_reports"].as_array().is_some_and(|reports| {
                reports
                    .iter()
                    .any(|report| report["role"] == json!("forwarded"))
            });
            violation(v, forwarded, || {
                format!("второй N+1: {}", flow["second_reports"])
            });
            violation(v, has_event(&events, "watch-finished"), || {
                "сторож не вышел по окончании попытки".to_owned()
            });
        }
        Flow::ShortCommitted => {
            let idle = flow["journal_at_release"]["state"] == json!("idle");
            violation(v, idle, || {
                format!("журнал при снятии: {}", flow["journal_at_release"])
            });
            violation(v, !has_event(&events, "watch-kill"), || {
                "сторож посылал сигнал".to_owned()
            });
            violation(v, has_event(&events, "watch-finished"), || {
                "сторож не вышел".to_owned()
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::barrier::B2;
    use crate::journal::TRANSITIONS;
    use std::collections::HashSet;

    #[test]
    fn every_b2_row_has_a_part_with_its_barriers() {
        for (index, row) in B2.iter().enumerate() {
            let parts: Vec<&Case> = CASES
                .iter()
                .filter(|case| case.origin == Origin::Row(index))
                .collect();
            assert!(
                !parts.is_empty(),
                "B2 «{} {}» без части",
                row.actor,
                row.boundary
            );
            let mut used: HashSet<String> = HashSet::new();
            for case in &parts {
                for barrier in case.armed.iter().chain(case.kills) {
                    used.insert(barrier.key());
                }
            }
            for barrier in row.barriers {
                assert!(
                    used.contains(&barrier.key()),
                    "B2 «{}»: барьер {} не поставлен ни одной частью",
                    row.boundary,
                    barrier.key()
                );
            }
        }
    }

    #[test]
    fn every_transition_except_deferral_is_covered() {
        let covered: HashSet<(State, State)> = CASES
            .iter()
            .flat_map(|case| case.transitions.iter().copied())
            .collect();
        for transition in TRANSITIONS {
            if matches!(transition.0, State::StagedDeferred)
                || matches!(transition.1, State::StagedDeferred)
            {
                continue;
            }
            assert!(covered.contains(&transition), "{transition:?} без части");
            assert!(crate::journal::allowed(transition.0, transition.1));
        }
        for case in CASES {
            for transition in case.transitions {
                assert!(
                    TRANSITIONS.contains(transition),
                    "{}: {transition:?} вне таблицы",
                    case.name
                );
            }
        }
    }

    #[test]
    fn every_journal_write_is_killed_before_and_after_rename() {
        let killed: HashSet<(State, bool)> = CASES
            .iter()
            .flat_map(|case| case.kills.iter())
            .filter_map(|barrier| match barrier.point {
                Point::Journal { state, renamed } => Some((state, renamed)),
                _ => None,
            })
            .collect();
        for state in State::ALL {
            if state == State::StagedDeferred {
                continue;
            }
            for renamed in [false, true] {
                assert!(
                    killed.contains(&(state, renamed)),
                    "запись {} {} без убийства",
                    state.name(),
                    if renamed {
                        "после rename"
                    } else {
                        "до rename"
                    }
                );
            }
        }
    }

    #[test]
    fn names_are_unique_and_kills_are_armed_or_chained() {
        let mut names = HashSet::new();
        for case in CASES {
            assert!(names.insert(case.name), "{} повторяется", case.name);
            if let Some(first) = case.kills.first() {
                assert!(
                    case.armed.contains(first),
                    "{}: первый барьер убийства не поставлен",
                    case.name
                );
            }
        }
    }
}
