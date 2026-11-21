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
use std::io::{BufRead, BufReader, Read};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, ExitStatus, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use rustix::process::{Pid, Signal, kill_process};
use serde_json::Value;
use shellio::owner::{Claim, SECOND_LAUNCH_TIMEOUT, Second, claim};
use shellio_control::test_support::ManualClock;
use shellio_control::{
    Address, Client, Exit, Launch, OkValue, Ownership, Probe, RequestBody, Response, TokioClock,
    lock_path, probe_owner, socket_path,
};
use shellio_core::{Command as CoreCommand, Query, ReplyValue, TreeScope, TreeView};
use shellio_store::Profile;
use tempfile::TempDir;

const FIXTURE: &str = env!("CARGO_BIN_EXE_shellio-owner-fixture");
const READY: &str = "shellio: владелец готов, pid ";
const HANDLED: &str = "shellio: запуск обработан ";
const ACTIVATION: &str = "shellio-owner-fixture: активация без UI";
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
        let mut guard = self.spawn(&[]);
        let stdout = BufReader::new(guard.child().stdout.take().unwrap());
        let mut owner = Owner { guard, stdout };
        let line = owner.line();
        let pid = owner.pid();
        assert_eq!(
            line,
            format!(
                "{READY}{pid}, сокет {}",
                socket_path(&self.profile()).display()
            )
        );
        owner
    }

    /// Второй запуск до выхода.
    fn second(&self, args: &[OsString]) -> Output {
        self.command(args).output().unwrap()
    }

    fn client(&self) -> Client<TokioClock> {
        Client::new(
            Address::Profile(self.profile()),
            TokioClock::default(),
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
    assert!(stderr.is_empty(), "stderr: {stderr}");
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
    assert_eq!(owner.handled()["launch"]["args"], serde_json::json!([]));

    assert_eq!(fixture.hello_pid(), pid, "владелец прежний");
    let (status, rest, stderr) = owner.stop(Signal::TERM);
    assert_clean_exit(status, &rest, &stderr, "SIGTERM");
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
    let output = Command::new(FIXTURE)
        .env_clear()
        .env("SHELLIO_STATE_DIR", "relative")
        .output()
        .unwrap();
    let stderr = text(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "stderr: {stderr}");
    assert!(
        stderr.starts_with("shellio: запуск не удался: профиль не вычислен"),
        "{stderr}"
    );
}

/// Профиль с созданным каталогом состояния для выбора роли в процессе теста.
fn claim_profile(fixture: &Fixture) -> Profile {
    let profile = fixture.profile();
    profile.ensure_state_dir().unwrap();
    profile
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
    let mut task = tokio::spawn({
        let (profile, clock, launch) = (profile.clone(), clock.clone(), launch(&fixture));
        async move { claim(&profile, clock, SECOND_LAUNCH_TIMEOUT, &launch).await }
    });
    clock.until_waiting(1).await;
    assert!(!task.is_finished(), "претендент ждёт, пока держится проба");
    drop(probe);
    // Процесс, порождаемый соседним тестом, до exec держит копию дескриптора пробы, и с ней
    // блокировку: тогда претендент ждёт ещё одну попытку.
    let mut waits = 1;
    let claimed = loop {
        clock.tick();
        waits += 1;
        tokio::select! {
            claimed = &mut task => break claimed,
            () = clock.until_waiting(waits) => {}
        }
    };

    match claimed.unwrap().unwrap() {
        Claim::Owner(ownership) => ownership.release().unwrap(),
        Claim::Second(second) => panic!("второй запуск: {second:?}"),
    }
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
        async move { claim(&profile, clock, SECOND_LAUNCH_TIMEOUT, &launch).await }
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
