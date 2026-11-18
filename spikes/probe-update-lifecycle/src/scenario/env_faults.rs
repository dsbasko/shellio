//! B3 «Ошибки среды» (задача 13; план P0.5, «B3. Ошибки среды»): сценарий `env-faults`.
//!
//! Каждая часть — свежий том APFS из образа ([`VOLUME_MIB`] МиБ, `hdiutil create -fs APFS`),
//! подключённый в `<state>/v<k>`. На томе лежат каталог состояния роли GUI `<том>/s`: установка
//! `s/install/probe-update.app`, журнал `s/update/journal.json`, `s/runtime/`, блокировка, сокеты
//! и записи сессий. Средства стенда — барьеры и отметки `s/barriers`, каталог помощника
//! `s/update/helper` (его копия, stdout и stderr помощника и N+1) и история журнала — ссылки на
//! каталог `<state>/f<k>` вне тома: на полном томе процесс иначе не сообщил бы, на чём отказал.
//!
//! Части:
//! - настоящий ENOSPC: том заполняется физической записью псевдослучайных данных до отказа, «полон»
//!   — когда не создаётся файл в 1500 байт ([`volume::probe`]); отказы подготовки кандидата, записи
//!   журнала (`staged → intent`) и подготовки `runtime/<N+1>/`;
//! - настоящий EXDEV: после `staged` кандидат заменяется томом только для чтения с тем же
//!   подписанным бандлом (тот же CDHash) в точке `.shellio-update-<attempt>`; обмен помощника —
//!   EXDEV;
//! - настоящий EACCES: каталог установки без права записи до постановки и перед обменом;
//! - инъекции ([`INJECTIONS`]) с пометкой `injected`: `rename(2)` временного файла журнала после
//!   настоящего обмена и обратный обмен отката — границы, куда настоящий отказ не достаётся.
//!
//! Общий оракул части: пока отказ действует, установка соответствует последнему записанному
//! состоянию и сессия `h1` жива с хвостом без пропуска и квитанцией; когда он снят, отдельный запуск
//! восстановления приводит к итогу своей строки таблицы; второе восстановление ничего не меняет.
//! Невыполненный оракул — `fail`; ошибка исполнения — только сломанный стенд.

use std::fs::{self, Permissions};
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{Value, json};

use crate::attach::{self, Attached, Connected};
use crate::barrier::{self, Barrier, Mutation, Point, Role};
use crate::compat::Staging;
use crate::generation::{Layout, Run};
use crate::host::SessionRecord;
use crate::journal::{self, Journal, Outcome, State};
use crate::observer::{self, ProcessKey};
use crate::owner::{self, Answer, AppReport, Lock, OwnerInfo, Reply, Request};
use crate::proto::{ATTACH_VERSIONS, Purpose};
use crate::pty::Winsize;
use crate::recover::{self, Seen};
use crate::results::{Cell, Diagnostics, Environment, Report, Status};
use crate::volume::{self, Fault, IMAGE_PREFIX, Volume};
use crate::{launch, sys, update};

use super::owner_basic::{Guard, ask, clear_state, ping, reap, reports, start_app};
use super::update_flow::{self, Stand};
use super::{PartSpec, Verdict};

pub const DEADLINE: Duration = Duration::from_secs(600);
pub const DIR: &str = "env-faults";
pub const STEPS_ARTIFACT: &str = "env-faults/steps.json";
pub const CELL: &str = "B3";
/// Том установки, журнала и рантаймов.
pub const VOLUME_MIB: u64 = 64;
/// Том кандидата части EXDEV.
pub const SIDE_MIB: u64 = 32;
/// Свободное место, оставленное части «подготовка кандидата»: меньше бандла (≈4,7 МБ).
pub const MARGIN_BYTES: u64 = 2 << 20;
/// errno инъекций.
pub const INJECTED_ERRNO: i32 = libc::EIO;
const STEP: Duration = update_flow::STEP;
const RECOVERY_RUNS: usize = 3;
/// Сколько раз дозаполнять том, если файл в 1500 байт ещё создаётся.
const FILL_ROUNDS: usize = 3;
const COMPLETION: &str = "ответ владельца, барьер, выход процесса или готовность владельца в срок шага (30 с); иначе ошибка исполнения";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    EnospcCandidate,
    EnospcJournal,
    EnospcRuntime,
    ExdevSwap,
    EaccesStage,
    EaccesSwap,
    InjectedJournalRename,
    InjectedReverseSwap,
}

#[derive(Clone, Copy, Debug)]
pub struct Case {
    pub kind: Kind,
    pub spec: PartSpec,
}

pub const CASES: [Case; 8] = [
    Case {
        kind: Kind::EnospcCandidate,
        spec: PartSpec {
            name: "enospc-candidate",
            stimulus: "том заполнен физической записью до ENOSPC, затем освобождено 2 МиБ; `update` поколением 2 у живого GUI N; после освобождения тома — `quit` и отдельный запуск восстановления",
            oracle: "`update` — окончательный отказ с ENOSPC от `ditto`; журнала нет, `.shellio-update-*` нет, установлен N, GUI N отвечает, `h1` прежняя с хвостом и квитанцией; после освобождения — восстановление без шагов, владелец N, `h1` прежняя, второе восстановление ничего не меняет",
        },
    },
    Case {
        kind: Kind::EnospcJournal,
        spec: PartSpec {
            name: "enospc-journal",
            stimulus: "`update` поколением 2 (`staged`); том заполнен до ENOSPC; `activate`; `quit` GUI N и запуск восстановления на полном томе; освобождение, запуск восстановления",
            oracle: "`activate` — отказ с ENOSPC записи временного файла журнала; журнал байт в байт прежний (`staged`), временного файла нет, помощник не запущен, GUI N отвечает, `h1` прежняя; восстановление на полном томе выходит с ошибкой ENOSPC, ничего не переставив; после освобождения — `aborting`, удаление кандидата, `idle` «прервана», владелец N, второе восстановление ничего не меняет",
        },
    },
    Case {
        kind: Kind::EnospcRuntime,
        spec: PartSpec {
            name: "enospc-runtime",
            stimulus: "`update`, `activate`; N+1 на барьере `next.runtime-partial` (хост уже в `runtime/.prep-2`); том заполнен до ENOSPC, барьер снят; запуск восстановления на полном томе; освобождение, запуск восстановления",
            oracle: "N+1 выходит с ошибкой ENOSPC копии в `runtime/.prep-2`, сокета не публикует; помощник видит выход владельца, запись `rollback-intent` отказывает ENOSPC, он выходит с ошибкой, ничего не переставив: журнал `launched`, установлен N+1, кандидат — N, владельца нет, `h1` прежняя; восстановление на полном томе выходит с ошибкой ENOSPC; после освобождения — откат до `idle` «откачена», владелец N, N+1 в `.shellio-failed-<attempt>`, второе восстановление ничего не меняет",
        },
    },
    Case {
        kind: Kind::ExdevSwap,
        spec: PartSpec {
            name: "exdev-swap",
            stimulus: "после `staged` кандидат заменён томом только для чтения с тем же подписанным бандлом (тот же CDHash) в точке `.shellio-update-<attempt>`; `activate`; на барьере `helper.journal-aborting-after` том кандидата отключён, барьер снят",
            oracle: "`renamex_np(RENAME_SWAP)` помощника — EXDEV, журнал `aborting` с этой причиной, установлен N; после снятия барьера помощник удаляет кандидата, пишет `idle` «прервана» и запускает N через `open -n`; владелец N, `h1` прежняя, второе восстановление ничего не меняет",
        },
    },
    Case {
        kind: Kind::EaccesStage,
        spec: PartSpec {
            name: "eacces-stage",
            stimulus: "каталог установки `chmod 0555`; `update` поколением 2 у живого GUI N; права возвращены, `quit` и запуск восстановления",
            oracle: "`update` — окончательный отказ с EACCES от `ditto`; журнала нет, кандидата нет, установлен N, GUI N отвечает, `h1` прежняя; восстановление без шагов, владелец N, второе восстановление ничего не меняет",
        },
    },
    Case {
        kind: Kind::EaccesSwap,
        spec: PartSpec {
            name: "eacces-swap",
            stimulus: "`update` (`staged`); каталог установки `chmod 0555`; `activate`; запуск восстановления без права записи; права возвращены, запуск восстановления",
            oracle: "`renamex_np(RENAME_SWAP)` помощника — EACCES, журнал `aborting`; удаление кандидата отказывает EACCES на каталоге установки, помощник выходит с ошибкой, N не запускает; установлен N, владельца нет, `h1` прежняя; восстановление без права записи выходит с ошибкой EACCES, журнал прежний; после возврата прав — удаление, `idle` «прервана», владелец N, второе восстановление ничего не меняет",
        },
    },
    Case {
        kind: Kind::InjectedJournalRename,
        spec: PartSpec {
            name: "injected-journal-rename",
            stimulus: "injected: EIO вместо `rename(2)` временного файла журнала у помощника на записи `swapped` (после настоящего `renamex_np`); `update`, `activate`; запуск восстановления",
            oracle: "инъекция сработала у помощника; он выходит с ошибкой EIO без `open`, журнал `intent`, временного файла нет, установлен N+1, кандидат — N, владельца нет, `h1` прежняя; восстановление по строке «`intent`, N+1 / N» пишет `swapped` и запускает N+1, тот подтверждается: `idle` «подтверждена», установлен N+1, второе восстановление ничего не меняет",
        },
    },
    Case {
        kind: Kind::InjectedReverseSwap,
        spec: PartSpec {
            name: "injected-reverse-swap",
            stimulus: "injected: EIO вместо обратного `renamex_np(RENAME_SWAP)` у помощника; `update`, `activate`; N+1 снят `SIGKILL` на `next.before-publish`; запуск восстановления",
            oracle: "инъекция сработала у помощника; он записал `rollback-intent` и выходит с ошибкой EIO: журнал `rollback-intent`, установлен N+1, кандидат — N, владельца нет, `h1` прежняя; восстановление делает обратный обмен, `rolled-back`, `retired`, `idle` «откачена», владелец N, N+1 в `.shellio-failed-<attempt>`, второе восстановление ничего не меняет",
        },
    },
];

/// Инъекции: часть и граница, на которой процесс получает [`INJECTED_ERRNO`].
pub const INJECTIONS: [(Kind, Barrier); 2] = [
    (
        Kind::InjectedJournalRename,
        Barrier::new(
            Role::Helper,
            Point::Journal {
                state: State::Swapped,
                renamed: false,
            },
        ),
    ),
    (
        Kind::InjectedReverseSwap,
        Barrier::new(
            Role::Helper,
            Point::Step {
                mutation: Mutation::Swap,
                done: false,
            },
        ),
    ),
];

fn injection(kind: Kind) -> Result<Barrier, String> {
    INJECTIONS
        .iter()
        .find(|(of, _)| *of == kind)
        .map(|(_, barrier)| *barrier)
        .ok_or_else(|| format!("стенд: у {kind:?} нет инъекции"))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

fn alive(key: ProcessKey) -> bool {
    observer::identify(key).is_ok_and(|found| found.is_alive())
}

fn io_err(path: &Path, err: std::io::Error) -> String {
    format!("{}: {err}", path.display())
}

fn names(dir: &Path) -> Vec<String> {
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

// --- стенд части ---

/// Образ тома; файл удаляется при выходе из области.
struct Image {
    path: PathBuf,
    created: Run,
}

impl Image {
    fn create(path: PathBuf, size_mib: u64, volname: &str) -> Result<Image, String> {
        let created = volume::create_image(&path, size_mib, volname)?;
        Ok(Image { path, created })
    }
}

impl Drop for Image {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Том кандидата части EXDEV: отключается раньше, чем удаляется его образ.
struct SideVolume {
    volume: Volume,
    image: Image,
}

/// Права каталога, возвращаемые при выходе из области.
#[derive(Default)]
struct Mode {
    path: Option<PathBuf>,
    original: u32,
}

impl Mode {
    fn set(&mut self, path: &Path, mode: u32) -> Result<Value, String> {
        let meta = fs::metadata(path).map_err(|err| io_err(path, err))?;
        self.original = meta.permissions().mode() & 0o7777;
        fs::set_permissions(path, Permissions::from_mode(mode)).map_err(|err| io_err(path, err))?;
        self.path = Some(path.to_path_buf());
        Ok(json!({
            "path": path,
            "from": format!("{:o}", self.original),
            "to": format!("{mode:o}"),
        }))
    }

    fn restore(&mut self) -> Result<(), String> {
        if let Some(path) = self.path.take() {
            fs::set_permissions(&path, Permissions::from_mode(self.original))
                .map_err(|err| io_err(&path, err))?;
        }
        Ok(())
    }
}

impl Drop for Mode {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

/// Сессия `h1` при проверке: прежние процессы, хвост без пропуска, квитанция ввода.
#[derive(Clone, Debug, Serialize)]
struct SessionSeen {
    same: bool,
    problems: Vec<String>,
    lines: usize,
    receipt: bool,
    seq: u64,
}

impl SessionSeen {
    fn ok(&self) -> bool {
        self.same && self.problems.is_empty() && self.receipt
    }
}

/// Раскладка на диске.
#[derive(Clone, Debug, Serialize)]
struct Snap {
    journal: Option<Journal>,
    journal_text: Option<String>,
    installed: Seen,
    staged: Option<Seen>,
    failed: Option<Seen>,
    dirs: Vec<String>,
    tmp: Vec<String>,
    runtime: Vec<String>,
    prep_next: Vec<String>,
    available: Option<u64>,
}

impl Snap {
    fn idle(&self, attempt: &str, outcome: Outcome) -> bool {
        self.journal.as_ref().is_some_and(|journal| {
            journal.attempt == attempt
                && journal.state == State::Idle
                && journal.outcome == Some(outcome)
        })
    }

    fn in_state(&self, attempt: &str, state: State) -> bool {
        self.journal
            .as_ref()
            .is_some_and(|journal| journal.attempt == attempt && journal.state == state)
    }
}

fn valid(seen: Option<&Seen>, cdhash: &str) -> bool {
    matches!(seen, Some(Seen::Valid { cdhash: found }) if found == cdhash)
}

/// Запуск, который должен выйти с ошибкой.
#[derive(Clone, Debug, Serialize)]
struct Blocked {
    name: String,
    pid: i32,
    code: Option<i32>,
    reports: Vec<AppReport>,
}

impl Blocked {
    fn error(&self) -> Option<&str> {
        self.reports.iter().find_map(|report| match report {
            AppReport::Error { reason, .. } => Some(reason.as_str()),
            _ => None,
        })
    }

    fn failed_with(&self, code: i32) -> bool {
        self.code == Some(i32::from(owner::EXIT_ERROR))
            && self
                .error()
                .is_some_and(|reason| Fault::of("", reason).is(code))
    }
}

#[derive(Clone, Debug, Serialize)]
struct Closed {
    quit: String,
    second_recovery: String,
    second_empty: bool,
    unchanged: bool,
}

impl Closed {
    fn ok(&self) -> bool {
        self.second_empty && self.unchanged
    }
}

struct Bench {
    out: PathBuf,
    root: PathBuf,
    stand: Stand,
    gen1: String,
    gen2: String,
    h1: Option<SessionRecord>,
    gui: Option<ProcessKey>,
    gui_index: usize,
    attempt: Option<String>,
    helper: Option<ProcessKey>,
    seq: u64,
    runs: usize,
    fillers: Vec<PathBuf>,
    faults: Vec<Fault>,
    steps: Vec<Value>,
}

impl Bench {
    fn state(&self) -> PathBuf {
        self.stand.state.clone()
    }

    fn step(&mut self, name: &str, facts: Value) {
        self.steps.push(json!({ "name": name, "facts": facts }));
    }

    fn fault(&mut self, source: &str, observed: &str) -> Fault {
        let fault = Fault::of(source, observed);
        self.faults.push(fault.clone());
        fault
    }

    fn journal(&self) -> Result<Option<Journal>, String> {
        self.stand.store.read().map_err(|err| err.to_string())
    }

    fn history(&self) -> Result<Vec<State>, String> {
        Ok(self
            .stand
            .store
            .history()?
            .into_iter()
            .filter(|entry| Some(&entry.journal.attempt) == self.attempt.as_ref())
            .map(|entry| entry.journal.state)
            .collect())
    }

    fn snapshot(&self) -> Result<Snap, String> {
        let journal = self.journal()?;
        let state = self.state();
        let update_dir = state.join(journal::UPDATE_DIR);
        let tmp_prefix = format!(".{}.tmp-", journal::JOURNAL_FILE);
        let runtime = crate::runtime::root(&state);
        Ok(Snap {
            journal_text: fs::read_to_string(self.stand.store.path()).ok(),
            installed: self.stand.check(&self.stand.installed)?,
            staged: match &journal {
                Some(journal) => Some(self.stand.check(&journal.staged_dir())?),
                None => None,
            },
            failed: match &journal {
                Some(journal) => Some(self.stand.check(&journal.failed_dir())?),
                None => None,
            },
            journal,
            dirs: self.stand.attempt_dirs(),
            tmp: names(&update_dir)
                .into_iter()
                .filter(|name| name.starts_with(&tmp_prefix))
                .collect(),
            runtime: names(&runtime),
            prep_next: names(&crate::runtime::prep_dir(&state, 2)),
            available: sys::volume_space(&self.root).ok().map(|(free, _)| free),
        })
    }

    fn session(&mut self) -> Result<SessionSeen, String> {
        let h1 = self.h1.clone().ok_or("стенд: нет h1")?;
        let same = update_flow::same_processes(&h1)?;
        let seq = self.seq;
        self.seq += 1;
        let hello = attach::hello(Purpose::Attach, ATTACH_VERSIONS, 0, Winsize::cells(0, 0));
        let (problems, lines, receipt) = match attach::connect(Path::new(&h1.socket), &hello, STEP)
        {
            Ok(Connected::Welcome(client)) => {
                let mut tail = Attached::new(client, Some(1));
                let receipt = tail.exchange(seq, "ping", Instant::now() + STEP).is_ok();
                let _ = tail.detach(Instant::now() + STEP);
                (
                    tail.transcript.problems.clone(),
                    tail.transcript.lines.len(),
                    receipt,
                )
            }
            Ok(Connected::Refused(reason)) => (vec![format!("attach h1: {reason}")], 0, false),
            Err(reason) => (vec![reason], 0, false),
        };
        Ok(SessionSeen {
            same,
            problems,
            lines,
            receipt,
            seq,
        })
    }

    /// Заполнение тома до ENOSPC с запасом `margin` байт, освобождённым после заполнения.
    fn fill(&mut self, margin: u64) -> Result<Value, String> {
        let margin_file = self.root.join("fill-margin");
        let reserve = if margin > 0 {
            let reserve = volume::fill(&margin_file, Some(margin))?;
            if reserve.bytes != margin {
                return Err(format!("стенд: запас не записан: {reserve:?}"));
            }
            Some(reserve)
        } else {
            None
        };
        let mut rounds = Vec::new();
        for round in 0..FILL_ROUNDS {
            let path = self.root.join(format!("fill-{}", self.fillers.len()));
            let filled = volume::fill(&path, None)?;
            self.fillers.push(path);
            let reached = filled.errno == Some(libc::ENOSPC);
            let probe = volume::probe(&self.root);
            let full = probe.file_errno == Some(libc::ENOSPC);
            rounds.push(json!({ "fill": filled, "probe": probe }));
            if !reached {
                return Err(format!("стенд: запись не дошла до ENOSPC: {rounds:?}"));
            }
            if full {
                break;
            }
            if round + 1 == FILL_ROUNDS {
                return Err(format!(
                    "стенд: после {FILL_ROUNDS} заполнений файл ещё создаётся: {rounds:?}"
                ));
            }
        }
        if margin > 0 {
            fs::remove_file(&margin_file).map_err(|err| io_err(&margin_file, err))?;
        }
        let facts = json!({
            "margin": margin,
            "reserve": reserve,
            "rounds": rounds,
            "probe_after": volume::probe(&self.root),
        });
        self.step("fill", facts.clone());
        Ok(facts)
    }

    fn free(&mut self) -> Result<Value, String> {
        for path in std::mem::take(&mut self.fillers) {
            fs::remove_file(&path).map_err(|err| io_err(&path, err))?;
        }
        let probe = volume::probe(&self.root);
        if !probe.writable() {
            return Err(format!(
                "стенд: том не пишется после освобождения: {probe:?}"
            ));
        }
        let facts = json!({ "probe": probe });
        self.step("free", facts.clone());
        Ok(facts)
    }

    fn start_run(&mut self, guard: &mut Guard, name: &str) -> Result<i32, String> {
        let child = start_app(&self.stand.app_bin, &self.stand.options, &self.out, name)?;
        let pid = child.id() as i32;
        guard.apps.push(child);
        Ok(pid)
    }

    /// Запуск восстановления, пока отказ действует: он должен выйти сам.
    fn blocked_run(&mut self, guard: &mut Guard, name: &str) -> Result<Blocked, String> {
        self.runs += 1;
        let pid = self.start_run(guard, name)?;
        if !launch::wait_exit(&[pid], STEP).is_empty() {
            return Err(format!("стенд: {name} ({pid}) не вышел за шаг"));
        }
        let child = guard
            .apps
            .iter_mut()
            .find(|child| child.id() as i32 == pid)
            .ok_or("стенд: запуск потерян")?;
        let code = child.wait().map_err(|err| err.to_string())?.code();
        let blocked = Blocked {
            name: name.to_owned(),
            pid,
            code,
            reports: reports(&self.out, name)?,
        };
        self.step("blocked-run", json!(blocked));
        Ok(blocked)
    }

    /// Запуски восстановления до готового владельца любого происхождения.
    fn recover_until_owner(&mut self, guard: &mut Guard) -> Result<OwnerInfo, String> {
        let mut tried = Vec::new();
        for _ in 0..RECOVERY_RUNS {
            self.runs += 1;
            let name = format!("recovery-{}", self.runs);
            let pid = self.start_run(guard, &name)?;
            match owner::wait_for_owner(&self.state(), Instant::now() + STEP) {
                Ok(info) => {
                    let first = reports(&self.out, &name).unwrap_or_default();
                    self.step(
                        "recovery",
                        json!({ "run": name, "pid": pid, "owner": info, "reports": first, "tried": tried }),
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

    fn wait_gone(&self, key: ProcessKey) -> bool {
        launch::wait_exit(&[key.pid], STEP).is_empty() || !alive(key)
    }

    fn helper_events(&self) -> Vec<Value> {
        let Some(attempt) = &self.attempt else {
            return Vec::new();
        };
        json_lines(&update::helper_dir(&self.state(), attempt).join(update::HELPER_STDOUT))
    }

    fn next_reports(&self) -> Vec<AppReport> {
        let Some(attempt) = &self.attempt else {
            return Vec::new();
        };
        json_lines(&update::helper_dir(&self.state(), attempt).join(update::NEXT_STDOUT))
            .into_iter()
            .filter(|line| line.get("event").is_none())
            .filter_map(|line| serde_json::from_value(line).ok())
            .collect()
    }

    /// Причина временного отказа клиенту, если владельца нет; `None` — владелец ответил.
    fn owner_absent(&self) -> Result<Option<String>, String> {
        match owner::request(&self.state(), &Request::Ping { wait_ms: 0 }, STEP)? {
            Answer::Unavailable(why) => Ok(Some(why.reason().to_owned())),
            Answer::Reply(_) => Ok(None),
        }
    }

    fn gen_of(&self, seen: Option<&Seen>) -> String {
        match seen {
            Some(Seen::Valid { cdhash }) if *cdhash == self.gen1 => "N".to_owned(),
            Some(Seen::Valid { cdhash }) if *cdhash == self.gen2 => "N+1".to_owned(),
            Some(Seen::Valid { cdhash }) => format!("чужой {cdhash}"),
            Some(Seen::Invalid { .. }) => "частичный".to_owned(),
            Some(Seen::Absent) | None => "нет".to_owned(),
        }
    }

    fn describe(&self, snap: &Snap) -> String {
        let journal = match &snap.journal {
            None => "журнала нет".to_owned(),
            Some(journal) => match journal.outcome {
                Some(outcome) => format!("журнал idle {outcome:?}"),
                None => format!("журнал {}", journal.state.name()),
            },
        };
        format!(
            "{journal}, установлен {}, кандидат {}, каталоги попытки {:?}",
            self.gen_of(Some(&snap.installed)),
            self.gen_of(snap.staged.as_ref()),
            snap.dirs
        )
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

fn event<'e>(events: &'e [Value], name: &str) -> Option<&'e Value> {
    events
        .iter()
        .find(|event| event.get("event").and_then(Value::as_str) == Some(name))
}

fn event_text(event: Option<&Value>, field: &str) -> String {
    event
        .and_then(|event| event.get("detail"))
        .and_then(|detail| detail.get(field))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

fn check(v: &mut Vec<String>, ok: bool, what: impl FnOnce() -> String) {
    if !ok {
        v.push(what());
    }
}

/// Каталог состояния роли GUI на томе и ссылки средств стенда на каталог вне тома.
fn prepare_state(vstate: &Path, side: &Path) -> Result<Value, String> {
    let barriers = side.join(barrier::BARRIERS_DIR);
    let helper = side.join(update::HELPER_DIR);
    let history = side.join(journal::HISTORY_FILE);
    for dir in [barrier::notes_dir(side), helper.clone()] {
        fs::create_dir_all(&dir).map_err(|err| io_err(&dir, err))?;
    }
    let update_dir = vstate.join(journal::UPDATE_DIR);
    fs::create_dir_all(&update_dir).map_err(|err| io_err(&update_dir, err))?;
    let links = [
        (barriers, barrier::dir(vstate)),
        (helper, update_dir.join(update::HELPER_DIR)),
        (history, vstate.join(journal::HISTORY_FILE)),
    ];
    for (target, link) in &links {
        symlink(target, link).map_err(|err| io_err(link, err))?;
    }
    Ok(json!(
        links
            .iter()
            .map(|(target, link)| json!({ "link": link, "target": target }))
            .collect::<Vec<_>>()
    ))
}

fn begin(b: &mut Bench, guard: &mut Guard) -> Result<(), String> {
    let (index, info) = b.stand.start_owner(guard, &b.out, "gui-n")?;
    b.gui = Some(info.key);
    b.gui_index = index;
    let h1 = b.stand.create_session(guard, "h1")?;
    let hello = attach::hello(Purpose::Attach, ATTACH_VERSIONS, 0, Winsize::cells(0, 0));
    let mut first = match attach::connect(Path::new(&h1.socket), &hello, STEP)? {
        Connected::Welcome(client) => Attached::new(client, Some(1)),
        Connected::Refused(reason) => return Err(format!("стенд: attach h1: {reason}")),
    };
    let deadline = Instant::now() + STEP;
    first.pump_until(deadline, |transcript| !transcript.lines.is_empty())?;
    first.exchange(1, "ping", deadline)?;
    first.detach(Instant::now() + STEP)?;
    b.seq = 2;
    b.step("begin", json!({ "owner": info, "h1": h1 }));
    b.h1 = Some(h1);
    Ok(())
}

fn stage(b: &mut Bench, layout: &Layout) -> Result<String, String> {
    let candidate = layout.get("gen-2")?.app.display().to_string();
    match ask(&b.state(), &Request::Update { candidate })? {
        Reply::Staging(staging) => match *staging {
            Staging::Staged { attempt, .. } => {
                b.attempt = Some(attempt.clone());
                b.step("staged", json!({ "attempt": attempt }));
                Ok(attempt)
            }
            other => Err(format!("стенд: update: {other:?}")),
        },
        other => Err(format!("стенд: update: {other:?}")),
    }
}

fn activate(b: &mut Bench, guard: &mut Guard) -> Result<ProcessKey, String> {
    let (attempt, helper) = update_flow::activate(&b.state())?;
    if b.attempt.as_deref() != Some(attempt.as_str()) {
        return Err(format!("стенд: activate другой попытки {attempt}"));
    }
    b.helper = Some(helper);
    let exit = reap(&mut guard.apps[b.gui_index], "GUI N после activate")?;
    b.step(
        "activated",
        json!({ "attempt": attempt, "helper": helper, "gui_exit": exit.code() }),
    );
    Ok(helper)
}

fn quit_gui(b: &mut Bench, guard: &mut Guard) -> Result<(), String> {
    let quit = ask(&b.state(), &Request::Quit)?;
    let exit = reap(&mut guard.apps[b.gui_index], "GUI N после quit")?;
    b.step(
        "quit-gui",
        json!({ "quit": format!("{quit:?}"), "exit": exit.code() }),
    );
    Ok(())
}

fn release(state: &Path, barrier: Barrier) -> Result<(), String> {
    barrier::release(state, barrier)?;
    let _ = fs::remove_file(barrier.reached_path(state));
    Ok(())
}

/// `quit` владельца, второе восстановление стендом под блокировкой, остановка `h1`, уборка.
fn close(b: &mut Bench, guard: &mut Guard, owner: &OwnerInfo) -> Result<Closed, String> {
    let state = b.state();
    let quit = ask(&state, &Request::Quit)?;
    if !launch::wait_exit(&[owner.key.pid], STEP).is_empty() && alive(owner.key) {
        return Err(format!(
            "стенд: владелец {} не вышел по quit",
            owner.key.pid
        ));
    }
    if let Some(helper) = b.helper
        && !b.wait_gone(helper)
    {
        return Err(format!("стенд: помощник {} не вышел", helper.pid));
    }
    for app in &mut guard.apps {
        let pid = app.id() as i32;
        if !launch::wait_exit(&[pid], STEP).is_empty() {
            return Err(format!("стенд: процесс {pid} не вышел"));
        }
        let _ = app.wait();
    }
    guard.apps.clear();
    let layout = |b: &Bench| -> Result<_, String> {
        Ok((
            fs::read(b.stand.store.path()).ok(),
            b.stand.store.history()?.len(),
            b.stand.attempt_dirs(),
            b.stand.check(&b.stand.installed)?,
        ))
    };
    let before = layout(b)?;
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
    let second =
        recover::recover(&b.stand.store, &b.stand.checker, &ctx).map_err(|err| err.to_string());
    drop(lock);
    let after = layout(b)?;
    let h1 = b.h1.clone().ok_or("стенд: нет h1")?;
    update_flow::stop(&h1)?;
    guard.sessions.clear();
    let left = launch::find_by_token(&guard.nonce.clone().unwrap_or_default());
    if !left.is_empty() {
        return Err(format!("стенд: остались процессы стенда {left:?}"));
    }
    clear_state(&state);
    guard.armed = false;
    let closed = Closed {
        quit: format!("{quit:?}"),
        second_recovery: match &second {
            Ok(report) => format!("{:?}", report.steps),
            Err(reason) => reason.clone(),
        },
        second_empty: second.as_ref().is_ok_and(|report| report.steps.is_empty()),
        unchanged: before == after,
    };
    b.step("closed", json!(closed));
    Ok(closed)
}

fn common_after(
    b: &Bench,
    v: &mut Vec<String>,
    owner: &OwnerInfo,
    generation: u32,
    session: &SessionSeen,
    closed: &Closed,
) {
    check(v, owner.generation == generation, || {
        format!(
            "после восстановления владелец поколения {}, ждали {generation}",
            owner.generation
        )
    });
    check(v, session.ok(), || {
        format!("после восстановления h1: {session:?}")
    });
    check(v, closed.ok(), || {
        format!("второе восстановление что-то изменило: {closed:?}")
    });
    let _ = b;
}

// --- части ---

fn enospc_candidate(b: &mut Bench, guard: &mut Guard, layout: &Layout) -> Result<Verdict, String> {
    let fill = b.fill(MARGIN_BYTES)?;
    let candidate = layout.get("gen-2")?.app.display().to_string();
    let reply = ask(&b.state(), &Request::Update { candidate })?;
    b.step("update", json!({ "reply": format!("{reply:?}") }));
    let during = b.snapshot()?;
    let pong = ping(&b.state());
    let s_during = b.session()?;
    b.step(
        "during",
        json!({ "snapshot": during, "ping": format!("{pong:?}"), "h1": s_during }),
    );
    b.free()?;
    quit_gui(b, guard)?;
    let owner = b.recover_until_owner(guard)?;
    let after = b.snapshot()?;
    let s_after = b.session()?;
    b.step("after", json!({ "snapshot": after, "h1": s_after }));
    let closed = close(b, guard, &owner)?;

    let mut v = Vec::new();
    let refusal = match &reply {
        Reply::Refused { reason, retry } => Some((reason.clone(), *retry)),
        _ => None,
    };
    let fault = refusal
        .as_ref()
        .map(|(reason, _)| b.fault("update", reason));
    check(&mut v, matches!(refusal, Some((_, false))), || {
        format!("update не окончательный отказ: {reply:?}")
    });
    check(
        &mut v,
        fault
            .as_ref()
            .is_some_and(|fault| fault.is(libc::ENOSPC) && fault.observed.contains("ditto")),
        || format!("отказ update не ENOSPC от ditto: {fault:?}"),
    );
    check(&mut v, during.journal_text.is_none(), || {
        format!("журнал записан: {:?}", during.journal)
    });
    check(&mut v, during.dirs.is_empty(), || {
        format!("каталоги попытки остались: {:?}", during.dirs)
    });
    check(&mut v, valid(Some(&during.installed), &b.gen1), || {
        format!("установлен не N: {:?}", during.installed)
    });
    check(
        &mut v,
        pong.as_ref()
            .is_ok_and(|info| info.generation == 1 && Some(info.key) == b.gui),
        || format!("GUI N не отвечает: {pong:?}"),
    );
    check(&mut v, s_during.ok(), || {
        format!("h1 на полном томе: {s_during:?}")
    });
    check(&mut v, owner.recovery.is_empty(), || {
        format!("восстановление сделало шаги: {:?}", owner.recovery)
    });
    check(
        &mut v,
        after.journal_text.is_none()
            && after.dirs.is_empty()
            && valid(Some(&after.installed), &b.gen1),
        || format!("после восстановления: {}", b.describe(&after)),
    );
    common_after(b, &mut v, &owner, 1, &s_after, &closed);
    let evidence = format!(
        "update: «{}» ({}, {}); запас {} байт, том полон по пробе; во время: {}; GUI N {} отвечает; h1 — хвост {} строк и квитанция; после освобождения: владелец N {} без шагов восстановления, {}",
        fault.as_ref().map_or("", |fault| fault.observed.as_str()),
        fault
            .as_ref()
            .and_then(|fault| fault.errno.as_ref())
            .map_or("?".to_owned(), |errno| errno.name.clone()),
        fault
            .as_ref()
            .and_then(|fault| fault.syscall.clone())
            .unwrap_or_default(),
        fill.get("margin").and_then(Value::as_u64).unwrap_or(0),
        b.describe(&during),
        b.gui.map_or(0, |key| key.pid),
        s_during.lines,
        owner.key.pid,
        b.describe(&after),
    );
    Ok(Verdict::of(v, evidence))
}

fn enospc_journal(b: &mut Bench, guard: &mut Guard, layout: &Layout) -> Result<Verdict, String> {
    let attempt = stage(b, layout)?;
    let before = b.snapshot()?;
    b.fill(0)?;
    let reply = ask(&b.state(), &Request::Activate)?;
    b.step("activate", json!({ "reply": format!("{reply:?}") }));
    let during = b.snapshot()?;
    let helper_started = update::helper_dir(&b.state(), &attempt)
        .join(update::HELPER_STDOUT)
        .exists();
    let pong = ping(&b.state());
    let s_during = b.session()?;
    b.step(
        "during",
        json!({ "snapshot": during, "helper_started": helper_started, "ping": format!("{pong:?}"), "h1": s_during }),
    );
    quit_gui(b, guard)?;
    let blocked = b.blocked_run(guard, "recovery-full")?;
    let after_blocked = b.snapshot()?;
    b.free()?;
    let owner = b.recover_until_owner(guard)?;
    let after = b.snapshot()?;
    let history = b.history()?;
    let s_after = b.session()?;
    b.step(
        "after",
        json!({ "blocked": after_blocked, "snapshot": after, "history": history, "h1": s_after }),
    );
    let closed = close(b, guard, &owner)?;

    let mut v = Vec::new();
    let reason = match &reply {
        Reply::Refused {
            reason,
            retry: false,
        } => Some(reason.clone()),
        _ => None,
    };
    let fault = reason.as_ref().map(|reason| b.fault("activate", reason));
    check(
        &mut v,
        fault
            .as_ref()
            .is_some_and(|fault| fault.is(libc::ENOSPC) && fault.observed.contains("журнал")),
        || format!("activate не отказ ENOSPC записи журнала: {reply:?}"),
    );
    check(
        &mut v,
        during.journal_text == before.journal_text && during.in_state(&attempt, State::Staged),
        || format!("журнал изменился: {:?}", during.journal),
    );
    check(&mut v, during.tmp.is_empty(), || {
        format!("временный файл журнала остался: {:?}", during.tmp)
    });
    check(&mut v, !helper_started, || "помощник запущен".to_owned());
    check(
        &mut v,
        valid(Some(&during.installed), &b.gen1) && valid(during.staged.as_ref(), &b.gen2),
        || format!("раскладка на полном томе: {}", b.describe(&during)),
    );
    check(
        &mut v,
        pong.as_ref()
            .is_ok_and(|info| info.generation == 1 && Some(info.key) == b.gui),
        || format!("GUI N не отвечает: {pong:?}"),
    );
    check(&mut v, s_during.ok(), || {
        format!("h1 на полном томе: {s_during:?}")
    });
    let blocked_fault = blocked
        .error()
        .map(|reason| b.fault("recovery-full", reason));
    check(&mut v, blocked.failed_with(libc::ENOSPC), || {
        format!("восстановление на полном томе: {blocked:?}")
    });
    check(
        &mut v,
        after_blocked.journal_text == before.journal_text
            && after_blocked.dirs == before.dirs
            && after_blocked.tmp.is_empty(),
        || {
            format!(
                "восстановление на полном томе переставило: {}",
                b.describe(&after_blocked)
            )
        },
    );
    check(
        &mut v,
        after.idle(&attempt, Outcome::Aborted)
            && history == [State::Staged, State::Aborting, State::Idle]
            && after.dirs.is_empty()
            && valid(Some(&after.installed), &b.gen1),
        || {
            format!(
                "после освобождения: {}, история {history:?}",
                b.describe(&after)
            )
        },
    );
    common_after(b, &mut v, &owner, 1, &s_after, &closed);
    let evidence = format!(
        "activate: «{}» ({}); во время: {}, временного файла нет, помощник не запущен, GUI N отвечает, h1 прежняя; восстановление на полном томе: код {:?}, «{}»; после освобождения: история {:?}, владелец N {}, {}",
        fault.as_ref().map_or("", |fault| fault.observed.as_str()),
        fault
            .as_ref()
            .and_then(|fault| fault.syscall.clone())
            .unwrap_or_default(),
        b.describe(&during),
        blocked.code,
        blocked_fault
            .as_ref()
            .map_or("", |fault| fault.observed.as_str()),
        history.iter().map(|state| state.name()).collect::<Vec<_>>(),
        owner.key.pid,
        b.describe(&after),
    );
    Ok(Verdict::of(v, evidence))
}

fn enospc_runtime(b: &mut Bench, guard: &mut Guard, layout: &Layout) -> Result<Verdict, String> {
    let state = b.state();
    let partial = Barrier::new(Role::Next, Point::RuntimePartial);
    barrier::arm(&state, partial)?;
    let attempt = stage(b, layout)?;
    let helper = activate(b, guard)?;
    let reached = barrier::wait_reached(&state, partial, Instant::now() + STEP)?;
    let at_barrier = b.snapshot()?;
    b.step(
        "at-barrier",
        json!({ "reached": reached, "snapshot": at_barrier }),
    );
    b.fill(0)?;
    release(&state, partial)?;
    let next_gone = b.wait_gone(reached.process);
    let helper_gone = b.wait_gone(helper);
    let during = b.snapshot()?;
    let events = b.helper_events();
    let next = b.next_reports();
    let absent = b.owner_absent()?;
    let s_during = b.session()?;
    b.step(
        "during",
        json!({ "next_gone": next_gone, "helper_gone": helper_gone, "snapshot": during, "events": events, "next": next, "absent": absent, "h1": s_during }),
    );
    let blocked = b.blocked_run(guard, "recovery-full")?;
    let after_blocked = b.snapshot()?;
    b.free()?;
    let owner = b.recover_until_owner(guard)?;
    let after = b.snapshot()?;
    let history = b.history()?;
    let s_after = b.session()?;
    b.step(
        "after",
        json!({ "blocked": after_blocked, "snapshot": after, "history": history, "h1": s_after }),
    );
    let closed = close(b, guard, &owner)?;

    let mut v = Vec::new();
    check(
        &mut v,
        at_barrier.prep_next == ["probe-update-host"]
            && at_barrier.runtime.iter().any(|name| name == ".prep-2")
            && !at_barrier.runtime.iter().any(|name| name == "2"),
        || {
            format!(
                "на барьере runtime {:?}, .prep-2 {:?}",
                at_barrier.runtime, at_barrier.prep_next
            )
        },
    );
    let next_error = next.iter().find_map(|report| match report {
        AppReport::Error { reason, .. } => Some(reason.clone()),
        _ => None,
    });
    let next_fault = next_error.as_ref().map(|reason| b.fault("N+1", reason));
    check(&mut v, next_gone, || "N+1 не вышел".to_owned());
    check(
        &mut v,
        next_fault.as_ref().is_some_and(|fault| {
            fault.is(libc::ENOSPC)
                && fault
                    .syscall
                    .as_deref()
                    .is_some_and(|syscall| syscall.starts_with("fs::copy"))
        }),
        || format!("N+1 не отказал ENOSPC копии в рантайм: {next:?}"),
    );
    let error = event(&events, "error");
    let helper_fault = error.map(|error| b.fault("помощник", &event_text(Some(error), "reason")));
    check(&mut v, helper_gone, || "помощник не вышел".to_owned());
    check(
        &mut v,
        event(&events, "watch-owner-exited").is_some()
            && helper_fault
                .as_ref()
                .is_some_and(|fault| fault.is(libc::ENOSPC))
            && event(&events, "done").is_none()
            && event(&events, "relaunched").is_none(),
        || format!("события помощника: {events:?}"),
    );
    check(
        &mut v,
        during.in_state(&attempt, State::Launched)
            && during.journal.as_ref().and_then(|journal| journal.owner) == Some(reached.process)
            && valid(Some(&during.installed), &b.gen2)
            && valid(during.staged.as_ref(), &b.gen1)
            && during.runtime.iter().any(|name| name == ".prep-2")
            && !during.runtime.iter().any(|name| name == "2")
            && during.tmp.is_empty(),
        || format!("раскладка на полном томе: {}", b.describe(&during)),
    );
    check(&mut v, absent.is_some(), || {
        "на полном томе отвечает владелец".to_owned()
    });
    check(&mut v, s_during.ok(), || {
        format!("h1 на полном томе: {s_during:?}")
    });
    check(
        &mut v,
        blocked.failed_with(libc::ENOSPC)
            && after_blocked.journal_text == during.journal_text
            && after_blocked.installed == during.installed
            && after_blocked.staged == during.staged,
        || format!("восстановление на полном томе: {blocked:?}"),
    );
    check(
        &mut v,
        after.idle(&attempt, Outcome::RolledBack)
            && history
                == [
                    State::Staged,
                    State::Intent,
                    State::Swapped,
                    State::Launched,
                    State::RollbackIntent,
                    State::RolledBack,
                    State::Retired,
                    State::Idle,
                ]
            && valid(Some(&after.installed), &b.gen1)
            && valid(after.failed.as_ref(), &b.gen2)
            && after.dirs == [format!("{}{attempt}", journal::FAILED_PREFIX)],
        || {
            format!(
                "после освобождения: {}, история {history:?}",
                b.describe(&after)
            )
        },
    );
    common_after(b, &mut v, &owner, 1, &s_after, &closed);
    let evidence = format!(
        "N+1 {} на барьере (в .prep-2 {:?}), после заполнения: «{}» ({}); помощник: «{}»; во время: {}, владельца нет («{}»), h1 прежняя; восстановление на полном томе — код {:?}; после освобождения: история {:?}, владелец N {}, {}",
        reached.process.pid,
        at_barrier.prep_next,
        next_fault
            .as_ref()
            .map_or("", |fault| fault.observed.as_str()),
        next_fault
            .as_ref()
            .and_then(|fault| fault.syscall.clone())
            .unwrap_or_default(),
        helper_fault
            .as_ref()
            .map_or("", |fault| fault.observed.as_str()),
        b.describe(&during),
        absent.unwrap_or_default(),
        blocked.code,
        history.iter().map(|state| state.name()).collect::<Vec<_>>(),
        owner.key.pid,
        b.describe(&after),
    );
    Ok(Verdict::of(v, evidence))
}

/// Том кандидата: копия кандидата попытки, подключённая только для чтения на его место.
fn mount_candidate(
    b: &mut Bench,
    side: &Path,
    candidate: &Path,
    cdhash: &str,
) -> Result<SideVolume, String> {
    let image = Image::create(
        side.join(format!("{IMAGE_PREFIX}exdev-swap-side.dmg")),
        SIDE_MIB,
        "psu-b3-side",
    )?;
    let writable = side.join("mb");
    fs::create_dir(&writable).map_err(|err| io_err(&writable, err))?;
    let mut writer = Volume::attach(&image.path, &writable, false)?;
    let copied = generation_ditto(candidate, writer.mount())?;
    let extra: Vec<String> = names(writer.mount())
        .into_iter()
        .filter(|name| name != "Contents")
        .collect();
    for name in &extra {
        let path = writer.mount().join(name);
        let _ = fs::remove_dir_all(&path).or_else(|_| fs::remove_file(&path));
    }
    let wrote = writer.detach()?;
    fs::remove_dir(&writable).map_err(|err| io_err(&writable, err))?;
    let kept = b.state().join("kept-candidate");
    fs::rename(candidate, &kept).map_err(|err| io_err(candidate, err))?;
    fs::create_dir(candidate).map_err(|err| io_err(candidate, err))?;
    let reader = Volume::attach(&image.path, candidate, true)?;
    let seen = b.stand.check(candidate)?;
    let install_dir = b
        .stand
        .installed
        .parent()
        .ok_or("стенд: нет каталога установки")?;
    let devices = (
        fs::metadata(candidate)
            .map_err(|err| io_err(candidate, err))?
            .dev(),
        fs::metadata(install_dir)
            .map_err(|err| io_err(install_dir, err))?
            .dev(),
    );
    b.step(
        "candidate-volume",
        json!({ "created": image.created, "copied": copied, "removed_root_entries": extra, "detached_writer": wrote, "kept": kept, "attachment": reader.attachment, "seen": seen, "devices": [devices.0, devices.1] }),
    );
    if !matches!(&seen, Seen::Valid { cdhash: found } if found == cdhash) {
        return Err(format!(
            "стенд: кандидат на томе не проходит проверку как N+1: {seen:?}"
        ));
    }
    if devices.0 == devices.1 {
        return Err("стенд: кандидат на том же устройстве".to_owned());
    }
    Ok(SideVolume {
        volume: reader,
        image,
    })
}

fn generation_ditto(source: &Path, dest: &Path) -> Result<Run, String> {
    let args: Vec<String> = ["--norsrc", "--noextattr", "--noacl"]
        .into_iter()
        .map(str::to_owned)
        .chain([source.display().to_string(), dest.display().to_string()])
        .collect();
    let run = crate::generation::run("/usr/bin/ditto", &args)?;
    if run.code == Some(0) {
        Ok(run)
    } else {
        Err(format!("стенд: ditto: {}", run.text().trim()))
    }
}

fn exdev_swap(
    b: &mut Bench,
    guard: &mut Guard,
    layout: &Layout,
    side: &Path,
    slot: &mut Option<SideVolume>,
) -> Result<Verdict, String> {
    let state = b.state();
    let attempt = stage(b, layout)?;
    let journal = b.journal()?.ok_or("стенд: после staged журнала нет")?;
    let cdhash = journal.cdhash_to.clone();
    *slot = Some(mount_candidate(b, side, &journal.staged_dir(), &cdhash)?);
    let aborting = Barrier::new(
        Role::Helper,
        Point::Journal {
            state: State::Aborting,
            renamed: true,
        },
    );
    barrier::arm(&state, aborting)?;
    let helper = activate(b, guard)?;
    let reached = barrier::wait_reached(&state, aborting, Instant::now() + STEP)?;
    let during = b.snapshot()?;
    let events_during = b.helper_events();
    let detached = match slot.as_mut() {
        Some(side) => side.volume.detach()?,
        None => return Err("стенд: тома кандидата нет".to_owned()),
    };
    let emptied = names(&journal.staged_dir());
    b.step(
        "at-barrier",
        json!({ "reached": reached, "snapshot": during, "events": events_during, "detached": detached, "candidate_entries": emptied }),
    );
    release(&state, aborting)?;
    let owner = owner::wait_for_owner(&state, Instant::now() + STEP)?;
    let helper_gone = b.wait_gone(helper);
    let events = b.helper_events();
    let after = b.snapshot()?;
    let history = b.history()?;
    let s_after = b.session()?;
    b.step(
        "after",
        json!({ "owner": owner, "helper_gone": helper_gone, "events": events, "snapshot": after, "history": history, "h1": s_after }),
    );
    let closed = close(b, guard, &owner)?;

    let mut v = Vec::new();
    check(&mut v, reached.process == helper, || {
        format!("на барьере не помощник: {reached:?}")
    });
    let failed = event(&events_during, "swap-failed");
    let errno = failed
        .and_then(|event| event.get("detail"))
        .and_then(|detail| detail.get("errno"))
        .and_then(Value::as_i64);
    let reason = during
        .journal
        .as_ref()
        .and_then(|journal| journal.reason.clone())
        .unwrap_or_default();
    let fault = b.fault("журнал aborting: reason", &reason);
    check(
        &mut v,
        errno == Some(i64::from(libc::EXDEV)) && fault.is(libc::EXDEV),
        || format!("обмен помощника не EXDEV: {failed:?}"),
    );
    check(
        &mut v,
        during.in_state(&attempt, State::Aborting)
            && during
                .journal
                .as_ref()
                .and_then(|journal| journal.reason.as_deref())
                .is_some_and(|reason| Fault::of("", reason).is(libc::EXDEV))
            && valid(Some(&during.installed), &b.gen1)
            && valid(during.staged.as_ref(), &b.gen2),
        || format!("на барьере: {}", b.describe(&during)),
    );
    check(
        &mut v,
        helper_gone
            && event(&events, "aborted").is_some()
            && event(&events, "relaunched").is_some()
            && event(&events, "done").is_some(),
        || format!("события помощника после барьера: {events:?}"),
    );
    check(
        &mut v,
        after.idle(&attempt, Outcome::Aborted)
            && history == [State::Staged, State::Intent, State::Aborting, State::Idle]
            && after.dirs.is_empty()
            && valid(Some(&after.installed), &b.gen1),
        || format!("после барьера: {}, история {history:?}", b.describe(&after)),
    );
    check(&mut v, Some(owner.key) != b.gui, || {
        "владелец — прежний GUI N".to_owned()
    });
    common_after(b, &mut v, &owner, 1, &s_after, &closed);
    let evidence = format!(
        "кандидат на отдельном томе (тот же CDHash {cdhash}); помощник {}: renamex_np — «{}» (errno {:?}); на барьере: {}; после отключения тома: история {:?}, N запущен помощником (владелец {}), {}",
        helper.pid,
        fault.observed,
        errno,
        b.describe(&during),
        history.iter().map(|state| state.name()).collect::<Vec<_>>(),
        owner.key.pid,
        b.describe(&after),
    );
    Ok(Verdict::of(v, evidence))
}

fn eacces_stage(
    b: &mut Bench,
    guard: &mut Guard,
    layout: &Layout,
    mode: &mut Mode,
) -> Result<Verdict, String> {
    let install_dir = b
        .stand
        .installed
        .parent()
        .ok_or("стенд: нет каталога установки")?
        .to_path_buf();
    let chmod = mode.set(&install_dir, 0o555)?;
    b.step("chmod", chmod);
    let candidate = layout.get("gen-2")?.app.display().to_string();
    let reply = ask(&b.state(), &Request::Update { candidate })?;
    b.step("update", json!({ "reply": format!("{reply:?}") }));
    let during = b.snapshot()?;
    let pong = ping(&b.state());
    let s_during = b.session()?;
    b.step(
        "during",
        json!({ "snapshot": during, "ping": format!("{pong:?}"), "h1": s_during }),
    );
    mode.restore()?;
    quit_gui(b, guard)?;
    let owner = b.recover_until_owner(guard)?;
    let after = b.snapshot()?;
    let s_after = b.session()?;
    b.step("after", json!({ "snapshot": after, "h1": s_after }));
    let closed = close(b, guard, &owner)?;

    let mut v = Vec::new();
    let reason = match &reply {
        Reply::Refused {
            reason,
            retry: false,
        } => Some(reason.clone()),
        _ => None,
    };
    let fault = reason.as_ref().map(|reason| b.fault("update", reason));
    check(
        &mut v,
        fault
            .as_ref()
            .is_some_and(|fault| fault.is(libc::EACCES) && fault.observed.contains("ditto")),
        || format!("update не окончательный отказ EACCES от ditto: {reply:?}"),
    );
    check(
        &mut v,
        during.journal_text.is_none()
            && during.dirs.is_empty()
            && valid(Some(&during.installed), &b.gen1),
        || format!("без права записи: {}", b.describe(&during)),
    );
    check(
        &mut v,
        pong.as_ref()
            .is_ok_and(|info| info.generation == 1 && Some(info.key) == b.gui),
        || format!("GUI N не отвечает: {pong:?}"),
    );
    check(&mut v, s_during.ok(), || {
        format!("h1 без права записи: {s_during:?}")
    });
    check(
        &mut v,
        owner.recovery.is_empty()
            && after.journal_text.is_none()
            && after.dirs.is_empty()
            && valid(Some(&after.installed), &b.gen1),
        || {
            format!(
                "после возврата прав: {}, шаги {:?}",
                b.describe(&after),
                owner.recovery
            )
        },
    );
    common_after(b, &mut v, &owner, 1, &s_after, &closed);
    let evidence = format!(
        "update: «{}» ({}); во время: {}, GUI N отвечает, h1 прежняя; после возврата прав: владелец N {} без шагов восстановления, {}",
        fault.as_ref().map_or("", |fault| fault.observed.as_str()),
        fault
            .as_ref()
            .and_then(|fault| fault.syscall.clone())
            .unwrap_or_default(),
        b.describe(&during),
        owner.key.pid,
        b.describe(&after),
    );
    Ok(Verdict::of(v, evidence))
}

fn eacces_swap(
    b: &mut Bench,
    guard: &mut Guard,
    layout: &Layout,
    mode: &mut Mode,
) -> Result<Verdict, String> {
    let attempt = stage(b, layout)?;
    let install_dir = b
        .stand
        .installed
        .parent()
        .ok_or("стенд: нет каталога установки")?
        .to_path_buf();
    let chmod = mode.set(&install_dir, 0o555)?;
    b.step("chmod", chmod);
    let helper = activate(b, guard)?;
    let helper_gone = b.wait_gone(helper);
    let during = b.snapshot()?;
    let events = b.helper_events();
    let absent = b.owner_absent()?;
    let s_during = b.session()?;
    b.step(
        "during",
        json!({ "helper_gone": helper_gone, "snapshot": during, "events": events, "absent": absent, "h1": s_during }),
    );
    let blocked = b.blocked_run(guard, "recovery-denied")?;
    let after_blocked = b.snapshot()?;
    mode.restore()?;
    let owner = b.recover_until_owner(guard)?;
    let after = b.snapshot()?;
    let history = b.history()?;
    let s_after = b.session()?;
    b.step(
        "after",
        json!({ "blocked": after_blocked, "snapshot": after, "history": history, "h1": s_after }),
    );
    let closed = close(b, guard, &owner)?;

    let mut v = Vec::new();
    let failed = event(&events, "swap-failed");
    let errno = failed
        .and_then(|event| event.get("detail"))
        .and_then(|detail| detail.get("errno"))
        .and_then(Value::as_i64);
    let reason = during
        .journal
        .as_ref()
        .and_then(|journal| journal.reason.clone())
        .unwrap_or_default();
    let swap_fault = b.fault("журнал aborting: reason", &reason);
    check(
        &mut v,
        errno == Some(i64::from(libc::EACCES)) && swap_fault.is(libc::EACCES),
        || format!("обмен помощника не EACCES: {failed:?}"),
    );
    let error = event(&events, "error");
    let remove_fault = b.fault("помощник: error", &event_text(error, "reason"));
    check(
        &mut v,
        helper_gone
            && remove_fault.is(libc::EACCES)
            && remove_fault.observed.contains("удаление кандидата")
            && event(&events, "relaunched").is_none(),
        || format!("помощник после отказа удаления: {events:?}"),
    );
    check(
        &mut v,
        during.in_state(&attempt, State::Aborting)
            && valid(Some(&during.installed), &b.gen1)
            && !valid(during.staged.as_ref(), &b.gen2)
            && during.dirs == [format!("{}{attempt}", journal::STAGED_PREFIX)],
        || format!("без права записи: {}", b.describe(&during)),
    );
    check(&mut v, absent.is_some(), || {
        "без права записи отвечает владелец".to_owned()
    });
    check(&mut v, s_during.ok(), || {
        format!("h1 без права записи: {s_during:?}")
    });
    let blocked_fault = blocked
        .error()
        .map(|reason| b.fault("recovery-denied", reason));
    check(
        &mut v,
        blocked.failed_with(libc::EACCES) && after_blocked.journal_text == during.journal_text,
        || format!("восстановление без права записи: {blocked:?}"),
    );
    check(
        &mut v,
        after.idle(&attempt, Outcome::Aborted)
            && history == [State::Staged, State::Intent, State::Aborting, State::Idle]
            && after.dirs.is_empty()
            && valid(Some(&after.installed), &b.gen1),
        || {
            format!(
                "после возврата прав: {}, история {history:?}",
                b.describe(&after)
            )
        },
    );
    common_after(b, &mut v, &owner, 1, &s_after, &closed);
    let evidence = format!(
        "помощник {}: renamex_np — «{}»; удаление кандидата — «{}», N не запущен; во время: {}, владельца нет, h1 прежняя; восстановление без прав — код {:?}, «{}»; после возврата прав: история {:?}, владелец N {}, {}",
        helper.pid,
        swap_fault.observed,
        remove_fault.observed,
        b.describe(&during),
        blocked.code,
        blocked_fault
            .as_ref()
            .map_or("", |fault| fault.observed.as_str()),
        history.iter().map(|state| state.name()).collect::<Vec<_>>(),
        owner.key.pid,
        b.describe(&after),
    );
    Ok(Verdict::of(v, evidence))
}

fn injected_journal_rename(
    b: &mut Bench,
    guard: &mut Guard,
    layout: &Layout,
) -> Result<Verdict, String> {
    let state = b.state();
    let fault_at = injection(Kind::InjectedJournalRename)?;
    barrier::arm_fault(&state, fault_at, INJECTED_ERRNO)?;
    let attempt = stage(b, layout)?;
    let helper = activate(b, guard)?;
    let helper_gone = b.wait_gone(helper);
    let injected = barrier::read_injected(&state, fault_at)?;
    let during = b.snapshot()?;
    let events = b.helper_events();
    let absent = b.owner_absent()?;
    let s_during = b.session()?;
    b.step(
        "during",
        json!({ "helper_gone": helper_gone, "injected": injected, "snapshot": during, "events": events, "absent": absent, "h1": s_during }),
    );
    let owner = b.recover_until_owner(guard)?;
    let after = b.snapshot()?;
    let history = b.history()?;
    let s_after = b.session()?;
    b.step(
        "after",
        json!({ "snapshot": after, "history": history, "h1": s_after }),
    );
    let closed = close(b, guard, &owner)?;

    let mut v = Vec::new();
    check(
        &mut v,
        injected
            .as_ref()
            .is_some_and(|injected| injected.process == helper && injected.errno == INJECTED_ERRNO)
            && !fault_at.fault_path(&state).exists(),
        || format!("инъекция не сработала у помощника: {injected:?}"),
    );
    let error = event(&events, "error");
    let fault = b.fault("помощник: error", &event_text(error, "reason"));
    check(
        &mut v,
        helper_gone
            && fault.injected
            && fault.is(INJECTED_ERRNO)
            && event(&events, "exchanged").is_some()
            && event(&events, "swapped").is_none()
            && event(&events, "launched").is_none()
            && event(&events, "released").is_none(),
        || format!("события помощника: {events:?}"),
    );
    check(
        &mut v,
        during.in_state(&attempt, State::Intent)
            && during.tmp.is_empty()
            && valid(Some(&during.installed), &b.gen2)
            && valid(during.staged.as_ref(), &b.gen1),
        || format!("после отказа rename: {}", b.describe(&during)),
    );
    check(&mut v, absent.is_some(), || {
        "после отказа помощника отвечает владелец".to_owned()
    });
    check(&mut v, s_during.ok(), || {
        format!("h1 после отказа: {s_during:?}")
    });
    check(
        &mut v,
        after.idle(&attempt, Outcome::Committed)
            && history
                == [
                    State::Staged,
                    State::Intent,
                    State::Swapped,
                    State::Launched,
                    State::Committed,
                    State::Cleaning,
                    State::Idle,
                ]
            && after.dirs.is_empty()
            && valid(Some(&after.installed), &b.gen2),
        || {
            format!(
                "после восстановления: {}, история {history:?}",
                b.describe(&after)
            )
        },
    );
    common_after(b, &mut v, &owner, 2, &s_after, &closed);
    let evidence = format!(
        "injected: помощник {} получил errno {INJECTED_ERRNO} на {}; ошибка «{}»; во время: {}, временного файла нет, владельца нет, h1 прежняя; восстановление: история {:?}, владелец N+1 {}, {}",
        helper.pid,
        fault_at.key(),
        fault.observed,
        b.describe(&during),
        history.iter().map(|state| state.name()).collect::<Vec<_>>(),
        owner.key.pid,
        b.describe(&after),
    );
    Ok(Verdict::of(v, evidence))
}

fn injected_reverse_swap(
    b: &mut Bench,
    guard: &mut Guard,
    layout: &Layout,
) -> Result<Verdict, String> {
    let state = b.state();
    let fault_at = injection(Kind::InjectedReverseSwap)?;
    barrier::arm_fault(&state, fault_at, INJECTED_ERRNO)?;
    let before_publish = Barrier::new(Role::Next, Point::BeforePublish);
    barrier::arm(&state, before_publish)?;
    let attempt = stage(b, layout)?;
    let helper = activate(b, guard)?;
    let reached = barrier::wait_reached(&state, before_publish, Instant::now() + STEP)?;
    if alive(reached.process) {
        // SAFETY: сигнал процессу, узнанному по PID и времени старта на барьере.
        unsafe { libc::kill(reached.process.pid, libc::SIGKILL) };
    }
    if !b.wait_gone(reached.process) {
        return Err(format!(
            "стенд: N+1 {} не вышел после SIGKILL",
            reached.process.pid
        ));
    }
    release(&state, before_publish)?;
    let helper_gone = b.wait_gone(helper);
    let injected = barrier::read_injected(&state, fault_at)?;
    let during = b.snapshot()?;
    let events = b.helper_events();
    let absent = b.owner_absent()?;
    let s_during = b.session()?;
    b.step(
        "during",
        json!({ "killed": reached, "helper_gone": helper_gone, "injected": injected, "snapshot": during, "events": events, "absent": absent, "h1": s_during }),
    );
    let owner = b.recover_until_owner(guard)?;
    let after = b.snapshot()?;
    let history = b.history()?;
    let s_after = b.session()?;
    b.step(
        "after",
        json!({ "snapshot": after, "history": history, "h1": s_after }),
    );
    let closed = close(b, guard, &owner)?;

    let mut v = Vec::new();
    check(
        &mut v,
        injected
            .as_ref()
            .is_some_and(|injected| injected.process == helper && injected.errno == INJECTED_ERRNO)
            && !fault_at.fault_path(&state).exists(),
        || format!("инъекция не сработала у помощника: {injected:?}"),
    );
    let error = event(&events, "error");
    let fault = b.fault("помощник: error", &event_text(error, "reason"));
    check(
        &mut v,
        helper_gone
            && event(&events, "watch-owner-exited").is_some()
            && fault.injected
            && fault.is(INJECTED_ERRNO)
            && event(&events, "done").is_none()
            && event(&events, "relaunched").is_none(),
        || format!("события помощника: {events:?}"),
    );
    check(
        &mut v,
        during.in_state(&attempt, State::RollbackIntent)
            && valid(Some(&during.installed), &b.gen2)
            && valid(during.staged.as_ref(), &b.gen1),
        || format!("после отказа обратного обмена: {}", b.describe(&during)),
    );
    check(&mut v, absent.is_some(), || {
        "после отказа помощника отвечает владелец".to_owned()
    });
    check(&mut v, s_during.ok(), || {
        format!("h1 после отказа: {s_during:?}")
    });
    check(
        &mut v,
        after.idle(&attempt, Outcome::RolledBack)
            && history
                == [
                    State::Staged,
                    State::Intent,
                    State::Swapped,
                    State::Launched,
                    State::RollbackIntent,
                    State::RolledBack,
                    State::Retired,
                    State::Idle,
                ]
            && valid(Some(&after.installed), &b.gen1)
            && valid(after.failed.as_ref(), &b.gen2)
            && after.dirs == [format!("{}{attempt}", journal::FAILED_PREFIX)],
        || {
            format!(
                "после восстановления: {}, история {history:?}",
                b.describe(&after)
            )
        },
    );
    common_after(b, &mut v, &owner, 1, &s_after, &closed);
    let evidence = format!(
        "injected: N+1 {} снят на next.before-publish, помощник {} получил errno {INJECTED_ERRNO} на {}; ошибка «{}»; во время: {}, владельца нет, h1 прежняя; восстановление: история {:?}, владелец N {}, {}",
        reached.process.pid,
        helper.pid,
        fault_at.key(),
        fault.observed,
        b.describe(&during),
        history.iter().map(|state| state.name()).collect::<Vec<_>>(),
        owner.key.pid,
        b.describe(&after),
    );
    Ok(Verdict::of(v, evidence))
}

// --- сценарий ---

#[allow(clippy::too_many_arguments)]
fn drive(
    case: &Case,
    side: &Path,
    mount: &Path,
    nonce: &str,
    layout: &Layout,
    out: &Path,
    slot: &mut Option<Bench>,
) -> Result<Verdict, String> {
    let image = Image::create(
        side.join(format!("{IMAGE_PREFIX}{}.dmg", case.spec.name)),
        VOLUME_MIB,
        "psu-b3",
    )?;
    let mut volume = Volume::attach(&image.path, mount, false)?;
    let root = volume.mount().to_path_buf();
    let (_, total) = sys::volume_space(&root).map_err(|err| io_err(&root, err))?;
    let vstate = root.join("s");
    fs::create_dir(&vstate).map_err(|err| io_err(&vstate, err))?;
    let links = prepare_state(&vstate, side)?;
    let stand = Stand::at(&vstate, nonce, layout, "gen-1", "install")?;
    let mut side_volume: Option<SideVolume> = None;
    let mut guard = Guard {
        state: vstate.clone(),
        apps: Vec::new(),
        sessions: Vec::new(),
        nonce: Some(nonce.to_owned()),
        armed: true,
    };
    let mut mode = Mode::default();
    *slot = Some(Bench {
        out: out.to_path_buf(),
        root: root.clone(),
        stand,
        gen1: layout.get("gen-1")?.cdhash.clone(),
        gen2: layout.get("gen-2")?.cdhash.clone(),
        h1: None,
        gui: None,
        gui_index: 0,
        attempt: None,
        helper: None,
        seq: 1,
        runs: 0,
        fillers: Vec::new(),
        faults: Vec::new(),
        steps: Vec::new(),
    });
    let b = slot.as_mut().ok_or("стенд: часть")?;
    b.step(
        "volume",
        json!({ "image": image.path, "created": image.created, "attachment": volume.attachment, "total_bytes": total, "links": links }),
    );
    begin(b, &mut guard)?;
    let verdict = match case.kind {
        Kind::EnospcCandidate => enospc_candidate(b, &mut guard, layout)?,
        Kind::EnospcJournal => enospc_journal(b, &mut guard, layout)?,
        Kind::EnospcRuntime => enospc_runtime(b, &mut guard, layout)?,
        Kind::ExdevSwap => exdev_swap(b, &mut guard, layout, side, &mut side_volume)?,
        Kind::EaccesStage => eacces_stage(b, &mut guard, layout, &mut mode)?,
        Kind::EaccesSwap => eacces_swap(b, &mut guard, layout, &mut mode)?,
        Kind::InjectedJournalRename => injected_journal_rename(b, &mut guard, layout)?,
        Kind::InjectedReverseSwap => injected_reverse_swap(b, &mut guard, layout)?,
    };
    let verdict = complete(verdict, &b.faults);
    drop(mode);
    drop(guard);
    if let Some(mut side) = side_volume.take() {
        if side.volume.is_attached() {
            side.volume.detach()?;
        }
        drop(side.image);
    }
    let detached = volume.detach()?;
    b.step("detached", json!(detached));
    Ok(verdict)
}

/// Каждый отказ части записан с errno и системным вызовом (план, «B3»), иначе часть — `fail`.
fn complete(verdict: Verdict, faults: &[Fault]) -> Verdict {
    let incomplete: Vec<String> = faults
        .iter()
        .filter(|fault| fault.errno.is_none() || fault.syscall.is_none())
        .map(|fault| format!("{}: «{}»", fault.source, fault.observed))
        .collect();
    if incomplete.is_empty() {
        return verdict;
    }
    let missing = format!(
        "отказ без errno или системного вызова: {}",
        incomplete.join("; ")
    );
    Verdict {
        status: Status::Fail,
        reason: match verdict.status {
            Status::Fail => format!("{}; {missing}", verdict.reason),
            _ => missing,
        },
    }
}

type CaseResult = Result<(Cell, Value), (String, Value)>;

/// Журналы помощника и N+1, отметки и история из каталога вне тома — в каталог части.
fn keep_evidence(side: &Path, out: &Path) {
    let copy_files = |from: &Path, to: &Path| {
        if let Ok(entries) = fs::read_dir(from) {
            let _ = fs::create_dir_all(to);
            for entry in entries.filter_map(Result::ok) {
                let name = entry.file_name().to_string_lossy().into_owned();
                let keep = name.ends_with(".json")
                    || name.ends_with(".stdout")
                    || name.ends_with(".stderr")
                    || name.ends_with(".reached")
                    || name.ends_with(barrier::INJECTED_SUFFIX);
                if keep && entry.file_type().is_ok_and(|kind| kind.is_file()) {
                    let _ = fs::copy(entry.path(), to.join(&name));
                }
            }
        }
    };
    let barriers = side.join(barrier::BARRIERS_DIR);
    copy_files(&barriers, &out.join("barriers"));
    copy_files(&barriers.join(barrier::NOTES_DIR), &out.join("notes"));
    for attempt in names(&side.join(update::HELPER_DIR)) {
        copy_files(
            &side.join(update::HELPER_DIR).join(&attempt),
            &out.join(format!("helper-{attempt}")),
        );
    }
    let history = side.join(journal::HISTORY_FILE);
    if history.is_file() {
        let _ = fs::copy(&history, out.join(journal::HISTORY_FILE));
    }
}

fn run_case(
    index: usize,
    case: &Case,
    scenario_state: &Path,
    nonce: &str,
    layout: &Layout,
    out: &Path,
) -> CaseResult {
    let side = scenario_state.join(format!("f{index}"));
    let mount = scenario_state.join(format!("v{index}"));
    for dir in [&side, &mount] {
        if let Err(err) = fs::create_dir(dir) {
            return Err((io_err(dir, err), Value::Null));
        }
    }
    let mut slot = None;
    let result = drive(case, &side, &mount, nonce, layout, out, &mut slot);
    let facts = slot.as_ref().map_or(Value::Null, |b: &Bench| {
        json!({ "attempt": b.attempt, "helper": b.helper, "gui": b.gui, "h1": b.h1, "faults": b.faults, "steps": b.steps })
    });
    drop(slot);
    keep_evidence(&side, out);
    let _ = fs::remove_dir_all(&side);
    let _ = fs::remove_dir(&mount);
    match result {
        Ok(verdict) => {
            let artifact = format!("{DIR}/{}", case.spec.name);
            Ok((
                super::required_cell(
                    CELL,
                    &case.spec,
                    COMPLETION,
                    verdict,
                    &[STEPS_ARTIFACT, &artifact],
                ),
                facts,
            ))
        }
        Err(reason) => Err((reason, facts)),
    }
}

pub fn run(out: &Path, environment: &Environment) -> Result<Report, String> {
    let (scenario_state, nonce, layout) = super::stand_inputs(environment)?;
    let dir = out.join(DIR);
    fs::create_dir(&dir).map_err(|err| io_err(&dir, err))?;
    let mut steps = Vec::new();
    let mut cells = Vec::new();
    let mut failure = None;
    for (index, case) in CASES.iter().enumerate() {
        let case_out = dir.join(case.spec.name);
        if let Err(err) = fs::create_dir(&case_out) {
            failure = Some(io_err(&case_out, err));
            break;
        }
        let started = Instant::now();
        match run_case(index + 1, case, &scenario_state, &nonce, &layout, &case_out) {
            Ok((cell, facts)) => {
                steps.push(json!({ "case": case.spec.name, "seconds": started.elapsed().as_secs_f64(), "facts": facts }));
                cells.push(cell);
            }
            Err((reason, facts)) => {
                steps.push(json!({ "case": case.spec.name, "error": reason, "facts": facts }));
                failure = Some(format!("{}: {reason}", case.spec.name));
                break;
            }
        }
    }
    let written = crate::results::write_json(&dir, "steps.json", &steps);
    if let Some(reason) = failure {
        return Err(reason);
    }
    written?;
    Ok(Report {
        cells,
        diagnostics: Diagnostics {
            notes: vec![format!(
                "частей {}; том установки {VOLUME_MIB} МиБ APFS без таблицы разделов, том кандидата EXDEV {SIDE_MIB} МиБ; запас части enospc-candidate {MARGIN_BYTES} байт; «полон» — файл в 1500 байт не создаётся с ENOSPC (до {FILL_ROUNDS} заполнений); errno инъекций {INJECTED_ERRNO} (EIO); срок шага стенда {} с",
                CASES.len(),
                STEP.as_secs()
            )],
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_injection_is_a_fault_site_and_marked_injected() {
        for (kind, barrier) in INJECTIONS {
            assert!(barrier::fault_site(barrier.point), "{}", barrier.key());
            let case = CASES.iter().find(|case| case.kind == kind).unwrap();
            assert!(
                case.spec.name.starts_with("injected-"),
                "{}",
                case.spec.name
            );
            assert!(
                case.spec.stimulus.starts_with("injected:"),
                "{}",
                case.spec.name
            );
        }
        for case in CASES {
            let injected = INJECTIONS.iter().any(|(kind, _)| *kind == case.kind);
            assert_eq!(
                case.spec.stimulus.contains("injected"),
                injected,
                "{}",
                case.spec.name
            );
        }
    }

    #[test]
    fn cases_cover_every_fault_kind_of_the_plan_once() {
        let names: Vec<&str> = CASES.iter().map(|case| case.spec.name).collect();
        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), names.len());
        for prefix in [
            "enospc-candidate",
            "enospc-journal",
            "enospc-runtime",
            "exdev-",
            "eacces-",
            "injected-",
        ] {
            assert!(
                names.iter().any(|name| name.starts_with(prefix)),
                "{prefix}"
            );
        }
    }

    #[test]
    fn a_fault_without_errno_or_syscall_fails_the_part() {
        let pass = || Verdict::of(Vec::new(), "ok".to_owned());
        let labelled = Fault::of(
            "r",
            "журнал: временный файл: No space left on device (os error 28)",
        );
        assert_eq!(
            complete(pass(), std::slice::from_ref(&labelled)).status,
            Status::Pass
        );
        let bare = Fault::of("r", "Cross-device link (os error 18)");
        let failed = complete(pass(), &[labelled, bare]);
        assert_eq!(failed.status, Status::Fail);
        assert!(failed.reason.contains("Cross-device"), "{}", failed.reason);
        assert_eq!(complete(pass(), &[Fault::of("r", "")]).status, Status::Fail);
    }

    #[test]
    fn blocked_run_must_exit_with_an_error_of_the_fault() {
        let blocked = Blocked {
            name: "r".to_owned(),
            pid: 1,
            code: Some(1),
            reports: vec![AppReport::Error {
                key: None,
                reason: "журнал: временный файл: No space left on device (os error 28)".to_owned(),
            }],
        };
        assert!(blocked.failed_with(libc::ENOSPC));
        assert!(!blocked.failed_with(libc::EACCES));
        let owner = Blocked {
            code: Some(0),
            ..blocked.clone()
        };
        assert!(!owner.failed_with(libc::ENOSPC));
    }
}
