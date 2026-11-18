//! Сценарий `owner` (задача 10): клетка C1 «Владелец и второй запуск». Пять проходов, по одному
//! на строку каталога [`barrier::C1`], каждый — на свежем стенде: установлен N (копия поколения
//! 1 в `<state>/install-<проход>/`), GUI N запущен через LaunchServices (`open -n`), живая сессия
//! `h1` от хоста 1, кандидат — поколение 2. Стенд останавливает процесс протокола на барьере
//! строки, запускает три стимула — обычный `open`, `open -n` и прямой запуск бинаря
//! установленного бандла — и ждёт отметок, что каждый запущенный процесс дошёл до протокола на
//! этой границе. Затем барьеры снимаются, проход доходит до готового владельца. Проход «после
//! смерти владельца» барьеров не ставит: обновление подтверждается, N+1 получает `SIGKILL`, стимулы
//! идут на оставшийся путь сокета.
//!
//! Части каждого прохода:
//!
//! - `<проход>/<вид>` — исход стимула: квитанция единственного владельца с записью у него, явный
//!   временный отказ с повтором или владение; у обычного `open` без нового процесса — исход
//!   LaunchServices;
//! - `<проход>/single-owner` — владельцев не больше одного в каждой точке наблюдения, inode
//!   блокировки один, сокет публикует и убирает только владелец.
//!
//! Часть `descriptors` — у помощника, хостов и нагрузок нет дескрипторов блокировки и сокета
//! владельца. Невыполненный оракул — `fail` части, сломанный стенд — ошибка исполнения. Шаги и
//! факты — в `owner/steps.json`, отметки, файлы помощника и stdout процессов — в
//! `owner/<проход>/`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Value, json};

use crate::attach::{self, Attached, Connected};
use crate::barrier::{self, Barrier, Boundary, Note, Point, Role};
use crate::compat::Staging;
use crate::generation::{APP_BIN, Layout};
use crate::host::SessionRecord;
use crate::journal::{HistoryEntry, Journal, Outcome, State, Store};
use crate::launch;
use crate::observer::{self, ProcessKey};
use crate::owner::{self, Answer, AppOptions, AppReport, Inode, OwnerInfo, Reply, Request};
use crate::proto::{ATTACH_VERSIONS, Purpose};
use crate::pty::Winsize;
use crate::results::{Cell, Diagnostics, Environment, Report, Requirement};
use crate::sys::{self, Descriptor};
use crate::{spawn, update};

use super::Verdict;
use super::owner_basic::{Guard, ask, clear_state, owner_leaks, reports, start_app};
use super::update_flow::{self, STEP, Stand};

pub const DEADLINE: Duration = Duration::from_secs(300);
pub const DIR: &str = "owner";
pub const STEPS_ARTIFACT: &str = "owner/steps.json";
pub const CELL: &str = "C1";
const OPEN: &str = "/usr/bin/open";
/// Строка `open`, когда LaunchServices считает приложение уже запущенным и только активирует его.
pub const ALREADY_RUNNING: &str = "was already running";
const COMPLETION: &str = "отметка стимула, барьер, готовность владельца или выход процесса в срок шага (30 с); иначе ошибка исполнения";
const PING_TIMEOUT: Duration = Duration::from_secs(5);
const BEFORE: &str = "до стимулов";
const AT: &str = "на границе после стимулов";
const END: &str = "в конце";

/// Помощник записал `launched` и отпустил блокировку: у прохода `launched` стимул идёт после этого.
const HELPER_RELEASED: Barrier = Barrier::new(Role::Helper, Point::Released);

/// Проход: строка каталога C1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Pass {
    BeforeExit,
    HelperLock,
    Launched,
    Bound,
    OwnerDead,
}

impl Pass {
    pub const ALL: [Pass; 5] = [
        Pass::BeforeExit,
        Pass::HelperLock,
        Pass::Launched,
        Pass::Bound,
        Pass::OwnerDead,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Pass::BeforeExit => "before-exit",
            Pass::HelperLock => "helper-lock",
            Pass::Launched => "launched",
            Pass::Bound => "bound",
            Pass::OwnerDead => "owner-dead",
        }
    }

    /// Номер строки каталога C1, которую проходит проход.
    pub fn row_index(self) -> usize {
        match self {
            Pass::BeforeExit => 0,
            Pass::HelperLock => 1,
            Pass::Launched => 2,
            Pass::Bound => 3,
            Pass::OwnerDead => 4,
        }
    }

    pub fn row(self) -> Boundary {
        barrier::C1[self.row_index()]
    }

    /// Барьеры, которые ставит стенд: барьеры строки и у `launched` ещё `helper.released`.
    pub fn armed(self) -> Vec<Barrier> {
        let mut armed = self.row().barriers.to_vec();
        if self == Pass::Launched {
            armed.push(HELPER_RELEASED);
        }
        armed
    }
}

/// Вид запуска стимула.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    /// Обычный `open` установленного бандла.
    Open,
    /// `open -n`.
    OpenNew,
    /// Прямой запуск исполняемого файла установленного бандла.
    Direct,
}

impl Kind {
    /// Порядок запуска в проходе: обычный `open` первым, чтобы его не поглотил экземпляр, только
    /// что запущенный `open -n` стимула.
    pub const ORDER: [Kind; 3] = [Kind::Open, Kind::OpenNew, Kind::Direct];

    pub fn name(self) -> &'static str {
        match self {
            Kind::Open => "open",
            Kind::OpenNew => "open-n",
            Kind::Direct => "direct",
        }
    }
}

/// Расписание: проходы по порядку и виды запуска в каждом. Его и обходит сценарий.
pub fn schedule() -> Vec<(Pass, Vec<Kind>)> {
    Pass::ALL
        .into_iter()
        .map(|pass| (pass, Kind::ORDER.to_vec()))
        .collect()
}

/// Уникальная метка стимула в его аргументах: по ней стенд находит его отметки, а сторож и
/// уборка — по nonce внутри неё.
pub fn token(pass: Pass, kind: Kind, nonce: &str) -> String {
    format!("c1-{}-{}-{nonce}", pass.name(), kind.name())
}

/// Как LaunchServices обошёлся с `open`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Launch {
    /// Запущен новый процесс.
    Spawned,
    /// Приложение сочтено запущенным: только активация, процесса и аргументов нет.
    Activated,
    /// `open` вышел с ошибкой.
    LsError,
}

pub fn classify_open(exit: Option<i32>, stderr: &str) -> Launch {
    if exit != Some(0) {
        Launch::LsError
    } else if stderr.contains(ALREADY_RUNNING) {
        Launch::Activated
    } else {
        Launch::Spawned
    }
}

/// Экземпляр роли GUI с меткой запуска.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Instance {
    pub pid: i32,
    pub path: String,
    pub update_attempt: bool,
    pub forward: Vec<String>,
}

fn instances(nonce: &str) -> Vec<Instance> {
    let suffix = format!("/Contents/MacOS/{APP_BIN}");
    launch::find_by_token(nonce)
        .into_iter()
        .filter_map(|record| {
            let path = sys::process_path(record.pid).ok()?;
            if !path.to_string_lossy().ends_with(&suffix) {
                return None;
            }
            let args = launch::process_args(record.pid).unwrap_or_default();
            let forward = args
                .iter()
                .position(|arg| arg == "--")
                .map(|at| args[at + 1..].to_vec())
                .unwrap_or_default();
            Some(Instance {
                pid: record.pid,
                path: path.display().to_string(),
                update_attempt: args.iter().any(|arg| arg == update::ATTEMPT_FLAG),
                forward,
            })
        })
        .collect()
}

/// Стимул прохода и что о нём известно.
#[derive(Clone, Debug, Serialize)]
pub struct Stimulus {
    pub kind: Kind,
    pub token: String,
    /// Имя файлов stdout и stderr в каталоге прохода.
    pub name: String,
    pub open_exit: Option<i32>,
    pub open_stderr: String,
    pub launch: Launch,
    /// Экземпляры роли GUI перед запуском.
    pub instances_before: Vec<Instance>,
    pub launched_ns: u64,
    pub key: Option<ProcessKey>,
    pub landed: bool,
    pub reports: Vec<AppReport>,
    pub exited: bool,
    pub notes: Vec<Note>,
}

/// Наблюдение стенда в одной точке.
#[derive(Clone, Debug, Serialize)]
pub struct Seen {
    pub point: &'static str,
    pub uptime_ns: u64,
    /// Процессы с меткой запуска и сокетом с адресом `<state>/.sock.*`: PID и адреса.
    pub owners: Vec<(i32, Vec<String>)>,
    pub socket: Option<Inode>,
    pub lock: Option<Inode>,
    pub ping: Option<OwnerInfo>,
    pub ping_refusal: Option<String>,
    pub journal: Option<Journal>,
}

fn look(state: &Path, nonce: &str, point: &'static str) -> Result<Seen, String> {
    let prefix = state.join(owner::BIND_PREFIX).display().to_string();
    let mut owners = Vec::new();
    for record in launch::find_by_token(nonce) {
        let Ok(found) = sys::descriptors(record.pid) else {
            continue;
        };
        let locals: Vec<String> = found
            .iter()
            .filter_map(|descriptor| match descriptor {
                Descriptor::Socket {
                    local: Some(local), ..
                } if local.starts_with(&prefix) => Some(local.clone()),
                _ => None,
            })
            .collect();
        if !locals.is_empty() {
            owners.push((record.pid, locals));
        }
    }
    let (ping, ping_refusal) =
        match owner::request(state, &Request::Ping { wait_ms: 0 }, PING_TIMEOUT)? {
            Answer::Reply(Reply::Pong(info)) => (Some(*info), None),
            Answer::Reply(other) => (None, Some(format!("{other:?}"))),
            Answer::Unavailable(why) => (None, Some(why.reason().to_owned())),
        };
    Ok(Seen {
        point,
        uptime_ns: sys::uptime_ns(),
        owners,
        socket: Inode::of(&owner::socket_path(state)).ok(),
        lock: Inode::of(&owner::lock_path(state)).ok(),
        ping,
        ping_refusal,
        journal: Store::new(state).read().map_err(|err| err.to_string())?,
    })
}

/// Сканирование дескрипторов одного процесса.
#[derive(Clone, Debug, Serialize)]
pub struct Scan {
    pub what: String,
    pub pid: i32,
    pub leaks: Vec<String>,
    /// Положительный контроль: у хоста — свой сокет сессии; у остальных не проверяется.
    pub own_socket: Option<bool>,
    pub descriptors: Vec<Descriptor>,
}

/// Положительный контроль владельца: дескриптор блокировки и слушающий сокет с адресом `bind`.
#[derive(Clone, Debug, Serialize)]
pub struct Control {
    pub owner: ProcessKey,
    pub holds_lock: bool,
    pub listener: Option<u64>,
}

fn control(state: &Path, owner: ProcessKey, lock: Inode) -> Result<Control, String> {
    let found = sys::descriptors(owner.pid)
        .map_err(|err| format!("дескрипторы владельца {}: {err}", owner.pid))?;
    let bind = owner::bind_path(state, owner.pid as u32)
        .display()
        .to_string();
    Ok(Control {
        owner,
        holds_lock: found.iter().any(|descriptor| {
            matches!(descriptor, Descriptor::Vnode { dev, ino, .. } if *dev == lock.dev && *ino == lock.ino)
        }),
        listener: found.iter().find_map(|descriptor| match descriptor {
            Descriptor::Socket {
                handle,
                local: Some(local),
                ..
            } if *local == bind => Some(*handle),
            _ => None,
        }),
    })
}

fn scan(
    state: &Path,
    what: String,
    pid: i32,
    lock: Inode,
    listener: u64,
    session_socket: Option<&str>,
) -> Result<Scan, String> {
    let found = sys::descriptors(pid).map_err(|err| format!("дескрипторы {what} {pid}: {err}"))?;
    let prefix = state.join(owner::BIND_PREFIX).display().to_string();
    let leaks = owner_leaks(&found, lock, &owner::socket_path(state), &prefix, listener);
    let own_socket = session_socket.map(|socket| {
        found.iter().any(|descriptor| {
            matches!(descriptor, Descriptor::Socket { local: Some(local), .. } if local == socket)
        })
    });
    Ok(Scan {
        what,
        pid,
        leaks,
        own_socket,
        descriptors: found,
    })
}

/// Факты прохода.
#[derive(Clone, Debug, Serialize)]
pub struct PassFacts {
    pub pass: Pass,
    pub boundary: &'static str,
    pub armed: Vec<String>,
    pub installed: PathBuf,
    pub gui_n: OwnerInfo,
    pub gui_n_reports: Vec<AppReport>,
    pub lock_start: Inode,
    pub attempt: String,
    pub helper: ProcessKey,
    /// Помощник: событие взятия блокировки (inode).
    pub helper_lock: Option<Inode>,
    /// Убитый владелец у прохода `owner-dead`.
    pub killed: Option<OwnerInfo>,
    /// Ping GUI N на границе `до выхода N`.
    pub gui_n_at_barrier: Option<OwnerInfo>,
    pub before: Seen,
    pub at: Seen,
    pub end: Seen,
    pub stimuli: Vec<Stimulus>,
    pub final_owner: OwnerInfo,
    pub history: Vec<HistoryEntry>,
    pub controls: Vec<Control>,
    pub scans: Vec<Scan>,
}

impl PassFacts {
    fn states(&self) -> Vec<State> {
        self.history
            .iter()
            .filter(|entry| entry.journal.attempt == self.attempt)
            .map(|entry| entry.journal.state)
            .collect()
    }
}

/// Ключи процессов стимулов по их отметкам `start`.
pub fn keys_by_token(notes: &[Note]) -> BTreeMap<String, ProcessKey> {
    notes
        .iter()
        .filter(|note| note.event == "start")
        .filter_map(|note| {
            let forward = note.detail.get("forward")?.as_array()?;
            match forward.as_slice() {
                [only] => Some((only.as_str()?.to_owned(), note.process)),
                _ => None,
            }
        })
        .collect()
}

fn note_from(note: &Note) -> Option<ProcessKey> {
    serde_json::from_value(note.detail.get("from")?.clone()).ok()
}

/// Дошёл ли процесс стимула `key` до протокола на границе прохода: ответ на `forward` или, у
/// `bound`, запрос, задержанный неготовым N+1.
pub fn landed(pass: Pass, notes: &[Note], key: ProcessKey) -> bool {
    match pass {
        Pass::OwnerDead => true,
        Pass::Bound => notes.iter().any(|note| {
            note.event == "forward-held" && note.role == Role::Next && note_from(note) == Some(key)
        }),
        Pass::BeforeExit | Pass::HelperLock | Pass::Launched => notes
            .iter()
            .any(|note| note.event == "answer" && note.process == key),
    }
}

pub fn run(out: &Path, environment: &Environment) -> Result<Report, String> {
    let (state, nonce, layout) = super::stand_inputs(environment)?;
    let dir = out.join(DIR);
    fs::create_dir(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    let mut guard = Guard {
        state: state.clone(),
        apps: Vec::new(),
        sessions: Vec::new(),
        nonce: Some(nonce.clone()),
        armed: true,
    };
    let mut steps = Vec::new();
    let walked = walk(&state, &nonce, &layout, &dir, &mut steps, &mut guard);
    drop(guard);
    if let Err(reason) = &walked {
        steps.push(json!({ "name": "error", "reason": reason }));
    }
    let written = crate::results::write_json(&dir, "steps.json", &steps);
    let cells = walked?;
    written?;
    Ok(Report {
        cells,
        diagnostics: Diagnostics {
            notes: vec![format!(
                "проходов {}, стимулов в проходе {}; срок шага стенда {} с, ожидание сокета вторым запуском {} с за круг, срок health {} с от intent",
                Pass::ALL.len(),
                Kind::ORDER.len(),
                STEP.as_secs(),
                owner::SOCKET_WAIT.as_secs(),
                update::HEALTH_WINDOW.as_secs()
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
    guard: &mut Guard,
) -> Result<Vec<Cell>, String> {
    let mut cells = Vec::new();
    let mut all = Vec::new();
    for (pass, kinds) in schedule() {
        let facts = run_pass(state, nonce, layout, out, pass, &kinds, guard)?;
        steps.push(json!({ "name": pass.name(), "facts": facts }));
        for stimulus in &facts.stimuli {
            cells.push(part(
                &format!("{}/{}", pass.name(), stimulus.kind.name()),
                &stimulus_spec(pass, stimulus.kind),
                KIND_ORACLE,
                stimulus_verdict(pass, stimulus, &facts.final_owner),
                &stimulus_artifacts(pass, stimulus),
            ));
        }
        cells.push(part(
            &format!("{}/single-owner", pass.name()),
            &format!(
                "проход «{}»: точки наблюдения до стимулов, на границе после стимулов и в конце",
                pass.row().boundary
            ),
            SINGLE_ORACLE,
            single_owner_verdict(&facts),
            &[format!("{DIR}/{}/notes", pass.name())],
        ));
        all.push(facts);
    }
    cells.push(part(
        "descriptors",
        "proc_pidfdinfo у помощника на границе «до выхода N» и у хостов и нагрузок h1 (запущены GUI N) и h2 (запущены владельцем в конце) каждого прохода",
        DESCRIPTORS_ORACLE,
        descriptors_verdict(&all),
        &[],
    ));
    guard.armed = false;
    Ok(cells)
}

fn part(name: &str, stimulus: &str, oracle: &str, verdict: Verdict, artifacts: &[String]) -> Cell {
    let mut all = vec![STEPS_ARTIFACT.to_owned()];
    all.extend(artifacts.iter().cloned());
    Cell {
        id: CELL.to_owned(),
        part: Some(name.to_owned()),
        requirement: Requirement::Required,
        status: verdict.status,
        stimulus: stimulus.to_owned(),
        oracle: oracle.to_owned(),
        completion: COMPLETION.to_owned(),
        reason: verdict.reason,
        artifacts: all,
    }
}

fn stimulus_spec(pass: Pass, kind: Kind) -> String {
    let how = match kind {
        Kind::Open => "обычный open установленного бандла",
        Kind::OpenNew => "open -n установленного бандла",
        Kind::Direct => "прямой запуск исполняемого файла установленного бандла",
    };
    format!(
        "{how} с аргументами «{}» на границе C1 «{}»",
        token(pass, kind, "<nonce>"),
        pass.row().boundary
    )
}

fn stimulus_artifacts(pass: Pass, stimulus: &Stimulus) -> Vec<String> {
    let mut artifacts = vec![format!("{DIR}/{}/notes", pass.name())];
    if stimulus.launch == Launch::Spawned {
        artifacts.push(format!("{DIR}/{}/{}.stdout", pass.name(), stimulus.name));
    }
    artifacts
}

pub const KIND_ORACLE: &str = "процесс стимула, если LaunchServices или стенд его запустил, заканчивается одним из исходов: стал единственным владельцем в конце прохода; квитанция владельца, который в конце прохода единственный и у которого в принятых есть этот номер от этого процесса с этими аргументами; явный временный отказ с признаком повтора (код 75). Квитанция другого процесса или без записи у владельца, ошибка, отказ без повтора или процесс без строки итога — нарушение. У open -n и прямого запуска процесс обязателен. Обычный open без нового процесса — исход LaunchServices, а не запрос клиента протокола: «was already running» (активирован экземпляр, аргументы не доставлены) или код open не 0; записывается с экземплярами роли GUI перед запуском";

pub const SINGLE_ORACLE: &str = "в каждой точке наблюдения процессов с меткой запуска, у которых есть сокет с адресом <state>/.sock.*, не больше одного, и он — ожидаемый владелец этой точки (GUI N до его выхода, N+1 после bind, в конце — владелец, ответивший на ping); его адрес — .sock.<его PID>; путь сокета — узел, который он сообщил; без владельца ping получает временный отказ, а путь сокета пуст или на нём оставшийся узел убитого владельца; inode блокировки один от начала до конца прохода у GUI N, помощника, N+1 и владельца в конце; новый владелец сообщает удалённым только оставшийся узел убитого владельца, остальные — ничего; владельцем стал не больше чем один стимул, и это владелец в конце; журнал в конце — idle, попытка подтверждена";

pub const DESCRIPTORS_ORACLE: &str = "положительный контроль: у GUI N на границе «до выхода N» и у владельца в конце каждого прохода есть дескриптор блокировки и слушающий сокет с адресом bind, у каждого хоста — свой сокет сессии; у помощника на границе «до выхода N» (GUI N держит блокировку и сокет, помощник блокировку ещё не брал), у хостов и нагрузок h1 и h2 нет ни дескриптора блокировки, ни сокета с адресом сокета владельца, ни дескриптора того же сокета ядра, что слушающий у владельца";

fn run_pass(
    state: &Path,
    nonce: &str,
    layout: &Layout,
    out: &Path,
    pass: Pass,
    kinds: &[Kind],
    guard: &mut Guard,
) -> Result<PassFacts, String> {
    let pass_out = out.join(pass.name());
    fs::create_dir(&pass_out).map_err(|err| format!("{}: {err}", pass_out.display()))?;
    let stand = Stand::at(
        state,
        nonce,
        layout,
        "gen-1",
        &format!("install-{}", pass.name()),
    )?;
    let notes_dir = barrier::notes_dir(state);
    fs::create_dir_all(&notes_dir).map_err(|err| format!("{}: {err}", notes_dir.display()))?;

    // GUI N через LaunchServices, сессия h1 с выводом и квитанцией, поставленное обновление.
    let opened = open_bundle(&stand, &stand.options, true, &pass_out, "gui-n")?;
    if classify_open(opened.0, &opened.1) != Launch::Spawned {
        return Err(format!(
            "стенд: open -n GUI N: код {:?}, {}",
            opened.0, opened.1
        ));
    }
    let gui_n = owner::wait_for_owner(state, Instant::now() + STEP)?;
    if gui_n.generation != 1 {
        return Err(format!("стенд: GUI N — поколение {}", gui_n.generation));
    }
    let lock_path = owner::lock_path(state);
    let lock_start =
        Inode::of(&lock_path).map_err(|err| format!("{}: {err}", lock_path.display()))?;
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
    let candidate = layout.get("gen-2")?;
    let staged = match ask(
        state,
        &Request::Update {
            candidate: candidate.app.display().to_string(),
        },
    )? {
        Reply::Staging(staging) => match *staging {
            Staging::Staged { attempt, .. } => attempt,
            other => return Err(format!("стенд: update gen-2: {other:?}")),
        },
        other => return Err(format!("стенд: update gen-2: {other:?}")),
    };
    let armed = pass.armed();
    for barrier in &armed {
        barrier::arm(state, *barrier)?;
    }
    let (attempt, helper) = update_flow::activate(state)?;
    if attempt != staged {
        return Err(format!(
            "стенд: activate попытки {attempt}, поставлена {staged}"
        ));
    }

    // Граница строки.
    let mut killed = None;
    let mut gui_n_at_barrier = None;
    let mut controls = Vec::new();
    let mut scans = Vec::new();
    match pass {
        Pass::BeforeExit => {
            barrier::wait_reached(state, armed[0], Instant::now() + STEP)?;
            gui_n_at_barrier =
                match owner::request(state, &Request::Ping { wait_ms: 0 }, PING_TIMEOUT)? {
                    Answer::Reply(Reply::Pong(info)) => Some(*info),
                    other => return Err(format!("стенд: ping GUI N на границе: {other:?}")),
                };
            let gui = control(state, gui_n.key, lock_start)?;
            let listener = gui.listener.ok_or_else(|| {
                format!("положительный контроль: у GUI N нет слушающего сокета: {gui:?}")
            })?;
            scans.push(scan(
                state,
                "помощник на границе «до выхода N»".to_owned(),
                helper.pid,
                lock_start,
                listener,
                None,
            )?);
            for (role, key) in [("хост", h1.host), ("нагрузка", h1.workload)] {
                scans.push(scan(
                    state,
                    format!("{role} h1 на границе «до выхода N»"),
                    key.pid,
                    lock_start,
                    listener,
                    (role == "хост").then_some(h1.socket.as_str()),
                )?);
            }
            controls.push(gui);
        }
        Pass::HelperLock | Pass::Bound => {
            barrier::wait_reached(state, armed[0], Instant::now() + STEP)?;
        }
        Pass::Launched => {
            barrier::wait_reached(state, armed[0], Instant::now() + STEP)?;
            barrier::wait_reached(state, HELPER_RELEASED, Instant::now() + STEP)?;
            barrier::release(state, HELPER_RELEASED)?;
        }
        Pass::OwnerDead => {
            if !launch::wait_exit(&[gui_n.key.pid], STEP).is_empty() {
                return Err("стенд: GUI N не вышел после activate".to_owned());
            }
            let next = owner::wait_for_owner(state, Instant::now() + STEP)?;
            if next.generation != 2 || next.update.is_none() {
                return Err(format!("стенд: N+1 не подтвердился: {next:?}"));
            }
            if !launch::wait_exit(&[helper.pid], STEP).is_empty() {
                return Err("стенд: помощник не вышел после подтверждения".to_owned());
            }
            if observer::identify(next.key)?.is_alive() {
                // SAFETY: сигнал процессу, узнанному по PID и времени старта.
                unsafe { libc::kill(next.key.pid, libc::SIGKILL) };
            }
            if !launch::wait_exit(&[next.key.pid], STEP).is_empty() {
                return Err(format!(
                    "стенд: N+1 {} не вышел после SIGKILL",
                    next.key.pid
                ));
            }
            killed = Some(next);
        }
    }
    let before = look(state, nonce, BEFORE)?;

    // Стимулы.
    let mut stimuli = Vec::new();
    for kind in kinds {
        let token = token(pass, *kind, nonce);
        let name = format!("stimulus-{}", kind.name());
        let options = AppOptions {
            forward: vec![token.clone()],
            ..stand.options.clone()
        };
        let instances_before = instances(nonce);
        let launched_ns = sys::uptime_ns();
        let (open_exit, open_stderr, launch) = match kind {
            Kind::Direct => {
                guard
                    .apps
                    .push(start_app(&stand.app_bin, &options, &pass_out, &name)?);
                (None, String::new(), Launch::Spawned)
            }
            Kind::Open | Kind::OpenNew => {
                let (exit, stderr) =
                    open_bundle(&stand, &options, *kind == Kind::OpenNew, &pass_out, &name)?;
                let launch = classify_open(exit, &stderr);
                (exit, stderr, launch)
            }
        };
        stimuli.push(Stimulus {
            kind: *kind,
            token,
            name,
            open_exit,
            open_stderr,
            launch,
            instances_before,
            launched_ns,
            key: None,
            landed: false,
            reports: Vec::new(),
            exited: false,
            notes: Vec::new(),
        });
    }
    let spawned: Vec<String> = stimuli
        .iter()
        .filter(|stimulus| stimulus.launch == Launch::Spawned)
        .map(|stimulus| stimulus.token.clone())
        .collect();
    let ready = |notes: &[Note]| {
        let keys = keys_by_token(notes);
        spawned
            .iter()
            .all(|token| keys.get(token).is_some_and(|key| landed(pass, notes, *key)))
    };
    barrier::wait_notes(state, Instant::now() + STEP, ready)
        .map_err(|err| format!("стенд: стимулы не дошли до границы: {err}"))?;
    let at = look(state, nonce, AT)?;
    for barrier in &armed {
        barrier::release(state, *barrier)?;
    }

    // Конец прохода.
    if !launch::wait_exit(&[gui_n.key.pid], STEP).is_empty() {
        return Err("стенд: GUI N не вышел".to_owned());
    }
    let final_owner = owner::wait_for_owner(state, Instant::now() + STEP)?;
    if !launch::wait_exit(&[helper.pid], STEP).is_empty() {
        return Err("стенд: помощник не вышел".to_owned());
    }
    let notes = barrier::read_notes(state)?;
    let keys = keys_by_token(&notes);
    for stimulus in &mut stimuli {
        stimulus.key = keys.get(&stimulus.token).copied();
        if let Some(key) = stimulus.key {
            stimulus.landed = landed(pass, &notes, key);
            if key != final_owner.key {
                stimulus.exited = launch::wait_exit(&[key.pid], STEP).is_empty();
            }
        }
    }
    for app in &mut guard.apps {
        let pid = app.id() as i32;
        if pid != final_owner.key.pid {
            reap_child(app)?;
        }
    }
    let notes = barrier::read_notes(state)?;
    for stimulus in &mut stimuli {
        if let Some(key) = stimulus.key {
            stimulus.notes = notes
                .iter()
                .filter(|note| note.process == key || note_from(note) == Some(key))
                .cloned()
                .collect();
            stimulus.reports = reports(&pass_out, &stimulus.name)?;
        }
    }
    let end = look(state, nonce, END)?;
    let final_owner = end.ping.clone().unwrap_or(final_owner);

    // Дескрипторы: сессия h2 от владельца в конце.
    let h2 = stand.create_session(guard, "h2")?;
    let last = control(state, final_owner.key, lock_start)?;
    if let Some(listener) = last.listener {
        for (label, record) in [("h1", &h1), ("h2", &h2)] {
            for (role, key) in [("хост", record.host), ("нагрузка", record.workload)] {
                scans.push(scan(
                    state,
                    format!("{role} {label} в конце"),
                    key.pid,
                    lock_start,
                    listener,
                    (role == "хост").then_some(record.socket.as_str()),
                )?);
            }
        }
    }
    controls.push(last);

    let kept = update_flow::keep_helper_files(state, &attempt, &pass_out)?;
    let helper_lock = fs::read_to_string(kept.join(update::HELPER_STDOUT))
        .ok()
        .and_then(|text| {
            text.lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .find(|event| event.get("event").and_then(Value::as_str) == Some("locked"))
        })
        .and_then(|event| serde_json::from_value(event.pointer("/detail/inode")?.clone()).ok());
    let history = stand.store.history()?;
    let gui_n_reports = reports(&pass_out, "gui-n")?;
    copy_notes(state, &pass_out)?;
    finish(&stand, guard, &final_owner, &[h1, h2])?;
    Ok(PassFacts {
        pass,
        boundary: pass.row().boundary,
        armed: armed.iter().map(|barrier| barrier.key()).collect(),
        installed: stand.installed.clone(),
        gui_n,
        gui_n_reports,
        lock_start,
        attempt,
        helper,
        helper_lock,
        killed,
        gui_n_at_barrier,
        before,
        at,
        end,
        stimuli,
        final_owner,
        history,
        controls,
        scans,
    })
}

/// `open` (`-n`, если `new`) установленного бандла с аргументами роли GUI; stdout и stderr
/// процесса — в `out/<name>.*`. Код выхода и stderr самого `open`.
fn open_bundle(
    stand: &Stand,
    options: &AppOptions,
    new: bool,
    out: &Path,
    name: &str,
) -> Result<(Option<i32>, String), String> {
    let mut args = Vec::new();
    if new {
        args.push("-n".to_owned());
    }
    args.extend([
        "--env".to_owned(),
        format!("{}={}", launch::STATE_DIR, stand.state.display()),
        "--stdout".to_owned(),
        out.join(format!("{name}.stdout")).display().to_string(),
        "--stderr".to_owned(),
        out.join(format!("{name}.stderr")).display().to_string(),
        stand.installed.display().to_string(),
        "--args".to_owned(),
    ]);
    args.extend(options.to_args());
    let output = spawn::output(Path::new(OPEN), &args).map_err(|err| format!("open: {err}"))?;
    Ok((
        output.exit.code,
        String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    ))
}

fn copy_notes(state: &Path, pass_out: &Path) -> Result<(), String> {
    let source = barrier::notes_dir(state);
    let dest = pass_out.join("notes");
    fs::create_dir_all(&dest).map_err(|err| format!("{}: {err}", dest.display()))?;
    for entry in fs::read_dir(&source)
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

/// Уборка прохода: `quit` владельца, остановка сессий, процессы стенда, каталог состояния.
fn finish(
    stand: &Stand,
    guard: &mut Guard,
    owner: &OwnerInfo,
    sessions: &[SessionRecord],
) -> Result<(), String> {
    let quit = ask(&stand.state, &Request::Quit)?;
    if quit != Reply::Done || !launch::wait_exit(&[owner.key.pid], STEP).is_empty() {
        return Err(format!("стенд: quit владельца {}: {quit:?}", owner.key.pid));
    }
    for record in sessions {
        if update_flow::same_processes(record)? {
            update_flow::stop(record)?;
        }
    }
    for app in &mut guard.apps {
        reap_child(app)?;
    }
    guard.apps.clear();
    guard.sessions.clear();
    let left = launch::find_by_token(&stand.options.tag);
    if !left.is_empty() {
        return Err(format!("стенд: остались процессы {left:?}"));
    }
    clear_state(&stand.state);
    Ok(())
}

/// Прямой стимул вышел в срок шага и прибран.
fn reap_child(child: &mut std::process::Child) -> Result<(), String> {
    let pid = child.id() as i32;
    if child
        .try_wait()
        .map_err(|err| format!("стенд: wait {pid}: {err}"))?
        .is_some()
    {
        return Ok(());
    }
    if !launch::wait_exit(&[pid], STEP).is_empty() {
        return Err(format!(
            "стенд: процесс {pid} не вышел за {} с",
            STEP.as_secs()
        ));
    }
    child
        .wait()
        .map(|_| ())
        .map_err(|err| format!("стенд: wait {pid}: {err}"))
}

fn violation(violations: &mut Vec<String>, ok: bool, what: impl FnOnce() -> String) {
    if !ok {
        violations.push(what());
    }
}

/// Итог стимула по оракулу [`KIND_ORACLE`].
pub fn stimulus_verdict(pass: Pass, stimulus: &Stimulus, final_owner: &OwnerInfo) -> Verdict {
    let mut v = Vec::new();
    if stimulus.launch != Launch::Spawned {
        violation(&mut v, stimulus.kind == Kind::Open, || {
            format!(
                "{} без нового процесса: код {:?}, {}",
                stimulus.kind.name(),
                stimulus.open_exit,
                stimulus.open_stderr
            )
        });
        let live: Vec<i32> = stimulus
            .instances_before
            .iter()
            .map(|instance| instance.pid)
            .collect();
        return Verdict::of(
            v,
            match stimulus.launch {
                Launch::Activated => format!(
                    "LaunchServices: «{ALREADY_RUNNING}», нового процесса нет, аргументы не доставлены; экземпляры роли GUI перед запуском: {live:?}"
                ),
                _ => format!(
                    "LaunchServices: open вышел с кодом {:?} ({}), нового процесса нет; экземпляры роли GUI перед запуском: {live:?}",
                    stimulus.open_exit, stimulus.open_stderr
                ),
            },
        );
    }
    let Some(key) = stimulus.key else {
        v.push("процесс стимула не оставил отметки start".to_owned());
        return Verdict::of(v, String::new());
    };
    violation(&mut v, stimulus.landed || pass == Pass::OwnerDead, || {
        "процесс не дошёл до протокола на границе".to_owned()
    });
    let reason = match stimulus.reports.first() {
        Some(AppReport::Owner(info)) => {
            violation(
                &mut v,
                info.key == key && final_owner.key == key && stimulus.reports.len() == 1,
                || format!("владение: {info:?}, владелец в конце {:?}", final_owner.key),
            );
            format!(
                "процесс {} стал владельцем (поколение {}), он же владелец в конце",
                key.pid, info.generation
            )
        }
        Some(AppReport::Forwarded {
            key: reported,
            receipt,
            owner,
            yielded,
            launched_installed,
        }) => {
            let recorded = final_owner.received.iter().any(|got| {
                got.seq == *receipt && got.from == key && got.args == [stimulus.token.clone()]
            });
            violation(&mut v, *reported == key && stimulus.exited, || {
                format!(
                    "квитанцию сообщил {reported:?}, процесс {key:?}, вышел {}",
                    stimulus.exited
                )
            });
            violation(&mut v, *owner == final_owner.key && recorded, || {
                format!(
                    "ложный успех: квитанция {receipt} от {owner:?}, владелец в конце {:?}, записана у него: {recorded}",
                    final_owner.key
                )
            });
            format!(
                "процесс {} получил квитанцию {receipt} от владельца {} (поколение {}), она записана у него; уступил блокировку: {}; запускал установленный: {launched_installed}",
                key.pid,
                owner.pid,
                final_owner.generation,
                yielded.as_deref().unwrap_or("нет")
            )
        }
        Some(AppReport::Refused {
            key: reported,
            reason,
            retry,
            yielded,
        }) => {
            violation(
                &mut v,
                *reported == key && *retry && stimulus.exited,
                || {
                    format!(
                        "отказ {reason:?}: повтор {retry}, процесс {reported:?}, вышел {}",
                        stimulus.exited
                    )
                },
            );
            format!(
                "процесс {} получил явный временный отказ «{reason}» с признаком повтора; уступил блокировку: {}",
                key.pid,
                yielded.as_deref().unwrap_or("нет")
            )
        }
        other => {
            v.push(format!("итог процесса {}: {other:?}", key.pid));
            String::new()
        }
    };
    Verdict::of(v, reason)
}

/// Ожидаемый владелец точки наблюдения: `None` — владельца нет.
fn expected_owner(facts: &PassFacts, seen: &Seen) -> Option<ProcessKey> {
    let journal_owner = seen.journal.as_ref().and_then(|journal| journal.owner);
    match (facts.pass, seen.point) {
        (_, END) => Some(facts.final_owner.key),
        (Pass::BeforeExit, _) => Some(facts.gui_n.key),
        (Pass::Bound, _) => journal_owner,
        // Без барьера стимул мог стать владельцем раньше точки «на границе».
        (Pass::OwnerDead, AT) if !seen.owners.is_empty() => Some(facts.final_owner.key),
        (Pass::HelperLock | Pass::Launched | Pass::OwnerDead, _) => None,
    }
}

/// Итог `single-owner` по оракулу [`SINGLE_ORACLE`].
pub fn single_owner_verdict(facts: &PassFacts) -> Verdict {
    let mut v = Vec::new();
    for seen in [&facts.before, &facts.at, &facts.end] {
        let at = seen.point;
        let expected = expected_owner(facts, seen);
        violation(&mut v, seen.owners.len() <= 1, || {
            format!("{at}: владельцев {}: {:?}", seen.owners.len(), seen.owners)
        });
        match (expected, seen.owners.as_slice()) {
            (Some(key), [(pid, locals)]) => {
                let bind = format!("{}{}", owner::BIND_PREFIX, key.pid);
                violation(
                    &mut v,
                    *pid == key.pid && locals.iter().all(|local| local.ends_with(&bind)),
                    || format!("{at}: сокет у {pid} {locals:?}, ждали {}", key.pid),
                );
                match &seen.ping {
                    Some(info) => violation(
                        &mut v,
                        info.key == key && seen.socket == Some(info.socket_inode),
                        || {
                            format!(
                                "{at}: ping {:?} с сокетом {:?}, на пути {:?}",
                                info.key, info.socket_inode, seen.socket
                            )
                        },
                    ),
                    None => v.push(format!("{at}: ping владельца: {:?}", seen.ping_refusal)),
                }
            }
            (None, []) => {
                let stale = facts.killed.as_ref().map(|killed| killed.socket_inode);
                violation(
                    &mut v,
                    seen.socket.is_none() || seen.socket == stale,
                    || format!("{at}: владельца нет, а путь сокета {:?}", seen.socket),
                );
                violation(
                    &mut v,
                    seen.ping.is_none()
                        && seen.ping_refusal.as_deref()
                            == Some(owner::Unavailable::Restarting.reason()),
                    || format!("{at}: без владельца ping: {:?}", seen.ping_refusal),
                );
            }
            (expected, owners) => v.push(format!(
                "{at}: ждали владельца {expected:?}, процессы с сокетом владельца {owners:?}"
            )),
        }
        violation(&mut v, seen.lock == Some(facts.lock_start), || {
            format!(
                "{at}: inode блокировки {:?}, был {:?}",
                seen.lock, facts.lock_start
            )
        });
    }
    let mut inodes = vec![
        ("GUI N", Some(facts.gui_n.lock_inode)),
        ("владелец в конце", Some(facts.final_owner.lock_inode)),
        ("помощник", facts.helper_lock),
    ];
    if let Some(killed) = &facts.killed {
        inodes.push(("убитый N+1", Some(killed.lock_inode)));
    }
    for (who, inode) in inodes {
        violation(&mut v, inode == Some(facts.lock_start), || {
            format!(
                "{who}: inode блокировки {inode:?}, ждали {:?}",
                facts.lock_start
            )
        });
    }
    let stale = facts.killed.as_ref().map(|killed| killed.socket_inode);
    violation(&mut v, facts.final_owner.stale_socket == stale, || {
        format!(
            "владелец в конце удалил {:?}, ждали {stale:?}",
            facts.final_owner.stale_socket
        )
    });
    let next = facts
        .history
        .iter()
        .find(|entry| {
            entry.journal.attempt == facts.attempt && entry.journal.state == State::Launched
        })
        .and_then(|entry| entry.journal.owner);
    let stimulus_keys: Vec<ProcessKey> = facts
        .stimuli
        .iter()
        .filter_map(|stimulus| stimulus.key)
        .collect();
    match facts.pass {
        Pass::OwnerDead => violation(
            &mut v,
            stimulus_keys.contains(&facts.final_owner.key)
                && facts.killed.as_ref().map(|killed| killed.key) == next,
            || {
                format!(
                    "владелец в конце {:?} — не стимул, убит {:?}, N+1 журнала {next:?}",
                    facts.final_owner.key,
                    facts.killed.as_ref().map(|killed| killed.key)
                )
            },
        ),
        _ => violation(
            &mut v,
            Some(facts.final_owner.key) == next && facts.final_owner.update.is_some(),
            || {
                format!(
                    "владелец в конце {:?} — не подтвердившийся N+1 журнала {next:?}",
                    facts.final_owner.key
                )
            },
        ),
    }
    violation(&mut v, facts.final_owner.generation == 2, || {
        format!(
            "владелец в конце — поколение {}",
            facts.final_owner.generation
        )
    });
    let owners: Vec<i32> = facts
        .stimuli
        .iter()
        .filter(|stimulus| matches!(stimulus.reports.first(), Some(AppReport::Owner(_))))
        .filter_map(|stimulus| stimulus.key.map(|key| key.pid))
        .collect();
    violation(
        &mut v,
        owners.len() <= 1 && owners.iter().all(|pid| *pid == facts.final_owner.key.pid),
        || format!("владельцами стали стимулы {owners:?}"),
    );
    let committed = matches!(
        &facts.end.journal,
        Some(journal) if journal.attempt == facts.attempt
            && journal.state == State::Idle
            && journal.outcome == Some(Outcome::Committed)
    );
    violation(&mut v, committed && facts.final_owner.ready, || {
        format!(
            "в конце журнал {:?}, владелец готов {}",
            facts
                .end
                .journal
                .as_ref()
                .map(|journal| (journal.state, journal.outcome)),
            facts.final_owner.ready
        )
    });
    let describe = |seen: &Seen| match seen.owners.as_slice() {
        [] => "нет".to_owned(),
        owners => owners
            .iter()
            .map(|(pid, _)| pid.to_string())
            .collect::<Vec<_>>()
            .join(", "),
    };
    Verdict::of(
        v,
        format!(
            "владелец до стимулов: {}, на границе: {}, в конце: {} (поколение {}); inode блокировки {} весь проход; история попытки: {}",
            describe(&facts.before),
            describe(&facts.at),
            describe(&facts.end),
            facts.final_owner.generation,
            facts.lock_start.ino,
            facts
                .states()
                .iter()
                .map(|state| state.name())
                .collect::<Vec<_>>()
                .join(" → ")
        ),
    )
}

/// Итог `descriptors` по оракулу [`DESCRIPTORS_ORACLE`].
pub fn descriptors_verdict(all: &[PassFacts]) -> Verdict {
    let mut v = Vec::new();
    let mut scanned = 0;
    for facts in all {
        let name = facts.pass.name();
        for control in &facts.controls {
            violation(
                &mut v,
                control.holds_lock && control.listener.is_some(),
                || format!("{name}: положительный контроль владельца {control:?}"),
            );
        }
        let expected = if facts.pass == Pass::BeforeExit { 7 } else { 4 };
        violation(&mut v, facts.scans.len() == expected, || {
            format!(
                "{name}: просканировано {}, ждали {expected}",
                facts.scans.len()
            )
        });
        for scan in &facts.scans {
            scanned += 1;
            violation(&mut v, scan.leaks.is_empty(), || {
                format!("{name}: {} {}: {:?}", scan.what, scan.pid, scan.leaks)
            });
            violation(&mut v, scan.own_socket != Some(false), || {
                format!(
                    "{name}: у {} {} нет своего сокета сессии",
                    scan.what, scan.pid
                )
            });
        }
    }
    Verdict::of(
        v,
        format!(
            "проходов {}, просканировано процессов {scanned}: дескрипторов блокировки и сокета владельца нет; положительный контроль у GUI N и владельца каждого прохода выполнен",
            all.len()
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Расписание, которое обходит сценарий, покрывает каждый барьер C1 каждым видом запуска.
    #[test]
    fn schedule_covers_every_c1_barrier_with_every_launch_kind() {
        let plan = schedule();
        assert_eq!(plan.len(), barrier::C1.len());
        for (index, row) in barrier::C1.iter().enumerate() {
            let passes: Vec<&(Pass, Vec<Kind>)> = plan
                .iter()
                .filter(|(pass, _)| pass.row_index() == index)
                .collect();
            assert_eq!(passes.len(), 1, "строка «{}»", row.boundary);
            let (pass, kinds) = passes[0];
            for kind in [Kind::Direct, Kind::OpenNew, Kind::Open] {
                assert!(kinds.contains(&kind), "{} без {}", pass.name(), kind.name());
            }
            let armed = pass.armed();
            for barrier in row.barriers {
                assert!(
                    armed.contains(barrier),
                    "{}: {}",
                    pass.name(),
                    barrier.key()
                );
            }
            assert_eq!(armed.is_empty(), row.barriers.is_empty(), "{}", pass.name());
        }
        let tokens: std::collections::HashSet<String> = plan
            .iter()
            .flat_map(|(pass, kinds)| kinds.iter().map(|kind| token(*pass, *kind, "ab12")))
            .collect();
        assert_eq!(tokens.len(), Pass::ALL.len() * Kind::ORDER.len());
        for one in &tokens {
            let others = tokens
                .iter()
                .filter(|other| other.contains(one.as_str()))
                .count();
            assert_eq!(others, 1, "метка {one} входит в другую");
        }
        assert_eq!(Kind::ORDER[0], Kind::Open, "обычный open — первым");
    }

    #[test]
    fn open_outcomes_are_classified_by_exit_and_stderr() {
        assert_eq!(classify_open(Some(0), ""), Launch::Spawned);
        assert_eq!(
            classify_open(
                Some(0),
                "Application /tmp/x/probe-update.app was already running and so the additional environment variables and redirected stdin/stdout/stderr provided could not be set."
            ),
            Launch::Activated
        );
        assert_eq!(
            classify_open(
                Some(1),
                "_LSOpenURLsWithCompletionHandler() failed with error -600."
            ),
            Launch::LsError
        );
        assert_eq!(classify_open(None, ""), Launch::LsError);
    }

    fn key(pid: i32) -> ProcessKey {
        ProcessKey { pid, start_us: 1 }
    }

    fn note(event: &str, role: Role, pid: i32, detail: Value) -> Note {
        Note {
            event: event.to_owned(),
            role,
            process: key(pid),
            seq: 1,
            uptime_ns: pid as u64,
            detail,
        }
    }

    #[test]
    fn stimuli_are_found_by_their_token_and_land_by_pass() {
        let notes = vec![
            note(
                "start",
                Role::Gui,
                10,
                json!({ "forward": ["c1-bound-direct-n1"] }),
            ),
            note("start", Role::Gui, 11, json!({ "forward": [] })),
            note("start", Role::Gui, 12, json!({ "forward": ["a", "b"] })),
            note("answer", Role::Gui, 10, json!({ "answer": "unavailable" })),
            note("forward-held", Role::Next, 20, json!({ "from": key(13) })),
        ];
        let keys = keys_by_token(&notes);
        assert_eq!(keys.len(), 1);
        assert_eq!(keys["c1-bound-direct-n1"], key(10));
        assert!(landed(Pass::HelperLock, &notes, key(10)));
        assert!(!landed(Pass::HelperLock, &notes, key(13)));
        assert!(landed(Pass::Bound, &notes, key(13)));
        assert!(
            !landed(Pass::Bound, &notes, key(10)),
            "у bound нужен запрос, задержанный N+1"
        );
        assert!(landed(Pass::OwnerDead, &[], key(99)));
    }
}
