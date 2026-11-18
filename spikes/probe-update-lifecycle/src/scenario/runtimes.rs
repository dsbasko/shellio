//! Сценарий `runtimes` (задача 12): клетка D1 «Рантаймы». Установленный N — копия поколения 1,
//! кандидат — поколение 2. Части:
//!
//! 1. `interrupted-preparation` — GUI N убит `SIGKILL` на барьере `gui.runtime-partial` посреди
//!    подготовки `runtime/1/` (скопирован только хост); новый прямой запуск удаляет оставшийся
//!    `runtime/.prep-1` и готовит `runtime/1/` заново;
//! 2. `old-host-kept` — после подтверждения N+1 хост и нагрузка `h1` и `h1b` прежние, их
//!    `proc_pidpath` — `runtime/1/`, сам `runtime/1/` не тронут;
//! 3. `next-attaches-v1` — N+1 подключается к хостам 1 по протоколу 1: health, `sessions` и
//!    отдельный процесс attach из `runtime/2/` с хвостом без пропуска и квитанцией;
//! 4. `new-session-runtime-2` — новая сессия `h2` у N+1 идёт от хоста из `runtime/2/`;
//! 5. `cleanup-candidate` — `runtime/1/` не кандидат на уборку, пока жив хоть один его хост, и
//!    кандидат после остановки последней его сессии;
//! 6. `no-overwrite` — совпадающий готовый `runtime/2/` переиспользуется, несовпадающий — явная
//!    ошибка запуска без перезаписи.
//!
//! Невыполненный оракул — `fail` части, сломанный стенд — ошибка исполнения. Шаги и факты — в
//! `runtimes/steps.json`, вызовы `codesign` и `shasum` — в `runtimes/runs.json`.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Value, json};

use crate::attach::{self, Attached, CliReport, Connected, Transcript};
use crate::barrier::{self, Barrier, Point, Reached, Role};
use crate::compat::Staging;
use crate::generation::{self, BUNDLE_ID, Layout, Manifest, Part, Run, Verify};
use crate::health::SessionFacts;
use crate::host::SessionRecord;
use crate::journal::{Outcome, State};
use crate::launch;
use crate::observer::{self, ProcessKey};
use crate::owner::{self, Answer, AppReport, Entry, Inode, OwnerInfo, Probe, Reply, Request};
use crate::proto::{ATTACH_VERSIONS, Purpose};
use crate::pty::Winsize;
use crate::results::{Cell, Diagnostics, Environment, Report, Requirement};
use crate::runtime::{self, Outcome as Prepared};
use crate::sys;

use super::owner_basic::{Guard, Registry, ask, clear_state, reap, reports, sessions, start_app};
use super::update_flow::{self, Followed, Stand, canonical};
use super::{PartSpec, Verdict};

pub const DEADLINE: Duration = Duration::from_secs(300);
pub const DIR: &str = "runtimes";
pub const STEPS_ARTIFACT: &str = "runtimes/steps.json";
const RUNS_ARTIFACT: &str = "runtimes/runs.json";
pub const CELL: &str = "D1";
const STEP: Duration = update_flow::STEP;
const COMPLETION: &str = "ответ владельца или хоста, барьер, выход процесса или готовность N+1 в срок шага (30 с); иначе ошибка исполнения";
/// Посреди подготовки `runtime/1/` у GUI N.
pub const PARTIAL: Barrier = Barrier::new(Role::Gui, Point::RuntimePartial);
/// Что стенд дописывает к копии attach в `runtime/2/`, чтобы готовый каталог разошёлся с бандлом.
const TAMPER: &[u8] = b"\nshellio-probe: runtime copy changed by stand\n";
/// Ответ запуска, отказавшего готовому каталогу (`runtime::prepare`).
const NO_OVERWRITE_REASON: &str = "перезапись запрещена";

pub const INTERRUPTED: PartSpec = PartSpec {
    name: "interrupted-preparation",
    stimulus: "барьер gui.runtime-partial: GUI N убит SIGKILL посреди подготовки runtime/1 (скопирован только хост), затем новый прямой запуск",
    oracle: "на барьере стоит запущенный GUI N; в runtime/.prep-1 только probe-update-host, runtime/1 нет, сокета нет; GUI N завершён сигналом 9, каталог подготовки после убийства прежний; новый владелец сообщает runtime/1 «создан» с удалённым .prep-1; .prep-1 нет; в runtime/1 ровно host, attach, fixture, каждый байт в байт равен бинарю бандла N и проходит codesign требованием своей части",
};
pub const OLD_HOST: PartSpec = PartSpec {
    name: "old-host-kept",
    stimulus: "h1 и h1b от хоста 1 с квитанцией ack 1 ping, update поколением 2, activate, N+1 подтверждён",
    oracle: "попытка idle «подтверждена», владелец — N+1 поколения 2; хост и нагрузка h1 и h1b живы с прежними PID и временем старта; proc_pidpath хоста — runtime/1/probe-update-host, нагрузки — runtime/1/probe-update-fixture до и после; runtime/1 после обновления — тот же каталог с теми же inode и SHA-256 файлов",
};
pub const ATTACH_V1: PartSpec = PartSpec {
    name: "next-attaches-v1",
    stimulus: "health N+1; sessions у N+1; отдельный процесс runtime/2/probe-update-attach (attach 1..2) — attach h1 с байта 0 и ввод 2 ping",
    oracle: "правило health без нарушений, в нём h1 и h1b найдены с прежними процессами, версия 1, хвост без пропуска, квитанция; sessions у N+1: probe h1 и h1b — Welcome версии 1; attach из runtime/2 (байт в байт равен attach бандла N+1): Welcome версии 1 от хоста поколения 1, хвост с первой строки без пропуска, квитанция ack 2 ping и ответ хоста",
};
pub const NEW_SESSION: PartSpec = PartSpec {
    name: "new-session-runtime-2",
    stimulus: "create-session h2 у N+1, строка ready нагрузки, затем attach из runtime/2 и ввод 1 ping",
    oracle: "N+1 сообщает runtime/2 «создан» без прерванной подготовки; запись h2 — поколение 2, протокол хоста из манифеста поколения 2, исполняемый файл runtime/2/probe-update-host; proc_pidpath хоста — runtime/2/probe-update-host, нагрузки — runtime/2/probe-update-fixture; attach: Welcome версии 2, хвост без пропуска, квитанция ack 1 ping",
};
pub const CLEANUP: PartSpec = PartSpec {
    name: "cleanup-candidate",
    stimulus: "sessions у N до обновления и у N+1: при живых h1 и h1b; с h2; после остановки h1b; после остановки h1",
    oracle: "кандидатов на уборку нет, пока жив хотя бы один хост из runtime/1 (у N — своё поколение); после остановки последней сессии runtime/1 кандидат — ровно [1]; runtime/2 кандидатом не бывает; живые сессии реестра на каждом шаге — ожидаемые; runtime/1 существует на каждом шаге, после остановки — те же inode и SHA-256 файлов, что до обновления",
};
pub const NO_OVERWRITE: PartSpec = PartSpec {
    name: "no-overwrite",
    stimulus: "quit N+1 и прямой запуск поколения 2; quit; к runtime/2/probe-update-attach дописаны байты; прямой запуск поколения 2",
    oracle: "перезапуск: runtime/2 «переиспользован» без прерванной подготовки, inode и SHA-256 файлов прежние, h2 в реестре; несовпадающий runtime/2: запуск выходит с кодом 1 и строкой error «… перезапись запрещена …», владельца нет (клиент пробы получает временный отказ), каталог и файлы runtime/2 — те же inode и SHA-256, что после правки, .prep-2 нет; h2 жива с прежними процессами",
};

fn violation(violations: &mut Vec<String>, ok: bool, what: impl FnOnce() -> String) {
    if !ok {
        violations.push(what());
    }
}

fn part(spec: &PartSpec, verdict: Verdict, artifacts: &[&str]) -> Cell {
    let mut all = vec![STEPS_ARTIFACT.to_owned(), RUNS_ARTIFACT.to_owned()];
    all.extend(artifacts.iter().map(|name| format!("{DIR}/{name}")));
    Cell {
        id: CELL.to_owned(),
        part: Some(spec.name.to_owned()),
        requirement: Requirement::Required,
        status: verdict.status,
        stimulus: spec.stimulus.to_owned(),
        oracle: spec.oracle.to_owned(),
        completion: COMPLETION.to_owned(),
        reason: verdict.reason,
        artifacts: all,
    }
}

// --- снимки ---

/// Файл каталога: inode, длина и SHA-256.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FileSnap {
    pub name: String,
    pub inode: Inode,
    pub len: u64,
    pub sha256: String,
}

/// Каталог: его inode (`None` — каталога нет) и файлы по именам.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Snapshot {
    pub dir: Option<Inode>,
    pub files: Vec<FileSnap>,
}

impl Snapshot {
    pub fn names(&self) -> Vec<&str> {
        self.files.iter().map(|file| file.name.as_str()).collect()
    }
}

fn io_err(path: &Path, err: io::Error) -> String {
    format!("{}: {err}", path.display())
}

fn snapshot(dir: &Path, runs: &mut Vec<Run>) -> Result<Snapshot, String> {
    match fs::symlink_metadata(dir) {
        Ok(_) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Ok(Snapshot {
                dir: None,
                files: Vec::new(),
            });
        }
        Err(err) => return Err(io_err(dir, err)),
    }
    let mut names: Vec<String> = fs::read_dir(dir)
        .map_err(|err| io_err(dir, err))?
        .map(|entry| {
            entry
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .map_err(|err| io_err(dir, err))
        })
        .collect::<Result<_, _>>()?;
    names.sort();
    let paths: Vec<PathBuf> = names.iter().map(|name| dir.join(name)).collect();
    let digests = if paths.is_empty() {
        Vec::new()
    } else {
        generation::sha256(&paths, runs)?
    };
    let mut files = Vec::new();
    for ((name, path), sha256) in names.into_iter().zip(&paths).zip(digests) {
        let meta = fs::symlink_metadata(path).map_err(|err| io_err(path, err))?;
        files.push(FileSnap {
            name,
            inode: Inode {
                dev: meta.dev(),
                ino: meta.ino(),
            },
            len: meta.len(),
            sha256,
        });
    }
    Ok(Snapshot {
        dir: Some(Inode::of(dir).map_err(|err| io_err(dir, err))?),
        files,
    })
}

/// Нарушение «каталог не тронут»: тот же inode каталога, те же имена, inode и SHA-256 файлов.
pub fn unchanged(what: &str, before: &Snapshot, after: &Snapshot) -> Option<String> {
    if before.dir.is_none() {
        return Some(format!("{what}: каталога не было до"));
    }
    (before != after).then(|| format!("{what}: было {before:?}, стало {after:?}"))
}

/// Копия части рантайма против бандла: байты и подпись требованием своей части.
#[derive(Clone, Debug, Serialize)]
pub struct PartCheck {
    pub part: Part,
    pub same_bytes: bool,
    pub verify: Verify,
}

fn against_bundle(
    dir: &Path,
    bundle: &Path,
    leaf: &str,
    runs: &mut Vec<Run>,
) -> Result<Vec<PartCheck>, String> {
    let mut checks = Vec::new();
    for part in Part::RUNTIME {
        let copy = dir.join(part.file_name());
        let source = part.path(bundle);
        let same_bytes = fs::read(&copy).map_err(|err| io_err(&copy, err))?
            == fs::read(&source).map_err(|err| io_err(&source, err))?;
        let requirement = generation::requirement(&part.identifier(BUNDLE_ID), leaf);
        let verify = generation::verify(&copy, &requirement, runs)?;
        checks.push(PartCheck {
            part,
            same_bytes,
            verify,
        });
    }
    Ok(checks)
}

fn path_of(pid: i32) -> Option<PathBuf> {
    sys::process_path(pid).ok().map(|path| canonical(&path))
}

fn alive(key: ProcessKey) -> bool {
    observer::identify(key).is_ok_and(|found| found.is_alive())
}

// --- факты частей ---

/// Прерванная подготовка `runtime/1/`.
#[derive(Clone, Debug, Serialize)]
pub struct Interrupted {
    pub started: i32,
    pub reached: Reached,
    pub prep_at_kill: Snapshot,
    pub ready_at_kill: bool,
    pub socket_at_kill: bool,
    pub signal: Option<i32>,
    pub prep_after_kill: Snapshot,
    pub ready_after_kill: bool,
    pub owner: OwnerInfo,
    pub prep_after_start: bool,
    pub runtime1: Snapshot,
    pub against_bundle: Vec<PartCheck>,
}

/// Сессия от хоста 1.
#[derive(Clone, Debug, Serialize)]
pub struct OldSession {
    pub record: SessionRecord,
    pub first: Transcript,
    pub host_before: Option<PathBuf>,
    pub workload_before: Option<PathBuf>,
    pub host_after: Option<PathBuf>,
    pub workload_after: Option<PathBuf>,
    pub same_after: bool,
    pub health: Option<SessionFacts>,
    pub probe: Option<Probe>,
}

/// Отдельный процесс attach.
#[derive(Clone, Debug, Serialize)]
pub struct CliRun {
    pub binary: PathBuf,
    pub same_as_bundle: bool,
    pub args: Vec<String>,
    pub code: Option<i32>,
    pub report: Option<CliReport>,
    pub stderr: String,
}

impl CliRun {
    /// Welcome версии `version` от хоста поколения `generation`, хвост с первой строки без
    /// пропуска, квитанция `ack <seq> ping` и ответ хоста.
    fn violations(&self, what: &str, version: u16, generation: u32, seq: u64) -> Vec<String> {
        let mut v = Vec::new();
        violation(&mut v, self.same_as_bundle, || {
            format!(
                "{what}: {} не совпадает с attach установленного бандла",
                self.binary.display()
            )
        });
        let Some(report) = &self.report else {
            v.push(format!(
                "{what}: attach вышел с кодом {:?}: {}",
                self.code, self.stderr
            ));
            return v;
        };
        violation(
            &mut v,
            report.outcome == "welcome"
                && report.welcome.as_ref().is_some_and(|welcome| {
                    welcome.version == version && welcome.generation == generation
                }),
            || {
                format!(
                    "{what}: {} {:?} {:?}",
                    report.outcome, report.welcome, report.refused
                )
            },
        );
        match &report.transcript {
            None => v.push(format!("{what}: нет стенограммы")),
            Some(transcript) => {
                violation(
                    &mut v,
                    transcript.problems.is_empty()
                        && transcript
                            .lines
                            .first()
                            .is_some_and(|line| line.number == 1),
                    || {
                        format!(
                            "{what}: хвост {:?}, первая строка {:?}",
                            transcript.problems,
                            transcript.lines.first()
                        )
                    },
                );
                violation(&mut v, transcript.receipt(seq, "ping").is_some(), || {
                    format!("{what}: нет квитанции ack {seq} ping")
                });
            }
        }
        violation(
            &mut v,
            report
                .acks
                .iter()
                .any(|(got, ack)| *got == seq && *ack == crate::proto::Ack::Accepted),
            || format!("{what}: ответ хоста на ввод {seq}: {:?}", report.acks),
        );
        v
    }
}

/// Новая сессия у N+1.
#[derive(Clone, Debug, Serialize)]
pub struct NewSession {
    pub record: SessionRecord,
    pub host: Option<PathBuf>,
    pub workload: Option<PathBuf>,
    pub manifest: Manifest,
    /// Строка `ready` нагрузки до первого ввода: ввод раньше неё PTY вернул бы эхом.
    pub ready: Transcript,
    pub cli: CliRun,
}

/// Шаг сверки кандидатов на уборку.
#[derive(Clone, Debug, Serialize)]
pub struct CleanupStep {
    pub label: &'static str,
    pub alive: Vec<String>,
    pub expected_alive: Vec<String>,
    pub cleanup: Vec<u32>,
    pub expected: Vec<u32>,
    pub runtime1: bool,
    pub entries: Vec<Entry>,
    pub problems: Vec<String>,
}

/// Переиспользование и отказ готового `runtime/2/`.
#[derive(Clone, Debug, Serialize)]
pub struct Overwrite {
    pub before: Snapshot,
    pub reuse: OwnerInfo,
    pub after_reuse: Snapshot,
    pub h2_in_registry: bool,
    pub tampered: Snapshot,
    pub refused_code: Option<i32>,
    pub refused_reports: Vec<AppReport>,
    pub no_owner: Option<String>,
    pub after_refusal: Snapshot,
    pub prep2: bool,
    pub h2_same: bool,
}

// --- ход ---

pub fn run(out: &Path, environment: &Environment) -> Result<Report, String> {
    let (state, nonce, layout) = super::stand_inputs(environment)?;
    let dir = out.join(DIR);
    fs::create_dir(&dir).map_err(|err| io_err(&dir, err))?;
    let mut guard = Guard {
        state: state.clone(),
        apps: Vec::new(),
        sessions: Vec::new(),
        nonce: Some(nonce.clone()),
        armed: true,
    };
    let mut steps = Vec::new();
    let mut runs = Vec::new();
    let walked = walk(
        &state, &nonce, &layout, &dir, &mut steps, &mut runs, &mut guard,
    );
    drop(guard);
    if let Err(reason) = &walked {
        steps.push(json!({ "name": "error", "reason": reason }));
    }
    let written = crate::results::write_json(&dir, "steps.json", &steps)
        .and_then(|_| crate::results::write_json(&dir, "runs.json", &runs));
    let cells = walked?;
    written?;
    Ok(Report {
        cells,
        diagnostics: Diagnostics {
            notes: vec![format!(
                "срок шага стенда {} с; барьер {}; к runtime/2/probe-update-attach дописано {} байт",
                STEP.as_secs(),
                PARTIAL.key(),
                TAMPER.len()
            )],
        },
    })
}

fn walk(
    state: &Path,
    nonce: &str,
    layout: &Layout,
    out: &Path,
    steps: &mut Vec<Value>,
    runs: &mut Vec<Run>,
    guard: &mut Guard,
) -> Result<Vec<Cell>, String> {
    let stand = Stand::new(state, nonce, layout, "gen-1")?;
    let gen2 = layout.get("gen-2")?.clone();
    let runtime1 = runtime::ready_dir(state, 1);
    let runtime2 = runtime::ready_dir(state, 2);

    // 1. Прерванная подготовка runtime/1/.
    let (interrupted, gui_index) = interrupt(&stand, guard, out, &layout.leaf, runs)?;
    steps.push(json!({ "name": "interrupted", "facts": interrupted }));

    // 2. Две сессии от хоста 1.
    let mut old = Vec::new();
    for name in ["h1", "h1b"] {
        let record = stand.create_session(guard, name)?;
        let first = first_ping(&record)?;
        old.push(OldSession {
            host_before: path_of(record.host.pid),
            workload_before: path_of(record.workload.pid),
            record,
            first,
            host_after: None,
            workload_after: None,
            same_after: false,
            health: None,
            probe: None,
        });
    }
    let r1_before = snapshot(&runtime1, runs)?;
    let mut cleanup = vec![cleanup_step(
        "N: h1 и h1b живы",
        state,
        &["h1", "h1b"],
        &[],
    )?];

    // 3. Обновление поколением 2.
    let attempt = match ask(
        state,
        &Request::Update {
            candidate: gen2.app.display().to_string(),
        },
    )? {
        Reply::Staging(staging) => match *staging {
            Staging::Staged { attempt, .. } => attempt,
            other => return Err(format!("стенд: update gen-2: {other:?}")),
        },
        other => return Err(format!("стенд: update gen-2: {other:?}")),
    };
    let activating = update_flow::activate(state)?;
    if activating.0 != attempt {
        return Err(format!(
            "стенд: activate попытки {}, поставлена {attempt}",
            activating.0
        ));
    }
    let followed =
        update_flow::follow(&stand, guard, gui_index, activating, false, out, "owner-n")?;
    steps.push(json!({ "name": "follow", "facts": followed.facts() }));
    let next = followed.next.clone();

    // 4. Прежние сессии при N+1.
    let health = next.update.as_ref().map(|confirmed| &confirmed.health);
    for session in &mut old {
        session.same_after = update_flow::same_processes(&session.record)?;
        session.host_after = path_of(session.record.host.pid);
        session.workload_after = path_of(session.record.workload.pid);
        session.health = health.and_then(|facts| {
            facts
                .sessions
                .iter()
                .find(|found| found.expected.session == session.record.session)
                .cloned()
        });
    }
    let r1_after_update = snapshot(&runtime1, runs)?;
    let with_old = cleanup_step("N+1: h1 и h1b живы", state, &["h1", "h1b"], &[])?;
    for session in &mut old {
        session.probe = with_old
            .entries
            .iter()
            .find(|entry| entry.record.session == session.record.session)
            .map(|entry| entry.probe.clone());
    }
    cleanup.push(with_old);
    let attach2 = runtime::binary(state, 2, Part::Attach);
    let cli_h1 = attach_cli(
        &attach2,
        &stand.installed,
        &old[0].record,
        nonce,
        2,
        out,
        "attach-h1",
    )?;
    steps.push(json!({ "name": "old-sessions", "sessions": old, "runtime1": r1_after_update, "attach_h1": cli_h1 }));

    // 5. Новая сессия у N+1.
    let record = stand.create_session(guard, "h2")?;
    let ready = wait_ready(&record)?;
    let h2 = NewSession {
        host: path_of(record.host.pid),
        workload: path_of(record.workload.pid),
        manifest: Manifest::read(&gen2.app)?,
        ready,
        cli: attach_cli(
            &attach2,
            &stand.installed,
            &record,
            nonce,
            1,
            out,
            "attach-h2",
        )?,
        record,
    };
    steps.push(json!({ "name": "new-session", "facts": h2 }));

    // 6. Кандидаты на уборку по живым хостам.
    cleanup.push(cleanup_step(
        "N+1: h1, h1b и h2 живы",
        state,
        &["h1", "h1b", "h2"],
        &[],
    )?);
    update_flow::stop(&old[1].record)?;
    cleanup.push(cleanup_step(
        "N+1: h1b остановлена",
        state,
        &["h1", "h2"],
        &[],
    )?);
    update_flow::stop(&old[0].record)?;
    cleanup.push(cleanup_step(
        "N+1: h1 и h1b остановлены",
        state,
        &["h2"],
        &[1],
    )?);
    let r1_after_stop = snapshot(&runtime1, runs)?;
    guard.sessions.retain(|kept| kept.session == "h2");
    steps.push(json!({ "name": "cleanup", "steps": cleanup, "runtime1": r1_after_stop }));

    // 7. Готовый runtime/2/: переиспользование и отказ без перезаписи.
    let overwrite = overwrite(&stand, guard, out, next.key, &h2.record, runs)?;
    steps.push(json!({ "name": "overwrite", "facts": overwrite }));

    let cells = vec![
        part(
            &INTERRUPTED,
            interrupted_verdict(&interrupted, state),
            &[
                "owner-killed.stdout",
                "owner-killed.stderr",
                "owner-n.stdout",
            ],
        ),
        part(
            &OLD_HOST,
            old_host_verdict(&followed, &old, &runtime1, &r1_before, &r1_after_update),
            &["owner-n.stdout", &format!("helper-{}", followed.attempt)],
        ),
        part(
            &ATTACH_V1,
            attach_verdict(&next, &old, &cli_h1),
            &["attach-h1.stdout", &format!("helper-{}", followed.attempt)],
        ),
        part(
            &NEW_SESSION,
            new_session_verdict(&next, &runtime2, &h2),
            &["attach-h2.stdout"],
        ),
        part(
            &CLEANUP,
            cleanup_verdict(&cleanup, &r1_before, &r1_after_stop),
            &[],
        ),
        part(
            &NO_OVERWRITE,
            overwrite_verdict(&overwrite, &runtime2),
            &[
                "owner-reuse.stdout",
                "owner-differs.stdout",
                "owner-differs.stderr",
            ],
        ),
    ];

    update_flow::stop(&h2.record)?;
    guard.sessions.clear();
    for app in &mut guard.apps {
        if app.try_wait().map_err(|err| err.to_string())?.is_none() {
            return Err(format!("стенд: GUI {} не вышел", app.id()));
        }
    }
    guard.apps.clear();
    clear_state(state);
    guard.armed = false;
    steps.push(json!({ "name": "finish" }));
    Ok(cells)
}

/// GUI N, убитый посреди подготовки `runtime/1/`, и новый прямой запуск. Возвращает факты и
/// номер нового GUI N в стенде.
fn interrupt(
    stand: &Stand,
    guard: &mut Guard,
    out: &Path,
    leaf: &str,
    runs: &mut Vec<Run>,
) -> Result<(Interrupted, usize), String> {
    let state = &stand.state;
    let prep = runtime::prep_dir(state, 1);
    let ready = runtime::ready_dir(state, 1);
    barrier::arm(state, PARTIAL)?;
    guard.apps.push(start_app(
        &stand.app_bin,
        &stand.options,
        out,
        "owner-killed",
    )?);
    let index = guard.apps.len() - 1;
    let started = guard.apps[index].id() as i32;
    let reached = barrier::wait_reached(state, PARTIAL, Instant::now() + STEP)?;
    if reached.process.pid != started {
        return Err(format!(
            "стенд: на барьере {} стоял {}, запущен {started}",
            PARTIAL.key(),
            reached.process.pid
        ));
    }
    let prep_at_kill = snapshot(&prep, runs)?;
    let ready_at_kill = ready.exists();
    let socket_at_kill = owner::socket_path(state).exists();
    if alive(reached.process) {
        // SAFETY: сигнал процессу, узнанному по PID и времени старта на барьере.
        unsafe { libc::kill(reached.process.pid, libc::SIGKILL) };
    }
    let status = reap(&mut guard.apps[index], "GUI N на барьере подготовки")?;
    barrier::release(state, PARTIAL)?;
    let _ = fs::remove_file(PARTIAL.reached_path(state));
    let prep_after_kill = snapshot(&prep, runs)?;
    let ready_after_kill = ready.exists();
    let (gui_index, owner) = stand.start_owner(guard, out, "owner-n")?;
    let runtime1 = snapshot(&ready, runs)?;
    let against_bundle = against_bundle(&ready, &stand.installed, leaf, runs)?;
    Ok((
        Interrupted {
            started,
            reached,
            prep_at_kill,
            ready_at_kill,
            socket_at_kill,
            signal: status.signal(),
            prep_after_kill,
            ready_after_kill,
            owner,
            prep_after_start: prep.exists(),
            runtime1,
            against_bundle,
        },
        gui_index,
    ))
}

/// Attach из процесса сценария с байта 0 и ввод `1 ping`.
fn first_ping(record: &SessionRecord) -> Result<Transcript, String> {
    let hello = attach::hello(Purpose::Attach, ATTACH_VERSIONS, 0, Winsize::cells(0, 0));
    let mut first = match attach::connect(Path::new(&record.socket), &hello, STEP)? {
        Connected::Welcome(client) => Attached::new(client, Some(1)),
        Connected::Refused(reason) => {
            return Err(format!("стенд: attach {}: {reason}", record.session));
        }
    };
    let deadline = Instant::now() + STEP;
    first.pump_until(deadline, |transcript| !transcript.lines.is_empty())?;
    first.exchange(1, "ping", deadline)?;
    first.detach(Instant::now() + STEP)?;
    Ok(first.transcript)
}

/// Attach из процесса сценария до строки `ready` нагрузки, без ввода.
fn wait_ready(record: &SessionRecord) -> Result<Transcript, String> {
    let hello = attach::hello(Purpose::Attach, ATTACH_VERSIONS, 0, Winsize::cells(0, 0));
    let mut attached = match attach::connect(Path::new(&record.socket), &hello, STEP)? {
        Connected::Welcome(client) => Attached::new(client, Some(1)),
        Connected::Refused(reason) => {
            return Err(format!("стенд: attach {}: {reason}", record.session));
        }
    };
    attached.pump_until(Instant::now() + STEP, |transcript| {
        !transcript.lines.is_empty()
    })?;
    attached.detach(Instant::now() + STEP)?;
    Ok(attached.transcript)
}

/// Отдельный процесс `probe-update-attach` из `runtime/2/`: attach с байта 0 и ввод `<seq> ping`.
fn attach_cli(
    binary: &Path,
    installed: &Path,
    record: &SessionRecord,
    nonce: &str,
    seq: u64,
    out: &Path,
    name: &str,
) -> Result<CliRun, String> {
    let bundle_attach = Part::Attach.path(installed);
    let same_as_bundle = fs::read(binary).map_err(|err| io_err(binary, err))?
        == fs::read(&bundle_attach).map_err(|err| io_err(&bundle_attach, err))?;
    let args: Vec<String> = [
        "attach",
        "--socket",
        &record.socket,
        "--tag",
        nonce,
        "--from",
        "0",
        "--expect",
        "1",
        "--seq",
        &seq.to_string(),
        "--input",
        "ping",
    ]
    .iter()
    .map(|arg| (*arg).to_owned())
    .collect();
    let output = Command::new(binary)
        .args(&args)
        .stdin(Stdio::null())
        .output()
        .map_err(|err| format!("запуск {}: {err}", binary.display()))?;
    for (suffix, bytes) in [("stdout", &output.stdout), ("stderr", &output.stderr)] {
        let path = out.join(format!("{name}.{suffix}"));
        fs::write(&path, bytes).map_err(|err| io_err(&path, err))?;
    }
    let report = output
        .status
        .success()
        .then(|| serde_json::from_slice::<CliReport>(&output.stdout).ok())
        .flatten();
    Ok(CliRun {
        binary: binary.to_path_buf(),
        same_as_bundle,
        args,
        code: output.status.code(),
        report,
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    })
}

fn cleanup_step(
    label: &'static str,
    state: &Path,
    expected_alive: &[&str],
    expected: &[u32],
) -> Result<CleanupStep, String> {
    let Registry {
        entries,
        problems,
        cleanup,
    } = sessions(state)?;
    let mut alive: Vec<String> = entries
        .iter()
        .filter(|entry| entry.host_alive && entry.workload_alive)
        .map(|entry| entry.record.session.clone())
        .collect();
    alive.sort();
    Ok(CleanupStep {
        label,
        alive,
        expected_alive: expected_alive
            .iter()
            .map(|name| (*name).to_owned())
            .collect(),
        cleanup,
        expected: expected.to_vec(),
        runtime1: runtime::ready_dir(state, 1).is_dir(),
        entries,
        problems,
    })
}

/// `quit` владельца, который не потомок сценария, и ожидание его выхода.
fn quit_owner(state: &Path, key: ProcessKey) -> Result<(), String> {
    let quit = ask(state, &Request::Quit)?;
    let left = launch::wait_exit(&[key.pid], STEP);
    if quit != Reply::Done || (!left.is_empty() && alive(key)) {
        return Err(format!("стенд: quit владельца {}: {quit:?}", key.pid));
    }
    Ok(())
}

fn overwrite(
    stand: &Stand,
    guard: &mut Guard,
    out: &Path,
    next: ProcessKey,
    h2: &SessionRecord,
    runs: &mut Vec<Run>,
) -> Result<Overwrite, String> {
    let state = &stand.state;
    let runtime2 = runtime::ready_dir(state, 2);
    let before = snapshot(&runtime2, runs)?;
    quit_owner(state, next)?;
    let (reuse_index, reuse) = stand.start_owner(guard, out, "owner-reuse")?;
    let after_reuse = snapshot(&runtime2, runs)?;
    let h2_in_registry = sessions(state)?.entries.iter().any(|entry| {
        entry.record.session == h2.session
            && entry.record.host == h2.host
            && entry.host_alive
            && entry.workload_alive
    });
    quit_owner(state, reuse.key)?;
    reap(&mut guard.apps[reuse_index], "владелец после quit")?;
    let attach = runtime::binary(state, 2, Part::Attach);
    OpenOptions::new()
        .append(true)
        .open(&attach)
        .and_then(|mut file| {
            file.write_all(TAMPER)?;
            file.sync_all()
        })
        .map_err(|err| io_err(&attach, err))?;
    let tampered = snapshot(&runtime2, runs)?;
    guard.apps.push(start_app(
        &stand.app_bin,
        &stand.options,
        out,
        "owner-differs",
    )?);
    let index = guard.apps.len() - 1;
    let status = reap(&mut guard.apps[index], "запуск при несовпадающем runtime/2")?;
    let refused_reports = reports(out, "owner-differs")?;
    let no_owner = match owner::request(state, &Request::Ping { wait_ms: 0 }, STEP)? {
        Answer::Unavailable(why) => Some(why.reason().to_owned()),
        Answer::Reply(_) => None,
    };
    Ok(Overwrite {
        before,
        reuse,
        after_reuse,
        h2_in_registry,
        tampered,
        refused_code: status.code(),
        refused_reports,
        no_owner,
        after_refusal: snapshot(&runtime2, runs)?,
        prep2: runtime::prep_dir(state, 2).exists(),
        h2_same: update_flow::same_processes(h2)?,
    })
}

// --- оракулы ---

pub fn interrupted_verdict(facts: &Interrupted, state: &Path) -> Verdict {
    let mut v = Vec::new();
    let host = Part::Host.file_name();
    violation(&mut v, facts.reached.process.pid == facts.started, || {
        format!("на барьере {:?}", facts.reached.process)
    });
    violation(
        &mut v,
        facts.prep_at_kill.dir.is_some() && facts.prep_at_kill.names() == [host],
        || format!("каталог подготовки на барьере: {:?}", facts.prep_at_kill),
    );
    violation(
        &mut v,
        !facts.ready_at_kill && !facts.socket_at_kill,
        || {
            format!(
                "на барьере runtime/1 есть: {}, сокет есть: {}",
                facts.ready_at_kill, facts.socket_at_kill
            )
        },
    );
    violation(&mut v, facts.signal == Some(libc::SIGKILL), || {
        format!("GUI N завершён сигналом {:?}", facts.signal)
    });
    violation(
        &mut v,
        facts.prep_after_kill == facts.prep_at_kill && !facts.ready_after_kill,
        || {
            format!(
                "после убийства: подготовка {:?}, runtime/1 есть: {}",
                facts.prep_after_kill, facts.ready_after_kill
            )
        },
    );
    let prep = runtime::prep_dir(state, 1);
    violation(
        &mut v,
        facts.owner.runtime.dir == runtime::ready_dir(state, 1)
            && facts.owner.runtime.outcome == Prepared::Created
            && facts.owner.runtime.recovered.as_deref() == Some(prep.as_path()),
        || format!("рантайм нового владельца: {:?}", facts.owner.runtime),
    );
    violation(&mut v, !facts.prep_after_start, || {
        format!("{} остался после запуска", prep.display())
    });
    let mut expected: Vec<&str> = Part::RUNTIME.iter().map(|part| part.file_name()).collect();
    expected.sort_unstable();
    violation(&mut v, facts.runtime1.names() == expected, || {
        format!("runtime/1: {:?}", facts.runtime1.names())
    });
    violation(
        &mut v,
        facts
            .against_bundle
            .iter()
            .all(|check| check.same_bytes && check.verify == Verify::Satisfied)
            && facts.against_bundle.len() == Part::RUNTIME.len(),
        || format!("runtime/1 против бандла: {:?}", facts.against_bundle),
    );
    Verdict::of(
        v,
        format!(
            "GUI N {} убит на {} с .prep-1 [{host}]; владелец {} удалил .prep-1 и создал runtime/1: три копии байт в байт и codesign своих частей",
            facts.started,
            PARTIAL.key(),
            facts.owner.key.pid
        ),
    )
}

fn session_paths_ok(session: &OldSession, runtime1: &Path) -> bool {
    let host = canonical(&runtime1.join(Part::Host.file_name()));
    let fixture = canonical(&runtime1.join(Part::Fixture.file_name()));
    session.host_before.as_deref() == Some(host.as_path())
        && session.host_after.as_deref() == Some(host.as_path())
        && session.workload_before.as_deref() == Some(fixture.as_path())
        && session.workload_after.as_deref() == Some(fixture.as_path())
}

pub fn old_host_verdict(
    followed: &Followed,
    old: &[OldSession],
    runtime1: &Path,
    before: &Snapshot,
    after: &Snapshot,
) -> Verdict {
    let mut v = Vec::new();
    match &followed.journal {
        Some(journal)
            if journal.attempt == followed.attempt
                && journal.state == State::Idle
                && journal.outcome == Some(Outcome::Committed) => {}
        other => v.push(format!("итог журнала: {other:?}")),
    }
    violation(
        &mut v,
        followed.next.ready && followed.next.generation == 2,
        || {
            format!(
                "N+1: готов {}, поколение {}",
                followed.next.ready, followed.next.generation
            )
        },
    );
    violation(&mut v, old.len() == 2, || format!("сессий {}", old.len()));
    for session in old {
        let name = &session.record.session;
        violation(
            &mut v,
            session.same_after && session.record.generation == 1,
            || {
                format!(
                    "{name}: процессы прежние {}, поколение хоста {}",
                    session.same_after, session.record.generation
                )
            },
        );
        violation(&mut v, session_paths_ok(session, runtime1), || {
            format!(
                "{name}: хост {:?} → {:?}, нагрузка {:?} → {:?}",
                session.host_before,
                session.host_after,
                session.workload_before,
                session.workload_after
            )
        });
    }
    if let Some(problem) = unchanged("runtime/1 после обновления", before, after) {
        v.push(problem);
    }
    Verdict::of(
        v,
        format!(
            "попытка {} подтверждена, N+1 {}; h1 и h1b с прежними процессами из runtime/1, runtime/1 тот же (inode каталога {:?})",
            followed.attempt,
            followed.next.key.pid,
            before.dir.as_ref().map(|inode| inode.ino)
        ),
    )
}

pub fn attach_verdict(next: &OwnerInfo, old: &[OldSession], cli: &CliRun) -> Verdict {
    let mut v = Vec::new();
    match &next.update {
        None => v.push("у N+1 нет отчёта подтверждения".to_owned()),
        Some(confirmed) => {
            let judged = crate::health::judge(&confirmed.health);
            violation(&mut v, judged.is_empty(), || {
                format!("правило health: {judged:?}")
            });
        }
    }
    for session in old {
        let name = &session.record.session;
        violation(
            &mut v,
            session.health.as_ref().is_some_and(|facts| {
                facts.found
                    && facts.same_processes
                    && facts.version == Some(1)
                    && facts.lines > 0
                    && facts.gaps.is_empty()
                    && facts.receipt
            }),
            || format!("{name} в health N+1: {:?}", session.health),
        );
        violation(
            &mut v,
            session.probe == Some(Probe::Welcome { version: 1 }),
            || format!("{name} в sessions N+1: {:?}", session.probe),
        );
    }
    v.extend(cli.violations("attach h1 из runtime/2", 1, 1, 2));
    let lines = cli
        .report
        .as_ref()
        .and_then(|report| report.transcript.as_ref())
        .map_or(0, |transcript| transcript.lines.len());
    Verdict::of(
        v,
        format!(
            "health N+1 {}: h1 и h1b — версия 1, хвост без пропуска, квитанция; probe у N+1 — версия 1; attach из runtime/2 — версия 1, {lines} строк без пропуска, ack 2 ping",
            next.key.pid
        ),
    )
}

pub fn new_session_verdict(next: &OwnerInfo, runtime2: &Path, h2: &NewSession) -> Verdict {
    let mut v = Vec::new();
    violation(
        &mut v,
        next.runtime.dir == runtime2
            && next.runtime.outcome == Prepared::Created
            && next.runtime.recovered.is_none(),
        || format!("рантайм N+1: {:?}", next.runtime),
    );
    let host = canonical(&runtime2.join(Part::Host.file_name()));
    let fixture = canonical(&runtime2.join(Part::Fixture.file_name()));
    let record = &h2.record;
    violation(
        &mut v,
        record.generation == 2
            && h2.manifest.generation == 2
            && record.versions == h2.manifest.host_versions
            && canonical(Path::new(&record.executable)) == host,
        || {
            format!(
                "запись h2: поколение {}, протокол {:?} (манифест {:?}), файл {}",
                record.generation, record.versions, h2.manifest.host_versions, record.executable
            )
        },
    );
    violation(
        &mut v,
        h2.host.as_deref() == Some(host.as_path())
            && h2.workload.as_deref() == Some(fixture.as_path()),
        || format!("h2: хост {:?}, нагрузка {:?}", h2.host, h2.workload),
    );
    v.extend(h2.cli.violations("attach h2 из runtime/2", 2, 2, 1));
    Verdict::of(
        v,
        format!(
            "N+1 создал runtime/2; h2 — хост {} и нагрузка из runtime/2, протокол {:?}, attach версии 2 с ack 1 ping",
            record.host.pid, record.versions
        ),
    )
}

pub fn cleanup_verdict(steps: &[CleanupStep], before: &Snapshot, after: &Snapshot) -> Verdict {
    let mut v = Vec::new();
    for step in steps {
        violation(&mut v, step.alive == step.expected_alive, || {
            format!(
                "{}: живые {:?}, ждали {:?}",
                step.label, step.alive, step.expected_alive
            )
        });
        violation(&mut v, step.cleanup == step.expected, || {
            format!(
                "{}: кандидаты {:?}, ждали {:?}",
                step.label, step.cleanup, step.expected
            )
        });
        violation(&mut v, step.runtime1, || {
            format!("{}: runtime/1 нет", step.label)
        });
        violation(&mut v, step.problems.is_empty(), || {
            format!("{}: реестр {:?}", step.label, step.problems)
        });
    }
    violation(
        &mut v,
        steps.last().is_some_and(|step| step.expected == [1]),
        || "нет шага после остановки последней сессии runtime/1".to_owned(),
    );
    if let Some(problem) = unchanged("runtime/1 после остановки сессий", before, after)
    {
        v.push(problem);
    }
    let trail: Vec<String> = steps
        .iter()
        .map(|step| format!("{} → {:?}", step.label, step.cleanup))
        .collect();
    Verdict::of(v, trail.join("; "))
}

pub fn overwrite_verdict(facts: &Overwrite, runtime2: &Path) -> Verdict {
    let mut v = Vec::new();
    violation(
        &mut v,
        facts.reuse.runtime.dir == runtime2
            && facts.reuse.runtime.outcome == Prepared::Reused
            && facts.reuse.runtime.recovered.is_none()
            && facts.reuse.generation == 2,
        || format!("перезапуск: {:?}", facts.reuse.runtime),
    );
    if let Some(problem) = unchanged(
        "runtime/2 при перезапуске",
        &facts.before,
        &facts.after_reuse,
    ) {
        v.push(problem);
    }
    violation(&mut v, facts.h2_in_registry, || {
        "перезапуск не нашёл h2".to_owned()
    });
    let attach = Part::Attach.file_name();
    let changed = |snapshot: &Snapshot| {
        snapshot
            .files
            .iter()
            .find(|file| file.name == attach)
            .map(|file| file.sha256.clone())
    };
    violation(
        &mut v,
        changed(&facts.tampered).is_some() && changed(&facts.tampered) != changed(&facts.before),
        || format!("правка {attach} не изменила файл: {:?}", facts.tampered),
    );
    let refused = facts.refused_reports.iter().any(|report| {
        matches!(report, AppReport::Error { reason, .. } if reason.contains(NO_OVERWRITE_REASON))
    });
    violation(
        &mut v,
        facts.refused_code == Some(i32::from(owner::EXIT_ERROR))
            && refused
            && facts.refused_reports.len() == 1,
        || {
            format!(
                "запуск при несовпадающем runtime/2: код {:?}, строки {:?}",
                facts.refused_code, facts.refused_reports
            )
        },
    );
    violation(&mut v, facts.no_owner.is_some(), || {
        "после отказа владелец отвечает".to_owned()
    });
    if let Some(problem) = unchanged(
        "runtime/2 после отказа",
        &facts.tampered,
        &facts.after_refusal,
    ) {
        v.push(problem);
    }
    violation(&mut v, !facts.prep2, || {
        "после отказа остался .prep-2".to_owned()
    });
    violation(&mut v, facts.h2_same, || {
        "h2: процессы сменились".to_owned()
    });
    Verdict::of(
        v,
        format!(
            "перезапуск {} переиспользовал runtime/2 с теми же inode; после правки {attach} запуск вышел с кодом {:?} («{NO_OVERWRITE_REASON}»), владельца нет ({}), runtime/2 не тронут, h2 жива",
            facts.reuse.key.pid,
            facts.refused_code,
            facts.no_owner.as_deref().unwrap_or("-")
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, ino: u64, sha: &str) -> FileSnap {
        FileSnap {
            name: name.to_owned(),
            inode: Inode { dev: 1, ino },
            len: 10,
            sha256: sha.to_owned(),
        }
    }

    fn snap(files: Vec<FileSnap>) -> Snapshot {
        Snapshot {
            dir: Some(Inode { dev: 1, ino: 100 }),
            files,
        }
    }

    #[test]
    fn unchanged_catches_inode_digest_and_name_changes() {
        let before = snap(vec![file("a", 1, "x"), file("b", 2, "y")]);
        assert_eq!(unchanged("r", &before, &before.clone()), None);
        for after in [
            snap(vec![file("a", 3, "x"), file("b", 2, "y")]),
            snap(vec![file("a", 1, "z"), file("b", 2, "y")]),
            snap(vec![file("a", 1, "x")]),
            Snapshot {
                dir: Some(Inode { dev: 1, ino: 101 }),
                files: before.files.clone(),
            },
            Snapshot {
                dir: None,
                files: Vec::new(),
            },
        ] {
            assert!(unchanged("r", &before, &after).is_some(), "{after:?}");
        }
        let absent = Snapshot {
            dir: None,
            files: Vec::new(),
        };
        assert!(
            unchanged("r", &absent, &absent).is_some(),
            "каталога не было"
        );
    }

    fn step(label: &'static str, alive: &[&str], cleanup: &[u32], expected: &[u32]) -> CleanupStep {
        let names: Vec<String> = alive.iter().map(|name| (*name).to_owned()).collect();
        CleanupStep {
            label,
            alive: names.clone(),
            expected_alive: names,
            cleanup: cleanup.to_vec(),
            expected: expected.to_vec(),
            runtime1: true,
            entries: Vec::new(),
            problems: Vec::new(),
        }
    }

    #[test]
    fn cleanup_candidate_needs_the_last_session_and_no_early_candidate() {
        let r1 = snap(vec![file("probe-update-host", 1, "x")]);
        let good = [
            step("с живыми", &["h1", "h2"], &[], &[]),
            step("после последней", &["h2"], &[1], &[1]),
        ];
        assert_eq!(
            cleanup_verdict(&good, &r1, &r1).status,
            crate::results::Status::Pass
        );
        // Кандидат при живом хосте прежнего рантайма — fail.
        let early = [
            step("с живыми", &["h1"], &[1], &[]),
            step("после последней", &[], &[1], &[1]),
        ];
        assert_eq!(
            cleanup_verdict(&early, &r1, &r1).status,
            crate::results::Status::Fail
        );
        // Без шага «после последней сессии» вывода нет.
        assert_eq!(
            cleanup_verdict(&good[..1], &r1, &r1).status,
            crate::results::Status::Fail
        );
        // Удалённый или подменённый runtime/1 — fail.
        let gone = Snapshot {
            dir: None,
            files: Vec::new(),
        };
        assert_eq!(
            cleanup_verdict(&good, &r1, &gone).status,
            crate::results::Status::Fail
        );
        let mut missing = good.clone();
        missing[1].runtime1 = false;
        assert_eq!(
            cleanup_verdict(&missing, &r1, &r1).status,
            crate::results::Status::Fail
        );
    }
}
