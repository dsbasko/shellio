//! Помощник замены и общий шаг `swapped → launched` (задача 8; план P0.5, «Протокол замены»).
//!
//! GUI N в `idle → staged` копирует помощника своего бандла в `<state>/update/helper/<attempt>/`
//! и проверяет копию требованием его части (`<id>.helper` и закреплённый лист): `--deep -R`
//! бандла требование к вложенному коду не применяет (задача 4). В `staged → intent` GUI N
//! запускает копию и выходит. Помощник ([`helper_main`]):
//!
//! 1. ждёт выхода GUI N (`kqueue` `NOTE_EXIT` по ключу из журнала);
//! 2. берёт блокировку и перечитывает журнал: только `intent` своей попытки, иначе выходит, ничего
//!    не трогая (общий `recover` он здесь не вызывает: строка «`intent`, N / N+1» прервала бы
//!    попытку);
//! 3. при раскладке «N установлен, кандидат — N+1» делает `renamex_np(RENAME_SWAP)`; ошибка обмена —
//!    `aborting`, удаление кандидата, `idle` и запуск N;
//! 4. `swapped → launched` ([`launch_next`]), отпускает блокировку и сторожит попытку
//!    ([`crate::watchdog`]).
//!
//! [`launch_next`] общий для помощника и для процесса, которому восстановление вернуло `swapped`
//! (роль «как помощник»). Перед `open -n` он ищет уже запущенный N+1 этой попытки: N+1 до первой
//! попытки взять блокировку пишет отчёт `started.<pid>.json` в каталог помощника, и процесс
//! признаётся, только если он жив, CDHash его кода из ядра равен CDHash N+1 журнала, а в его
//! аргументах есть `--update-attempt <attempt>`. Поэтому новый N+1 не запускается, если первый уже
//! есть. Не появился за [`LAUNCH_WAIT`] — `swapped → rollback-intent` и откат.
//!
//! События помощника — строки JSON `{"event": …}` в stdout, по одной на шаг, сразу на диск: их
//! читает стенд, в том числе после `SIGKILL`.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::barrier::{self, Point};
use crate::generation::{self, Part, Verify};
use crate::journal::{self, Journal, Outcome, State, Store};
use crate::observer::{self, ProcessKey};
use crate::owner::{self, AppMode, AppOptions, Lock};
use crate::recover::{self, Actor, Codesign, Slot};
use crate::{EXIT_USAGE, kq, launch, say, spawn, sys};

/// Срок health от записи `intent`; задаётся один раз на попытку.
pub const HEALTH_WINDOW: Duration = Duration::from_secs(60);
/// Сколько помощник ждёт отчёта N+1 после `open -n`.
pub const LAUNCH_WAIT: Duration = Duration::from_secs(20);
/// Сколько помощник ждёт выхода GUI N.
pub const GUI_EXIT_WAIT: Duration = Duration::from_secs(60);
pub const HELPER_DIR: &str = "helper";
pub const STARTED_PREFIX: &str = "started.";
pub const HELPER_STDOUT: &str = "helper.stdout";
pub const HELPER_STDERR: &str = "helper.stderr";
pub const NEXT_STDOUT: &str = "next.stdout";
pub const NEXT_STDERR: &str = "next.stderr";
pub const RELAUNCH_STDOUT: &str = "relaunch.stdout";
pub const RELAUNCH_STDERR: &str = "relaunch.stderr";
const OPEN: &str = "/usr/bin/open";
pub const ATTEMPT_FLAG: &str = "--update-attempt";

pub fn helper_dir(state: &Path, attempt: &str) -> PathBuf {
    state
        .join(journal::UPDATE_DIR)
        .join(HELPER_DIR)
        .join(attempt)
}

pub fn helper_copy(state: &Path, attempt: &str) -> PathBuf {
    helper_dir(state, attempt).join(generation::HELPER_BIN)
}

/// Копия помощника установленного бандла в каталог попытки и её проверка требованием части.
pub fn stage_helper(
    state: &Path,
    installed: &Path,
    identifier: &str,
    leaf: &str,
    attempt: &str,
) -> Result<PathBuf, String> {
    let dir = helper_dir(state, attempt);
    fs::create_dir_all(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    let copy = helper_copy(state, attempt);
    let _ = fs::remove_file(&copy);
    let source = Part::Helper.path(installed);
    let args = vec![
        "--norsrc".to_owned(),
        "--noextattr".to_owned(),
        "--noacl".to_owned(),
        source.display().to_string(),
        copy.display().to_string(),
    ];
    let run = generation::run("/usr/bin/ditto", &args)?;
    if run.code != Some(0) {
        return Err(format!("копия помощника: ditto: {}", run.text().trim()));
    }
    let requirement = generation::requirement(&Part::Helper.identifier(identifier), leaf);
    let mut runs = Vec::new();
    match generation::verify(&copy, &requirement, &mut runs)? {
        Verify::Satisfied => Ok(copy),
        other => Err(format!(
            "копия помощника {} не проходит проверку требованием {requirement}: {}",
            copy.display(),
            other.name()
        )),
    }
}

/// Отчёт N+1 о запуске с номером попытки.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Started {
    pub attempt: String,
    pub key: ProcessKey,
}

/// N+1 сообщает о себе до первой попытки взять блокировку.
pub fn report_started(state: &Path, attempt: &str, key: ProcessKey) -> Result<(), String> {
    let dir = helper_dir(state, attempt);
    fs::create_dir_all(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    let text = serde_json::to_vec(&Started {
        attempt: attempt.to_owned(),
        key,
    })
    .map_err(|err| err.to_string())?;
    let path = dir.join(format!("{STARTED_PREFIX}{}.json", key.pid));
    let tmp = dir.join(format!(".{STARTED_PREFIX}{}", key.pid));
    fs::write(&tmp, text)
        .and_then(|()| fs::rename(&tmp, &path))
        .map_err(|err| format!("{}: {err}", path.display()))
}

/// Почему отчёт о запуске не признан.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub key: ProcessKey,
    pub accepted: bool,
    pub reason: String,
}

/// Признание отчёта: жив, CDHash процесса из ядра — N+1 журнала, номер попытки в аргументах.
pub fn judge_started(
    journal: &Journal,
    started: &Started,
    alive: bool,
    cdhash: Option<&str>,
    args: Option<&[String]>,
) -> Result<(), String> {
    if started.attempt != journal.attempt {
        return Err(format!("отчёт другой попытки {}", started.attempt));
    }
    if !alive {
        return Err("процесса нет".to_owned());
    }
    if cdhash != Some(journal.cdhash_to.as_str()) {
        return Err(format!(
            "CDHash процесса {cdhash:?}, ждали N+1 {}",
            journal.cdhash_to
        ));
    }
    let flagged = args.is_some_and(|args| {
        args.windows(2)
            .any(|pair| pair[0] == ATTEMPT_FLAG && pair[1] == journal.attempt)
    });
    if !flagged {
        return Err(format!(
            "в аргументах нет {ATTEMPT_FLAG} {}",
            journal.attempt
        ));
    }
    Ok(())
}

/// Отчёты N+1 этой попытки с решением по каждому; признанные — по времени старта.
pub fn find_next(state: &Path, journal: &Journal) -> Result<Vec<Candidate>, String> {
    let dir = helper_dir(state, &journal.attempt);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(format!("{}: {err}", dir.display())),
    };
    let mut found = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(STARTED_PREFIX) || !name.ends_with(".json") {
            continue;
        }
        let Ok(text) = fs::read(entry.path()) else {
            continue;
        };
        let Ok(started) = serde_json::from_slice::<Started>(&text) else {
            continue;
        };
        let alive = observer::identify(started.key).is_ok_and(|found| found.is_alive());
        let cdhash = sys::process_cdhash(started.key.pid).ok();
        let args = launch::process_args(started.key.pid);
        let verdict = judge_started(journal, &started, alive, cdhash.as_deref(), args.as_deref());
        found.push(Candidate {
            key: started.key,
            accepted: verdict.is_ok(),
            reason: verdict.err().unwrap_or_else(|| "признан".to_owned()),
        });
    }
    found.sort_by_key(|candidate| (!candidate.accepted, candidate.key.start_us));
    Ok(found)
}

/// Аргументы режима из журнала как аргументы роли GUI.
pub fn mode(journal: &Journal) -> Result<AppOptions, String> {
    match owner::parse_app_args(&journal.launch, None)? {
        AppMode::Run(options) => Ok(options),
        AppMode::Stub(_) => Err("в журнале аргументы заглушки".to_owned()),
        AppMode::Keychain(_) => Err("в журнале аргументы операции keychain".to_owned()),
        AppMode::Tcc(_) => Err("в журнале аргументы операции tcc".to_owned()),
    }
}

pub fn checker(options: &AppOptions) -> Codesign {
    Codesign {
        requirement: generation::requirement(&options.identifier, &options.leaf),
    }
}

/// `open -n` установленного бандла с аргументами режима журнала; `attempt` — для N+1.
pub fn open_installed(
    state: &Path,
    journal: &Journal,
    attempt: Option<&str>,
    (stdout, stderr): (&str, &str),
) -> Result<(), String> {
    let dir = helper_dir(state, &journal.attempt);
    fs::create_dir_all(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    let mut args = vec![
        "-n".to_owned(),
        "--env".to_owned(),
        format!("{}={}", launch::STATE_DIR, state.display()),
        "--stdout".to_owned(),
        dir.join(stdout).display().to_string(),
        "--stderr".to_owned(),
        dir.join(stderr).display().to_string(),
        journal.installed.display().to_string(),
        "--args".to_owned(),
    ];
    args.extend(journal.launch.iter().cloned());
    if let Some(attempt) = attempt {
        args.extend([ATTEMPT_FLAG.to_owned(), attempt.to_owned()]);
    }
    let output = spawn::output(Path::new(OPEN), &args).map_err(|err| format!("open: {err}"))?;
    if output.exit.success() {
        Ok(())
    } else {
        Err(format!(
            "open -n {}: {}; {}",
            journal.installed.display(),
            output.exit,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// Событие помощника строкой JSON в stdout.
pub fn event(name: &str, detail: serde_json::Value) {
    let line = json!({
        "event": name,
        "uptime_ns": sys::uptime_ns(),
        "pid": std::process::id(),
        "detail": detail,
    });
    let mut stdout = io::stdout().lock();
    let _ = writeln!(stdout, "{line}");
    let _ = stdout.flush();
}

/// Итог `swapped → launched`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Launched {
    /// `launched` записан с ключом владельца.
    Owner(ProcessKey),
    /// N+1 не появился за срок: попытка откачена до `idle`, N запускать вызывающему.
    RolledBack,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

/// Восстановление под блокировкой вызывающего, с проверкой подписи по аргументам режима.
pub fn recover_now(
    store: &Store,
    options: &AppOptions,
    me: ProcessKey,
) -> Result<recover::Report, String> {
    let alive = |key: &ProcessKey| observer::identify(*key).is_ok_and(|found| found.is_alive());
    let ctx = recover::Context {
        now_ms: now_ms(),
        actor: Actor {
            key: me,
            launched_for: None,
        },
        alive: &alive,
    };
    recover::recover(store, &checker(options), &ctx).map_err(|err| err.to_string())
}

/// `swapped → launched` под блокировкой вызывающего: уже запущенный N+1 этой попытки или `open -n`
/// и ожидание его отчёта не дольше [`LAUNCH_WAIT`].
pub fn launch_next(store: &Store, journal: &Journal, me: ProcessKey) -> Result<Launched, String> {
    let state = store.state();
    if journal.state != State::Swapped {
        return Err(format!("launch_next в {}", journal.state.name()));
    }
    let options = mode(journal)?;
    let dir = helper_dir(state, &journal.attempt);
    fs::create_dir_all(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    let queue = kq::Queue::on_dir(&dir)?;
    let mut candidates = find_next(state, journal)?;
    let reused = candidates
        .first()
        .is_some_and(|candidate| candidate.accepted);
    if !reused {
        open_installed(
            state,
            journal,
            Some(&journal.attempt),
            (NEXT_STDOUT, NEXT_STDERR),
        )?;
        event(
            "opened",
            json!({ "installed": journal.installed, "candidates": candidates }),
        );
        let deadline = Instant::now() + LAUNCH_WAIT;
        loop {
            candidates = find_next(state, journal)?;
            if candidates
                .first()
                .is_some_and(|candidate| candidate.accepted)
            {
                break;
            }
            if queue.wait(deadline)? == kq::Event::TimedOut {
                candidates = find_next(state, journal)?;
                break;
            }
        }
    }
    let found = candidates
        .first()
        .filter(|candidate| candidate.accepted)
        .map(|candidate| candidate.key);
    event(
        "identified",
        json!({ "reused": reused, "owner": found, "candidates": candidates }),
    );
    match found {
        Some(key) => {
            barrier::reach(state, Point::Opened);
            store
                .advance(&journal.attempt, &[State::Swapped], |next| {
                    next.state = State::Launched;
                    next.owner = Some(key);
                })
                .map_err(|err| err.to_string())?;
            event("launched", json!({ "owner": key }));
            Ok(Launched::Owner(key))
        }
        None => {
            store
                .advance(&journal.attempt, &[State::Swapped], |next| {
                    next.state = State::RollbackIntent;
                    next.reason = Some(format!(
                        "N+1 не появился за {} с после open -n",
                        LAUNCH_WAIT.as_secs()
                    ));
                })
                .map_err(|err| err.to_string())?;
            let report = recover_now(store, &options, me)?;
            event(
                "rolled-back",
                json!({ "steps": format!("{:?}", report.steps) }),
            );
            Ok(Launched::RolledBack)
        }
    }
}

// --- помощник ---

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelperArgs {
    pub state: PathBuf,
    pub attempt: String,
    pub tag: Option<String>,
}

pub const HELPER_USAGE: &str =
    "использование: probe-update-helper --state <каталог> --attempt <номер> [--tag <метка>]";

pub fn parse_helper_args(args: &[String]) -> Result<HelperArgs, String> {
    let (mut state, mut attempt, mut tag) = (None, None, None);
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        let value = rest.next().ok_or_else(|| format!("{flag}: нет значения"))?;
        let slot = match flag.as_str() {
            "--state" => &mut state,
            "--attempt" => &mut attempt,
            "--tag" => &mut tag,
            other => return Err(format!("неизвестный аргумент «{other}»")),
        };
        if slot.replace(value.clone()).is_some() {
            return Err(format!("{flag} задан дважды"));
        }
    }
    let state = PathBuf::from(state.ok_or("нет --state")?);
    if !state.is_absolute() {
        return Err(format!(
            "--state {}: нужен абсолютный путь",
            state.display()
        ));
    }
    let attempt = attempt.ok_or("нет --attempt")?;
    if !journal::is_attempt(&attempt) {
        return Err(format!("--attempt {attempt}: не номер попытки"));
    }
    Ok(HelperArgs {
        state,
        attempt,
        tag,
    })
}

impl HelperArgs {
    pub fn to_args(&self) -> Vec<String> {
        let mut args = vec![
            "--state".to_owned(),
            self.state.display().to_string(),
            "--attempt".to_owned(),
            self.attempt.clone(),
        ];
        if let Some(tag) = &self.tag {
            args.extend(["--tag".to_owned(), tag.clone()]);
        }
        args
    }
}

/// Вход `probe-update-helper`. Аргументы `--manifest` и `--report` — заглушка задачи 4.
pub fn helper_main(args: &[String]) -> ExitCode {
    if args.iter().any(|arg| arg == "--report") {
        return generation::stub_main(generation::HELPER_BIN, args);
    }
    let parsed = match parse_helper_args(args) {
        Ok(parsed) => parsed,
        Err(reason) => {
            say(&format!(
                "{}: {reason}\n\n{HELPER_USAGE}",
                generation::HELPER_BIN
            ));
            return ExitCode::from(EXIT_USAGE);
        }
    };
    barrier::set_role(barrier::Role::Helper);
    match run_helper(&parsed) {
        Ok(outcome) => {
            event("done", json!({ "outcome": outcome }));
            ExitCode::SUCCESS
        }
        Err(reason) => {
            event("error", json!({ "reason": reason }));
            say(&format!("{}: {reason}", generation::HELPER_BIN));
            ExitCode::from(owner::EXIT_ERROR)
        }
    }
}

/// Ждёт выхода процесса `key` не дольше `limit`; `Ok(true)` — вышел.
fn wait_gone(key: ProcessKey, limit: Duration) -> Result<bool, String> {
    let queue = kq::Queue::new()?;
    let watching = queue.watch_exit(key.pid)?;
    if !watching || !observer::identify(key)?.is_alive() {
        return Ok(true);
    }
    let deadline = Instant::now() + limit;
    loop {
        match queue.wait(deadline)? {
            kq::Event::Exited(pid) if pid == key.pid => return Ok(true),
            kq::Event::TimedOut => return Ok(!observer::identify(key)?.is_alive()),
            _ => {}
        }
    }
}

fn run_helper(args: &HelperArgs) -> Result<String, String> {
    let me = observer::current()?.key;
    let store = Store::new(&args.state);
    let journal = read_attempt(&store, &args.attempt, State::Intent)?;
    let Some(journal) = journal else {
        return Ok("попытка уже не в intent до выхода GUI N: помощник ничего не делает".to_owned());
    };
    let options = mode(&journal)?;
    let gui = journal.gui_from.ok_or("в intent нет ключа GUI N")?;
    event(
        "started",
        json!({ "key": me, "attempt": args.attempt, "gui": gui }),
    );
    if !wait_gone(gui, GUI_EXIT_WAIT)? {
        return Err(format!(
            "GUI N {} не вышел за {} с",
            gui.pid,
            GUI_EXIT_WAIT.as_secs()
        ));
    }
    event("gui-exited", json!({ "gui": gui }));
    barrier::reach(&args.state, Point::GuiExited);
    let lock = Lock::acquire(&owner::lock_path(&args.state))
        .map_err(|err| format!("блокировка: {err}"))?;
    event(
        "locked",
        json!({ "inode": { "dev": lock.dev, "ino": lock.ino } }),
    );
    barrier::reach(&args.state, Point::Locked);
    let Some(journal) = read_attempt(&store, &args.attempt, State::Intent)? else {
        return Ok("под блокировкой попытка уже не в intent: помощник ничего не делает".to_owned());
    };
    let gui_alive = observer::identify(gui)?.is_alive();
    if gui_alive {
        return Err(format!("GUI N {} жив под блокировкой помощника", gui.pid));
    }
    let seen = recover::observe(&journal, &checker(&options)).map_err(|err| err.to_string())?;
    if (
        seen.installed.clone(),
        seen.staged.clone(),
        seen.failed.clone(),
    ) != (Slot::From, Slot::To, Slot::Absent)
        || !seen.strangers.is_empty()
    {
        return Err(format!(
            "раскладка перед обменом не «N установлен, кандидат — N+1»: {seen:?}; ничего не переставлено"
        ));
    }
    let install_dir = journal
        .installed
        .parent()
        .ok_or("у установленного пути нет каталога")?;
    match sys::swap_paths(&journal.installed, &journal.staged_dir()) {
        Err(err) => {
            event(
                "swap-failed",
                json!({ "errno": err.raw_os_error(), "error": err.to_string() }),
            );
            store
                .advance(&journal.attempt, &[State::Intent], |next| {
                    next.state = State::Aborting;
                    next.helper = Some(me);
                    next.reason = Some(format!("renamex_np(RENAME_SWAP): {err}"));
                })
                .map_err(|err| err.to_string())?;
            let report = recover_now(&store, &options, me)?;
            event("aborted", json!({ "steps": format!("{:?}", report.steps) }));
            drop(lock);
            relaunch_installed(&args.state, &journal)?;
            return Ok(format!(
                "обмен не выполнен ({err}): попытка прервана, N запущен"
            ));
        }
        Ok(()) => {
            sys::sync_dir(install_dir).map_err(|err| format!("fsync каталога установки: {err}"))?;
        }
    }
    event("exchanged", json!({ "gui_alive": gui_alive }));
    barrier::reach(&args.state, Point::Exchanged);
    let swapped = store
        .advance(&journal.attempt, &[State::Intent], |next| {
            next.state = State::Swapped;
            next.helper = Some(me);
        })
        .map_err(|err| err.to_string())?;
    event("swapped", json!({}));
    let owner_key = match launch_next(&store, &swapped, me)? {
        Launched::Owner(key) => key,
        Launched::RolledBack => {
            drop(lock);
            relaunch_installed(&args.state, &journal)?;
            return Ok("N+1 не появился: попытка откачена, N запущен".to_owned());
        }
    };
    drop(lock);
    event("released", json!({ "owner": owner_key }));
    barrier::reach(&args.state, Point::Released);
    let deadline = journal.deadline_ms.ok_or("в intent нет срока")?;
    let outcome = crate::watchdog::watch(&store, &journal.attempt, owner_key, deadline, me)?;
    if outcome.relaunch {
        relaunch_installed(&args.state, &journal)?;
    }
    Ok(outcome.summary)
}

/// Запуск установленного N после отката или прерывания: `open -n` с аргументами режима журнала.
pub fn relaunch_installed(state: &Path, journal: &Journal) -> Result<(), String> {
    open_installed(state, journal, None, (RELAUNCH_STDOUT, RELAUNCH_STDERR))?;
    event("relaunched", json!({ "installed": journal.installed }));
    Ok(())
}

/// Журнал попытки `attempt` в состоянии `state`; `None` — попытка в другом состоянии или другая.
fn read_attempt(store: &Store, attempt: &str, state: State) -> Result<Option<Journal>, String> {
    match store.read().map_err(|err| err.to_string())? {
        Some(journal) if journal.attempt == attempt && journal.state == state => Ok(Some(journal)),
        Some(journal) => {
            event(
                "foreign-state",
                json!({ "attempt": journal.attempt, "state": journal.state }),
            );
            Ok(None)
        }
        None => Err("журнала нет".to_owned()),
    }
}

/// Попытка закончена подтверждением этого процесса: `idle` с итогом «подтверждена».
pub fn committed(journal: &Journal, attempt: &str) -> bool {
    journal.attempt == attempt
        && journal.state == State::Idle
        && journal.outcome == Some(Outcome::Committed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::tests::{ATTEMPT, CD_TO, journal_in};

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    #[test]
    fn helper_args_round_trip() {
        let args = HelperArgs {
            state: PathBuf::from("/tmp/psu.x"),
            attempt: "ab12".to_owned(),
            tag: Some("n1".to_owned()),
        };
        assert_eq!(parse_helper_args(&args.to_args()), Ok(args));
        assert!(parse_helper_args(&strings(&["--state", "x", "--attempt", "ab"])).is_err());
        assert!(parse_helper_args(&strings(&["--state", "/x", "--attempt", "XY"])).is_err());
        assert!(parse_helper_args(&strings(&["--state", "/x"])).is_err());
        assert!(
            parse_helper_args(&strings(&[
                "--state",
                "/x",
                "--attempt",
                "a",
                "--attempt",
                "b"
            ]))
            .is_err()
        );
    }

    #[test]
    fn started_report_is_accepted_only_for_the_live_n_plus_1_of_this_attempt() {
        let journal = journal_in(State::Swapped, Path::new("/i/probe-update.app"));
        let started = Started {
            attempt: ATTEMPT.to_owned(),
            key: ProcessKey {
                pid: 7,
                start_us: 1,
            },
        };
        let args = strings(&["--state", "/s", ATTEMPT_FLAG, ATTEMPT]);
        assert_eq!(
            judge_started(&journal, &started, true, Some(CD_TO), Some(&args)),
            Ok(())
        );
        let other = Started {
            attempt: "ffff".to_owned(),
            ..started.clone()
        };
        assert!(judge_started(&journal, &other, true, Some(CD_TO), Some(&args)).is_err());
        assert!(judge_started(&journal, &started, false, Some(CD_TO), Some(&args)).is_err());
        assert!(
            judge_started(
                &journal,
                &started,
                true,
                Some(&journal.cdhash_from),
                Some(&args)
            )
            .is_err(),
            "старый код N по тому же пути"
        );
        assert!(judge_started(&journal, &started, true, None, Some(&args)).is_err());
        let plain = strings(&["--state", "/s"]);
        assert!(
            judge_started(&journal, &started, true, Some(CD_TO), Some(&plain)).is_err(),
            "второй open -n пользователя без номера попытки"
        );
        assert!(judge_started(&journal, &started, true, Some(CD_TO), None).is_err());
    }

    #[test]
    fn started_reports_are_written_atomically_and_found() {
        let dir = crate::journal::tests::temp_dir("started");
        let me = observer::current().unwrap().key;
        report_started(&dir, ATTEMPT, me).unwrap();
        let journal = journal_in(
            State::Swapped,
            &dir.join("install").join("probe-update.app"),
        );
        let found = find_next(&dir, &journal).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].key, me);
        assert!(!found[0].accepted, "тест не N+1: {}", found[0].reason);
        let names: Vec<String> = fs::read_dir(helper_dir(&dir, ATTEMPT))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, [format!("{STARTED_PREFIX}{}.json", me.pid)]);
        fs::remove_dir_all(&dir).unwrap();
    }
}
