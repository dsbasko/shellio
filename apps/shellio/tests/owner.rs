//! Владелец профиля отдельными процессами `shellio-owner-fixture` и выбор роли в процессе
//! теста на ручных часах (ADR-0006).
//!
//! Процессы получают только `SHELLIO_STATE_DIR` временного профиля в `/tmp`: путь сокета в
//! `/var/folders/…` длиннее 103 байт. Синхронизация — строки stdout владельца, завершение
//! процессов и ответы сокета; ожидания по времени нет. Чтение идёт клиентом `shellio-control`.

// allow-unwrap-in-tests не распространяется на вспомогательные функции интеграционного теста.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::ffi::OsString;
use std::fs::{self, File};
use std::future::{Future, pending};
use std::io::{BufRead, BufReader, Read};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, ExitStatus, Output, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use rustix::process::{Pid, Signal, kill_process};
use serde_json::Value;
use shellio::owner::{
    ActivationHandoff, Claim, HANDOFF_TIMEOUT, Handoff, HandoffOutcome, Reopen,
    SECOND_LAUNCH_TIMEOUT, Second, claim,
};
use shellio_control::test_support::ManualClock;
use shellio_control::{
    Activation, Address, Client, ControlError, ControlErrorCode, Exit, HelloReply, Launch,
    LaunchReceipt, LineReader, OkValue, OwnerState, Ownership, PROTOCOL, Probe, REQUEST_LINE_LIMIT,
    RequestBody, RequestId, Response, TokioClock, encode_line, lock_path, probe_owner, socket_path,
};
use shellio_core::{Command as CoreCommand, Query, ReplyValue, TreeScope, TreeView};
use shellio_store::Profile;
use tempfile::TempDir;
use tokio::io::AsyncWriteExt;

const FIXTURE: &str = env!("CARGO_BIN_EXE_shellio-owner-fixture");
/// Продуктовый бинарь: проверяются только отказы до профиля, блокировки и AppKit.
const SHELLIO: &str = env!("CARGO_BIN_EXE_shellio");
/// Срок процесса, который должен выйти сам: ограничивает зависание, а не синхронизирует.
const EXIT_GUARD: Duration = Duration::from_secs(30);
const READY: &str = "shellio: владелец готов, pid ";
const HANDLED: &str = "shellio: запуск обработан ";
const ACTIVATION: &str = "shellio-owner-fixture: активация без UI";
/// Переменная фикстуры, задерживающая передачу активации второго запуска.
const HANDOFF_VAR: &str = "SHELLIO_FIXTURE_HANDOFF";
/// Срок клиента теста: ограничивает зависание, а не синхронизирует.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);

/// Временный профиль в `/tmp` и рабочий каталог процессов рядом с ним.
struct Fixture {
    _dir: TempDir,
    /// Разрешённый путь каталога: `/tmp` — ссылка, а рабочий каталог процесса разрешён.
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("shellio-owner-")
            .tempdir_in("/tmp")
            .unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir(root.join("work")).unwrap();
        Self { _dir: dir, root }
    }

    fn state(&self) -> PathBuf {
        self.root.join("state")
    }

    fn work(&self) -> PathBuf {
        self.root.join("work")
    }

    fn profile(&self) -> Profile {
        Profile::isolated(self.state())
    }

    fn command(&self, args: &[OsString]) -> Command {
        let mut command = Command::new(FIXTURE);
        command
            .env_clear()
            .env("SHELLIO_STATE_DIR", self.state())
            .args(args)
            .current_dir(self.work())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn spawn(&self, args: &[OsString]) -> Guard {
        Guard(Some(self.command(args).spawn().unwrap()))
    }

    /// Запустить владельца и дождаться строки готовности.
    fn owner(&self) -> Owner {
        self.owner_with(&[], "normal")
    }

    /// Запустить владельца с аргументами `args` и дождаться строки готовности с режимом `mode`.
    /// Окна у фикстуры нет.
    fn owner_with(&self, args: &[OsString], mode: &str) -> Owner {
        let mut guard = self.spawn(args);
        let stdout = BufReader::new(guard.child().stdout.take().unwrap());
        let mut owner = Owner { guard, stdout };
        let line = owner.line();
        let pid = owner.pid();
        assert_eq!(
            line,
            format!(
                "{READY}{pid}, сокет {}, режим {mode}, окно нет",
                socket_path(&self.profile()).display()
            )
        );
        owner
    }

    /// Второй запуск до выхода.
    fn second(&self, args: &[OsString]) -> Output {
        self.command(args).output().unwrap()
    }

    /// Второй запуск, у которого передача активации не завершается.
    fn second_with_hung_handoff(&self) -> Output {
        self.command(&[]).env(HANDOFF_VAR, "hang").output().unwrap()
    }

    fn client(&self) -> Client<TokioClock> {
        Client::new(
            Address::Profile(self.profile()),
            TokioClock,
            "owner-test",
            CLIENT_TIMEOUT,
        )
    }

    /// PID владельца по `hello` сокета профиля.
    fn hello_pid(&self) -> u32 {
        match block_on(self.client().version()).unwrap() {
            Response::Ok {
                value: OkValue::Hello(hello),
                ..
            } => hello.pid,
            frame => panic!("hello: {frame:?}"),
        }
    }

    fn call(&self, body: RequestBody) -> ReplyValue {
        match block_on(self.client().call(body)).unwrap() {
            Response::Ok {
                value: OkValue::Reply(reply),
                ..
            } => reply.value,
            frame => panic!("ответ: {frame:?}"),
        }
    }

    fn tree(&self) -> TreeView {
        match self.call(RequestBody::Query(Query::Tree {
            scope: TreeScope::All,
        })) {
            ReplyValue::Tree(tree) => tree,
            value => panic!("дерево: {value:?}"),
        }
    }

    fn inode(&self, name: &str) -> u64 {
        fs::metadata(self.state().join(name)).unwrap().ino()
    }
}

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

/// Порождённый процесс, который тест не дождался, убивается при выходе из теста.
struct Guard(Option<Child>);

impl Guard {
    fn child(&mut self) -> &mut Child {
        self.0.as_mut().unwrap()
    }

    fn pid(&self) -> u32 {
        self.0.as_ref().unwrap().id()
    }

    fn signal(&self, signal: Signal) {
        kill_process(Pid::from_child(self.0.as_ref().unwrap()), signal).unwrap();
    }

    /// Дождаться выхода и вернуть статус и остаток stderr.
    fn wait(&mut self) -> (ExitStatus, String) {
        let mut child = self.0.take().unwrap();
        let mut stderr = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            pipe.read_to_string(&mut stderr).unwrap();
        }
        (child.wait().unwrap(), stderr)
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Владелец с открытым stdout.
struct Owner {
    guard: Guard,
    stdout: BufReader<ChildStdout>,
}

impl Owner {
    fn pid(&self) -> u32 {
        self.guard.pid()
    }

    /// Следующая строка stdout. Конец вывода — провал с stderr владельца.
    fn line(&mut self) -> String {
        let mut line = String::new();
        if self.stdout.read_line(&mut line).unwrap() == 0 {
            let (status, stderr) = self.guard.wait();
            panic!("владелец вышел ({status}) без строки, stderr: {stderr}");
        }
        line.strip_suffix('\n').unwrap().to_owned()
    }

    /// Строки обработки одного запуска: активация, затем JSON кадра и квитанции.
    fn handled(&mut self) -> Value {
        assert_eq!(self.line(), ACTIVATION);
        let line = self.line();
        let body = line
            .strip_prefix(HANDLED)
            .unwrap_or_else(|| panic!("не строка обработки: {line}"));
        serde_json::from_str(body).unwrap()
    }

    /// Сигнал, выход и остаток stdout.
    fn stop(mut self, signal: Signal) -> (ExitStatus, String, String) {
        self.guard.signal(signal);
        let mut rest = String::new();
        self.stdout.read_to_string(&mut rest).unwrap();
        let (status, stderr) = self.guard.wait();
        (status, rest, stderr)
    }
}

fn os(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// Строка исхода передачи второго запуска фикстуры владельцу `pid`.
fn fixture_handoff_line(pid: u32) -> String {
    format!(
        "shellio: передача активации pid {pid}: второй запуск активен — нет, приложение владельца \
         найдено — нет, активация владельца — не запрашивалась, LaunchServices — не запрашивался\n"
    )
}

fn assert_clean_exit(status: ExitStatus, rest: &str, stderr: &str, cause: &str) {
    assert_eq!(status.code(), Some(0), "stderr: {stderr}");
    assert_eq!(rest, format!("shellio: завершение: {cause}\n"));
    assert!(stderr.is_empty(), "stderr: {stderr}");
}

#[test]
fn second_launch_forwards_arguments_to_owner() {
    let fixture = Fixture::new();
    let mut owner = fixture.owner();
    let pid = owner.pid();
    assert_eq!(fixture.hello_pid(), pid);
    // Собственные аргументы владельца идут тем же обработчиком.
    let own = owner.handled();
    assert_eq!(own["launch"]["args"], serde_json::json!([]));
    assert_eq!(own["receipt"]["activation"], "no_ui");

    let work = fixture.work();
    fs::create_dir(work.join("folder")).unwrap();
    fs::write(work.join("file.txt"), b"").unwrap();
    let folder = work.join("folder").display().to_string();
    let file = work.join("file.txt").display().to_string();
    let missing = work.join("missing").display().to_string();
    let mut args = os(&["folder", &file, "missing", "-x"]);
    args.push(OsString::from_vec(b"bad\xff".to_vec()));

    let output = fixture.second(&args);
    let stderr = text(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");
    assert_eq!(
        stderr,
        fixture_handoff_line(pid),
        "исход передачи одной строкой"
    );
    assert_eq!(
        text(&output.stdout),
        format!(
            "shellio: запуск передан владельцу (pid {pid}), активации нет: владелец без UI\n\
             shellio: {folder}: folder_open_unsupported — папка не открыта: открытие папок пока не поддерживается\n\
             shellio: {file}: not_a_directory — не открыт: это не папка\n\
             shellio: {missing}: path_not_found — не открыт: путь не найден\n\
             shellio: -x: unknown_argument — неизвестный аргумент\n\
             shellio: bad\u{fffd}: arg_not_utf8 — не передан: аргумент не в UTF-8\n"
        )
    );
    let handled = owner.handled();
    assert_eq!(
        handled["launch"],
        serde_json::json!({
            "args": [folder, file, missing, "-x"],
            "cwd": work.display().to_string(),
        })
    );
    assert_eq!(
        handled["receipt"]["refused"][0],
        serde_json::json!({"arg": folder, "reason": "folder_open_unsupported"})
    );

    let output = fixture.second(&[]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        text(&output.stdout),
        format!("shellio: запуск передан владельцу (pid {pid}), активации нет: владелец без UI\n")
    );
    assert_eq!(text(&output.stderr), fixture_handoff_line(pid));
    assert_eq!(owner.handled()["launch"]["args"], serde_json::json!([]));

    assert_eq!(fixture.hello_pid(), pid, "владелец прежний");
    let (status, rest, stderr) = owner.stop(Signal::TERM);
    assert_clean_exit(status, &rest, &stderr, "SIGTERM");
}

/// `--gallery` второго запуска живому владельцу: отказ в квитанции, код 0, режим владельца не
/// меняется (ADR-0006, «Уточнено в P1.3»).
#[test]
fn second_launch_with_gallery_gets_refusal_in_receipt() {
    let fixture = Fixture::new();
    let mut owner = fixture.owner();
    let pid = owner.pid();
    owner.handled();

    let output = fixture.second(&os(&["--gallery"]));
    let stderr = text(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");
    assert_eq!(stderr, fixture_handoff_line(pid));
    assert_eq!(
        text(&output.stdout),
        format!(
            "shellio: запуск передан владельцу (pid {pid}), активации нет: владелец без UI\n\
             shellio: --gallery: gallery_not_at_start — галерея открывается только при запуске \
             владельца\n"
        )
    );
    let handled = owner.handled();
    assert_eq!(handled["launch"]["args"], serde_json::json!(["--gallery"]));
    assert_eq!(
        handled["receipt"]["refused"],
        serde_json::json!([{"arg": "--gallery", "reason": "gallery_not_at_start"}])
    );

    assert_eq!(fixture.hello_pid(), pid, "владелец прежний");
    let (status, rest, stderr) = owner.stop(Signal::TERM);
    assert_clean_exit(status, &rest, &stderr, "SIGTERM");
}

/// `shellioctl` из того же каталога сборки, что фикстура. `CARGO_BIN_EXE_*` видит только бинари
/// своего пакета, а `cargo test` в корне workspace (`make check`) собирает бинари всех пакетов до
/// запуска тестов.
fn shellioctl() -> PathBuf {
    let path = std::path::Path::new(FIXTURE).with_file_name("shellioctl");
    assert!(
        path.is_file(),
        "{} не собран: тест идёт из `cargo test` в корне workspace",
        path.display()
    );
    path
}

impl Fixture {
    /// `shellioctl` с пустым окружением и профилем фикстуры.
    fn ctl(&self, args: &[&str]) -> Output {
        Command::new(shellioctl())
            .env_clear()
            .env("SHELLIO_STATE_DIR", self.state())
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }
}

/// Запросы `ui` к владельцу без UI: `shellioctl` получает `no_ui` с кодом 1 и в JSON, и в тексте;
/// владелец после них жив. Без владельца — `not_running` (69), ошибка использования — 2.
#[test]
fn shellioctl_ui_calls_to_fixture_get_no_ui() {
    let fixture = Fixture::new();
    let gone = fixture.ctl(&["ui", "window", "--json"]);
    assert_eq!(gone.status.code(), Some(69), "{gone:?}");

    let mut owner = fixture.owner();
    owner.handled();
    for call in [
        &["ui", "window"][..],
        &["ui", "inspect"],
        &["ui", "stats"],
        &["ui", "appearance", "dark"],
        &["window", "close"],
        &["app", "quit"],
    ] {
        let mut args = call.to_vec();
        args.push("--json");
        let output = fixture.ctl(&args);
        assert_eq!(output.status.code(), Some(1), "{call:?}: {output:?}");
        let frame = Response::parse(text(&output.stdout).trim_end().as_bytes()).unwrap();
        match frame {
            Response::Error { error, .. } => {
                assert_eq!(error.code(), ControlErrorCode::NoUi, "{call:?}");
                assert!(!error.retryable());
            }
            other => panic!("{call:?}: ожидался no_ui, получено {other:?}"),
        }
        assert!(output.stderr.is_empty(), "{call:?}: {output:?}");

        let plain = fixture.ctl(call);
        assert_eq!(plain.status.code(), Some(1), "{call:?}");
        assert!(plain.stdout.is_empty(), "{call:?}: {plain:?}");
        assert!(
            text(&plain.stderr).starts_with("shellioctl: no_ui: "),
            "{call:?}: {plain:?}"
        );
    }
    let usage = fixture.ctl(&["ui", "appearance", "sepia"]);
    assert_eq!(usage.status.code(), Some(2));

    assert_eq!(fixture.hello_pid(), owner.pid(), "владелец жив");
    let (status, rest, stderr) = owner.stop(Signal::TERM);
    assert_clean_exit(status, &rest, &stderr, "SIGTERM");
}

/// Владелец с `--gallery` называет режим в строке готовности, а его собственный `launch` идёт
/// без флага и без отказов.
#[test]
fn gallery_owner_names_its_mode_and_handles_own_launch_without_flag() {
    let fixture = Fixture::new();
    let mut owner = fixture.owner_with(&os(&["--gallery"]), "gallery");
    let own = owner.handled();
    assert_eq!(own["launch"]["args"], serde_json::json!([]));
    assert_eq!(own["receipt"]["refused"], serde_json::json!([]));

    let output = fixture.second(&[]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(owner.handled()["receipt"]["refused"], serde_json::json!([]));
    let (status, rest, stderr) = owner.stop(Signal::TERM);
    assert_clean_exit(status, &rest, &stderr, "SIGTERM");
}

/// Вывод процесса, который должен выйти сам. Не вышел за [`EXIT_GUARD`] — процесс убивается,
/// тест падает, а не зависает.
fn output_within_guard(command: &mut Command) -> Output {
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = Pid::from_child(&child);
    let (done, outcome) = mpsc::channel();
    thread::spawn(move || {
        let _ = done.send(child.wait_with_output());
    });
    match outcome.recv_timeout(EXIT_GUARD) {
        Ok(output) => output.unwrap(),
        Err(_) => {
            let _ = kill_process(pid, Signal::KILL);
            panic!("процесс не вышел за {EXIT_GUARD:?}");
        }
    }
}

/// Галерея на профиле по умолчанию — ошибка использования до каталога состояния и блокировки, у
/// продуктового бинаря и у фикстуры.
#[test]
fn gallery_without_explicit_profile_is_a_usage_error() {
    for exe in [SHELLIO, FIXTURE] {
        let fixture = Fixture::new();
        let home = fixture.root.join("home");
        fs::create_dir(&home).unwrap();
        let output = output_within_guard(
            Command::new(exe)
                .env_clear()
                .env("HOME", &home)
                .arg("--gallery")
                .current_dir(fixture.work()),
        );
        let stderr = text(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "{exe}: {stderr}");
        assert!(output.stdout.is_empty(), "{exe}");
        assert!(
            stderr.contains("галерея запускается только на отдельном профиле"),
            "{exe}: {stderr}"
        );
        assert_eq!(
            fs::read_dir(&home).unwrap().count(),
            0,
            "{exe}: каталог состояния профиля по умолчанию не создан"
        );

        let output = output_within_guard(
            Command::new(exe)
                .env_clear()
                .env("HOME", &home)
                .args(["--gallery", "--self-check"])
                .current_dir(fixture.work()),
        );
        assert_eq!(output.status.code(), Some(2), "{exe}");
        assert!(text(&output.stderr).contains("--self-check"), "{exe}");
    }
}

#[test]
fn two_simultaneous_claimants_yield_one_owner() {
    let fixture = Fixture::new();
    let mut guards = [fixture.spawn(&[]), fixture.spawn(&[])];
    let (lines, received) = mpsc::channel();
    for (index, guard) in guards.iter_mut().enumerate() {
        let stdout = BufReader::new(guard.child().stdout.take().unwrap());
        let lines = lines.clone();
        thread::spawn(move || {
            for line in stdout.lines() {
                let _ = lines.send((index, Some(line.unwrap())));
            }
            let _ = lines.send((index, None));
        });
    }
    drop(lines);

    // Владелец печатает готовность и живёт, второй печатает квитанцию и закрывает вывод.
    let mut output = [Vec::new(), Vec::new()];
    let mut ready = None;
    let mut closed = None;
    while ready.is_none() || closed.is_none() {
        match received.recv().unwrap() {
            (index, Some(line)) => {
                if line.starts_with(READY) {
                    assert_eq!(ready.replace(index), None, "два владельца");
                }
                output[index].push(line);
            }
            (index, None) => {
                assert_eq!(closed.replace(index), None, "оба вышли");
            }
        }
    }
    let (owner, second) = (ready.unwrap(), closed.unwrap());
    assert_ne!(owner, second);
    let (status, stderr) = guards[second].wait();
    assert_eq!(status.code(), Some(0), "stderr: {stderr}");
    let owner_pid = guards[owner].pid();
    assert_eq!(
        output[second],
        [format!(
            "shellio: запуск передан владельцу (pid {owner_pid}), активации нет: владелец без UI"
        )]
    );
    assert_eq!(fixture.hello_pid(), owner_pid);

    guards[owner].signal(Signal::TERM);
    let (status, stderr) = guards[owner].wait();
    assert_eq!(status.code(), Some(0), "stderr: {stderr}");
}

#[test]
fn killed_owner_is_replaced_by_next_launch() {
    let fixture = Fixture::new();
    let mut first = fixture.owner();
    let first_pid = first.pid();
    let lock = fixture.inode("shellio.lock");
    let socket = fixture.inode("shellio.sock");
    first.guard.child().kill().unwrap();
    first.guard.wait();
    assert_eq!(fixture.inode("shellio.sock"), socket, "сокет остался");

    let next = fixture.owner();
    assert_ne!(next.pid(), first_pid);
    assert_eq!(fixture.hello_pid(), next.pid());
    assert_ne!(fixture.inode("shellio.sock"), socket, "сокет заменён");
    assert_eq!(
        fixture.inode("shellio.lock"),
        lock,
        "файл блокировки прежний"
    );

    let (status, rest, stderr) = next.stop(Signal::TERM);
    assert_eq!(status.code(), Some(0), "stderr: {stderr}");
    assert!(rest.ends_with("shellio: завершение: SIGTERM\n"), "{rest}");
}

#[test]
fn terminated_owner_cleans_up_and_keeps_accepted_command() {
    let fixture = Fixture::new();
    let mut owner = fixture.owner();
    owner.handled();
    let lock = fixture.inode("shellio.lock");
    fixture.call(RequestBody::Command {
        command: CoreCommand::WorkspaceCreate {
            title: "Сохранённый".to_owned(),
        },
        scope: TreeScope::All,
    });

    let (status, rest, stderr) = owner.stop(Signal::TERM);
    assert_clean_exit(status, &rest, &stderr, "SIGTERM");
    let profile = fixture.profile();
    assert!(!socket_path(&profile).exists(), "сокет удалён");
    assert_eq!(
        fixture.inode("shellio.lock"),
        lock,
        "файл блокировки на месте"
    );
    assert_eq!(probe_owner(&profile).unwrap(), Probe::NotRunning);

    let mut next = fixture.owner();
    next.handled();
    let titles: Vec<_> = fixture
        .tree()
        .workspaces
        .iter()
        .map(|workspace| workspace.title.as_str().to_owned())
        .collect();
    assert!(
        titles.iter().any(|title| title == "Сохранённый"),
        "{titles:?}"
    );

    let (status, rest, stderr) = next.stop(Signal::INT);
    assert_clean_exit(status, &rest, &stderr, "SIGINT");
    assert!(!socket_path(&profile).exists());
    assert_eq!(probe_owner(&profile).unwrap(), Probe::NotRunning);
}

#[test]
fn start_failure_keeps_foreign_node_and_frees_lock() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.state()).unwrap();
    let foreign = socket_path(&fixture.profile());
    fs::write(&foreign, b"foreign").unwrap();

    let output = fixture.second(&[]);
    let stderr = text(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "stderr: {stderr}");
    assert!(output.stdout.is_empty());
    assert!(
        stderr.starts_with("shellio: запуск не удался: ") && stderr.contains("обычный файл"),
        "{stderr}"
    );
    assert_eq!(fs::read(&foreign).unwrap(), b"foreign");
    assert_eq!(probe_owner(&fixture.profile()).unwrap(), Probe::NotRunning);
}

#[test]
fn unusable_profile_is_refused_before_lock() {
    let fixture = Fixture::new();
    let output = Command::new(FIXTURE)
        .env_clear()
        .env("SHELLIO_STATE_DIR", "relative")
        .current_dir(fixture.work())
        .output()
        .unwrap();
    let stderr = text(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "stderr: {stderr}");
    assert!(
        stderr.starts_with("shellio: запуск не удался: профиль не вычислен"),
        "{stderr}"
    );
    assert_eq!(
        fs::read_dir(fixture.work()).unwrap().count(),
        0,
        "относительный каталог состояния не создан"
    );
}

/// Профиль с созданным каталогом состояния для выбора роли в процессе теста.
fn claim_profile(fixture: &Fixture) -> Profile {
    let profile = fixture.profile();
    profile.ensure_state_dir().unwrap();
    profile
}

/// Факты передачи, которые возвращают подделки порта.
const FACTS: Handoff = Handoff {
    self_active: true,
    owner_found: true,
    activated: Some(true),
    reopen: Reopen::NotRequested,
};

/// Запись [`RecordedHandoff`]: PID и исход, `None` — вызов передачи.
type HandoffEntry = (u32, Option<HandoffOutcome>);

/// Передача без UI: факты сразу. Вызовы и исходы записываются по порядку.
#[derive(Clone, Default)]
struct RecordedHandoff {
    log: Arc<Mutex<Vec<HandoffEntry>>>,
}

impl RecordedHandoff {
    fn log(&self) -> Vec<HandoffEntry> {
        self.log.lock().unwrap().clone()
    }
}

impl ActivationHandoff for RecordedHandoff {
    fn hand_off(&self, pid: u32) -> impl Future<Output = Handoff> + Send {
        self.log.lock().unwrap().push((pid, None));
        std::future::ready(FACTS)
    }

    fn outcome(&self, pid: u32, outcome: HandoffOutcome) {
        self.log.lock().unwrap().push((pid, Some(outcome)));
    }
}

/// Передача, которую держит тест: сообщает PID начала, завершается фактами, отправленными
/// тестом, и не завершается, пока их нет. Исходы записываются по порядку.
struct HeldHandoff {
    started: tokio::sync::mpsc::UnboundedSender<u32>,
    facts: Mutex<Option<tokio::sync::oneshot::Receiver<Handoff>>>,
    outcomes: tokio::sync::mpsc::UnboundedSender<(u32, HandoffOutcome)>,
}

/// Управление [`HeldHandoff`] из теста.
struct HandoffControl {
    started: tokio::sync::mpsc::UnboundedReceiver<u32>,
    /// Отправка фактов завершает передачу. Брошенный отправитель — передача зависла.
    facts: tokio::sync::oneshot::Sender<Handoff>,
    outcomes: tokio::sync::mpsc::UnboundedReceiver<(u32, HandoffOutcome)>,
}

impl HeldHandoff {
    fn new() -> (Self, HandoffControl) {
        let (started, started_rx) = tokio::sync::mpsc::unbounded_channel();
        let (facts, facts_rx) = tokio::sync::oneshot::channel();
        let (outcomes, outcomes_rx) = tokio::sync::mpsc::unbounded_channel();
        let handoff = Self {
            started,
            facts: Mutex::new(Some(facts_rx)),
            outcomes,
        };
        let control = HandoffControl {
            started: started_rx,
            facts,
            outcomes: outcomes_rx,
        };
        (handoff, control)
    }
}

impl ActivationHandoff for HeldHandoff {
    fn hand_off(&self, pid: u32) -> impl Future<Output = Handoff> + Send {
        let _ = self.started.send(pid);
        let facts = self.facts.lock().unwrap().take();
        async move {
            match facts {
                Some(facts) => match facts.await {
                    Ok(facts) => facts,
                    Err(_) => pending().await,
                },
                None => pending().await,
            }
        }
    }

    fn outcome(&self, pid: u32, outcome: HandoffOutcome) {
        let _ = self.outcomes.send((pid, outcome));
    }
}

fn launch(fixture: &Fixture) -> Launch {
    Launch {
        args: Vec::new(),
        cwd: fixture.work(),
    }
}

/// Проба клиента держит разделяемую блокировку: претендент проигрывает `try_lock`, сокета нет,
/// он ждёт попытки и после освобождения пробы становится владельцем.
#[tokio::test]
async fn claimant_outlasts_client_probe_and_becomes_owner() {
    let fixture = Fixture::new();
    let profile = claim_profile(&fixture);
    let probe = File::create(lock_path(&profile)).unwrap();
    probe.try_lock_shared().unwrap();

    let clock = ManualClock::new();
    let task = tokio::spawn({
        let (profile, clock, launch) = (profile.clone(), clock.clone(), launch(&fixture));
        async move {
            claim(
                &profile,
                clock,
                SECOND_LAUNCH_TIMEOUT,
                &launch,
                &RecordedHandoff::default(),
            )
            .await
        }
    });
    clock.until_waiting(1).await;
    assert!(!task.is_finished(), "претендент ждёт, пока держится проба");
    // Явное снятие: копия дескриптора у процесса, который соседний тест порождает до exec,
    // держала бы блокировку после одного закрытия.
    probe.unlock().unwrap();
    drop(probe);
    clock.tick();

    match task.await.unwrap().unwrap() {
        Claim::Owner(ownership) => ownership.release().unwrap(),
        Claim::Second(second) => panic!("второй запуск: {second:?}"),
    }
    assert_eq!(clock.waits(), 1, "владелец с первой попытки после пробы");
    assert_eq!(probe_owner(&profile).unwrap(), Probe::NotRunning);
}

/// Блокировка занята, сокета нет до срока: исход повторяемый, код 75.
#[tokio::test]
async fn claim_without_socket_expires_with_retry_code() {
    let fixture = Fixture::new();
    let profile = claim_profile(&fixture);
    let held = Ownership::try_acquire(&profile).unwrap().unwrap();

    let clock = ManualClock::new();
    let task = tokio::spawn({
        let (profile, clock, launch) = (profile.clone(), clock.clone(), launch(&fixture));
        async move {
            claim(
                &profile,
                clock,
                SECOND_LAUNCH_TIMEOUT,
                &launch,
                &RecordedHandoff::default(),
            )
            .await
        }
    });
    clock.until_waiting(1).await;
    clock.expire();

    match task.await.unwrap().unwrap() {
        Claim::Second(second @ Second::Expired(_)) => assert_eq!(second.exit(), Exit::Retry),
        other => panic!("ожидался срок: {other:?}"),
    }
    held.release().unwrap();
    assert!(!socket_path(&profile).exists());
}

/// Второй запуск передаёт активацию PID владельца из `hello` и отправляет `launch` только после
/// передачи: запуск соседнего процесса, прошедший целиком, пока передача ждёт, владелец
/// обрабатывает раньше. Ручные часы не дают сроку передачи истечь, пока идёт соседний процесс.
#[tokio::test]
async fn second_launch_hands_off_to_hello_pid_before_sending_launch() {
    let fixture = Fixture::new();
    let mut owner = fixture.owner();
    let pid = owner.pid();
    owner.handled();

    let (handoff, mut control) = HeldHandoff::new();
    let marker = fixture.root.join("marker").display().to_string();
    let task = tokio::spawn({
        let profile = fixture.profile();
        let launch = Launch {
            args: vec![marker.clone()],
            cwd: fixture.work(),
        };
        let clock = ManualClock::new();
        async move { claim(&profile, clock, SECOND_LAUNCH_TIMEOUT, &launch, &handoff).await }
    });

    assert_eq!(
        control.started.recv().await,
        Some(pid),
        "передача получает PID из hello"
    );
    assert!(!task.is_finished(), "запуск ждёт передачи");
    let output = fixture.second(&[]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        text(&output.stderr)
    );
    control.facts.send(FACTS).unwrap();

    match task.await.unwrap().unwrap() {
        Claim::Second(Second::Delivered { pid: got, .. }) => assert_eq!(got, pid),
        other => panic!("ожидалась квитанция: {other:?}"),
    }
    assert_eq!(owner.handled()["launch"]["args"], serde_json::json!([]));
    assert_eq!(
        owner.handled()["launch"]["args"],
        serde_json::json!([marker]),
        "launch отправлен после передачи"
    );
    assert_eq!(
        control.outcomes.try_recv().unwrap(),
        (pid, HandoffOutcome::Done(FACTS))
    );
    assert!(control.started.try_recv().is_err(), "передача одна");
    assert!(control.outcomes.try_recv().is_err(), "исход один");
    let (status, rest, stderr) = owner.stop(Signal::TERM);
    assert_clean_exit(status, &rest, &stderr, "SIGTERM");
}

/// Передача второго запуска-процесса зависла: по сроку передачи `launch` всё равно доставлен,
/// квитанция в stdout прежняя, исход «срок истёк» — одной строкой в stderr.
#[test]
fn second_launch_process_delivers_launch_after_hung_handoff_expires() {
    let fixture = Fixture::new();
    let mut owner = fixture.owner();
    let pid = owner.pid();
    owner.handled();

    let output = fixture.second_with_hung_handoff();
    let stderr = text(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");
    assert_eq!(
        text(&output.stdout),
        format!("shellio: запуск передан владельцу (pid {pid}), активации нет: владелец без UI\n")
    );
    assert_eq!(
        stderr,
        format!("shellio: передача активации pid {pid}: срок передачи истёк\n")
    );
    assert_eq!(owner.handled()["launch"]["args"], serde_json::json!([]));

    let (status, rest, stderr) = owner.stop(Signal::TERM);
    assert_clean_exit(status, &rest, &stderr, "SIGTERM");
}

/// PID поддельного владельца в `hello`.
const FAKE_PID: u32 = 77;

/// Поддельный владелец: блокировку профиля держит тест, на опубликованном сокете отвечают
/// заданные кадры.
struct FakeOwner {
    listener: tokio::net::UnixListener,
    ownership: Ownership,
}

impl FakeOwner {
    fn start(profile: &Profile) -> Self {
        let mut ownership = Ownership::try_acquire(profile).unwrap().unwrap();
        let listener = tokio::net::UnixListener::from_std(ownership.publish().unwrap()).unwrap();
        Self {
            listener,
            ownership,
        }
    }

    async fn accept(&self) -> FakePeer {
        let (stream, _) = self.listener.accept().await.unwrap();
        let (reader, writer) = stream.into_split();
        FakePeer {
            reader: LineReader::new(tokio::io::BufReader::new(reader), REQUEST_LINE_LIMIT),
            writer,
        }
    }

    fn stop(self) {
        drop(self.listener);
        self.ownership.release().unwrap();
    }
}

/// Соединение поддельного владельца со вторым запуском.
struct FakePeer {
    reader: LineReader<tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>>,
    writer: tokio::net::unix::OwnedWriteHalf,
}

impl FakePeer {
    /// Следующая строка, `None` — второй запуск закрыл соединение.
    async fn line(&mut self) -> Option<String> {
        self.reader
            .next_line()
            .await
            .unwrap()
            .map(|line| String::from_utf8_lossy(line).into_owned())
    }

    async fn reply(&mut self, frame: &Response) {
        self.writer
            .write_all(&encode_line(frame).unwrap())
            .await
            .unwrap();
    }

    /// Прочитать `hello` и ответить владельцем в состоянии `owner`.
    async fn hello(&mut self, owner: OwnerState) {
        let line = self.line().await.unwrap();
        assert!(line.contains("\"hello\""), "{line}");
        self.reply(&Response::Ok {
            id: RequestId(1),
            value: OkValue::Hello(HelloReply {
                protocol: PROTOCOL,
                server: "fake".to_owned(),
                pid: FAKE_PID,
                owner,
            }),
        })
        .await;
    }

    /// Прочитать `launch`.
    async fn launch(&mut self) {
        let line = self.line().await.unwrap();
        assert!(line.contains("\"launch\""), "{line}");
    }
}

fn spawn_claim(
    profile: &Profile,
    clock: &ManualClock,
    fixture: &Fixture,
) -> tokio::task::JoinHandle<Second> {
    spawn_claim_with(profile, clock, fixture, RecordedHandoff::default())
}

fn spawn_claim_with(
    profile: &Profile,
    clock: &ManualClock,
    fixture: &Fixture,
    handoff: impl ActivationHandoff + Send + 'static,
) -> tokio::task::JoinHandle<Second> {
    let (profile, clock, launch) = (profile.clone(), clock.clone(), launch(fixture));
    tokio::spawn(async move {
        match claim(&profile, clock, SECOND_LAUNCH_TIMEOUT, &launch, &handoff).await {
            Ok(Claim::Second(second)) => second,
            other => panic!("ожидался второй запуск: {other:?}"),
        }
    })
}

fn receipt_frame() -> Response {
    Response::Ok {
        id: RequestId(2),
        value: OkValue::Launch(LaunchReceipt {
            activation: Activation::NoUi,
            refused: Vec::new(),
        }),
    }
}

/// Зависшая передача истекает ровно через [`HANDOFF_TIMEOUT`] от `hello`: `launch` отправлен,
/// квитанция получена, код 0. Передача, завершённая до этого момента, даёт свои факты.
#[tokio::test]
async fn hung_handoff_expires_after_handoff_timeout_and_launch_is_sent() {
    let fixture = Fixture::new();
    let profile = claim_profile(&fixture);
    let fake = FakeOwner::start(&profile);
    let clock = ManualClock::new();

    let (handoff, mut hung) = HeldHandoff::new();
    let task = spawn_claim_with(&profile, &clock, &fixture, handoff);
    let mut peer = fake.accept().await;
    peer.hello(OwnerState::Ready).await;
    assert_eq!(hung.started.recv().await, Some(FAKE_PID));
    clock.advance(HANDOFF_TIMEOUT);
    assert_eq!(
        hung.outcomes.recv().await,
        Some((FAKE_PID, HandoffOutcome::Expired))
    );
    peer.launch().await;
    peer.reply(&receipt_frame()).await;
    let second = task.await.unwrap();
    assert!(
        matches!(second, Second::Delivered { pid: FAKE_PID, .. }),
        "{second:?}"
    );
    assert_eq!(second.exit().code(), 0);

    let (handoff, mut held) = HeldHandoff::new();
    let task = spawn_claim_with(&profile, &clock, &fixture, handoff);
    let mut peer = fake.accept().await;
    peer.hello(OwnerState::Ready).await;
    assert_eq!(held.started.recv().await, Some(FAKE_PID));
    clock.advance(HANDOFF_TIMEOUT - Duration::from_millis(1));
    held.facts.send(FACTS).unwrap();
    assert_eq!(
        held.outcomes.recv().await,
        Some((FAKE_PID, HandoffOutcome::Done(FACTS))),
        "срок передачи ещё не истёк"
    );
    peer.launch().await;
    peer.reply(&receipt_frame()).await;
    assert!(matches!(task.await.unwrap(), Second::Delivered { .. }));
    assert_eq!(clock.waits(), 0, "повтора нет");
    drop(hung);
    fake.stop();
}

/// Передача, начатая меньше чем за [`HANDOFF_TIMEOUT`] до конца срока второго запуска,
/// обрезается этим сроком: `launch` не отправлен, исход повторяемый, код 75.
#[tokio::test]
async fn handoff_is_cut_by_rest_of_second_launch_deadline() {
    let fixture = Fixture::new();
    let profile = claim_profile(&fixture);
    let fake = FakeOwner::start(&profile);
    let clock = ManualClock::new();

    let (handoff, mut control) = HeldHandoff::new();
    let task = spawn_claim_with(&profile, &clock, &fixture, handoff);
    // Подключение принято: срок второго запуска уже выдан.
    let mut peer = fake.accept().await;
    let half = HANDOFF_TIMEOUT / 2;
    clock.advance(SECOND_LAUNCH_TIMEOUT - half);
    peer.hello(OwnerState::Ready).await;
    assert_eq!(control.started.recv().await, Some(FAKE_PID));
    clock.advance(half);

    let second = task.await.unwrap();
    assert!(matches!(second, Second::Expired(_)), "{second:?}");
    assert_eq!(second.exit().code(), 75);
    assert_eq!(
        control.outcomes.try_recv().unwrap(),
        (FAKE_PID, HandoffOutcome::Expired)
    );
    assert_eq!(peer.line().await, None, "launch не отправлен");
    assert_eq!(clock.waits(), 0);
    drop(control);
    fake.stop();
}

/// После полной отправки `launch` обрыв даёт неизвестный исход без повтора: владелец не получит
/// аргументы дважды.
#[tokio::test]
async fn launch_cut_after_full_send_is_unknown_without_retry() {
    let fixture = Fixture::new();
    let profile = claim_profile(&fixture);
    let fake = FakeOwner::start(&profile);
    let clock = ManualClock::new();
    let task = spawn_claim(&profile, &clock, &fixture);

    let mut peer = fake.accept().await;
    peer.hello(OwnerState::Ready).await;
    peer.launch().await;
    drop(peer);

    let second = task.await.unwrap();
    assert!(matches!(second, Second::Unknown(_)), "{second:?}");
    assert_eq!(second.exit(), Exit::OutcomeUnknown);
    assert_eq!(clock.waits(), 0, "повтора нет");
    fake.stop();
}

/// Отказ кадром на `hello` или на `launch` — исход второго запуска с кодом по кадру, без повтора.
#[tokio::test]
async fn refusal_frame_ends_second_launch_without_retry() {
    let fixture = Fixture::new();
    let profile = claim_profile(&fixture);
    let fake = FakeOwner::start(&profile);
    let clock = ManualClock::new();

    let handoff = RecordedHandoff::default();
    let task = spawn_claim_with(&profile, &clock, &fixture, handoff.clone());
    let mut peer = fake.accept().await;
    assert!(peer.line().await.unwrap().contains("\"hello\""));
    peer.reply(&Response::Error {
        id: Some(RequestId(1)),
        error: ControlError::protocol_mismatch(PROTOCOL),
    })
    .await;
    let second = task.await.unwrap();
    assert!(
        matches!(&second, Second::Refused(Response::Error { error, .. })
            if error.code() == ControlErrorCode::ProtocolMismatch),
        "{second:?}"
    );
    assert_eq!(second.exit(), Exit::Failure);
    assert_eq!(peer.line().await, None, "launch не отправлен");
    assert_eq!(handoff.log(), [], "отказ на hello — без передачи");

    let task = spawn_claim(&profile, &clock, &fixture);
    let mut peer = fake.accept().await;
    peer.hello(OwnerState::Ready).await;
    peer.launch().await;
    peer.reply(&Response::Error {
        id: Some(RequestId(2)),
        error: ControlError::malformed_request("поддельный отказ"),
    })
    .await;
    let second = task.await.unwrap();
    assert!(
        matches!(&second, Second::Refused(Response::Error { error, .. })
            if error.code() == ControlErrorCode::MalformedRequest),
        "{second:?}"
    );
    assert_eq!(second.exit(), Exit::Failure);
    assert_eq!(clock.waits(), 0, "повтора нет");
    fake.stop();
}

/// Владелец, который уходит (`hello` с `leaving` или `owner_leaving` на `launch`), `launch` не
/// исполнил: второй запуск повторяет попытку с блокировки и доставляет аргументы следующему.
#[tokio::test]
async fn leaving_owner_is_retried_until_launch_is_delivered() {
    let fixture = Fixture::new();
    let profile = claim_profile(&fixture);
    let fake = FakeOwner::start(&profile);
    let clock = ManualClock::new();
    let handoff = RecordedHandoff::default();
    let task = spawn_claim_with(&profile, &clock, &fixture, handoff.clone());

    let mut leaving = fake.accept().await;
    leaving.hello(OwnerState::Leaving).await;
    clock.until_waiting(1).await;
    clock.tick();

    let mut refusing = fake.accept().await;
    assert_eq!(leaving.line().await, None, "уходящему launch не отправлен");
    refusing.hello(OwnerState::Ready).await;
    refusing.launch().await;
    refusing
        .reply(&Response::Error {
            id: Some(RequestId(2)),
            error: ControlError::owner_leaving(),
        })
        .await;
    clock.until_waiting(2).await;
    clock.tick();

    let mut ready = fake.accept().await;
    ready.hello(OwnerState::Ready).await;
    ready.launch().await;
    ready
        .reply(&Response::Ok {
            id: RequestId(2),
            value: OkValue::Launch(LaunchReceipt {
                activation: Activation::NoUi,
                refused: Vec::new(),
            }),
        })
        .await;

    let second = task.await.unwrap();
    assert!(
        matches!(second, Second::Delivered { pid: FAKE_PID, .. }),
        "{second:?}"
    );
    assert_eq!(second.exit(), Exit::Success);
    assert_eq!(clock.waits(), 2);
    let done = Some(HandoffOutcome::Done(FACTS));
    assert_eq!(
        handoff.log(),
        [
            (FAKE_PID, None),
            (FAKE_PID, done),
            (FAKE_PID, None),
            (FAKE_PID, done)
        ],
        "передача на каждый hello с ready, уходящему — нет"
    );
    fake.stop();
}
