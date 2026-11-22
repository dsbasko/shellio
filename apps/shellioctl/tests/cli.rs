//! `shellioctl` отдельным процессом против сервера в процессе теста.
//!
//! Процесс получает пустое окружение и только `SHELLIO_STATE_DIR` временного профиля в `/tmp`
//! (путь сокета укладывается в `sun_path`). Сервер публикует сокет под настоящей блокировкой,
//! как владелец. Синхронизация — завершение процессов и строки их вывода. Процессы ждутся
//! в `spawn_blocking`: сервер в это время обслуживает их на рабочих потоках runtime.

use std::error::Error;
use std::fs;
use std::future::{Future, ready};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;

use shellio_control::test_support::ManualClock;
use shellio_control::{
    Activation, ControlErrorCode, LOCK_FILE, Launch, LaunchHandler, LaunchReceipt, OkValue,
    OwnerState, Ownership, RequestId, Response, SHUTDOWN_GRACE, SOCKET_FILE, Server, ServerConfig,
    UiGate, UiHandler, UiRefusal, UiReply, UiRequest, WithoutUi,
};
use shellio_core::test_support::{ManualClock as CoreClock, MemoryPersist, SequentialIds};
use shellio_core::{
    CommandValue, Core, ErrorCode, EventsSince, FsRootResolver, Reply, ReplyValue, Revision, Route,
    TreeView, UnixMillis, WorkspaceId,
};
use shellio_store::Profile;
use tempfile::TempDir;
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

const NOW: UnixMillis = UnixMillis(1_795_000_000_000);
const PID: u32 = 4242;

/// Обработчик запуска без отказов: `shellioctl` его не вызывает.
struct Launcher;

impl LaunchHandler for Launcher {
    fn launch(&self, _launch: Launch) -> impl Future<Output = LaunchReceipt> + Send {
        ready(LaunchReceipt {
            activation: Activation::NoUi,
            refused: Vec::new(),
        })
    }
}

/// Временный каталог в `/tmp` с профилем `state` внутри. Каталог состояния не создаётся.
struct Dir {
    profile: Profile,
    dir: TempDir,
}

impl Dir {
    fn new() -> TestResult<Self> {
        let dir = tempfile::Builder::new()
            .prefix("shellio-")
            .tempdir_in("/tmp")?;
        let profile = Profile::isolated(dir.path().join("state"));
        Ok(Self { profile, dir })
    }

    fn state(&self) -> &Path {
        self.profile.state_dir()
    }

    /// Взять блокировку, как претендент, не публикуя сокет.
    fn hold(&self) -> TestResult<Ownership> {
        self.profile.ensure_state_dir()?;
        Ok(Ownership::try_acquire(&self.profile)?.ok_or("блокировка занята")?)
    }

    fn entries(&self) -> TestResult<Vec<String>> {
        let mut names = fs::read_dir(self.state())?
            .map(|entry| Ok(entry?.file_name().to_string_lossy().into_owned()))
            .collect::<Result<Vec<_>, std::io::Error>>()?;
        names.sort();
        Ok(names)
    }

    /// `shellioctl` с пустым окружением и профилем этого каталога.
    fn ctl(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_shellioctl"));
        command
            .env_clear()
            .env("SHELLIO_STATE_DIR", self.state())
            .args(args)
            .stdin(Stdio::null());
        command
    }

    async fn run(&self, args: &[&str]) -> TestResult<Run> {
        run(self.ctl(args)).await
    }
}

/// Итог процесса.
#[derive(Debug)]
struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Run {
    fn expect(self, code: i32) -> TestResult<Self> {
        if self.code == code {
            Ok(self)
        } else {
            Err(format!("ожидался код {code}, получен {self:?}").into())
        }
    }

    /// Единственная строка stdout как кадр.
    fn frame(&self) -> TestResult<Response> {
        let mut lines = self.stdout.lines();
        let line = lines.next().ok_or("stdout пуст")?;
        if lines.next().is_some() {
            return Err(format!("в stdout больше одной строки: {}", self.stdout).into());
        }
        Ok(Response::parse(line.as_bytes())?)
    }

    fn reply(&self) -> TestResult<Reply> {
        match self.frame()? {
            Response::Ok {
                id: RequestId(2),
                value: OkValue::Reply(reply),
            } => Ok(reply),
            other => Err(format!("ожидался ответ ядра, получено {other:?}").into()),
        }
    }

    fn command_value(&self) -> TestResult<CommandValue> {
        match self.reply()?.value {
            ReplyValue::Command(value) => Ok(value),
            other => Err(format!("ожидался ответ команды, получено {other:?}").into()),
        }
    }

    /// Код отказа клиента или транспорта из кадра `error`.
    fn control_code(&self) -> TestResult<ControlErrorCode> {
        match self.frame()? {
            Response::Error { error, .. } => Ok(error.code()),
            other => Err(format!("ожидался отказ транспорта, получено {other:?}").into()),
        }
    }
}

async fn run(mut command: Command) -> TestResult<Run> {
    let output = tokio::task::spawn_blocking(move || command.output()).await??;
    Ok(Run {
        code: output.status.code().ok_or("процесс завершён сигналом")?,
        stdout: String::from_utf8(output.stdout)?,
        stderr: String::from_utf8(output.stderr)?,
    })
}

/// Владелец профиля: блокировка, опубликованный сокет и сервер над ядром в памяти.
struct Owner<U: UiHandler = WithoutUi> {
    server: Option<Server<ManualClock, Launcher, U>>,
    ownership: Option<Ownership>,
    clock: ManualClock,
}

impl Owner {
    async fn start(dir: &Dir) -> TestResult<Self> {
        Self::start_with(dir, WithoutUi).await
    }
}

impl<U: UiHandler> Owner<U> {
    async fn start_with(dir: &Dir, ui: U) -> TestResult<Self> {
        let mut ownership = dir.hold()?;
        let listener = ownership.publish()?;
        let core = Core::start(
            &Handle::current(),
            Box::new(MemoryPersist::new()),
            Box::new(CoreClock::new(NOW)),
            Box::new(SequentialIds::new()),
            Arc::new(FsRootResolver),
        )?;
        let config = ServerConfig {
            server: "test 0.1.0".to_owned(),
            pid: PID,
            grace: SHUTDOWN_GRACE,
        };
        let clock = ManualClock::new();
        let server = Server::start(
            &Handle::current(),
            listener,
            core,
            Launcher,
            ui,
            clock.clone(),
            config,
        )?;
        Ok(Self {
            server: Some(server),
            ownership: Some(ownership),
            clock,
        })
    }

    fn shutdown(&mut self) -> TestResult<JoinHandle<()>> {
        let server = self.server.take().ok_or("сервер уже завершён")?;
        Ok(tokio::spawn(server.shutdown()))
    }

    /// Завершить сервер, удалить сокет и отпустить блокировку.
    async fn stop(mut self) -> TestResult {
        if self.server.is_some() {
            self.shutdown()?.await?;
        }
        if let Some(ownership) = self.ownership.take() {
            ownership.release()?;
        }
        Ok(())
    }
}

async fn tree(dir: &Dir, scope: &[&str]) -> TestResult<TreeView> {
    let mut args = vec!["tree", "--json"];
    args.extend_from_slice(scope);
    match dir.run(&args).await?.expect(0)?.reply()?.value {
        ReplyValue::Tree(tree) => Ok(tree),
        other => Err(format!("ожидалось дерево, получено {other:?}").into()),
    }
}

fn workspace_title(tree: &TreeView, id: WorkspaceId) -> TestResult<String> {
    let workspace = tree
        .workspaces
        .iter()
        .find(|workspace| workspace.id == id)
        .ok_or("воркспейса нет в дереве")?;
    Ok(workspace.title.to_string())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_command_and_read_with_json_and_text() -> TestResult {
    let dir = Dir::new()?;
    let owner = Owner::start(&dir).await?;

    match dir.run(&["version", "--json"]).await?.expect(0)?.frame()? {
        Response::Ok {
            id: RequestId(1),
            value: OkValue::Hello(hello),
        } => {
            assert_eq!(hello.pid, PID);
            assert_eq!(hello.owner, OwnerState::Ready);
        }
        other => return Err(format!("ожидался hello, получено {other:?}").into()),
    }
    let version = dir.run(&["version"]).await?.expect(0)?;
    assert!(version.stdout.contains("PID 4242"), "{version:?}");

    let status = dir.run(&["status", "--json"]).await?.expect(0)?.reply()?;
    assert!(matches!(status.value, ReplyValue::Status(_)), "{status:?}");
    let status = dir.run(&["status"]).await?.expect(0)?;
    assert!(status.stdout.contains("схема "), "{status:?}");

    let home = tree(&dir, &["--all"])
        .await?
        .active_workspace
        .ok_or("нет активного воркспейса")?;
    let home_id = home.to_string();

    let created = dir
        .run(&["workspace", "new", "Альфа", "--all", "--json"])
        .await?
        .expect(0)?;
    let CommandValue::Workspace { workspace: alpha } = created.command_value()? else {
        return Err(format!("ожидался воркспейс: {created:?}").into());
    };
    let alpha_id = alpha.to_string();
    assert_eq!(
        workspace_title(&tree(&dir, &["--all"]).await?, alpha)?,
        "Альфа"
    );

    let renamed = dir
        .run(&[
            "workspace",
            "rename",
            "--target",
            &alpha_id,
            "Бета",
            "--all",
        ])
        .await?
        .expect(0)?;
    assert!(renamed.stdout.starts_with("ревизия "), "{renamed:?}");
    assert_eq!(
        workspace_title(&tree(&dir, &["--all"]).await?, alpha)?,
        "Бета"
    );

    dir.run(&[
        "workspace",
        "rename",
        "Гамма",
        "--workspace",
        &alpha_id,
        "--json",
    ])
    .await?
    .expect(0)?
    .reply()?;
    assert_eq!(
        workspace_title(&tree(&dir, &["--all"]).await?, alpha)?,
        "Гамма"
    );

    dir.run(&["workspace", "focus", "--workspace", &alpha_id])
        .await?
        .expect(0)?;
    assert_eq!(tree(&dir, &["--all"]).await?.active_workspace, Some(alpha));

    let source = dir.dir.path().join("src/app");
    fs::create_dir_all(&source)?;
    let mut add = dir.ctl(&[
        "project",
        "add",
        "src/app",
        "--title",
        "Приложение",
        "--workspace",
        &alpha_id,
        "--json",
    ]);
    add.current_dir(dir.dir.path());
    let added = run(add).await?.expect(0)?;
    let CommandValue::Project { project, .. } = added.command_value()? else {
        return Err(format!("ожидался проект: {added:?}").into());
    };
    let shown = tree(&dir, &["--workspace", &alpha_id]).await?;
    let listed = &shown.workspaces[0].projects[0];
    assert_eq!(listed.id, project);
    assert_eq!(listed.title.to_string(), "Приложение");
    assert_eq!(
        listed.roots[0].path,
        fs::canonicalize(&source)?,
        "путь от рабочего каталога CLI"
    );

    let bench = dir
        .run(&[
            "workbench",
            "new",
            "--target",
            &project.to_string(),
            "Основная",
            "--all",
            "--json",
        ])
        .await?
        .expect(0)?;
    let CommandValue::Workbench { workbench } = bench.command_value()? else {
        return Err(format!("ожидалась рабочая область: {bench:?}").into());
    };
    let workbench_id = workbench.to_string();
    let renamed = dir
        .run(&[
            "workbench",
            "rename",
            "--target",
            &workbench_id,
            "Вторая",
            "--workspace",
            &alpha_id,
            "--json",
        ])
        .await?
        .expect(0)?
        .reply()?;
    let opened = dir
        .run(&["workbench", "open", "--target", &workbench_id, "--all"])
        .await?
        .expect(0)?;
    assert!(opened.stdout.contains(": готово"), "{opened:?}");

    let shown = tree(&dir, &["--all"]).await?;
    assert_eq!(shown.route, Some(Route::Workbench(workbench)));
    let text = dir.run(&["tree", "--all"]).await?.expect(0)?;
    assert!(
        text.stdout
            .contains(&format!("рабочая область {workbench_id} «Вторая»")),
        "{text:?}"
    );
    assert!(
        text.stdout.contains(&format!("воркспейс {home_id} «")),
        "{text:?}"
    );

    let since = renamed.revision.0.to_string();
    match dir
        .run(&["events", "--since", &since, "--all", "--json"])
        .await?
        .expect(0)?
        .frame()?
    {
        Response::Ok {
            id: RequestId(2),
            value: OkValue::EventsSince(EventsSince::Events(events)),
        } => {
            assert_eq!(events.len(), 1, "{events:?}");
            assert_eq!(events[0].revision, Revision(renamed.revision.0 + 1));
        }
        other => return Err(format!("ожидались события, получено {other:?}").into()),
    }
    let events = dir
        .run(&["events", "--since", &since, "--all"])
        .await?
        .expect(0)?;
    assert!(events.stdout.contains("window_set "), "{events:?}");

    // Каждая мутация — и во втором режиме вывода.
    let project_id = project.to_string();
    let delta = dir
        .run(&["workspace", "new", "Дельта", "--all"])
        .await?
        .expect(0)?;
    assert!(delta.stdout.contains(": воркспейс "), "{delta:?}");
    let focused = dir
        .run(&["workspace", "focus", "--workspace", &home_id, "--json"])
        .await?
        .expect(0)?
        .reply()?;
    assert_eq!(focused.value, ReplyValue::Command(CommandValue::None));
    assert_eq!(tree(&dir, &["--all"]).await?.active_workspace, Some(home));
    let library = dir.dir.path().join("src/lib");
    fs::create_dir_all(&library)?;
    let library = library.to_str().ok_or("путь не в UTF-8")?;
    let added = dir
        .run(&["project", "add", library, "--workspace", &alpha_id])
        .await?
        .expect(0)?;
    assert!(added.stdout.contains(": проект "), "{added:?}");
    assert!(added.stdout.contains(", корень "), "{added:?}");
    let third = dir
        .run(&[
            "workbench",
            "new",
            "--target",
            &project_id,
            "Третья",
            "--workspace",
            &alpha_id,
        ])
        .await?
        .expect(0)?;
    assert!(third.stdout.contains(": рабочая область "), "{third:?}");
    let fourth = dir
        .run(&[
            "workbench",
            "rename",
            "--target",
            &workbench_id,
            "Четвёртая",
            "--all",
        ])
        .await?
        .expect(0)?;
    assert!(fourth.stdout.contains(": готово"), "{fourth:?}");
    let reopened = dir
        .run(&[
            "workbench",
            "open",
            "--target",
            &workbench_id,
            "--workspace",
            &alpha_id,
            "--json",
        ])
        .await?
        .expect(0)?
        .reply()?;
    assert_eq!(reopened.value, ReplyValue::Command(CommandValue::None));
    let shown = tree(&dir, &["--all"]).await?;
    assert_eq!(shown.route, Some(Route::Workbench(workbench)));
    let titles: Vec<String> = shown
        .workspaces
        .iter()
        .flat_map(|workspace| &workspace.projects)
        .flat_map(|project| &project.workbenches)
        .map(|workbench| workbench.title.to_string())
        .collect();
    assert_eq!(titles, ["Четвёртая", "Третья"]);

    owner.stop().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scope_is_checked_by_server() -> TestResult {
    let dir = Dir::new()?;
    let owner = Owner::start(&dir).await?;
    let home = tree(&dir, &["--all"])
        .await?
        .active_workspace
        .ok_or("нет активного воркспейса")?;
    let created = dir
        .run(&["workspace", "new", "Работа", "--all", "--json"])
        .await?
        .expect(0)?;
    let CommandValue::Workspace { workspace: work } = created.command_value()? else {
        return Err(format!("ожидался воркспейс: {created:?}").into());
    };
    let (home_id, work_id) = (home.to_string(), work.to_string());
    let before = tree(&dir, &["--all"]).await?;

    let foreign = dir
        .run(&[
            "workspace",
            "rename",
            "--target",
            &home_id,
            "Чужое",
            "--workspace",
            &work_id,
        ])
        .await?
        .expect(1)?;
    assert!(foreign.stderr.contains("not_found"), "{foreign:?}");
    assert!(foreign.stdout.is_empty(), "{foreign:?}");
    let foreign = dir
        .run(&[
            "workspace",
            "focus",
            "--target",
            &home_id,
            "--workspace",
            &work_id,
            "--json",
        ])
        .await?
        .expect(1)?;
    match foreign.frame()? {
        Response::CoreError { error, .. } => assert_eq!(error.code(), ErrorCode::NotFound),
        other => return Err(format!("ожидался отказ ядра, получено {other:?}").into()),
    }
    assert_eq!(
        tree(&dir, &["--all"]).await?,
        before,
        "ничего не изменилось"
    );

    let scoped = tree(&dir, &["--workspace", &work_id]).await?;
    assert_eq!(
        scoped.active_workspace, None,
        "активный чужой воркспейс скрыт"
    );
    assert_eq!(scoped.route, None);
    let ids: Vec<WorkspaceId> = scoped
        .workspaces
        .iter()
        .map(|workspace| workspace.id)
        .collect();
    assert_eq!(ids, [work]);
    owner.stop().await
}

/// `events --follow --all` отдельным процессом: строки stdout приходят в канал по мере вывода.
struct Follower {
    child: Child,
    lines: mpsc::UnboundedReceiver<String>,
    reader: std::thread::JoinHandle<()>,
}

impl Follower {
    fn spawn(dir: &Dir, extra: &[&str]) -> TestResult<Self> {
        let mut args = vec!["events", "--follow", "--all"];
        args.extend_from_slice(extra);
        let mut child = dir
            .ctl(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdout = child.stdout.take().ok_or("нет stdout")?;
        let (sender, lines) = mpsc::unbounded_channel();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            child,
            lines,
            reader,
        })
    }

    async fn line(&mut self) -> TestResult<String> {
        Ok(self.lines.recv().await.ok_or("поток кончился")?)
    }

    async fn frame(&mut self) -> TestResult<Response> {
        Ok(Response::parse(self.line().await?.as_bytes())?)
    }

    /// Дождаться выхода процесса: код и stderr. После выхода строк больше нет.
    async fn finish(mut self) -> TestResult<(i32, String)> {
        let child = self.child;
        let output = tokio::task::spawn_blocking(move || child.wait_with_output()).await??;
        assert!(
            self.lines.recv().await.is_none(),
            "после последнего кадра вывода нет"
        );
        self.reader.join().map_err(|_| "поток чтения упал")?;
        let code = output.status.code().ok_or("процесс завершён сигналом")?;
        Ok((code, String::from_utf8(output.stderr)?))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn follow_streams_until_owner_leaves() -> TestResult {
    let dir = Dir::new()?;
    let mut owner = Owner::start(&dir).await?;
    let mut follower = Follower::spawn(&dir, &["--json"])?;

    let snapshot = match follower.frame().await? {
        Response::Snapshot {
            id: RequestId(2),
            snapshot,
        } => snapshot,
        other => return Err(format!("ожидался снимок, получено {other:?}").into()),
    };
    let created = dir
        .run(&["workspace", "new", "Альфа", "--all", "--json"])
        .await?
        .expect(0)?
        .reply()?;
    match follower.frame().await? {
        Response::Event {
            id: RequestId(2),
            event,
        } => {
            assert_eq!(event.revision, created.revision);
            assert_eq!(event.revision, Revision(snapshot.revision.0 + 1));
        }
        other => return Err(format!("ожидалось событие, получено {other:?}").into()),
    }

    let done = owner.shutdown()?;
    match follower.frame().await? {
        Response::Error {
            id: Some(RequestId(2)),
            error,
        } => assert_eq!(error.code(), ControlErrorCode::OwnerLeaving),
        other => return Err(format!("ожидался owner_leaving, получено {other:?}").into()),
    }
    let (code, stderr) = follower.finish().await?;
    assert_eq!(code, 75);
    assert!(stderr.is_empty(), "{stderr}");
    done.await?;
    owner.stop().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn follow_in_text_reports_owner_leaving_in_stderr() -> TestResult {
    let dir = Dir::new()?;
    let mut owner = Owner::start(&dir).await?;
    let mut follower = Follower::spawn(&dir, &[])?;

    let first = follower.line().await?;
    assert!(first.starts_with("снимок: ревизия "), "{first}");
    dir.run(&["workspace", "new", "Альфа", "--all"])
        .await?
        .expect(0)?;
    let event = follower.line().await?;
    assert!(event.starts_with("ревизия "), "{event}");
    assert!(event.contains("workspace_inserted "), "{event}");
    assert!(event.contains("«Альфа»"), "{event}");

    let done = owner.shutdown()?;
    let (code, stderr) = follower.finish().await?;
    assert_eq!(code, 75);
    assert!(stderr.contains("owner_leaving"), "{stderr}");
    done.await?;
    owner.stop().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_profile_is_not_running_without_creating_files() -> TestResult {
    let dir = Dir::new()?;
    let missing = dir.run(&["tree", "--all"]).await?.expect(69)?;
    assert!(missing.stderr.contains("not_running"), "{missing:?}");
    assert!(!dir.state().exists(), "каталог состояния не создан");

    fs::create_dir(dir.state())?;
    let empty = dir.run(&["status", "--json"]).await?.expect(69)?;
    assert_eq!(empty.control_code()?, ControlErrorCode::NotRunning);
    assert!(empty.stderr.is_empty(), "{empty:?}");
    assert!(dir.entries()?.is_empty(), "файлы не созданы");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn held_lock_without_socket_is_owner_starting() -> TestResult {
    let dir = Dir::new()?;
    let _contender = dir.hold()?;
    let starting = dir
        .run(&["--timeout", "50", "status", "--json"])
        .await?
        .expect(75)?;
    assert_eq!(starting.control_code()?, ControlErrorCode::OwnerStarting);
    let starting = dir
        .run(&["--timeout", "50", "tree", "--all"])
        .await?
        .expect(75)?;
    assert!(starting.stderr.contains("owner_starting"), "{starting:?}");
    assert_eq!(dir.entries()?, [LOCK_FILE], "сокет не создан");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn free_lock_after_owner_stops_is_not_running() -> TestResult {
    let dir = Dir::new()?;
    Owner::start(&dir).await?.stop().await?;
    assert_eq!(
        dir.entries()?,
        [LOCK_FILE],
        "файл блокировки на месте, сокета нет"
    );
    let stopped = dir.run(&["version"]).await?.expect(69)?;
    assert!(stopped.stderr.contains("not_running"), "{stopped:?}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_missing_socket_is_unavailable() -> TestResult {
    let dir = Dir::new()?;
    let owner = Owner::start(&dir).await?;
    let missing: PathBuf = dir.dir.path().join("missing.sock");
    let missing = missing.to_str().ok_or("путь не в UTF-8")?;

    let flagged = dir
        .run(&["--socket", missing, "--timeout", "50", "version", "--json"])
        .await?
        .expect(75)?;
    assert_eq!(flagged.control_code()?, ControlErrorCode::Unavailable);

    let mut by_env = dir.ctl(&["--timeout", "50", "status"]);
    by_env.env("SHELLIO_SOCKET", missing);
    let by_env = run(by_env).await?.expect(75)?;
    assert!(by_env.stderr.contains("unavailable"), "{by_env:?}");

    let socket = dir.state().join(SOCKET_FILE);
    let socket = socket.to_str().ok_or("путь не в UTF-8")?;
    let explicit = dir
        .run(&["--socket", socket, "status", "--json"])
        .await?
        .expect(0)?;
    assert!(matches!(explicit.reply()?.value, ReplyValue::Status(_)));
    owner.stop().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_errors_exit_2_without_contacting_owner() -> TestResult {
    let dir = Dir::new()?;
    for args in [
        &["tree"][..],
        &["status", "--all"],
        &[
            "version",
            "--workspace",
            "00000000-0000-0000-0000-000000000001",
        ],
        &["events", "--since", "1", "--follow", "--all"],
        &["workspace", "rename", "Бета", "--all"],
        &["launch", "/tmp"],
    ] {
        let refused = dir.run(args).await?.expect(2)?;
        assert!(refused.stdout.is_empty(), "{args:?}: {refused:?}");
        assert!(refused.stderr.contains("--help"), "{args:?}: {refused:?}");
    }
    let mut empty_env = dir.ctl(&["status"]);
    empty_env.env("SHELLIO_SOCKET", "");
    run(empty_env).await?.expect(2)?;

    let help = dir.run(&["--help"]).await?.expect(0)?;
    assert!(
        help.stdout.starts_with("Использование: shellioctl"),
        "{help:?}"
    );
    assert!(!dir.state().exists(), "каталог состояния не создан");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_profile_environment_fails_without_socket() -> TestResult {
    let mut bare = Command::new(env!("CARGO_BIN_EXE_shellioctl"));
    bare.env_clear().args(["status"]).stdin(Stdio::null());
    let bare = run(bare).await?.expect(1)?;
    assert!(bare.stderr.contains("профиль"), "{bare:?}");
    Ok(())
}

/// Порт UI, который отдаёт тесту отметку начала каждого запроса и не отвечает никогда.
#[derive(Clone)]
struct StuckUi {
    gates: mpsc::UnboundedSender<UiGate>,
}

impl UiHandler for StuckUi {
    fn ui(
        &self,
        _request: UiRequest,
        gate: UiGate,
    ) -> impl Future<Output = Result<UiReply, UiRefusal>> + Send {
        let _ = self.gates.send(gate);
        std::future::pending()
    }
}

/// Срок порта UI: работа, не начатая на главном потоке, — повторяемый `ui_timeout` (75); начатое
/// действие — `outcome_unknown` (74) без повтора, в JSON и в тексте.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ui_deadline_codes_are_75_and_74() -> TestResult {
    let dir = Dir::new()?;
    let (gates, mut started) = mpsc::unbounded_channel();
    let owner = Owner::start_with(&dir, StuckUi { gates }).await?;
    for (args, begin, code, error) in [
        (
            &["ui", "window", "--json"][..],
            false,
            75,
            ControlErrorCode::UiTimeout,
        ),
        (
            &["window", "close", "--json"],
            true,
            74,
            ControlErrorCode::OutcomeUnknown,
        ),
        (
            &["ui", "appearance", "light", "--json"],
            false,
            75,
            ControlErrorCode::UiTimeout,
        ),
    ] {
        let process = tokio::spawn(run(dir.ctl(args)));
        let gate = started.recv().await.ok_or("порт не вызван")?;
        if begin {
            assert!(gate.begin());
        }
        owner.clock.expire();
        let done = process.await??.expect(code)?;
        assert_eq!(done.control_code()?, error, "{args:?}");
        if !begin {
            assert!(!gate.begin(), "отменённая работа не начинается");
        }
    }
    let process = tokio::spawn(run(dir.ctl(&["app", "quit"])));
    let gate = started.recv().await.ok_or("порт не вызван")?;
    assert!(gate.begin());
    owner.clock.expire();
    let done = process.await??.expect(74)?;
    assert!(done.stdout.is_empty(), "{done:?}");
    assert!(
        done.stderr.starts_with("shellioctl: outcome_unknown: "),
        "{done:?}"
    );
    owner.stop().await
}
