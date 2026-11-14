//! Режим `--launch`: запуск процесса пробы из бандла с окружением, собранным с нуля, и внешний
//! сторож. Работает без AppKit и без `ghostty_init`.
//!
//! Внешний сторож ловит то, чего не видит внутренний: зависание до `result.json`, в том числе в
//! `ghostty_init`, где внутреннего сторожа ещё нет, падение и процессы, оставшиеся после выхода.
//! Процессы пробы узнаются по nonce в аргументах, а не по имени: на маршруте LaunchServices
//! процесс пробы не потомок запуска.

use std::collections::BTreeMap;
use std::ffi::c_int;
use std::fs;
use std::io::{self, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use crate::args::{Launch, Route, Target};
use crate::clock;
use crate::env;
use crate::results::{self, Execution, ExitRecord, LaunchRecord, ProcessRecord};
use crate::scenario::EXIT_EXECUTION;

/// Имя исполняемого файла в `Contents/MacOS` бандла.
pub const EXECUTABLE: &str = env!("CARGO_PKG_NAME");
const OPEN: &str = "/usr/bin/open";
/// Запас внешнего сторожа сверх срока внутреннего: запуск, `ghostty_init`, запись итога.
const MARGIN: Duration = Duration::from_secs(15);
/// Сколько ждать выхода после `SIGKILL` от внешнего сторожа.
const KILL_GRACE: Duration = Duration::from_secs(5);
/// Сколько процессы с nonce могут завершаться после выхода процесса пробы.
const LEFTOVER_GRACE: Duration = Duration::from_secs(3);
const COMMAND_LIMIT: usize = 240;
/// Вывод процесса пробы на маршруте LaunchServices: `open` перенаправляет его сам.
pub const LS_STDOUT: &str = "probe-stdout.txt";
pub const LS_STDERR: &str = "probe-stderr.txt";

/// Программа и аргументы для маршрута.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub program: PathBuf,
    pub args: Vec<String>,
}

/// Аргументы процесса пробы: заданные после `--` и выставленные запуском.
pub fn probe_args(args: &[String], route: Route, nonce: &str) -> Vec<String> {
    let mut all = args.to_vec();
    all.extend([
        "--route".to_owned(),
        route.name().to_owned(),
        "--nonce".to_owned(),
        nonce.to_owned(),
    ]);
    all
}

/// Прямой маршрут запускает исполняемый файл бандла, маршрут LaunchServices — `open -n -W`.
pub fn plan(app: &Path, route: Route, probe_args: Vec<String>, out: Option<&Path>) -> Plan {
    match route {
        Route::Direct => Plan {
            program: app.join("Contents/MacOS").join(EXECUTABLE),
            args: probe_args,
        },
        Route::LaunchServices => {
            let mut args = vec!["-n".to_owned(), "-W".to_owned()];
            if let Some(out) = out {
                args.extend([
                    "--stdout".to_owned(),
                    out.join(LS_STDOUT).display().to_string(),
                    "--stderr".to_owned(),
                    out.join(LS_STDERR).display().to_string(),
                ]);
            }
            args.push(app.display().to_string());
            args.push("--args".to_owned());
            args.extend(probe_args);
            Plan {
                program: PathBuf::from(OPEN),
                args,
            }
        }
    }
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

/// Итог проверок запуска; любая причина — ошибка исполнения процесса.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Verdicts {
    pub reasons: Vec<String>,
}

/// Причины ошибки по фактам запуска.
pub fn judge(
    exit: Option<ExitRecord>,
    watchdog: Option<f64>,
    result_present: Option<bool>,
    leftovers: &[ProcessRecord],
    state_dir_leftovers: &[String],
) -> Verdicts {
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
    if result_present == Some(false) && watchdog.is_none() {
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
            "{} не пуст после выхода: {}",
            env::STATE_DIR,
            state_dir_leftovers.join(", ")
        ));
    }
    Verdicts { reasons }
}

pub fn run(launch: Launch) -> ExitCode {
    match execute(launch) {
        Ok(code) => code,
        Err(reason) => {
            say(&format!("launch: execution=error\nlaunch: reason={reason}"));
            ExitCode::from(EXIT_EXECUTION)
        }
    }
}

fn execute(launch: Launch) -> Result<ExitCode, String> {
    let app = fs::canonicalize(&launch.app)
        .map_err(|err| format!("бандл {}: {err}", launch.app.display()))?;
    let (out, deadline) = match &launch.target {
        Target::Demo => (None, None),
        Target::Scenario { scenario, out, .. } => {
            if !out.is_dir() {
                return Err(format!("--out {}: каталога нет", out.display()));
            }
            (Some(out.clone()), Some(scenario.deadline() + MARGIN))
        }
    };
    let nonce = nonce()?;
    let tmp = std::env::var("TMPDIR")
        .ok()
        .filter(|tmp| !tmp.is_empty())
        .unwrap_or_else(|| "/tmp".to_owned());
    let state_dir = Path::new(&tmp).join(format!("shellio-probe-state.{nonce}"));
    fs::create_dir(&state_dir)
        .map_err(|err| format!("{} {}: {err}", env::STATE_DIR, state_dir.display()))?;
    let outcome = supervise(&launch, &app, out.as_deref(), deadline, &nonce, &state_dir);
    let state_dir_leftovers = list_dir(&state_dir);
    let _ = fs::remove_dir_all(&state_dir);
    let mut supervised = match outcome {
        Ok(supervised) => supervised,
        Err(reason) => {
            // Без launch.json агрегирование сочло бы процесс запущенным мимо --launch.
            if let Some(out) = &out {
                let record = LaunchRecord {
                    schema: results::SCHEMA,
                    route: launch.route.name().to_owned(),
                    program: String::new(),
                    args: launch.args.clone(),
                    env: BTreeMap::new(),
                    nonce,
                    state_dir: state_dir.display().to_string(),
                    spawn_uptime_ns: 0,
                    exit_uptime_ns: None,
                    exit: None,
                    deadline_s: deadline.map_or(0.0, |d| d.as_secs_f64()),
                    watchdog_fired: false,
                    killed: Vec::new(),
                    leftovers: Vec::new(),
                    state_dir_leftovers,
                    result_present: false,
                    execution: Execution::error(reason.clone()),
                };
                results::write_launch(out, &record)?;
            }
            return Err(reason);
        }
    };

    let result_present = out
        .as_deref()
        .map(|out| out.join(results::RESULT_FILE).is_file());
    let verdicts = judge(
        supervised.exit,
        supervised
            .watchdog_fired
            .then(|| deadline.map_or(0.0, |d| d.as_secs_f64())),
        result_present,
        &supervised.leftovers,
        &state_dir_leftovers,
    );
    let execution = if verdicts.reasons.is_empty() {
        Execution::Ok
    } else {
        Execution::error(verdicts.reasons.join("; "))
    };
    let record = LaunchRecord {
        schema: results::SCHEMA,
        route: launch.route.name().to_owned(),
        program: supervised.plan.program.display().to_string(),
        args: std::mem::take(&mut supervised.plan.args),
        env: supervised.env.into_iter().collect::<BTreeMap<_, _>>(),
        nonce,
        state_dir: state_dir.display().to_string(),
        spawn_uptime_ns: supervised.spawn_ns,
        exit_uptime_ns: supervised.exit_ns,
        exit: supervised.exit,
        deadline_s: deadline.map_or(0.0, |d| d.as_secs_f64()),
        watchdog_fired: supervised.watchdog_fired,
        killed: supervised.killed,
        leftovers: supervised.leftovers,
        state_dir_leftovers,
        result_present: result_present.unwrap_or(false),
        execution: execution.clone(),
    };
    if let Some(out) = &out {
        results::write_launch(out, &record)?;
    }
    say(&describe(&record));
    Ok(exit_code(&launch, &record, out.as_deref()))
}

/// Код выхода запуска: ошибка запуска — `EXIT_EXECUTION`; иначе на прямом маршруте код процесса
/// пробы, а на маршруте LaunchServices — исполнение из `result.json`: код `open` вывода не несёт.
fn exit_code(launch: &Launch, record: &LaunchRecord, out: Option<&Path>) -> ExitCode {
    if !record.execution.is_ok() {
        return ExitCode::from(EXIT_EXECUTION);
    }
    match launch.route {
        Route::Direct => match record.exit.and_then(|exit| exit.code) {
            Some(code) => ExitCode::from(u8::try_from(code).unwrap_or(EXIT_EXECUTION)),
            None => ExitCode::from(EXIT_EXECUTION),
        },
        Route::LaunchServices => match out.map(results::read_result) {
            None => ExitCode::SUCCESS,
            Some(Ok(result)) if result.execution.is_ok() => ExitCode::SUCCESS,
            Some(_) => ExitCode::from(EXIT_EXECUTION),
        },
    }
}

struct Supervised {
    plan: Plan,
    env: Vec<(String, String)>,
    spawn_ns: u64,
    exit_ns: Option<u64>,
    exit: Option<ExitRecord>,
    watchdog_fired: bool,
    killed: Vec<ProcessRecord>,
    leftovers: Vec<ProcessRecord>,
}

fn supervise(
    launch: &Launch,
    app: &Path,
    out: Option<&Path>,
    deadline: Option<Duration>,
    nonce: &str,
    state_dir: &Path,
) -> Result<Supervised, String> {
    let env = env::build(
        |name| std::env::var(name).ok(),
        launch.route,
        &state_dir.display().to_string(),
        &launch.set,
    )
    .map_err(|err| err.to_string())?;
    let plan = plan(
        app,
        launch.route,
        probe_args(&launch.args, launch.route, nonce),
        out,
    );
    if launch.route == Route::Direct && !plan.program.is_file() {
        return Err(format!("в бандле нет {}", plan.program.display()));
    }
    let mut command = Command::new(&plan.program);
    command
        .args(&plan.args)
        .env_clear()
        .envs(env.iter().map(|(name, value)| (name, value)))
        .stdin(Stdio::null());
    let spawn_ns = clock::uptime_ns();
    let mut child = command
        .spawn()
        .map_err(|err| format!("{} не запущен: {err}", plan.program.display()))?;
    let child_pid = child.id() as i32;
    let (sender, receiver) = mpsc::channel::<(io::Result<ExitStatus>, u64)>();
    thread::Builder::new()
        .name("probe-launch-wait".to_owned())
        .spawn(move || {
            let status = child.wait();
            let _ = sender.send((status, clock::uptime_ns()));
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
                receiver.recv_timeout(KILL_GRACE).ok()
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
    let leftovers = reap_leftovers(nonce);
    Ok(Supervised {
        plan,
        env,
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
        "launch: route={} nonce={} exit={exit} watchdog={} killed={} leftovers={}",
        record.route,
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

fn say(text: &str) {
    let _ = writeln!(io::stderr().lock(), "{text}");
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

/// Процессы с nonce в аргументах, кроме самого запуска.
fn find_by_nonce(nonce: &str) -> Vec<ProcessRecord> {
    let own = std::process::id() as i32;
    all_pids()
        .into_iter()
        .filter(|pid| *pid > 0 && *pid != own)
        .filter_map(|pid| {
            let args = process_args(pid)?;
            carries_nonce(&args, nonce).then(|| ProcessRecord {
                pid,
                command: command_line(&args),
            })
        })
        .collect()
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

/// Ждёт выхода процессов с nonce до `LEFTOVER_GRACE`, оставшихся снимает и возвращает.
fn reap_leftovers(nonce: &str) -> Vec<ProcessRecord> {
    let found = find_by_nonce(nonce);
    if found.is_empty() {
        return found;
    }
    let pids: Vec<i32> = found.iter().map(|process| process.pid).collect();
    let alive = wait_exit(&pids, LEFTOVER_GRACE);
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

fn process_args(pid: i32) -> Option<Vec<String>> {
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
fn wait_exit(pids: &[i32], timeout: Duration) -> Vec<i32> {
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

    #[test]
    fn probe_args_append_route_and_nonce() {
        assert_eq!(
            probe_args(
                &strings(&["--scenario", "window", "--out", "o"]),
                Route::LaunchServices,
                "ab12"
            ),
            strings(&[
                "--scenario",
                "window",
                "--out",
                "o",
                "--route",
                "launch-services",
                "--nonce",
                "ab12"
            ])
        );
    }

    #[test]
    fn direct_route_runs_bundle_executable() {
        let app = Path::new("/b/probe-ghostty-surface.app");
        let args = strings(&["--scenario", "window"]);
        assert_eq!(
            plan(app, Route::Direct, args.clone(), Some(Path::new("/o"))),
            Plan {
                program: PathBuf::from(
                    "/b/probe-ghostty-surface.app/Contents/MacOS/probe-ghostty-surface"
                ),
                args,
            }
        );
    }

    #[test]
    fn launch_services_route_goes_through_open() {
        let app = Path::new("/b/probe-ghostty-surface.app");
        let plan_with_out = plan(
            app,
            Route::LaunchServices,
            strings(&["--scenario", "window"]),
            Some(Path::new("/o")),
        );
        assert_eq!(plan_with_out.program, PathBuf::from(OPEN));
        assert_eq!(
            plan_with_out.args,
            strings(&[
                "-n",
                "-W",
                "--stdout",
                "/o/probe-stdout.txt",
                "--stderr",
                "/o/probe-stderr.txt",
                "/b/probe-ghostty-surface.app",
                "--args",
                "--scenario",
                "window"
            ])
        );
        let demo = plan(app, Route::LaunchServices, Vec::new(), None);
        assert_eq!(
            demo.args,
            strings(&["-n", "-W", "/b/probe-ghostty-surface.app", "--args"])
        );
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
            "/b/p.app/Contents/MacOS/probe-ghostty-surface",
            5,
            &["probe-ghostty-surface", "--nonce", "ab12"],
            &["HOME=/Users/test", "LANG=en_US.UTF-8"],
        );
        assert_eq!(
            parse_procargs(&bytes),
            Some(strings(&["probe-ghostty-surface", "--nonce", "ab12"]))
        );
        let args = parse_procargs(&bytes).unwrap();
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
        assert_eq!(
            judge(exited(0), None, Some(true), &[], &[]).reasons.len(),
            0
        );
        assert_eq!(
            judge(exited(3), None, Some(true), &[], &[]).reasons.len(),
            0,
            "ошибку исполнения сценария несёт result.json"
        );
        let crash = judge(
            Some(ExitRecord {
                code: None,
                signal: Some(11),
            }),
            None,
            Some(false),
            &[],
            &[],
        );
        assert_eq!(
            crash.reasons,
            ["падение: сигнал 11", "result.json не появился"]
        );
        let hung = judge(
            Some(ExitRecord {
                code: None,
                signal: Some(9),
            }),
            Some(30.0),
            Some(false),
            &[],
            &[],
        );
        assert_eq!(
            hung.reasons,
            ["внешний сторож: процесс не завершился за 30 с"]
        );
        let stuck = judge(None, Some(30.0), Some(false), &[], &[]);
        assert_eq!(stuck.reasons.len(), 2);
        let leftovers = judge(
            exited(0),
            None,
            Some(true),
            &[ProcessRecord {
                pid: 812,
                command: "probe-ghostty-surface --fixture".to_owned(),
            }],
            &["sessions".to_owned()],
        );
        assert_eq!(leftovers.reasons.len(), 2);
        assert!(leftovers.reasons[0].contains("812"));
        assert!(leftovers.reasons[1].contains("sessions"));
        assert_eq!(judge(exited(0), None, None, &[], &[]).reasons.len(), 0);
    }

    #[test]
    fn nonce_is_hex_and_unique() {
        let first = nonce().unwrap();
        let second = nonce().unwrap();
        assert_eq!(first.len(), 16);
        assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }

    #[test]
    fn own_process_is_found_by_its_arguments_but_excluded() {
        let args = process_args(std::process::id() as i32).unwrap();
        assert!(!args.is_empty());
        let marker = &args[0];
        assert!(
            find_by_nonce(marker)
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
}
