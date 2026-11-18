//! Общий ход обновления для сценариев задачи 8 (`swap`, `identity-live`, `reject`,
//! `incompatible-activate`): установка поколения в `<state>/install/`, GUI N прямым запуском,
//! `update`, `activate`, выход GUI N, помощник, N+1 через `open -n` и уборка. Здесь только стенд;
//! оракулы клеток — в сценариях. Невыполненное предусловие стенда — ошибка исполнения.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Value, json};

use crate::barrier::{self, Barrier, Point};
use crate::generation::{self, BUNDLE_ID, Layout, Part};
use crate::host::SessionRecord;
use crate::journal::{HistoryEntry, Journal, State, Store};
use crate::launch;
use crate::observer::{self, ProcessKey};
use crate::owner::{self, AppOptions, AppReport, OwnerInfo, Reply, Request};
use crate::recover::{Checker, Codesign, Seen};
use crate::update;

use super::owner_basic::{Guard, ask, clear_state, reap, reports, start_app};
use super::reject::{INSTALL_DIR, install};

/// Срок одного шага стенда обновления: `update` и `activate` вызывают `codesign` и `ditto`, N+1
/// проверяет подписи, готовит рантайм и выполняет health.
pub const STEP: Duration = Duration::from_secs(30);

/// Где N+1 останавливается для проверки «N хранится до подтверждения»: перед `rename` записи
/// `committed`, health уже выполнен.
pub const PAUSE: Barrier = Barrier::new(
    barrier::Role::Next,
    Point::Journal {
        state: State::Committed,
        renamed: false,
    },
);

/// Стенд: установленный бандл и аргументы GUI N.
pub struct Stand {
    pub state: PathBuf,
    pub installed: PathBuf,
    pub app_bin: PathBuf,
    pub options: AppOptions,
    pub store: Store,
    pub checker: Codesign,
}

impl Stand {
    /// Копия бандла `source` из раскладки в `<state>/install/probe-update.app`.
    pub fn new(state: &Path, nonce: &str, layout: &Layout, source: &str) -> Result<Stand, String> {
        let installed = state.join(INSTALL_DIR).join(generation::APP_NAME);
        install(&layout.get(source)?.app, &installed)?;
        Ok(Stand {
            state: state.to_path_buf(),
            app_bin: Part::App.path(&installed),
            options: AppOptions {
                state: state.to_path_buf(),
                installed: installed.clone(),
                leaf: layout.leaf.clone(),
                identifier: BUNDLE_ID.to_owned(),
                tag: nonce.to_owned(),
                update_attempt: None,
                window: false,
                forward: Vec::new(),
            },
            installed,
            store: Store::new(state),
            checker: Codesign {
                requirement: generation::requirement(BUNDLE_ID, &layout.leaf),
            },
        })
    }

    /// GUI N прямым запуском из установленного бандла; ждёт готовности владельца.
    pub(super) fn start_owner(
        &self,
        guard: &mut Guard,
        out: &Path,
        name: &str,
    ) -> Result<(usize, OwnerInfo), String> {
        guard
            .apps
            .push(start_app(&self.app_bin, &self.options, out, name)?);
        let index = guard.apps.len() - 1;
        let info = owner::wait_for_owner(&self.state, Instant::now() + STEP)?;
        if info.key.pid != guard.apps[index].id() as i32 {
            return Err(format!(
                "стенд: владелец {} — не запущенный {}",
                info.key.pid,
                guard.apps[index].id()
            ));
        }
        Ok((index, info))
    }

    pub(super) fn create_session(
        &self,
        guard: &mut Guard,
        name: &str,
    ) -> Result<SessionRecord, String> {
        match ask(
            &self.state,
            &Request::CreateSession {
                name: name.to_owned(),
            },
        )? {
            Reply::Session(record) => {
                guard.sessions.push((*record).clone());
                Ok(*record)
            }
            other => Err(format!("стенд: create-session {name}: {other:?}")),
        }
    }

    pub fn check(&self, path: &Path) -> Result<Seen, String> {
        self.checker.check(path)
    }

    /// Каталоги попыток рядом с установленным бандлом.
    pub fn attempt_dirs(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .installed
            .parent()
            .and_then(|dir| fs::read_dir(dir).ok())
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .filter(|name| name.starts_with(".shellio-"))
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    /// Каталоги помощника в `<state>/update/helper/`.
    pub fn helper_dirs(&self) -> Vec<String> {
        let dir = self
            .state
            .join(crate::journal::UPDATE_DIR)
            .join(update::HELPER_DIR);
        let mut names: Vec<String> = fs::read_dir(dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }
}

/// `activate`: ответ `activating` с номером попытки и ключом помощника.
pub fn activate(state: &Path) -> Result<(String, ProcessKey), String> {
    match ask(state, &Request::Activate)? {
        Reply::Activating { attempt, helper } => Ok((attempt, helper)),
        other => Err(format!("activate: {other:?}")),
    }
}

/// Что увидел стенд на остановке N+1 перед `committed`.
#[derive(Clone, Debug, Serialize)]
pub struct Paused {
    pub reached: barrier::Reached,
    pub journal: Option<Journal>,
    pub installed: Seen,
    pub staged: Seen,
    pub gui_alive: bool,
}

/// Итог обновления от `activate` до готового N+1.
#[derive(Debug)]
pub struct Followed {
    pub attempt: String,
    pub helper: ProcessKey,
    pub gui: ProcessKey,
    pub gui_exit: ExitStatus,
    pub gui_reports: Vec<AppReport>,
    pub paused: Option<Paused>,
    pub next: OwnerInfo,
    pub next_reports: Vec<AppReport>,
    pub helper_events: Vec<Value>,
    pub helper_exited: bool,
    pub journal: Option<Journal>,
    pub history: Vec<HistoryEntry>,
    pub staged_after: Seen,
}

impl Followed {
    /// Состояния истории этой попытки по порядку.
    pub fn states(&self) -> Vec<State> {
        self.history
            .iter()
            .filter(|entry| entry.journal.attempt == self.attempt)
            .map(|entry| entry.journal.state)
            .collect()
    }

    /// Запись истории в состоянии `state`.
    pub fn entry(&self, state: State) -> Option<&HistoryEntry> {
        self.history
            .iter()
            .find(|entry| entry.journal.attempt == self.attempt && entry.journal.state == state)
    }

    /// Событие помощника `name`.
    pub fn event(&self, name: &str) -> Option<&Value> {
        self.helper_events
            .iter()
            .find(|event| event.get("event").and_then(Value::as_str) == Some(name))
    }

    pub fn event_ns(&self, name: &str) -> Option<u64> {
        self.event(name)
            .and_then(|event| event.get("uptime_ns"))
            .and_then(Value::as_u64)
    }

    pub fn facts(&self) -> Value {
        json!({
            "attempt": self.attempt,
            "helper": self.helper,
            "gui": self.gui,
            "gui_exit": self.gui_exit.code(),
            "gui_reports": self.gui_reports,
            "paused": self.paused,
            "next": self.next,
            "next_reports": self.next_reports,
            "helper_events": self.helper_events,
            "helper_exited": self.helper_exited,
            "journal": self.journal,
            "history": self.history,
            "staged_after": self.staged_after,
        })
    }
}

/// Строки JSON файла; нет файла — пусто.
fn json_lines(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .map(|text| {
            text.lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Копии файлов каталога помощника попытки в каталог прогона `out/helper-<attempt>/`.
pub fn keep_helper_files(state: &Path, attempt: &str, out: &Path) -> Result<PathBuf, String> {
    let source = update::helper_dir(state, attempt);
    let dest = out.join(format!("helper-{attempt}"));
    fs::create_dir_all(&dest).map_err(|err| format!("{}: {err}", dest.display()))?;
    for name in [
        update::HELPER_STDOUT,
        update::HELPER_STDERR,
        update::NEXT_STDOUT,
        update::NEXT_STDERR,
        update::RELAUNCH_STDOUT,
        update::RELAUNCH_STDERR,
    ] {
        let from = source.join(name);
        if from.exists() {
            fs::copy(&from, dest.join(name)).map_err(|err| format!("{}: {err}", from.display()))?;
        }
    }
    Ok(dest)
}

/// От ответа `activating` до готового N+1: выход GUI N, остановка N+1 перед `committed` (если
/// `pause`), готовность N+1, выход помощника, журнал и история.
pub(super) fn follow(
    stand: &Stand,
    guard: &mut Guard,
    gui_index: usize,
    (attempt, helper): (String, ProcessKey),
    pause: bool,
    out: &Path,
    gui_name: &str,
) -> Result<Followed, String> {
    let gui_child: &mut Child = &mut guard.apps[gui_index];
    let gui = observer::inspect(gui_child.id() as i32)?
        .map(|info| info.key)
        .ok_or("GUI N не найден")?;
    let gui_exit = reap(gui_child, "GUI N после activate")?;
    let gui_reports = reports(out, gui_name)?;
    let paused = if pause {
        let reached = barrier::wait_reached(&stand.state, PAUSE, Instant::now() + STEP)?;
        let journal = stand.store.read().map_err(|err| err.to_string())?;
        let staged = journal
            .as_ref()
            .map(Journal::staged_dir)
            .ok_or("на остановке журнала нет")?;
        let paused = Paused {
            reached,
            installed: stand.check(&stand.installed)?,
            staged: stand.check(&staged)?,
            journal,
            gui_alive: observer::identify(gui)?.is_alive(),
        };
        barrier::release(&stand.state, PAUSE)?;
        Some(paused)
    } else {
        None
    };
    let next = owner::wait_for_owner(&stand.state, Instant::now() + STEP)?;
    let helper_exited = launch::wait_exit(&[helper.pid], STEP).is_empty()
        || !observer::identify(helper)?.is_alive();
    let journal = stand.store.read().map_err(|err| err.to_string())?;
    let history = stand.store.history()?;
    let staged_after = match &journal {
        Some(journal) => stand.check(&journal.staged_dir())?,
        None => Seen::Absent,
    };
    let kept = keep_helper_files(&stand.state, &attempt, out)?;
    let helper_events = json_lines(&kept.join(update::HELPER_STDOUT));
    let next_reports = json_lines(&kept.join(update::NEXT_STDOUT))
        .into_iter()
        .filter(|line| line.get("event").is_none())
        .filter_map(|line| serde_json::from_value(line).ok())
        .collect();
    Ok(Followed {
        attempt,
        helper,
        gui,
        gui_exit,
        gui_reports,
        paused,
        next,
        next_reports,
        helper_events,
        helper_exited,
        journal,
        history,
        staged_after,
    })
}

/// Уборка после обновления: `quit` владельца (он не потомок сценария), остановка сессий,
/// каталог состояния. Оставшиеся процессы — ошибка стенда.
pub(super) fn finish(stand: &Stand, guard: &mut Guard, owner: ProcessKey) -> Result<Value, String> {
    let quit = ask(&stand.state, &Request::Quit)?;
    let left = launch::wait_exit(&[owner.pid], STEP);
    if quit != Reply::Done || (!left.is_empty() && observer::identify(owner)?.is_alive()) {
        return Err(format!("стенд: quit владельца {}: {quit:?}", owner.pid));
    }
    for record in guard.sessions.clone() {
        if observer::identify(record.host)?.is_alive() {
            stop(&record)?;
        }
    }
    guard
        .sessions
        .retain(|record| observer::identify(record.host).is_ok_and(|found| found.is_alive()));
    if !guard.sessions.is_empty() {
        return Err(format!("стенд: остались сессии {:?}", guard.sessions));
    }
    for app in &mut guard.apps {
        if app.try_wait().map_err(|err| err.to_string())?.is_none() {
            return Err(format!("стенд: GUI {} не вышел", app.id()));
        }
    }
    guard.apps.clear();
    clear_state(&stand.state);
    guard.armed = false;
    Ok(json!({ "quit": format!("{quit:?}"), "owner": owner }))
}

/// Остановка сессии через управляющее соединение и ожидание выхода её процессов. Стенд говорит
/// диапазоном протокола хоста из записи сессии: хост поколения 3 (2..2) с диапазоном attach
/// сборки раннера (1..1) не согласуется.
pub fn stop(record: &SessionRecord) -> Result<(), String> {
    let hello = crate::attach::hello(
        crate::proto::Purpose::Control,
        record.versions,
        0,
        owner::SESSION_SIZE,
    );
    match crate::attach::connect(Path::new(&record.socket), &hello, STEP)? {
        crate::attach::Connected::Welcome(mut client) => client.stop()?,
        crate::attach::Connected::Refused(reason) => {
            return Err(format!("control {}: {reason}", record.session));
        }
    }
    let left = launch::wait_exit(&[record.host.pid, record.workload.pid], STEP);
    if left.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "сессия {} не остановилась: {left:?}",
            record.session
        ))
    }
}

/// Хост и нагрузка сессии живы с прежними ключами.
pub fn same_processes(record: &SessionRecord) -> Result<bool, String> {
    Ok(observer::identify(record.host)?.is_alive()
        && observer::identify(record.workload)?.is_alive())
}

pub fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}
