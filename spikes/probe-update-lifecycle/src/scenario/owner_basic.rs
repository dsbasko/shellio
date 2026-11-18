//! Сценарий стенда `owner-basic` (задача 6): роль GUI без обновления на настоящем подписанном
//! поколении 1, установленном копией в `<state>/install/`. Части по порядку:
//!
//! 1. `owner` — первый запуск становится владельцем: `ping` с номером поколения, CDHash процесса
//!    из ядра равен CDHash установленного бандла, исполняемый файл — установленный, сокет и
//!    блокировка на месте, рантайм поколения подготовлен;
//! 2. `session` — сессия идёт от хоста из `runtime/1/`, копии в рантайме совпадают с бандлом и
//!    проходят проверку подписи своих частей, attach получает квитанцию ввода, реестр видит её;
//! 3. `forward` — второй запуск, не получивший блокировку, передаёт аргументы владельцу и выходит,
//!    владелец и его сокет прежние;
//! 4. `restart` — после `SIGKILL` владельца путь сокета остаётся, клиент пробы получает явный
//!    временный отказ с признаком повтора; новый запуск удаляет оставшийся путь, публикует свой
//!    сокет, переиспользует рантайм и находит прежнюю сессию с теми же процессами и хвостом без
//!    пропуска;
//! 5. `descriptors` — у хостов и нагрузок, запущенных прежним и новым владельцем, нет дескрипторов
//!    блокировки и сокета владельца; положительный контроль — те же признаки у самого владельца и
//!    собственный сокет у хоста.
//!
//! Это клетка стенда: невыполненный оракул — ошибка исполнения, а не `fail`. После частей
//! владелец выходит по `Quit` (убирает свой сокет, блокировка остаётся), сессии останавливаются,
//! каталог состояния очищается: файл блокировки приложение не удаляет никогда, его убирает
//! сценарий. Шаги и факты — в `owner-basic/steps.json`.

use std::fs::{self, File};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::json;

use crate::attach::{self, Attached, Connected};
use crate::fixture;
use crate::generation::{self, BUNDLE_ID, Part, Verify};
use crate::host::{self, SessionRecord};
use crate::launch;
use crate::observer::{self, ProcessKey};
use crate::owner::{
    self, Answer, AppOptions, AppReport, Entry, Inode, OwnerInfo, Probe, Reply, Request,
    Unavailable,
};
use crate::proto::{ATTACH_VERSIONS, Ack, Purpose};
use crate::results::{self, Cell, Diagnostics, Environment, OWNER_BASIC_CELL, Report, Requirement};
use crate::runtime;
use crate::sys::{self, Descriptor};

use super::reject::{INSTALL_DIR, install};

/// Срок процесса сценария.
pub const DEADLINE: Duration = Duration::from_secs(120);
pub const DIR: &str = "owner-basic";
pub const STEPS_ARTIFACT: &str = "owner-basic/steps.json";
/// Срок одного шага: запуск владельца проверяет подписи и готовит рантайм.
const STEP: Duration = Duration::from_secs(20);
const INSTALLED_GENERATION: u32 = 1;

#[derive(Debug, Default, Serialize)]
struct Evidence {
    steps: Vec<Step>,
}

#[derive(Debug, Serialize)]
struct Step {
    name: &'static str,
    facts: serde_json::Value,
}

impl Evidence {
    fn step(&mut self, name: &'static str, facts: serde_json::Value) {
        self.steps.push(Step { name, facts });
    }
}

/// Пройденная часть клетки стенда.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Part5 {
    pub name: &'static str,
    pub stimulus: &'static str,
    pub oracle: &'static str,
    pub reason: String,
    pub artifacts: Vec<String>,
}

/// Процессы и файлы стенда, если сценарий не дошёл до проверенной уборки.
struct Guard {
    state: PathBuf,
    apps: Vec<Child>,
    sessions: Vec<SessionRecord>,
    armed: bool,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        for app in &mut self.apps {
            let _ = app.kill();
            let _ = app.wait();
        }
        let mut pids = Vec::new();
        for record in &self.sessions {
            for key in [record.host, record.workload] {
                if observer::identify(key).is_ok_and(|identity| identity.is_alive()) {
                    // SAFETY: сигнал процессу, узнанному по PID и времени старта.
                    unsafe { libc::kill(key.pid, libc::SIGKILL) };
                    pids.push(key.pid);
                }
            }
        }
        launch::wait_exit(&pids, STEP);
        clear_state(&self.state);
    }
}

/// Всё содержимое каталога состояния.
fn clear_state(state: &Path) {
    if let Ok(entries) = fs::read_dir(state) {
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                let _ = fs::remove_dir_all(&path);
            } else {
                let _ = fs::remove_file(&path);
            }
        }
    }
}

fn check(ok: bool, what: impl FnOnce() -> String) -> Result<(), String> {
    if ok { Ok(()) } else { Err(what()) }
}

fn artifact(name: &str) -> String {
    format!("{DIR}/{name}")
}

/// Запуск роли GUI напрямую из установленного бандла; stdout и stderr — в файлы прогона.
fn start_app(binary: &Path, options: &AppOptions, out: &Path, name: &str) -> Result<Child, String> {
    let stdout = File::create(out.join(format!("{name}.stdout")))
        .map_err(|err| format!("{name}.stdout: {err}"))?;
    let stderr = File::create(out.join(format!("{name}.stderr")))
        .map_err(|err| format!("{name}.stderr: {err}"))?;
    Command::new(binary)
        .args(options.to_args())
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .spawn()
        .map_err(|err| format!("запуск {}: {err}", binary.display()))
}

/// Строки итога роли GUI из её stdout.
fn reports(out: &Path, name: &str) -> Result<Vec<AppReport>, String> {
    let path = out.join(format!("{name}.stdout"));
    let text = fs::read_to_string(&path).map_err(|err| format!("{}: {err}", path.display()))?;
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line).map_err(|err| format!("{name}.stdout «{line}»: {err}"))
        })
        .collect()
}

/// Ждёт выхода процесса не дольше шага и прибирает его.
fn reap(child: &mut Child, what: &str) -> Result<std::process::ExitStatus, String> {
    let alive = launch::wait_exit(&[child.id() as i32], STEP);
    if !alive.is_empty() {
        return Err(format!("{what} не вышел за {} с", STEP.as_secs()));
    }
    child.wait().map_err(|err| format!("{what}: wait: {err}"))
}

fn ask(state: &Path, request: &Request) -> Result<Reply, String> {
    match owner::request(state, request, STEP)? {
        Answer::Reply(reply) => Ok(reply),
        Answer::Unavailable(why) => Err(format!("{request:?}: {}", why.reason())),
    }
}

/// Ответ `Sessions`.
struct Registry {
    entries: Vec<Entry>,
    problems: Vec<String>,
    cleanup: Vec<u32>,
}

fn sessions(state: &Path) -> Result<Registry, String> {
    match ask(state, &Request::Sessions)? {
        Reply::Sessions {
            entries,
            problems,
            cleanup,
        } => Ok(Registry {
            entries,
            problems,
            cleanup,
        }),
        other => Err(format!("на Sessions пришло {other:?}")),
    }
}

fn ping(state: &Path) -> Result<OwnerInfo, String> {
    match ask(state, &Request::Ping { wait_ms: 0 })? {
        Reply::Pong(info) => Ok(*info),
        other => Err(format!("на Ping пришло {other:?}")),
    }
}

fn path_of(pid: i32) -> Result<PathBuf, String> {
    sys::process_path(pid).map_err(|err| format!("proc_pidpath {pid}: {err}"))
}

fn alive(key: ProcessKey) -> Result<bool, String> {
    Ok(observer::identify(key)?.is_alive())
}

/// Запись сессии `name` в реестре: жива, отвечает на `probe` версией 1 и с теми же процессами.
fn found_alive(entries: &[Entry], record: &SessionRecord) -> bool {
    entries.iter().any(|entry| {
        entry.record.session == record.session
            && entry.record.host == record.host
            && entry.record.workload == record.workload
            && entry.host_alive
            && entry.workload_alive
            && entry.probe == Probe::Welcome { version: 1 }
    })
}

/// Нарушения отсутствия дескрипторов владельца у процесса стенда.
pub fn owner_leaks(
    found: &[Descriptor],
    lock: Inode,
    socket: &Path,
    bind_prefix: &str,
    listener: u64,
) -> Vec<String> {
    let socket = socket.display().to_string();
    found
        .iter()
        .filter_map(|descriptor| match descriptor {
            Descriptor::Vnode { fd, dev, ino, path } if *dev == lock.dev && *ino == lock.ino => {
                Some(format!("fd {fd}: блокировка {path}"))
            }
            Descriptor::Socket {
                fd, handle, local, ..
            } => {
                let owners = local
                    .as_deref()
                    .is_some_and(|local| local == socket || local.starts_with(bind_prefix));
                (owners || *handle == listener).then(|| {
                    format!("fd {fd}: сокет владельца (адрес {local:?}, ядро {handle:#x})")
                })
            }
            _ => None,
        })
        .collect()
}

pub fn run(out: &Path, environment: &Environment) -> Result<Report, String> {
    let nonce = environment
        .process
        .nonce
        .as_deref()
        .ok_or("предусловие: нет nonce")?;
    let state = environment
        .process
        .state_dir
        .as_deref()
        .map(PathBuf::from)
        .ok_or_else(|| format!("предусловие: нет {}", launch::STATE_DIR))?;
    let (_, layout) = super::bundles(environment)?;
    let dir = out.join(DIR);
    fs::create_dir(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    let mut guard = Guard {
        state: state.clone(),
        apps: Vec::new(),
        sessions: Vec::new(),
        armed: true,
    };
    let mut evidence = Evidence::default();
    let walked = walk(&state, nonce, &layout, &dir, &mut evidence, &mut guard);
    drop(guard);
    if let Err(reason) = &walked {
        evidence.step("error", json!({ "reason": reason }));
    }
    let written = results::write_json(&dir, "steps.json", &evidence);
    let parts = walked?;
    written?;
    Ok(report(&parts))
}

/// Части клетки стенда `owner-basic`.
pub fn report(parts: &[Part5]) -> Report {
    let cells = parts
        .iter()
        .map(|part| {
            let mut artifacts = vec![STEPS_ARTIFACT.to_owned()];
            artifacts.extend(part.artifacts.iter().cloned());
            Cell {
                id: OWNER_BASIC_CELL.to_owned(),
                part: Some(part.name.to_owned()),
                requirement: Requirement::Required,
                status: results::Status::Pass,
                stimulus: part.stimulus.to_owned(),
                oracle: part.oracle.to_owned(),
                completion:
                    "ответ владельца, хоста или выход процесса в срок шага; иначе ошибка исполнения"
                        .to_owned(),
                reason: part.reason.clone(),
                artifacts,
            }
        })
        .collect();
    Report {
        cells,
        diagnostics: Diagnostics {
            notes: vec![format!(
                "ожидание сокета вторым запуском {} с, срок ответа владельца {} с",
                owner::SOCKET_WAIT.as_secs(),
                owner::REQUEST_TIMEOUT.as_secs()
            )],
        },
    }
}

fn walk(
    state: &Path,
    nonce: &str,
    layout: &generation::Layout,
    out: &Path,
    evidence: &mut Evidence,
    guard: &mut Guard,
) -> Result<Vec<Part5>, String> {
    let mut parts = Vec::new();
    let installed = state.join(INSTALL_DIR).join(generation::APP_NAME);
    install(&layout.get("gen-1")?.app, &installed)?;
    let app_bin = Part::App.path(&installed);
    let lock = owner::lock_path(state);
    let socket = owner::socket_path(state);
    host::check_socket_path(&socket).map_err(|err| format!("предусловие: {err}"))?;
    let mut runs = Vec::new();
    let installed_details = generation::details(&installed, &mut runs)?
        .map_err(|err| format!("стенд: установленный бандл: {err}"))?;
    let options = |forward: &[&str]| AppOptions {
        state: state.to_path_buf(),
        installed: installed.clone(),
        leaf: layout.leaf.clone(),
        identifier: BUNDLE_ID.to_owned(),
        tag: nonce.to_owned(),
        update_attempt: None,
        window: false,
        forward: forward.iter().map(|arg| (*arg).to_owned()).collect(),
    };

    // 1. Владелец.
    guard
        .apps
        .push(start_app(&app_bin, &options(&["first"]), out, "owner-a")?);
    let a_pid = guard.apps[0].id() as i32;
    let info_a = owner::wait_for_owner(state, Instant::now() + STEP)?;
    let a_reports = reports(out, "owner-a")?;
    let a_cdhash = sys::process_cdhash(a_pid).map_err(|err| format!("csops {a_pid}: {err}"))?;
    let a_path = path_of(a_pid)?;
    let a_key = observer::inspect(a_pid)?
        .ok_or("владелец вышел сразу после готовности")?
        .key;
    let lock_inode = Inode::of(&lock).map_err(|err| format!("{}: {err}", lock.display()))?;
    let socket_meta =
        fs::symlink_metadata(&socket).map_err(|err| format!("{}: {err}", socket.display()))?;
    let socket_a = Inode::of(&socket).map_err(|err| format!("{}: {err}", socket.display()))?;
    evidence.step(
        "owner",
        json!({
            "info": info_a,
            "reports": a_reports,
            "csops_cdhash": a_cdhash,
            "proc_pidpath": a_path,
            "installed_details": installed_details,
            "lock_inode": lock_inode,
            "socket_inode": socket_a,
        }),
    );
    let runtime_dir = runtime::ready_dir(state, INSTALLED_GENERATION);
    check(
        info_a.generation == INSTALLED_GENERATION && info_a.key == a_key,
        || {
            format!(
                "ping: поколение {}, ключ {:?}; процесс {a_key:?}",
                info_a.generation, info_a.key
            )
        },
    )?;
    check(
        info_a.cdhash == a_cdhash
            && a_cdhash == installed_details.cdhash
            && info_a.installed_cdhash == installed_details.cdhash,
        || {
            format!(
                "CDHash: владелец сообщил {}, ядро {a_cdhash}, установленный по ping {}, по codesign {}",
                info_a.cdhash, info_a.installed_cdhash, installed_details.cdhash
            )
        },
    )?;
    check(
        a_path == app_bin && Path::new(&info_a.executable) == app_bin,
        || {
            format!(
                "исполняемый файл: proc_pidpath {}, ping {}, ожидался {}",
                a_path.display(),
                info_a.executable,
                app_bin.display()
            )
        },
    )?;
    check(
        socket_meta.file_type().is_socket()
            && info_a.socket_inode == socket_a
            && info_a.lock_inode == lock_inode
            && info_a.stale_socket.is_none()
            && info_a.recovery.is_empty(),
        || {
            format!(
                "сокет и блокировка: {info_a:?}; сокет на диске {socket_a:?}, блокировка {lock_inode:?}"
            )
        },
    )?;
    check(
        info_a.runtime.dir == runtime_dir && info_a.runtime.outcome == runtime::Outcome::Created,
        || format!("рантайм: {:?}", info_a.runtime),
    )?;
    check(
        info_a.received.len() == 1
            && info_a.received[0].from == a_key
            && info_a.received[0].args == ["first"],
        || format!("свои аргументы владельца: {:?}", info_a.received),
    )?;
    check(
        matches!(a_reports.as_slice(), [AppReport::Owner(reported)] if reported.key == a_key),
        || format!("stdout владельца: {a_reports:?}"),
    )?;
    parts.push(Part5 {
        name: "owner",
        stimulus: "прямой запуск probe-update-app установленного поколения 1 с --state, --installed, --leaf, --tag",
        oracle: "ping: поколение 1, ключ процесса; CDHash процесса из ядра (csops) = сообщённый = CDHash установленного бандла по codesign; proc_pidpath — установленный бандл; сокет на месте и слушает, inode блокировки и сокета равны сообщённым; оставшегося сокета и шагов восстановления нет; runtime/1 подготовлен; свои аргументы приняты первым запросом",
        reason: format!(
            "владелец {} (CDHash {}), сокет inode {}, блокировка inode {}, рантайм {}",
            a_key.pid,
            &a_cdhash[..8],
            socket_a.ino,
            lock_inode.ino,
            runtime_dir.display()
        ),
        artifacts: vec![artifact("owner-a.stdout"), artifact("owner-a.stderr")],
    });

    // 2. Сессия из runtime/1/.
    let record = match ask(
        state,
        &Request::CreateSession {
            name: "h1".to_owned(),
        },
    )? {
        Reply::Session(record) => *record,
        other => return Err(format!("CreateSession h1: {other:?}")),
    };
    guard.sessions.push(record.clone());
    let host_bin = runtime::binary(state, INSTALLED_GENERATION, Part::Host);
    let fixture_bin = runtime::binary(state, INSTALLED_GENERATION, Part::Fixture);
    let host_path = path_of(record.host.pid)?;
    let workload_path = path_of(record.workload.pid)?;
    let host_info = observer::inspect(record.host.pid)?.ok_or("хост h1 вышел")?;
    let workload_info = observer::inspect(record.workload.pid)?.ok_or("нагрузка h1 вышла")?;
    let mut copies = Vec::new();
    for part in Part::RUNTIME {
        let copy = runtime::binary(state, INSTALLED_GENERATION, part);
        let same = fs::read(&copy).map_err(|err| format!("{}: {err}", copy.display()))?
            == fs::read(part.path(&installed))
                .map_err(|err| format!("{}: {err}", part.path(&installed).display()))?;
        let requirement = generation::requirement(&part.identifier(BUNDLE_ID), &layout.leaf);
        let verify = generation::verify(&copy, &requirement, &mut runs)?;
        copies.push((part, same, verify));
    }
    let names = generation::files(&runtime_dir)?;
    let client = match attach::connect(
        Path::new(&record.socket),
        &attach::hello(Purpose::Attach, ATTACH_VERSIONS, 0, owner::SESSION_SIZE),
        STEP,
    )? {
        Connected::Welcome(client) => client,
        Connected::Refused(reason) => return Err(format!("attach h1: {reason}")),
    };
    let mut first = Attached::new(client, Some(1));
    let deadline = Instant::now() + STEP;
    first.pump_until(deadline, |transcript| !transcript.lines.is_empty())?;
    first.exchange(1, "ping", deadline)?;
    first.detach(Instant::now() + STEP)?;
    let resume = first.transcript.resume_offset();
    let next_number = first
        .transcript
        .next_number()
        .ok_or("после attach нет ни одной строки")?;
    let Registry {
        entries,
        problems,
        cleanup,
    } = sessions(state)?;
    evidence.step(
        "session",
        json!({
            "record": record,
            "host": host_info,
            "workload": workload_info,
            "host_path": host_path,
            "workload_path": workload_path,
            "runtime_files": names,
            "copies": copies.iter().map(|(part, same, verify)| json!({"part": part, "same_bytes": same, "verify": verify})).collect::<Vec<_>>(),
            "transcript": first.transcript,
            "acks": first.acks,
            "registry": entries,
            "problems": problems,
            "cleanup": cleanup,
        }),
    );
    check(
        host_path == host_bin
            && Path::new(&record.executable) == host_bin
            && workload_path == fixture_bin,
        || {
            format!(
                "хост {} (запись {}), нагрузка {}; ожидались {} и {}",
                host_path.display(),
                record.executable,
                workload_path.display(),
                host_bin.display(),
                fixture_bin.display()
            )
        },
    )?;
    check(
        host_info.ppid == 1 && workload_info.ppid == record.host.pid && record.generation == 1,
        || {
            format!(
                "хост PPID {}, нагрузка PPID {}, поколение {}",
                host_info.ppid, workload_info.ppid, record.generation
            )
        },
    )?;
    let mut expected_names: Vec<String> = Part::RUNTIME
        .iter()
        .map(|part| part.file_name().to_owned())
        .collect();
    expected_names.sort();
    check(
        names == expected_names
            && copies
                .iter()
                .all(|(_, same, verify)| *same && *verify == Verify::Satisfied),
        || format!("runtime/1: файлы {names:?}, сверка {copies:?}"),
    )?;
    let receipt = fixture::receipt(1, "ping");
    check(
        first.transcript.problems.is_empty()
            && first.transcript.receipt(1, "ping").is_some()
            && first.acks == [(1, Ack::Accepted)],
        || format!("attach h1: {:?}, ответы {:?}", first.transcript, first.acks),
    )?;
    check(
        found_alive(&entries, &record) && problems.is_empty() && cleanup.is_empty(),
        || format!("реестр: {entries:?}, проблемы {problems:?}, уборка {cleanup:?}"),
    )?;
    parts.push(Part5 {
        name: "session",
        stimulus: "CreateSession h1 через управляющий сокет, attach с начала и ввод «1 ping», затем Sessions",
        oracle: "proc_pidpath хоста — runtime/1/probe-update-host, нагрузки — runtime/1/probe-update-fixture; хост вне дерева (PPID 1), нагрузка — его потомок; в runtime/1 ровно host, attach, fixture, каждый байт в байт равен бинарю бандла и проходит codesign со своим требованием; квитанция «ack 1 ping» без пропуска строк; реестр: h1 жива, probe отвечает версией 1; кандидатов на уборку нет",
        reason: format!(
            "хост {} из {}, квитанция «{receipt}» в строке {}, реестр: {} сессия",
            record.host.pid,
            host_bin.display(),
            first.transcript.receipt(1, "ping").map_or(0, |line| line.number),
            entries.len()
        ),
        artifacts: Vec::new(),
    });

    // 3. Второй запуск передаёт аргументы.
    let mut second = start_app(&app_bin, &options(&["second", nonce]), out, "second")?;
    let second_pid = second.id() as i32;
    let second_status = reap(&mut second, "второй запуск")?;
    let second_reports = reports(out, "second")?;
    let after_forward = ping(state)?;
    let socket_after_forward =
        Inode::of(&socket).map_err(|err| format!("{}: {err}", socket.display()))?;
    evidence.step(
        "forward",
        json!({
            "pid": second_pid,
            "exit": second_status.code(),
            "reports": second_reports,
            "owner": after_forward,
            "socket_inode": socket_after_forward,
        }),
    );
    let Some(AppReport::Forwarded {
        key: second_key,
        receipt: seq,
        owner: told_owner,
        yielded,
        launched_installed,
    }) = second_reports.first().cloned()
    else {
        return Err(format!(
            "второй запуск: {second_reports:?}, {second_status}"
        ));
    };
    check(
        second_status.success()
            && second_reports.len() == 1
            && second_key.pid == second_pid
            && told_owner == a_key
            && yielded.is_none()
            && !launched_installed,
        || format!("второй запуск: {second_status}, {second_reports:?}"),
    )?;
    check(
        after_forward.key == a_key
            && after_forward.received.iter().any(|got| {
                got.seq == seq && got.from == second_key && got.args == ["second", nonce]
            })
            && after_forward.socket_inode == socket_a
            && socket_after_forward == socket_a
            && Inode::of(&lock).is_ok_and(|inode| inode == lock_inode),
        || format!("владелец после передачи: {after_forward:?}, сокет {socket_after_forward:?}"),
    )?;
    parts.push(Part5 {
        name: "forward",
        stimulus: "второй прямой запуск того же бинаря с аргументами «second <nonce>» при живом владельце",
        oracle: "блокировку не получил и не держал (yielded нет), передал аргументы и вышел с кодом 0 со строкой forwarded и квитанцией; владелец прежний, принял аргументы под этим номером от этого процесса; inode сокета и блокировки прежние",
        reason: format!(
            "второй запуск {second_pid} получил квитанцию {seq} от владельца {}, вышел: {second_status}",
            a_key.pid
        ),
        artifacts: vec![artifact("second.stdout"), artifact("second.stderr")],
    });

    // 4. Перезапуск после SIGKILL владельца.
    guard.apps[0]
        .kill()
        .map_err(|err| format!("SIGKILL владельцу: {err}"))?;
    let killed = reap(&mut guard.apps[0], "убитый владелец")?;
    let stale = Inode::of(&socket).ok();
    let stale_is_socket =
        fs::symlink_metadata(&socket).is_ok_and(|meta| meta.file_type().is_socket());
    let refusal = owner::request(state, &Request::Ping { wait_ms: 0 }, STEP)?;
    let sessions_alive = (alive(record.host)?, alive(record.workload)?);
    let host_runtime_inode =
        Inode::of(&host_bin).map_err(|err| format!("{}: {err}", host_bin.display()))?;
    guard
        .apps
        .push(start_app(&app_bin, &options(&["third"]), out, "owner-c")?);
    let c_pid = guard.apps[1].id() as i32;
    let info_c = owner::wait_for_owner(state, Instant::now() + STEP)?;
    let socket_c = Inode::of(&socket).map_err(|err| format!("{}: {err}", socket.display()))?;
    let Registry {
        entries: entries_c,
        problems: problems_c,
        ..
    } = sessions(state)?;
    let rejoin = match attach::connect(
        Path::new(&record.socket),
        &attach::hello(
            Purpose::Attach,
            ATTACH_VERSIONS,
            resume,
            owner::SESSION_SIZE,
        ),
        STEP,
    )? {
        Connected::Welcome(client) => client,
        Connected::Refused(reason) => return Err(format!("повторный attach h1: {reason}")),
    };
    let mut again = Attached::new(rejoin, Some(next_number));
    let deadline = Instant::now() + STEP;
    again.exchange(2, "ping", deadline)?;
    again.detach(Instant::now() + STEP)?;
    evidence.step(
        "restart",
        json!({
            "killed": { "signal": killed.signal(), "code": killed.code() },
            "stale_socket": stale,
            "stale_is_socket": stale_is_socket,
            "client_refusal": format!("{refusal:?}"),
            "sessions_alive": sessions_alive,
            "info": info_c,
            "socket_inode": socket_c,
            "registry": entries_c,
            "problems": problems_c,
            "transcript": again.transcript,
            "acks": again.acks,
        }),
    );
    check(killed.signal() == Some(libc::SIGKILL), || {
        format!("владелец вышел не от SIGKILL: {killed}")
    })?;
    check(stale == Some(socket_a) && stale_is_socket, || {
        format!("после SIGKILL путь сокета: {stale:?} (был {socket_a:?})")
    })?;
    check(
        refusal == Answer::Unavailable(Unavailable::Restarting) && Unavailable::Restarting.retry(),
        || format!("клиент без владельца получил {refusal:?}"),
    )?;
    check(sessions_alive == (true, true), || {
        format!("после SIGKILL владельца хост и нагрузка: {sessions_alive:?}")
    })?;
    check(
        info_c.key.pid == c_pid
            && info_c.key != a_key
            && info_c.stale_socket == Some(socket_a)
            && info_c.socket_inode == socket_c
            && socket_c != socket_a
            && info_c.lock_inode == lock_inode
            && Inode::of(&lock).is_ok_and(|inode| inode == lock_inode),
        || format!("новый владелец: {info_c:?}, сокет на диске {socket_c:?}, прежний {socket_a:?}"),
    )?;
    check(
        info_c.runtime.outcome == runtime::Outcome::Reused
            && Inode::of(&host_bin).is_ok_and(|inode| inode == host_runtime_inode),
        || format!("рантайм нового владельца: {:?}", info_c.runtime),
    )?;
    check(
        found_alive(&entries_c, &record) && problems_c.is_empty(),
        || format!("реестр нового владельца: {entries_c:?}, проблемы {problems_c:?}"),
    )?;
    check(
        again.transcript.problems.is_empty()
            && again.transcript.lines.first().map(|line| line.number) == Some(next_number)
            && again.transcript.receipt(2, "ping").is_some()
            && again.acks == [(2, Ack::Accepted)],
        || {
            format!(
                "хвост h1 после перезапуска: {:?}, ответы {:?}",
                again.transcript, again.acks
            )
        },
    )?;
    parts.push(Part5 {
        name: "restart",
        stimulus: "SIGKILL владельцу, ping клиента пробы без владельца, затем новый прямой запуск",
        oracle: "после SIGKILL путь сокета остаётся тем же узлом; клиент пробы получает явный временный отказ «владелец запускается или перезапускается» с признаком повтора; хост и нагрузка живы; новый владелец сообщает удалённый оставшийся путь (inode прежнего), публикует новый узел сокета, inode блокировки прежний, рантайм переиспользован без перезаписи; реестр с диска находит h1 с теми же процессами; хвост с прежнего байта без пропуска и квитанция «ack 2 ping»",
        reason: format!(
            "сокет inode {} → {}, блокировка inode {}, владелец {} → {}, h1: строки с {next_number}, квитанция 2",
            socket_a.ino, socket_c.ino, lock_inode.ino, a_key.pid, c_pid
        ),
        artifacts: vec![artifact("owner-c.stdout"), artifact("owner-c.stderr")],
    });

    // 5. Дескрипторы потомков.
    let record2 = match ask(
        state,
        &Request::CreateSession {
            name: "h2".to_owned(),
        },
    )? {
        Reply::Session(record) => *record,
        other => return Err(format!("CreateSession h2: {other:?}")),
    };
    guard.sessions.push(record2.clone());
    let owner_fds =
        sys::descriptors(c_pid).map_err(|err| format!("дескрипторы владельца: {err}"))?;
    let bind_path = owner::bind_path(state, c_pid as u32);
    let bind_prefix = state.join(owner::BIND_PREFIX).display().to_string();
    let listener = owner_fds.iter().find_map(|descriptor| match descriptor {
        Descriptor::Socket {
            handle,
            local: Some(local),
            ..
        } if Path::new(local) == bind_path => Some(*handle),
        _ => None,
    });
    let holds_lock = owner_fds.iter().any(|descriptor| {
        matches!(descriptor, Descriptor::Vnode { dev, ino, .. } if *dev == lock_inode.dev && *ino == lock_inode.ino)
    });
    let listener = listener.ok_or_else(|| {
        format!(
            "положительный контроль: у владельца нет слушающего сокета {}: {owner_fds:?}",
            bind_path.display()
        )
    })?;
    check(holds_lock, || {
        format!("положительный контроль: у владельца нет дескриптора блокировки: {owner_fds:?}")
    })?;
    let mut scanned = Vec::new();
    let mut leaks = Vec::new();
    for (label, rec) in [("h1", &record), ("h2", &record2)] {
        for (role, key) in [("host", rec.host), ("workload", rec.workload)] {
            let found = sys::descriptors(key.pid)
                .map_err(|err| format!("дескрипторы {label} {role} {}: {err}", key.pid))?;
            if role == "host" {
                let own = found.iter().any(|descriptor| {
                    matches!(descriptor, Descriptor::Socket { local: Some(local), .. } if *local == rec.socket)
                });
                check(own, || {
                    format!(
                        "положительный контроль: у хоста {label} нет своего сокета {}: {found:?}",
                        rec.socket
                    )
                })?;
            }
            for leak in owner_leaks(&found, lock_inode, &socket, &bind_prefix, listener) {
                leaks.push(format!("{label} {role} {}: {leak}", key.pid));
            }
            scanned.push(
                json!({ "session": label, "role": role, "pid": key.pid, "descriptors": found }),
            );
        }
    }
    evidence.step(
        "descriptors",
        json!({
            "owner": owner_fds,
            "listener_handle": format!("{listener:#x}"),
            "scanned": scanned,
            "leaks": leaks,
        }),
    );
    check(leaks.is_empty(), || {
        format!("унаследованные дескрипторы: {leaks:?}")
    })?;
    parts.push(Part5 {
        name: "descriptors",
        stimulus: "proc_pidfdinfo у хоста и нагрузки h1 (запущены прежним владельцем) и h2 (запущены новым)",
        oracle: "положительный контроль: у владельца есть дескриптор блокировки (dev и inode) и слушающий сокет с адресом bind, у каждого хоста — свой сокет сессии; у хостов и нагрузок нет ни дескриптора блокировки, ни сокета с адресом сокета владельца, ни дескриптора того же сокета ядра, что слушающий у владельца",
        reason: format!(
            "проверено 4 процесса, дескрипторов владельца нет; у владельца блокировка inode {} и слушающий сокет {listener:#x}",
            lock_inode.ino
        ),
        artifacts: Vec::new(),
    });

    // Уборка: Quit владельца, остановка сессий, каталог состояния.
    let quit = ask(state, &Request::Quit)?;
    let quit_status = reap(&mut guard.apps[1], "владелец после Quit")?;
    let c_reports = reports(out, "owner-c")?;
    let socket_left = fs::symlink_metadata(&socket).is_ok();
    let lock_after = Inode::of(&lock).ok();
    let mut stopped = Vec::new();
    for rec in [&record, &record2] {
        let mut control = match attach::connect(
            Path::new(&rec.socket),
            &attach::hello(Purpose::Control, ATTACH_VERSIONS, 0, owner::SESSION_SIZE),
            STEP,
        )? {
            Connected::Welcome(client) => client,
            Connected::Refused(reason) => return Err(format!("control {}: {reason}", rec.session)),
        };
        control.stop()?;
        stopped.extend([rec.host.pid, rec.workload.pid]);
    }
    let still = launch::wait_exit(&stopped, STEP);
    evidence.step(
        "quit",
        json!({
            "reply": format!("{quit:?}"),
            "exit": quit_status.code(),
            "reports": c_reports,
            "socket_left": socket_left,
            "lock_inode": lock_after,
            "sessions_alive_after_stop": still,
        }),
    );
    check(
        quit == Reply::Done
            && quit_status.success()
            && matches!(c_reports.last(), Some(AppReport::Stopped { key }) if *key == info_c.key),
        || format!("Quit: {quit:?}, {quit_status}, {c_reports:?}"),
    )?;
    check(!socket_left && lock_after == Some(lock_inode), || {
        format!("после Quit: сокет остался {socket_left}, блокировка {lock_after:?}")
    })?;
    check(still.is_empty(), || {
        format!("сессии не остановились: {still:?}")
    })?;
    guard.sessions.clear();
    guard.apps.clear();
    clear_state(state);
    let left: Vec<String> = fs::read_dir(state)
        .map_err(|err| format!("{}: {err}", state.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    check(left.is_empty(), || {
        format!("каталог состояния не очистился: {left:?}")
    })?;
    guard.armed = false;
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaks_are_found_by_lock_inode_socket_address_and_kernel_handle() {
        let lock = Inode { dev: 1, ino: 2 };
        let socket = Path::new("/s/shellio.sock");
        let prefix = "/s/.sock.";
        let clean = vec![
            Descriptor::Vnode {
                fd: 3,
                dev: 1,
                ino: 3,
                path: "/s/other".to_owned(),
            },
            Descriptor::Socket {
                fd: 4,
                family: libc::AF_UNIX,
                handle: 9,
                local: Some("/s/s/h1.sock".to_owned()),
                peer: None,
            },
            Descriptor::Socket {
                fd: 5,
                family: libc::AF_UNIX,
                handle: 10,
                local: None,
                peer: Some("/s/shellio.sock".to_owned()),
            },
        ];
        assert!(owner_leaks(&clean, lock, socket, prefix, 77).is_empty());
        let leaky = vec![
            Descriptor::Vnode {
                fd: 6,
                dev: 1,
                ino: 2,
                path: "/s/shellio.lock".to_owned(),
            },
            Descriptor::Socket {
                fd: 7,
                family: libc::AF_UNIX,
                handle: 11,
                local: Some("/s/.sock.123".to_owned()),
                peer: None,
            },
            Descriptor::Socket {
                fd: 8,
                family: libc::AF_UNIX,
                handle: 77,
                local: None,
                peer: None,
            },
        ];
        let leaks = owner_leaks(&leaky, lock, socket, prefix, 77);
        assert_eq!(leaks.len(), 3, "{leaks:?}");
    }

    #[test]
    fn parts_become_valid_stand_cells() {
        let parts = vec![Part5 {
            name: "owner",
            stimulus: "s",
            oracle: "o",
            reason: "r".to_owned(),
            artifacts: vec![artifact("owner-a.stdout")],
        }];
        let report = report(&parts);
        assert_eq!(report.cells.len(), 1);
        assert_eq!(report.cells[0].id, OWNER_BASIC_CELL);
        assert!(
            report.cells[0].problems().is_empty(),
            "{:?}",
            report.cells[0].problems()
        );
        assert_eq!(report.cells[0].artifacts[0], STEPS_ARTIFACT);
    }

    #[test]
    fn owner_socket_and_bind_name_fit_under_a_launch_state_dir() {
        // Каталог состояния строится так же, как в `launch`: канонический $TMPDIR и psu.<nonce>.
        let state = fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(launch::state_dir_name("0123456789abcdef"));
        let socket = owner::socket_path(&state);
        assert!(
            host::check_socket_path(&socket).is_ok(),
            "{}",
            socket.display()
        );
        let bind = owner::bind_path(&state, i32::MAX as u32);
        assert!(host::check_socket_path(&bind).is_ok(), "{}", bind.display());
    }
}
