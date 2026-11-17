//! Режим `--launch`: процесс сценария с окружением, собранным с нуля, временным
//! `SHELLIO_STATE_DIR` и внешним сторожем; здесь же проверка этого окружения в самом процессе.
//!
//! Внешний сторож ловит то, чего процесс сценария сам не видит: зависание до `result.json`,
//! падение и процессы, оставшиеся после выхода. Процессы стенда узнаются по nonce в аргументах.

use std::collections::BTreeMap;
use std::ffi::c_int;
use std::fs;
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use crate::args::Launch;
use crate::results::{self, Execution, ExitRecord, LaunchRecord, ProcessRecord};
use crate::{EXIT_EXECUTION, say, sys};

/// Запас внешнего сторожа сверх срока сценария: запуск процесса и запись итога.
const MARGIN: Duration = Duration::from_secs(10);
const COMMAND_LIMIT: usize = 240;

/// Сроки внешнего сторожа после срока сценария.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Grace {
    /// Сколько ждать выхода после `SIGKILL` от сторожа.
    pub kill: Duration,
    /// Сколько процессы с nonce могут завершаться после выхода процесса сценария.
    pub leftover: Duration,
}

pub const GRACE: Grace = Grace {
    kill: Duration::from_secs(5),
    leftover: Duration::from_secs(3),
};

/// Переменные, которые берутся из окружения запуска как есть.
pub const INHERITED: [&str; 6] = ["HOME", "USER", "LOGNAME", "PATH", "TMPDIR", "SHELL"];
/// Без них процесс сценария не запускается.
const REQUIRED: [&str; 2] = ["HOME", "PATH"];
pub const STATE_DIR: &str = "SHELLIO_STATE_DIR";
pub const LANG: &str = "LANG";
pub const LANG_VALUE: &str = "en_US.UTF-8";
/// Сведения о сборке, которые `run.sh` передаёт запуску; процесс сценария git и rustc не вызывает.
pub const BUILD_COMMIT: &str = "SHELLIO_PROBE_COMMIT";
pub const BUILD_RUSTC: &str = "SHELLIO_PROBE_RUSTC";
const BUILD_VARS: [&str; 2] = [BUILD_COMMIT, BUILD_RUSTC];
/// Версии наборов пробы и их Zig, которые `run.sh` снимает перед сценариями: переменная и ключ
/// `environment.versions`. Имена не начинаются с `ZMX_` и `GHOSTTY_`: такие переменные запрещены.
pub const SET_VERSIONS: [(&str, &str); 5] = [
    ("SHELLIO_PROBE_VERSION_ZIG_ZMX", "zig-zmx"),
    ("SHELLIO_PROBE_VERSION_ZMX", "zmx"),
    ("SHELLIO_PROBE_VERSION_ZMX_PREV", "zmx-prev"),
    ("SHELLIO_PROBE_VERSION_ZIG_VT", "zig-vt"),
    ("SHELLIO_PROBE_VERSION_VT", "vt"),
];
/// SHA-256 штампов наборов пробы: переменная и ключ `environment.stamps`.
pub const SET_STAMPS: [(&str, &str); 3] = [
    ("SHELLIO_PROBE_STAMP_ZMX", "zmx"),
    ("SHELLIO_PROBE_STAMP_ZMX_PREV", "zmx-prev"),
    ("SHELLIO_PROBE_STAMP_VT", "vt"),
];

/// Ревизия и SHA-256 штампа продуктового набора ghostty, из которого собрана поверхность:
/// `run.sh surface` передаёт их только сценариям поверхности, headless-сценарии их не требуют.
pub const SURFACE_FACTS: [(&str, &str); 2] = [
    ("SHELLIO_PROBE_VERSION_SURFACE", "ghostty-surface"),
    ("SHELLIO_PROBE_STAMP_SURFACE", "ghostty-surface"),
];

/// Каталог наборов пробы, который `run.sh` собрал или сверил: из него процесс сценария берёт
/// бинари zmx. Сам каталог в `environment` не попадает, его содержимое описывают штампы.
pub const SETS_DIR: &str = "SHELLIO_PROBE_SETS_DIR";

/// Хост предыдущего поколения для G1 и G2 Rust-хоста: `probe-session-host`, который `run.sh`
/// собрал с функцией `proto-v1` в отдельный каталог cargo.
pub const HOST_V1: &str = "SHELLIO_PROBE_HOST_V1";

/// Каталог текущих `probe-session-host` и `probe-session-attach` для сценариев поверхности: бандл
/// их не содержит, `run.sh` собирает хост с функцией `vt` отдельным вызовом cargo.
pub const HOST_DIR: &str = "SHELLIO_PROBE_HOST_DIR";

/// Переменные `run.sh`, которые запуск передаёт процессу сценария как есть, если они не пусты.
fn passed_vars() -> impl Iterator<Item = &'static str> {
    BUILD_VARS
        .into_iter()
        .chain(SET_VERSIONS.into_iter().map(|(name, _)| name))
        .chain(SET_STAMPS.into_iter().map(|(name, _)| name))
        .chain(SURFACE_FACTS.into_iter().map(|(name, _)| name))
        .chain([SETS_DIR, HOST_V1, HOST_DIR])
}
/// Префиксы, которых не бывает в окружении процесса сценария: терминал и ghostty, nvim и
/// настоящие сессии пользователя.
pub const FORBIDDEN_PREFIXES: [&str; 6] = ["GHOSTTY_", "TERM", "LC_", "AGTERM_", "NVIM", "ZMX_"];
const STATE_DIR_PREFIX: &str = "shellio-probe-state.";

/// Имя временного `SHELLIO_STATE_DIR`, который создаёт запуск с этим nonce.
pub fn state_dir_name(nonce: &str) -> String {
    format!("{STATE_DIR_PREFIX}{nonce}")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnvError {
    Missing(&'static str),
}

impl std::fmt::Display for EnvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EnvError::Missing(name) => write!(f, "в окружении запуска нет {name}"),
        }
    }
}

/// Окружение процесса сценария. `inherited` читает окружение запуска.
pub fn build_env(
    inherited: impl Fn(&str) -> Option<String>,
    state_dir: &str,
) -> Result<Vec<(String, String)>, EnvError> {
    let mut env = Vec::new();
    for name in INHERITED {
        match inherited(name).filter(|value| !value.is_empty()) {
            Some(value) => env.push((name.to_owned(), value)),
            None if REQUIRED.contains(&name) => return Err(EnvError::Missing(name)),
            None => {}
        }
    }
    env.push((STATE_DIR.to_owned(), state_dir.to_owned()));
    env.push((LANG.to_owned(), LANG_VALUE.to_owned()));
    for name in passed_vars() {
        if let Some(value) = inherited(name).filter(|value| !value.is_empty()) {
            env.push((name.to_owned(), value));
        }
    }
    Ok(env)
}

/// Итог проверки фактического окружения процесса сценария.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Check {
    /// Нарушения предусловия: процесс запущен не через `--launch` или окружение чужое.
    pub violations: Vec<String>,
    /// Переменные вне белого списка, которые не запрещены.
    pub extra: Vec<String>,
}

/// Сверяет окружение процесса сценария с тем, что строит `--launch`.
pub fn check_env(env: &[(String, String)], nonce: Option<&str>) -> Check {
    let mut result = Check::default();
    let lookup = |name: &str| {
        env.iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };
    match (nonce, lookup(STATE_DIR)) {
        (None, _) => result
            .violations
            .push("нет --nonce: процесс запущен не через --launch".to_owned()),
        (Some(_), None) => result.violations.push(format!(
            "нет {STATE_DIR}: процесс запущен не через --launch"
        )),
        (Some(nonce), Some(state_dir)) => {
            let expected = state_dir_name(nonce);
            let path = Path::new(state_dir);
            let name = path.file_name().and_then(|name| name.to_str());
            if !path.is_absolute() || name != Some(expected.as_str()) {
                result.violations.push(format!(
                    "{STATE_DIR}={state_dir}: не временный каталог запуска {expected}"
                ));
            }
        }
    }
    if lookup(LANG) != Some(LANG_VALUE) {
        result
            .violations
            .push(format!("{LANG}={:?}, ожидается {LANG_VALUE}", lookup(LANG)));
    }
    for (name, _) in env {
        if let Some(prefix) = FORBIDDEN_PREFIXES
            .iter()
            .find(|prefix| name.starts_with(*prefix))
        {
            result
                .violations
                .push(format!("{name}: запрещённый префикс {prefix}"));
        } else if !INHERITED.contains(&name.as_str())
            && !passed_vars().any(|passed| passed == name)
            && name != STATE_DIR
            && name != LANG
        {
            result.extra.push(name.clone());
        }
    }
    result
}

/// Программа и аргументы процесса сценария.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub program: PathBuf,
    pub args: Vec<String>,
}

/// Аргументы процесса сценария: заданные после `--` и nonce запуска.
pub fn probe_args(args: &[String], nonce: &str) -> Vec<String> {
    let mut all = args.to_vec();
    all.extend(["--nonce".to_owned(), nonce.to_owned()]);
    all
}

/// Аргументы процесса сценария с абсолютным `--out`.
pub fn absolute_out(args: &[String], out: &Path) -> Vec<String> {
    let mut args = args.to_vec();
    if let Some(index) = args.iter().position(|arg| arg == "--out")
        && let Some(value) = args.get_mut(index + 1)
    {
        *value = out.display().to_string();
    }
    args
}

/// Разбирает ответ `KERN_PROCARGS2`: `argc`, путь исполняемого файла, выравнивающие нули и
/// `argc` строк аргументов. Окружение после аргументов не читается.
pub fn parse_procargs(bytes: &[u8]) -> Option<Vec<String>> {
    let argc = i32::from_ne_bytes(bytes.get(..4)?.try_into().ok()?);
    let argc = usize::try_from(argc).ok()?;
    let rest = &bytes[4..];
    let path_end = rest.iter().position(|byte| *byte == 0)?;
    let mut cursor = path_end;
    while rest.get(cursor) == Some(&0) {
        cursor += 1;
    }
    let mut args = Vec::with_capacity(argc);
    for _ in 0..argc {
        let tail = rest.get(cursor..)?;
        let end = tail
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(tail.len());
        args.push(String::from_utf8_lossy(&tail[..end]).into_owned());
        cursor += end + 1;
    }
    Some(args)
}

/// Аргументы содержат nonce.
pub fn carries_nonce(args: &[String], nonce: &str) -> bool {
    args.iter().any(|arg| arg.contains(nonce))
}

fn command_line(args: &[String]) -> String {
    let line = args.join(" ");
    match line.char_indices().nth(COMMAND_LIMIT) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line,
    }
}

/// Причины ошибки исполнения по фактам запуска; пусто — запуск чистый. Код выхода процесса
/// сценария сюда не входит: его ошибку исполнения несёт `result.json`.
pub fn judge(
    exit: Option<ExitRecord>,
    watchdog: Option<f64>,
    result_present: bool,
    leftovers: &[ProcessRecord],
    state_dir_leftovers: &[String],
) -> Vec<String> {
    let mut reasons = Vec::new();
    if let Some(deadline) = watchdog {
        reasons.push(format!(
            "внешний сторож: процесс не завершился за {deadline} с"
        ));
    }
    match exit {
        None => reasons.push("процесс не завершился после SIGKILL".to_owned()),
        Some(ExitRecord {
            signal: Some(signal),
            ..
        }) if watchdog.is_none() => reasons.push(format!("падение: сигнал {signal}")),
        Some(_) => {}
    }
    if !result_present && watchdog.is_none() {
        reasons.push(format!("{} не появился", results::RESULT_FILE));
    }
    if !leftovers.is_empty() {
        let pids: Vec<String> = leftovers.iter().map(|p| p.pid.to_string()).collect();
        reasons.push(format!(
            "после выхода остались процессы с nonce: {}",
            pids.join(", ")
        ));
    }
    if !state_dir_leftovers.is_empty() {
        reasons.push(format!(
            "{STATE_DIR} не пуст после выхода: {}",
            state_dir_leftovers.join(", ")
        ));
    }
    reasons
}

pub fn run(launch: Launch, program: &Path) -> ExitCode {
    match execute(&launch, program) {
        Ok(code) => code,
        Err(reason) => {
            say(&format!("launch: execution=error\nlaunch: reason={reason}"));
            ExitCode::from(EXIT_EXECUTION)
        }
    }
}

fn execute(launch: &Launch, program: &Path) -> Result<ExitCode, String> {
    let target = &launch.target;
    let out = fs::canonicalize(&target.out)
        .ok()
        .filter(|out| out.is_dir())
        .ok_or_else(|| format!("--out {}: каталога нет", target.out.display()))?;
    let deadline = target.scenario.deadline() + MARGIN;
    let nonce = nonce()?;
    let tmp = std::env::var("TMPDIR")
        .ok()
        .filter(|tmp| !tmp.is_empty())
        .unwrap_or_else(|| "/tmp".to_owned());
    let state_dir = fs::canonicalize(&tmp)
        .map_err(|err| format!("TMPDIR {tmp}: {err}"))?
        .join(state_dir_name(&nonce));
    fs::create_dir(&state_dir)
        .map_err(|err| format!("{STATE_DIR} {}: {err}", state_dir.display()))?;
    let plan = Plan {
        program: program.to_path_buf(),
        args: probe_args(&absolute_out(&launch.args, &out), &nonce),
    };
    let outcome = build_env(
        |name| std::env::var(name).ok(),
        &state_dir.display().to_string(),
    )
    .map_err(|err| err.to_string())
    .and_then(|env| supervise(&plan, &env, Some(deadline), &nonce, GRACE).map(|done| (env, done)));
    let state_dir_leftovers = list_dir(&state_dir);
    let _ = fs::remove_dir_all(&state_dir);
    restore_input_sources(&out);
    let mut record = LaunchRecord {
        schema: results::SCHEMA,
        scenario: target.scenario.name().to_owned(),
        backend: target.backend,
        variant: target.variant.clone(),
        program: plan.program.display().to_string(),
        args: plan.args.clone(),
        env: BTreeMap::new(),
        nonce,
        state_dir: state_dir.display().to_string(),
        spawn_uptime_ns: 0,
        exit_uptime_ns: None,
        exit: None,
        deadline_s: deadline.as_secs_f64(),
        watchdog_fired: false,
        killed: Vec::new(),
        leftovers: Vec::new(),
        state_dir_leftovers,
        result_present: false,
        execution: Execution::Ok,
    };
    let (env, supervised) = match outcome {
        Ok(done) => done,
        Err(reason) => {
            // Без launch.json агрегирование сочло бы процесс запущенным мимо --launch.
            record.execution = Execution::error(reason.clone());
            results::write_launch(&out, &record)?;
            return Err(reason);
        }
    };
    record.env = env.into_iter().collect();
    record.spawn_uptime_ns = supervised.spawn_ns;
    record.exit_uptime_ns = supervised.exit_ns;
    record.exit = supervised.exit;
    record.watchdog_fired = supervised.watchdog_fired;
    record.killed = supervised.killed;
    record.leftovers = supervised.leftovers;
    record.result_present = out.join(results::RESULT_FILE).is_file();
    let reasons = judge(
        record.exit,
        record.watchdog_fired.then_some(record.deadline_s),
        record.result_present,
        &record.leftovers,
        &record.state_dir_leftovers,
    );
    if !reasons.is_empty() {
        record.execution = Execution::error(reasons.join("; "));
    }
    results::write_launch(&out, &record)?;
    say(&describe(&record));
    Ok(exit_code(&record))
}

/// Источник ввода, который GUI прохода F4 выбрала и не вернула (процесс снят сторожем или
/// `SIGKILL`): возврат по файлам состояния в каталогах GUI.
#[cfg(feature = "surface")]
fn restore_input_sources(out: &Path) {
    for note in crate::surface::input::restore_all(out) {
        say(&format!("{}: --launch: {note}", crate::PROBE_NAME));
    }
}

#[cfg(not(feature = "surface"))]
fn restore_input_sources(_out: &Path) {}

/// Код выхода запуска: ошибка запуска — `EXIT_EXECUTION`, иначе код процесса сценария.
fn exit_code(record: &LaunchRecord) -> ExitCode {
    if !record.execution.is_ok() {
        return ExitCode::from(EXIT_EXECUTION);
    }
    match record.exit.and_then(|exit| exit.code) {
        Some(code) => ExitCode::from(u8::try_from(code).unwrap_or(EXIT_EXECUTION)),
        None => ExitCode::from(EXIT_EXECUTION),
    }
}

/// Факты запуска процесса сценария.
#[derive(Debug)]
pub struct Supervised {
    pub spawn_ns: u64,
    pub exit_ns: Option<u64>,
    pub exit: Option<ExitRecord>,
    pub watchdog_fired: bool,
    /// Процессы, которые снял сторож.
    pub killed: Vec<ProcessRecord>,
    /// Процессы с nonce, оставшиеся после выхода и снятые запуском.
    pub leftovers: Vec<ProcessRecord>,
}

/// Запускает процесс с окружением `env` и ждёт его не дольше `deadline`. По истечении срока
/// снимает его и все процессы с nonce; после выхода ищет оставшиеся процессы с nonce.
pub fn supervise(
    plan: &Plan,
    env: &[(String, String)],
    deadline: Option<Duration>,
    nonce: &str,
    grace: Grace,
) -> Result<Supervised, String> {
    let mut command = Command::new(&plan.program);
    command
        .args(&plan.args)
        .env_clear()
        .envs(env.iter().map(|(name, value)| (name, value)))
        .stdin(Stdio::null());
    let spawn_ns = sys::uptime_ns();
    let mut child = command
        .spawn()
        .map_err(|err| format!("{} не запущен: {err}", plan.program.display()))?;
    let child_pid = child.id() as i32;
    let (sender, receiver) = mpsc::channel::<(io::Result<ExitStatus>, u64)>();
    thread::Builder::new()
        .name("probe-launch-wait".to_owned())
        .spawn(move || {
            let status = child.wait();
            let _ = sender.send((status, sys::uptime_ns()));
        })
        .map_err(|err| format!("поток ожидания не запущен: {err}"))?;

    let mut watchdog_fired = false;
    let mut killed = Vec::new();
    let waited = match deadline {
        Some(deadline) => match receiver.recv_timeout(deadline) {
            Ok(done) => Some(done),
            Err(RecvTimeoutError::Timeout) => {
                watchdog_fired = true;
                killed = kill_all(child_pid, nonce);
                receiver.recv_timeout(grace.kill).ok()
            }
            Err(RecvTimeoutError::Disconnected) => None,
        },
        None => receiver.recv().ok(),
    };
    let (exit, exit_ns) = match waited {
        Some((Ok(status), at)) => (
            Some(ExitRecord {
                code: status.code(),
                signal: status.signal(),
            }),
            Some(at),
        ),
        Some((Err(err), _)) => return Err(format!("ожидание процесса: {err}")),
        None => (None, None),
    };
    let leftovers = reap_leftovers(nonce, grace.leftover);
    Ok(Supervised {
        spawn_ns,
        exit_ns,
        exit,
        watchdog_fired,
        killed,
        leftovers,
    })
}

fn describe(record: &LaunchRecord) -> String {
    let exit = match record.exit {
        Some(ExitRecord {
            code: Some(code), ..
        }) => format!("code {code}"),
        Some(ExitRecord {
            signal: Some(signal),
            ..
        }) => format!("signal {signal}"),
        _ => "none".to_owned(),
    };
    let mut text = format!(
        "launch: nonce={} exit={exit} watchdog={} killed={} leftovers={}",
        record.nonce,
        record.watchdog_fired,
        record.killed.len(),
        record.leftovers.len()
    );
    match &record.execution {
        Execution::Ok => text.push_str("\nlaunch: execution=ok"),
        Execution::Error { reason, .. } => {
            text.push_str(&format!(
                "\nlaunch: execution=error\nlaunch: reason={reason}"
            ));
        }
    }
    text
}

pub fn nonce() -> Result<String, String> {
    let mut bytes = [0u8; 8];
    // SAFETY: буфер на стеке длиной 8 байт, `getentropy` пишет не больше указанной длины.
    let status = unsafe { libc::getentropy(bytes.as_mut_ptr().cast(), bytes.len()) };
    if status != 0 {
        return Err(format!("getentropy: {}", io::Error::last_os_error()));
    }
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn list_dir(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Сколько раз перечитываются аргументы своего процесса, которые не прочитались: посреди `exec`
/// ядро `KERN_PROCARGS2` не отдаёт.
const ARGS_RETRIES: usize = 1000;

/// Процессы с nonce в аргументах, кроме самого запуска.
fn find_by_nonce(nonce: &str) -> Vec<ProcessRecord> {
    find_by_token(nonce)
}

/// Процессы со строкой `token` в аргументах, кроме вызывающего.
pub fn find_by_token(token: &str) -> Vec<ProcessRecord> {
    let own = std::process::id() as i32;
    // SAFETY: вызов без аргументов.
    let uid = unsafe { libc::getuid() };
    let mut found = Vec::new();
    let mut unread = Vec::new();
    let take = |pid: i32, args: &[String], found: &mut Vec<ProcessRecord>| {
        if carries_nonce(args, token) {
            found.push(ProcessRecord {
                pid,
                command: command_line(args),
            });
        }
    };
    for pid in all_pids().into_iter().filter(|pid| *pid > 0 && *pid != own) {
        match process_args(pid) {
            Some(args) => take(pid, &args, &mut found),
            None if owner(pid) == Some(uid) => unread.push(pid),
            None => {}
        }
    }
    for _ in 0..ARGS_RETRIES {
        if unread.is_empty() {
            break;
        }
        std::thread::yield_now();
        unread.retain(|pid| match process_args(*pid) {
            Some(args) => {
                take(*pid, &args, &mut found);
                false
            }
            None => owner(*pid) == Some(uid),
        });
    }
    found
}

/// Владелец процесса; `None`, если процесса уже нет.
fn owner(pid: i32) -> Option<libc::uid_t> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdshortinfo>::uninit();
    let size = c_int::try_from(size_of::<libc::proc_bsdshortinfo>()).ok()?;
    // SAFETY: для `PROC_PIDT_SHORTBSDINFO` ядро пишет `proc_bsdshortinfo` размера `size`.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDT_SHORTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    // SAFETY: при полном ответе структура записана целиком.
    (written == size).then(|| unsafe { info.assume_init() }.pbsi_uid)
}

/// Снимает прямого потомка по PID, ведь он мог сменить argv через `exec`, и все процессы с
/// nonce.
fn kill_all(child: i32, nonce: &str) -> Vec<ProcessRecord> {
    let mut found = find_by_nonce(nonce);
    if !found.iter().any(|process| process.pid == child) {
        found.push(ProcessRecord {
            pid: child,
            command: process_args(child).map_or_else(|| "?".to_owned(), |args| command_line(&args)),
        });
    }
    for process in &found {
        // SAFETY: сигнал процессу по PID, памяти вызов не касается.
        unsafe { libc::kill(process.pid, libc::SIGKILL) };
    }
    found
}

/// Ждёт выхода процессов с nonce до `grace`, оставшихся снимает и возвращает.
fn reap_leftovers(nonce: &str, grace: Duration) -> Vec<ProcessRecord> {
    let found = find_by_nonce(nonce);
    if found.is_empty() {
        return found;
    }
    let pids: Vec<i32> = found.iter().map(|process| process.pid).collect();
    let alive = wait_exit(&pids, grace);
    let survivors: Vec<ProcessRecord> = found
        .into_iter()
        .filter(|process| alive.contains(&process.pid))
        .collect();
    for process in &survivors {
        // SAFETY: сигнал процессу по PID, памяти вызов не касается.
        unsafe { libc::kill(process.pid, libc::SIGKILL) };
    }
    survivors
}

fn all_pids() -> Vec<i32> {
    // SAFETY: без буфера вызов возвращает оценку числа процессов.
    let estimate = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    let Ok(estimate) = usize::try_from(estimate) else {
        return Vec::new();
    };
    let mut pids = vec![0i32; estimate + 64];
    let bytes = c_int::try_from(pids.len() * size_of::<i32>()).unwrap_or(c_int::MAX);
    // SAFETY: буфер длиной `bytes` байт жив на время вызова.
    let count = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    let Ok(count) = usize::try_from(count) else {
        return Vec::new();
    };
    pids.truncate(count.min(pids.len()));
    pids
}

/// Аргументы процесса по `KERN_PROCARGS2`; `None`, если ядро их не отдало.
pub fn process_args(pid: i32) -> Option<Vec<String>> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size = 0usize;
    // SAFETY: первый вызов узнаёт размер ответа, буфер не передаётся.
    let status = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if status != 0 || size == 0 {
        return None;
    }
    let mut buffer = vec![0u8; size];
    // SAFETY: буфер длиной `size` жив на время вызова.
    let status = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buffer.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if status != 0 {
        return None;
    }
    buffer.truncate(size);
    parse_procargs(&buffer)
}

/// Ждёт выхода процессов через `kqueue` не дольше `timeout`; возвращает тех, кто ещё жив.
pub fn wait_exit(pids: &[i32], timeout: Duration) -> Vec<i32> {
    // SAFETY: `kqueue` без аргументов возвращает дескриптор или -1.
    let queue = unsafe { libc::kqueue() };
    if queue < 0 {
        return pids.to_vec();
    }
    let mut alive = Vec::new();
    for &pid in pids {
        let change = libc::kevent {
            ident: pid as usize,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD | libc::EV_ONESHOT,
            fflags: libc::NOTE_EXIT,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        // SAFETY: одно изменение на стеке, список событий не передаётся.
        let status =
            unsafe { libc::kevent(queue, &change, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
        // Процесс, вышедший до регистрации, kqueue отвергает с ESRCH.
        if status == 0 {
            alive.push(pid);
        }
    }
    let until = Instant::now() + timeout;
    while !alive.is_empty() {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let wait = libc::timespec {
            tv_sec: left.as_secs() as libc::time_t,
            tv_nsec: libc::c_long::from(left.subsec_nanos()),
        };
        let mut event = libc::kevent {
            ident: 0,
            filter: 0,
            flags: 0,
            fflags: 0,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        // SAFETY: одно событие на стеке и срок ожидания живут на время вызова.
        let count = unsafe { libc::kevent(queue, std::ptr::null(), 0, &mut event, 1, &wait) };
        if count < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if count == 0 {
            break;
        }
        alive.retain(|pid| *pid as usize != event.ident);
    }
    // SAFETY: дескриптор очереди открыт этой функцией и больше не используется.
    unsafe { libc::close(queue) };
    alive
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    fn pairs(env: &[(&str, &str)]) -> Vec<(String, String)> {
        env.iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn probe_args_append_nonce_and_out_is_absolute() {
        let args = strings(&["--scenario", "smoke", "--out", "rel/o", "--variant", "base"]);
        assert_eq!(
            probe_args(&absolute_out(&args, Path::new("/abs/o")), "ab12"),
            strings(&[
                "--scenario",
                "smoke",
                "--out",
                "/abs/o",
                "--variant",
                "base",
                "--nonce",
                "ab12"
            ])
        );
        assert_eq!(
            absolute_out(&strings(&["--out"]), Path::new("/abs/o")),
            strings(&["--out"])
        );
    }

    #[test]
    fn environment_is_built_from_allow_list() {
        let parent = |name: &str| match name {
            "HOME" => Some("/Users/t".to_owned()),
            "PATH" => Some("/usr/bin:/bin".to_owned()),
            "TMPDIR" => Some(String::new()),
            "TERM" => Some("xterm-ghostty".to_owned()),
            "ZMX_DIR" => Some("/Users/t/.zmx".to_owned()),
            BUILD_COMMIT => Some("a9cbb05".to_owned()),
            _ => None,
        };
        let env = build_env(parent, "/tmp/shellio-probe-state.n1").unwrap();
        assert_eq!(
            env,
            pairs(&[
                ("HOME", "/Users/t"),
                ("PATH", "/usr/bin:/bin"),
                (STATE_DIR, "/tmp/shellio-probe-state.n1"),
                (LANG, LANG_VALUE),
                (BUILD_COMMIT, "a9cbb05"),
            ])
        );
        assert_eq!(
            build_env(|_| None, "/tmp/s"),
            Err(EnvError::Missing("HOME"))
        );
    }

    #[test]
    fn launched_environment_passes_the_check() {
        let parent = |name: &str| match name {
            "HOME" => Some("/Users/t".to_owned()),
            "PATH" => Some("/usr/bin".to_owned()),
            _ => None,
        };
        let env = build_env(parent, "/tmp/shellio-probe-state.n1").unwrap();
        assert_eq!(check_env(&env, Some("n1")), Check::default());
    }

    #[test]
    fn set_facts_pass_through_launch() {
        let parent = |name: &str| match name {
            "HOME" => Some("/Users/t".to_owned()),
            "PATH" => Some("/usr/bin".to_owned()),
            "SHELLIO_PROBE_VERSION_ZMX" => Some("zmx 0.8.1; ghostty_vt g".to_owned()),
            "SHELLIO_PROBE_VERSION_ZMX_PREV" => Some(String::new()),
            "SHELLIO_PROBE_STAMP_ZMX_PREV" => Some("ab12".to_owned()),
            "SHELLIO_PROBE_STAMP_VT" => Some("cd34".to_owned()),
            "SHELLIO_PROBE_STAMP_UNKNOWN" => Some("ef56".to_owned()),
            _ => None,
        };
        let env = build_env(parent, "/tmp/shellio-probe-state.n1").unwrap();
        assert_eq!(
            env[env.len() - 3..],
            pairs(&[
                ("SHELLIO_PROBE_VERSION_ZMX", "zmx 0.8.1; ghostty_vt g"),
                ("SHELLIO_PROBE_STAMP_ZMX_PREV", "ab12"),
                ("SHELLIO_PROBE_STAMP_VT", "cd34"),
            ])
        );
        assert_eq!(check_env(&env, Some("n1")), Check::default());
        for (name, key) in SET_VERSIONS.iter().chain(SET_STAMPS.iter()) {
            assert!(name.starts_with("SHELLIO_PROBE_"), "{name}");
            assert!(!key.is_empty());
        }
    }

    #[test]
    fn sets_dir_passes_through_launch() {
        let parent = |name: &str| match name {
            "HOME" => Some("/Users/t".to_owned()),
            "PATH" => Some("/usr/bin".to_owned()),
            SETS_DIR => Some("/r/target/probes/session-survival".to_owned()),
            _ => None,
        };
        let env = build_env(parent, "/tmp/shellio-probe-state.n1").unwrap();
        assert_eq!(
            env.last(),
            Some(&(
                SETS_DIR.to_owned(),
                "/r/target/probes/session-survival".to_owned()
            ))
        );
        assert_eq!(check_env(&env, Some("n1")), Check::default());
        assert!(!SETS_DIR.starts_with("ZMX_") && !SETS_DIR.starts_with("GHOSTTY_"));
    }

    #[test]
    fn previous_host_generation_passes_through_launch() {
        let parent = |name: &str| match name {
            "HOME" => Some("/Users/t".to_owned()),
            "PATH" => Some("/usr/bin".to_owned()),
            HOST_V1 => Some("/r/target/proto-v1/release/probe-session-host".to_owned()),
            _ => None,
        };
        let env = build_env(parent, "/tmp/shellio-probe-state.n1").unwrap();
        assert!(env.contains(&(
            HOST_V1.to_owned(),
            "/r/target/proto-v1/release/probe-session-host".to_owned()
        )));
        assert_eq!(check_env(&env, Some("n1")), Check::default());
    }

    #[test]
    fn host_dir_passes_through_launch() {
        let parent = |name: &str| match name {
            "HOME" => Some("/Users/t".to_owned()),
            "PATH" => Some("/usr/bin".to_owned()),
            HOST_DIR => Some("/r/target/release".to_owned()),
            _ => None,
        };
        let env = build_env(parent, "/tmp/shellio-probe-state.n1").unwrap();
        assert!(env.contains(&(HOST_DIR.to_owned(), "/r/target/release".to_owned())));
        assert_eq!(check_env(&env, Some("n1")), Check::default());
    }

    #[test]
    fn foreign_environment_is_a_violation() {
        let check = check_env(
            &pairs(&[
                (STATE_DIR, "/Users/t/Library/Application Support/shellio"),
                (LANG, LANG_VALUE),
                ("ZMX_DIR", "/Users/t/.zmx"),
                ("TERM", "xterm"),
                ("__CF_USER_TEXT_ENCODING", "0x1F5"),
            ]),
            Some("n1"),
        );
        assert_eq!(check.violations.len(), 3, "{:?}", check.violations);
        assert!(check.violations[0].contains("не временный каталог запуска"));
        assert!(check.violations[1].starts_with("ZMX_DIR"));
        assert!(check.violations[2].starts_with("TERM"));
        assert_eq!(check.extra, ["__CF_USER_TEXT_ENCODING"]);
        let relative = check_env(
            &pairs(&[(STATE_DIR, "shellio-probe-state.n1"), (LANG, LANG_VALUE)]),
            Some("n1"),
        );
        assert_eq!(relative.violations.len(), 1);
    }

    #[test]
    fn missing_launch_marks_are_violations() {
        let without_nonce = check_env(&pairs(&[(LANG, LANG_VALUE)]), None);
        assert_eq!(
            without_nonce.violations,
            ["нет --nonce: процесс запущен не через --launch"]
        );
        let without_state = check_env(&pairs(&[(LANG, "C")]), Some("n1"));
        assert_eq!(without_state.violations.len(), 2);
        assert!(without_state.violations[0].starts_with("нет SHELLIO_STATE_DIR"));
        assert!(without_state.violations[1].starts_with("LANG="));
    }

    fn procargs(argc: i32, path: &str, padding: usize, args: &[&str], env: &[&str]) -> Vec<u8> {
        let mut bytes = argc.to_ne_bytes().to_vec();
        bytes.extend(path.as_bytes());
        bytes.extend(std::iter::repeat_n(0u8, 1 + padding));
        for arg in args.iter().chain(env) {
            bytes.extend(arg.as_bytes());
            bytes.push(0);
        }
        bytes
    }

    #[test]
    fn procargs_are_parsed_without_environment() {
        let bytes = procargs(
            3,
            "/t/release/probe-session",
            5,
            &["probe-session", "--nonce", "ab12"],
            &["HOME=/Users/test", "LANG=en_US.UTF-8"],
        );
        let args = parse_procargs(&bytes).unwrap();
        assert_eq!(args, strings(&["probe-session", "--nonce", "ab12"]));
        assert!(carries_nonce(&args, "ab12"));
        assert!(!carries_nonce(&args, "cd34"));
    }

    #[test]
    fn broken_procargs_are_rejected() {
        assert_eq!(parse_procargs(&[]), None);
        assert_eq!(parse_procargs(&[1, 0]), None);
        assert_eq!(parse_procargs(&(-1i32).to_ne_bytes()), None);
        let mut truncated = procargs(2, "/bin/sh", 0, &["sh"], &[]);
        truncated.truncate(truncated.len() - 1);
        assert_eq!(parse_procargs(&truncated), None);
    }

    #[test]
    fn command_line_is_truncated() {
        let long = vec!["x".repeat(COMMAND_LIMIT * 2)];
        let line = command_line(&long);
        assert_eq!(line.chars().count(), COMMAND_LIMIT + 1);
        assert!(line.ends_with('…'));
        assert_eq!(command_line(&strings(&["a", "b"])), "a b");
    }

    fn exited(code: i32) -> Option<ExitRecord> {
        Some(ExitRecord {
            code: Some(code),
            signal: None,
        })
    }

    #[test]
    fn judge_separates_launch_failures() {
        assert!(judge(exited(0), None, true, &[], &[]).is_empty());
        assert!(
            judge(exited(3), None, true, &[], &[]).is_empty(),
            "ошибку исполнения сценария несёт result.json"
        );
        let crash = judge(
            Some(ExitRecord {
                code: None,
                signal: Some(11),
            }),
            None,
            false,
            &[],
            &[],
        );
        assert_eq!(crash, ["падение: сигнал 11", "result.json не появился"]);
        let hung = judge(
            Some(ExitRecord {
                code: None,
                signal: Some(9),
            }),
            Some(20.0),
            false,
            &[],
            &[],
        );
        assert_eq!(hung, ["внешний сторож: процесс не завершился за 20 с"]);
        assert_eq!(judge(None, Some(20.0), false, &[], &[]).len(), 2);
        let leftovers = judge(
            exited(0),
            None,
            true,
            &[ProcessRecord {
                pid: 812,
                command: "zmx run s1".to_owned(),
            }],
            &["sessions".to_owned()],
        );
        assert_eq!(leftovers.len(), 2);
        assert!(leftovers[0].contains("812"));
        assert!(leftovers[1].contains("sessions"));
    }

    #[test]
    fn nonce_is_hex_and_unique() {
        let first = nonce().unwrap();
        let second = nonce().unwrap();
        assert_eq!(first.len(), 16);
        assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(first, second);
        assert_eq!(
            state_dir_name(&first),
            format!("shellio-probe-state.{first}")
        );
    }

    #[test]
    fn own_process_is_found_by_its_arguments_but_excluded() {
        let args = process_args(std::process::id() as i32).unwrap();
        assert!(!args.is_empty());
        assert!(
            find_by_nonce(&args[0])
                .iter()
                .all(|process| process.pid != std::process::id() as i32)
        );
    }

    #[test]
    fn exited_process_is_not_waited_for() {
        let mut child = Command::new("/usr/bin/true").spawn().unwrap();
        let pid = child.id() as i32;
        child.wait().unwrap();
        assert!(wait_exit(&[pid], Duration::from_secs(1)).is_empty());
    }

    fn shell(script: &str, nonce: &str) -> Plan {
        Plan {
            program: PathBuf::from("/bin/sh"),
            args: strings(&["-c", script, "sh", nonce]),
        }
    }

    const QUICK: Grace = Grace {
        kill: Duration::from_secs(5),
        leftover: Duration::ZERO,
    };

    fn env() -> Vec<(String, String)> {
        pairs(&[("PATH", "/usr/bin:/bin"), ("PROBE_MARK", "m1")])
    }

    #[test]
    fn supervised_exit_keeps_code_and_environment() {
        let nonce = nonce().unwrap();
        let done = supervise(
            &shell(
                r#"[ "$PROBE_MARK" = m1 ] && [ -z "$HOME" ] && exit 3"#,
                &nonce,
            ),
            &env(),
            Some(Duration::from_secs(30)),
            &nonce,
            QUICK,
        )
        .unwrap();
        assert_eq!(done.exit, exited(3));
        assert!(!done.watchdog_fired);
        assert!(done.killed.is_empty() && done.leftovers.is_empty());
        assert!(done.exit_ns.unwrap() >= done.spawn_ns);
    }

    #[test]
    fn watchdog_kills_a_hung_process() {
        let nonce = nonce().unwrap();
        let done = supervise(
            &shell("while :; do :; done", &nonce),
            &env(),
            Some(Duration::from_millis(200)),
            &nonce,
            QUICK,
        )
        .unwrap();
        assert!(done.watchdog_fired);
        assert_eq!(
            done.exit,
            Some(ExitRecord {
                code: None,
                signal: Some(libc::SIGKILL),
            })
        );
        assert!(!done.killed.is_empty());
    }

    #[test]
    fn leftover_with_nonce_is_found_and_killed() {
        let nonce = nonce().unwrap();
        // Оставшийся процесс завершается сам, даже если поиск по nonce его не найдёт.
        let script = format!(r#"/bin/sh -c "sleep 30; :" leftover-{nonce} & exit 0"#);
        let done = supervise(
            &shell(&script, &nonce),
            &env(),
            Some(Duration::from_secs(30)),
            &nonce,
            QUICK,
        )
        .unwrap();
        assert_eq!(done.exit, exited(0));
        assert_eq!(done.leftovers.len(), 1, "{:?}", done.leftovers);
        assert!(
            done.leftovers[0]
                .command
                .contains(&format!("leftover-{nonce}"))
        );
        let pids: Vec<i32> = done.leftovers.iter().map(|p| p.pid).collect();
        assert!(wait_exit(&pids, Duration::from_secs(5)).is_empty());
    }

    #[test]
    fn missing_program_is_a_launch_error() {
        let plan = Plan {
            program: PathBuf::from("/nonexistent/probe-session"),
            args: Vec::new(),
        };
        let err = supervise(&plan, &env(), None, "n1", QUICK).unwrap_err();
        assert!(err.contains("не запущен"), "{err}");
    }
}
