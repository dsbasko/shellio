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
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use shellio_control::test_support::ManualClock;
use shellio_control::{
    Accepted, Axis, AxisSource, CellContrast, CellMotion, CellView, ComponentView, Contrast,
    FontView, InspectView, Lightness, Motion, Rect, Size, StyleView,
};
use shellio_control::{
    Activation, ControlErrorCode, LOCK_FILE, Launch, LaunchHandler, LaunchReceipt, OkValue,
    OwnerState, Ownership, ReceiptView, ReceiptsView, RequestId, Response, SHUTDOWN_GRACE,
    SOCKET_FILE, Server, ServerConfig, UiGate, UiHandler, UiRefusal, UiReply, UiRequest, WithoutUi,
};
use shellio_core::test_support::{
    ManualClock as CoreClock, MemoryPersist, SequentialIds, TestMigrate, TestPlugin, registry,
    registry_of,
};
use shellio_core::{
    CommandValue, Core, CoreError, ErrorCode, EventsSince, FsRootResolver, IdGen, LayoutView,
    OpenOutcome, PanelId, PanelStatus, PanelView, PluginsView, Reply, ReplyValue, Revision, Route,
    TreeView, UnixMillis, WorkbenchId, WorkspaceId,
};
use shellio_layout::{Viewport, Zone};
use shellio_plugin_api::{Cardinality, Registry};
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

    /// Владелец с размером окна: так главный поток передаёт его ядру до публикации сокета.
    async fn start_sized(dir: &Dir, viewport: Viewport) -> TestResult<Self> {
        Self::start_inner(
            dir,
            WithoutUi,
            Some(viewport),
            MemoryPersist::new(),
            registry(),
        )
        .await
    }

    /// Владелец с размером окна над хранилищем `persist` и реестром `registry`: следующий
    /// владелец над тем же хранилищем видит записанное этим, как новый запуск приложения.
    async fn start_plugged(
        dir: &Dir,
        persist: MemoryPersist,
        registry: Arc<Registry>,
    ) -> TestResult<Self> {
        let viewport = Viewport {
            width: 1180,
            height: 720,
        };
        Self::start_inner(dir, WithoutUi, Some(viewport), persist, registry).await
    }
}

impl<U: UiHandler> Owner<U> {
    async fn start_with(dir: &Dir, ui: U) -> TestResult<Self> {
        Self::start_inner(dir, ui, None, MemoryPersist::new(), registry()).await
    }

    async fn start_inner(
        dir: &Dir,
        ui: U,
        viewport: Option<Viewport>,
        persist: MemoryPersist,
        registry: Arc<Registry>,
    ) -> TestResult<Self> {
        let mut ownership = dir.hold()?;
        let listener = ownership.publish()?;
        let core = Core::start(
            &Handle::current(),
            Box::new(persist),
            Box::new(CoreClock::new(NOW)),
            Box::new(SequentialIds::new()),
            Arc::new(FsRootResolver),
            registry,
        )?;
        core.set_viewport(viewport);
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

/// `ui receipts` у владельца без UI: отказ `no_ui` с тем же кодом выхода, что у `ui stats`, без
/// записи и без новой ревизии; область и нечисловой курсор — ошибки использования до сокета.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ui_receipts_without_ui_is_no_ui_and_writes_nothing() -> TestResult {
    let dir = Dir::new()?;
    let owner = Owner::start(&dir).await?;
    open_workbench(&dir).await?;
    let before = revision(&dir).await?;

    let stats = dir.run(&["ui", "stats", "--json"]).await?;
    assert_eq!(stats.control_code()?, ControlErrorCode::NoUi);
    for args in [
        &["ui", "receipts", "--json"][..],
        &["ui", "receipts", "--after", "3", "--json"],
    ] {
        let refused = dir.run(args).await?.expect(stats.code)?;
        assert_eq!(refused.control_code()?, ControlErrorCode::NoUi, "{args:?}");
    }
    let text = dir.run(&["ui", "receipts"]).await?.expect(stats.code)?;
    assert!(text.stdout.is_empty(), "{text:?}");
    assert!(text.stderr.starts_with("shellioctl: no_ui: "), "{text:?}");
    assert_eq!(revision(&dir).await?, before, "чтение квитанций не пишет");

    for (args, expected) in [
        (&["ui", "receipts", "--all"][..], "не применим"),
        (&["ui", "receipts", "--workspace", "x"], "не применим"),
        (&["ui", "receipts", "--after", "x"], "номер квитанции"),
        (&["ui", "receipts", "--after", "-1"], "номер квитанции"),
    ] {
        let usage = dir.run(args).await?.expect(2)?;
        assert!(usage.stdout.is_empty(), "{usage:?}");
        assert!(usage.stderr.contains(expected), "{args:?}: {usage:?}");
    }
    assert_eq!(revision(&dir).await?, before);
    owner.stop().await
}

/// Порт UI с тремя квитанциями: отвечает на `ui.receipts` записями с номером больше `after`.
#[derive(Clone)]
struct ReceiptsUi {
    workbench: WorkbenchId,
    panel: PanelId,
}

impl ReceiptsUi {
    fn view(&self, after: Option<u64>) -> ReceiptsView {
        let record = |seq, plugin: &str| ReceiptView {
            seq,
            workbench: Some(self.workbench),
            plugin: plugin.into(),
            outcome: None,
            panel: None,
            revision: None,
            error: None,
        };
        let all = [
            ReceiptView {
                outcome: Some(OpenOutcome::Created),
                panel: Some(self.panel),
                revision: Some(Revision(5)),
                ..record(1, "notes")
            },
            ReceiptView {
                error: Some("busy".into()),
                ..record(2, "terminal")
            },
            ReceiptView {
                outcome: Some(OpenOutcome::Focused),
                panel: Some(self.panel),
                revision: Some(Revision(5)),
                ..record(3, "notes")
            },
        ];
        ReceiptsView {
            last: 3,
            receipts: all
                .into_iter()
                .filter(|receipt| after.is_none_or(|after| receipt.seq > after))
                .collect(),
        }
    }
}

impl UiHandler for ReceiptsUi {
    fn ui(
        &self,
        request: UiRequest,
        gate: UiGate,
    ) -> impl Future<Output = Result<UiReply, UiRefusal>> + Send {
        let reply = match request {
            UiRequest::Receipts { after } if gate.begin() => {
                Ok(UiReply::Receipts(self.view(after)))
            }
            _ => Err(UiRefusal::NoUi),
        };
        std::future::ready(reply)
    }
}

/// `ui receipts` отдельным процессом: курсор `--after` доходит до владельца, кадр `--json` несёт
/// его ответ как есть, текст различает успех и отказ, ревизия ядра прежняя.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ui_receipts_reads_the_ring_from_the_cursor() -> TestResult {
    let dir = Dir::new()?;
    let mut ids = SequentialIds::new();
    let ids: &mut dyn IdGen = &mut ids;
    let ui = ReceiptsUi {
        workbench: WorkbenchId::generate(ids),
        panel: PanelId::generate(ids),
    };
    let owner = Owner::start_with(&dir, ui.clone()).await?;
    let before = revision(&dir).await?;

    for (args, after) in [
        (&["ui", "receipts", "--json"][..], None),
        (&["ui", "receipts", "--after", "0", "--json"], Some(0)),
        (&["ui", "receipts", "--after", "2", "--json"], Some(2)),
        (&["ui", "receipts", "--after=3", "--json"], Some(3)),
    ] {
        match dir.run(args).await?.expect(0)?.frame()? {
            Response::Ok {
                value: OkValue::Ui(UiReply::Receipts(view)),
                ..
            } => assert_eq!(view, ui.view(after), "{args:?}"),
            other => {
                return Err(format!("{args:?}: ожидались квитанции, получено {other:?}").into());
            }
        }
    }
    let text = dir
        .run(&["ui", "receipts", "--after", "1"])
        .await?
        .expect(0)?;
    assert!(text.stderr.is_empty(), "{text:?}");
    assert_eq!(
        text.stdout,
        format!(
            "квитанций 2, последняя 3\n\
             \x20 2: terminal, рабочая область {workbench}: отказ busy\n\
             \x20 3: notes, рабочая область {workbench}: панель {panel} в фокусе, ревизия 5\n",
            workbench = ui.workbench,
            panel = ui.panel
        )
    );
    assert_eq!(revision(&dir).await?, before, "чтение квитанций не пишет");
    owner.stop().await
}

/// Порт UI с ячейкой: `ui.cell` у владельца с явным каталогом состояния меняет оси, иначе —
/// отказ; `ui.inspect` отдаёт узел с ключом сверки и свойствами вида.
#[derive(Clone)]
struct CellUi {
    explicit: bool,
    axes: Arc<Mutex<CellAxes>>,
}

/// Переопределения осей и поколение порта [`CellUi`].
#[derive(Default)]
struct CellAxes {
    contrast: Option<Contrast>,
    motion: Option<Motion>,
    generation: u64,
}

impl CellUi {
    fn new(explicit: bool) -> Self {
        Self {
            explicit,
            axes: Arc::new(Mutex::new(CellAxes::default())),
        }
    }

    fn cell(&self, contrast: Option<CellContrast>, motion: Option<CellMotion>) -> CellView {
        let mut axes = self
            .axes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(contrast) = contrast {
            axes.contrast = contrast.value();
        }
        if let Some(motion) = motion {
            axes.motion = motion.value();
        }
        axes.generation += 1;
        let source = |set: bool| {
            if set {
                AxisSource::Override
            } else {
                AxisSource::System
            }
        };
        CellView {
            pid: PID,
            lightness: Axis {
                value: Lightness::Dark,
                source: AxisSource::System,
            },
            contrast: Axis {
                value: axes.contrast.unwrap_or(Contrast::Normal),
                source: source(axes.contrast.is_some()),
            },
            motion: Axis {
                value: axes.motion.unwrap_or(Motion::Normal),
                source: source(axes.motion.is_some()),
            },
            generation: axes.generation,
        }
    }

    fn inspect() -> InspectView {
        InspectView {
            window_number: 7,
            content: Size {
                width: 1180.0,
                height: 720.0,
            },
            generation: 1,
            components: vec![ComponentView {
                id: "chrome.search.kbd".into(),
                class: "KeyBadge".into(),
                object: 1,
                parent: Some("chrome.search".into()),
                ck: Some("chrome.search.kbd".into()),
                frame: Rect {
                    x: 790.0,
                    y: 692.0,
                    width: 23.0,
                    height: 17.0,
                },
                visible: true,
                first_responder: false,
                enabled: None,
                role: None,
                label: Some("⌘K".into()),
                help: None,
                selected: None,
                expanded: None,
                animation_keys: 0,
                style: Some(StyleView {
                    font: Some(FontView {
                        family: "JetBrains Mono".into(),
                        face: "JetBrainsMono-Regular".into(),
                        weight: 400.0,
                        size: 10.5,
                        file: Some("file:///tmp/Fonts/JetBrainsMono-Regular.ttf".into()),
                    }),
                    radius: Some(4.0),
                    ..StyleView::default()
                }),
                truncated: Some(false),
                state: None,
            }],
            materials: Vec::new(),
            gallery: None,
        }
    }
}

impl UiHandler for CellUi {
    fn ui(
        &self,
        request: UiRequest,
        gate: UiGate,
    ) -> impl Future<Output = Result<UiReply, UiRefusal>> + Send {
        let reply = match request {
            UiRequest::Cell { .. } if !self.explicit => Err(UiRefusal::IsolatedProfileOnly),
            UiRequest::Cell { contrast, motion } if gate.begin() => {
                Ok(UiReply::Cell(self.cell(contrast, motion)))
            }
            UiRequest::Inspect if gate.begin() => Ok(UiReply::Inspect(Box::new(Self::inspect()))),
            UiRequest::WindowSize { .. } if !self.explicit => Err(UiRefusal::IsolatedProfileOnly),
            UiRequest::WindowSize { .. } if gate.begin() => Ok(UiReply::Accepted(Accepted {
                command: request.name().to_owned(),
                pid: std::process::id(),
                generation: 1,
            })),
            _ => Err(UiRefusal::NoUi),
        };
        std::future::ready(reply)
    }
}

/// `ui cell` и `ui inspect` отдельным процессом: оси и источники доходят до владельца и обратно,
/// `system` снимает переопределение, пропущенная ось не меняется; `ui inspect` несёт ключ сверки,
/// свойства вида и обрезку текста.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ui_cell_and_inspect_reach_the_owner() -> TestResult {
    let dir = Dir::new()?;
    let owner = Owner::start_with(&dir, CellUi::new(true)).await?;
    let cell = async |args: &[&str]| -> TestResult<CellView> {
        match dir.run(args).await?.expect(0)?.frame()? {
            Response::Ok {
                value: OkValue::Ui(UiReply::Cell(view)),
                ..
            } => Ok(view),
            other => Err(format!("{args:?}: ожидалась ячейка, получено {other:?}").into()),
        }
    };
    let high = cell(&[
        "ui",
        "cell",
        "--contrast",
        "high",
        "--motion",
        "reduce",
        "--json",
    ])
    .await?;
    assert_eq!(
        (high.contrast.value, high.contrast.source),
        (Contrast::Increased, AxisSource::Override)
    );
    assert_eq!(
        (high.motion.value, high.motion.source),
        (Motion::Reduced, AxisSource::Override)
    );
    let contrast_back = cell(&["ui", "cell", "--contrast", "system", "--json"]).await?;
    assert_eq!(contrast_back.contrast.source, AxisSource::System);
    assert_eq!(
        (contrast_back.motion.value, contrast_back.motion.source),
        (Motion::Reduced, AxisSource::Override),
        "пропущенная ось не меняется"
    );
    assert!(contrast_back.generation > high.generation);
    let text = dir
        .run(&["ui", "cell", "--motion", "system"])
        .await?
        .expect(0)?;
    assert!(text.stderr.is_empty(), "{text:?}");
    assert_eq!(
        text.stdout,
        format!(
            "ячейка: светлота тёмная (система), контраст обычный (система), движение обычное \
             (система), PID {PID}, поколение 3\n"
        )
    );

    match dir
        .run(&["ui", "inspect", "--json"])
        .await?
        .expect(0)?
        .frame()?
    {
        Response::Ok {
            value: OkValue::Ui(UiReply::Inspect(view)),
            ..
        } => assert_eq!(*view, CellUi::inspect()),
        other => return Err(format!("ожидалось дерево, получено {other:?}").into()),
    }
    let text = dir.run(&["ui", "inspect"]).await?.expect(0)?;
    assert!(
        text.stdout
            .contains("ключ chrome.search.kbd, шрифт JetBrainsMono-Regular 10.5"),
        "{text:?}"
    );
    owner.stop().await
}

/// `ui cell` у владельца без явного каталога состояния: `isolated_profile_only`, код 1, без
/// повтора; без осей — ошибка использования, код 2, владельца никто не спрашивает.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ui_cell_needs_an_isolated_owner_and_an_axis() -> TestResult {
    let dir = Dir::new()?;
    let owner = Owner::start_with(&dir, CellUi::new(false)).await?;
    let refused = dir
        .run(&["ui", "cell", "--contrast", "high", "--json"])
        .await?
        .expect(1)?;
    assert_eq!(
        refused.control_code()?,
        ControlErrorCode::IsolatedProfileOnly
    );
    let text = dir
        .run(&["ui", "cell", "--contrast", "high"])
        .await?
        .expect(1)?;
    assert!(text.stderr.contains("isolated_profile_only"), "{text:?}");
    let usage = dir.run(&["ui", "cell"]).await?.expect(2)?;
    assert!(usage.stderr.contains("--contrast"), "{usage:?}");
    owner.stop().await
}

/// `ui window size` отдельным процессом: владелец с явным каталогом состояния отвечает квитанцией
/// с именем запроса, владелец без него — `isolated_profile_only`, код 1; неверные стороны — ошибка
/// использования, код 2.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ui_window_size_reaches_only_an_isolated_owner() -> TestResult {
    let dir = Dir::new()?;
    let owner = Owner::start_with(&dir, CellUi::new(true)).await?;
    let done = dir
        .run(&["ui", "window", "size", "1600", "720", "--json"])
        .await?
        .expect(0)?;
    assert!(done.stdout.contains("\"ui.window.size\""), "{done:?}");
    let usage = dir
        .run(&["ui", "window", "size", "1600"])
        .await?
        .expect(2)?;
    assert!(usage.stderr.contains("ширина и высота"), "{usage:?}");
    owner.stop().await?;

    let dir = Dir::new()?;
    let owner = Owner::start_with(&dir, CellUi::new(false)).await?;
    let refused = dir
        .run(&["ui", "window", "size", "1600", "720", "--json"])
        .await?
        .expect(1)?;
    assert_eq!(
        refused.control_code()?,
        ControlErrorCode::IsolatedProfileOnly
    );
    owner.stop().await
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

/// Рабочая область с раскладкой по умолчанию, открытая в окне: штатный путь CLI.
async fn open_workbench(dir: &Dir) -> TestResult<WorkbenchId> {
    let home = tree(dir, &["--all"])
        .await?
        .active_workspace
        .ok_or("нет активного воркспейса")?
        .to_string();
    let source = dir.dir.path().join("src");
    fs::create_dir_all(&source)?;
    let added = dir
        .run(&[
            "project",
            "add",
            &source.to_string_lossy(),
            "--target",
            &home,
            "--all",
            "--json",
        ])
        .await?
        .expect(0)?;
    let CommandValue::Project { project, .. } = added.command_value()? else {
        return Err(format!("ожидался проект: {added:?}").into());
    };
    let created = dir
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
    let CommandValue::Workbench { workbench } = created.command_value()? else {
        return Err(format!("ожидалась рабочая область: {created:?}").into());
    };
    dir.run(&[
        "workbench",
        "open",
        "--target",
        &workbench.to_string(),
        "--all",
    ])
    .await?
    .expect(0)?;
    Ok(workbench)
}

async fn layout(dir: &Dir, extra: &[&str]) -> TestResult<LayoutView> {
    let mut args = vec!["layout", "get", "--all", "--json"];
    args.extend_from_slice(extra);
    match dir.run(&args).await?.expect(0)?.reply()?.value {
        ReplyValue::Layout(view) => Ok(*view),
        other => Err(format!("ожидалась раскладка, получено {other:?}").into()),
    }
}

/// Панель плагина `plugin` в ответе `layout get`.
fn panel_of(view: &LayoutView, plugin: &str) -> TestResult<PanelId> {
    view.panels
        .iter()
        .find(|panel| panel.plugin.as_str() == plugin)
        .map(|panel| panel.id)
        .ok_or_else(|| format!("нет панели {plugin}: {view:?}").into())
}

fn zone_tabs(view: &LayoutView, zone: Zone) -> Vec<Vec<PanelId>> {
    view.layout
        .zones
        .get(zone)
        .parts
        .iter()
        .map(|part| part.tabs.clone())
        .collect()
}

/// Отказ ядра: код 1 и кадр `core_error` с причиной `reason` в stdout `--json`.
fn assert_refused(run: Run, reason: &str) -> TestResult {
    let run = run.expect(1)?;
    match run.frame()? {
        Response::CoreError { .. } if run.stdout.contains(&format!("\"{reason}\"")) => Ok(()),
        other => Err(format!("ожидался отказ {reason}, получено {other:?}").into()),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn layout_and_panel_calls_against_sized_owner() -> TestResult {
    let dir = Dir::new()?;
    let owner = Owner::start_sized(
        &dir,
        Viewport {
            width: 1180,
            height: 720,
        },
    )
    .await?;
    let workbench = open_workbench(&dir).await?;
    let bench_id = workbench.to_string();

    let start = layout(&dir, &[]).await?;
    assert_eq!(start.workbench, workbench);
    assert!(start.frames.is_some(), "кадры по размеру окна");
    assert_eq!(layout(&dir, &["--workbench", &bench_id]).await?, start);
    let navigator = panel_of(&start, "navigator")?;
    let terminal = panel_of(&start, "terminal")?;
    assert_eq!(start.layout.navigator, Some(navigator));
    assert_eq!(zone_tabs(&start, Zone::Center), vec![vec![terminal]]);
    let (term, nav) = (terminal.to_string(), navigator.to_string());

    let text = dir.run(&["layout", "get", "--all"]).await?.expect(0)?;
    let parsed: LayoutView = serde_json::from_str(&text.stdout)?;
    assert_eq!(parsed, start, "текстовый вывод layout get — JSON раскладки");

    let split = dir
        .run(&[
            "layout", "split", &term, "--open", "terminal", "--all", "--json",
        ])
        .await?
        .expect(0)?;
    let CommandValue::Panel { panel: second } = split.command_value()? else {
        return Err(format!("ожидалась новая панель: {split:?}").into());
    };
    let sec = second.to_string();
    let view = layout(&dir, &[]).await?;
    assert_eq!(
        zone_tabs(&view, Zone::Center),
        vec![vec![terminal], vec![second]]
    );
    assert_eq!(view.layout.focus.last(), Some(&second));

    assert_refused(
        dir.run(&["layout", "split", &term, "--all", "--json"])
            .await?,
        "split_needs_panel",
    )?;
    assert_refused(
        dir.run(&[
            "layout", "split", &nav, "--open", "terminal", "--all", "--json",
        ])
        .await?,
        "navigator_not_splittable",
    )?;
    assert_refused(
        dir.run(&["layout", "split", &term, "--open", "Bad", "--all", "--json"])
            .await?,
        "plugin_invalid",
    )?;

    let focused = dir
        .run(&["panel", "focus", &term, "--all"])
        .await?
        .expect(0)?;
    assert!(focused.stdout.starts_with("ревизия "), "{focused:?}");
    assert_eq!(
        layout(&dir, &[]).await?.layout.focus.last(),
        Some(&terminal)
    );
    dir.run(&[
        "panel",
        "focus",
        "--dir",
        "l",
        "--workbench",
        &bench_id,
        "--all",
    ])
    .await?
    .expect(0)?;
    assert_eq!(layout(&dir, &[]).await?.layout.focus.last(), Some(&second));
    dir.run(&["panel", "select", &term, "--all"])
        .await?
        .expect(0)?;
    assert_eq!(
        layout(&dir, &[]).await?.layout.focus.last(),
        Some(&terminal)
    );

    dir.run(&["panel", "move", &sec, "--zone", "right", "--all"])
        .await?
        .expect(0)?;
    let view = layout(&dir, &[]).await?;
    assert_eq!(zone_tabs(&view, Zone::Right), vec![vec![second]]);
    assert_eq!(zone_tabs(&view, Zone::Center), vec![vec![terminal]]);

    dir.run(&[
        "panel",
        "move-part",
        &sec,
        "--to",
        &term,
        "--edge",
        "after",
        "--all",
    ])
    .await?
    .expect(0)?;
    let view = layout(&dir, &[]).await?;
    assert!(zone_tabs(&view, Zone::Right).is_empty(), "{view:?}");
    assert_eq!(
        zone_tabs(&view, Zone::Center),
        vec![vec![terminal], vec![second]]
    );

    dir.run(&[
        "layout",
        "resize",
        "--zone",
        "center",
        "--part-of",
        &term,
        "400",
        "--all",
    ])
    .await?
    .expect(0)?;
    let shares = |view: &LayoutView| -> Vec<u16> {
        view.layout
            .zones
            .center
            .parts
            .iter()
            .map(|part| part.share)
            .collect()
    };
    assert_eq!(shares(&layout(&dir, &[]).await?), vec![400, 600]);
    assert_refused(
        dir.run(&[
            "layout",
            "resize",
            "--zone",
            "center",
            "--part-of",
            &term,
            "10",
            "--all",
            "--json",
        ])
        .await?,
        "resize_out_of_range",
    )?;
    assert_refused(
        dir.run(&[
            "layout", "resize", "--zone", "right", "300", "--all", "--json",
        ])
        .await?,
        "zone_empty",
    )?;
    dir.run(&["layout", "equal", "--zone", "center", "--all"])
        .await?
        .expect(0)?;
    assert_eq!(shares(&layout(&dir, &[]).await?), vec![500, 500]);

    dir.run(&["layout", "zoom", &term, "--all"])
        .await?
        .expect(0)?;
    assert_eq!(layout(&dir, &[]).await?.layout.zoomed, Some(terminal));
    dir.run(&["layout", "zoom", "--off", "--workbench", &bench_id, "--all"])
        .await?
        .expect(0)?;
    assert_eq!(layout(&dir, &[]).await?.layout.zoomed, None);

    dir.run(&["layout", "only", "--all"]).await?.expect(0)?;
    assert_eq!(layout(&dir, &[]).await?.layout.only, Some(Zone::Center));
    dir.run(&["layout", "only", "--all"]).await?.expect(0)?;
    assert_eq!(layout(&dir, &[]).await?.layout.only, None);

    dir.run(&["layout", "hide", "navigator", "--all"])
        .await?
        .expect(0)?;
    assert!(layout(&dir, &[]).await?.layout.hidden.navigator);
    dir.run(&[
        "layout",
        "show",
        "navigator",
        "--workbench",
        &bench_id,
        "--all",
    ])
    .await?
    .expect(0)?;
    assert!(!layout(&dir, &[]).await?.layout.hidden.navigator);
    assert_refused(
        dir.run(&["layout", "hide", "plate", "--all", "--json"])
            .await?,
        "zone_empty",
    )?;

    dir.run(&["panel", "close", &sec, "--all"])
        .await?
        .expect(0)?;
    let view = layout(&dir, &[]).await?;
    assert_eq!(view.panels.len(), 2, "{view:?}");
    assert_eq!(zone_tabs(&view, Zone::Center), vec![vec![terminal]]);

    let missing = dir
        .run(&["panel", "close", &sec, "--all", "--json"])
        .await?;
    let missing = missing.expect(1)?;
    match missing.frame()? {
        Response::CoreError { error, .. } => assert_eq!(error.code(), ErrorCode::NotFound),
        other => return Err(format!("ожидался not_found, получено {other:?}").into()),
    }

    let usage = dir.run(&["layout", "split", "--all"]).await?.expect(2)?;
    assert!(usage.stderr.contains("операндов"), "{usage:?}");
    let usage = dir
        .run(&["panel", "close", &term, "--workbench", &bench_id, "--all"])
        .await?
        .expect(2)?;
    assert!(usage.stderr.contains("не применим"), "{usage:?}");

    owner.stop().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn layout_without_viewport_refuses_only_room_commands() -> TestResult {
    let dir = Dir::new()?;
    let owner = Owner::start(&dir).await?;
    let workbench = open_workbench(&dir).await?;
    let view = layout(&dir, &["--workbench", &workbench.to_string()]).await?;
    assert_eq!(view.frames, None, "без размера окна кадров нет");
    let term = panel_of(&view, "terminal")?.to_string();

    for args in [
        vec!["layout", "split", term.as_str(), "--open", "terminal"],
        vec!["layout", "resize", "--zone", "bottom", "300"],
    ] {
        let mut args = args;
        args.extend(["--all", "--json"]);
        assert_refused(dir.run(&args).await?, "no_ui")?;
    }
    let text = dir
        .run(&["layout", "split", &term, "--open", "terminal", "--all"])
        .await?
        .expect(1)?;
    assert!(text.stdout.is_empty(), "{text:?}");
    assert!(text.stderr.contains("no_ui"), "{text:?}");

    dir.run(&["panel", "move", &term, "--zone", "bottom", "--all"])
        .await?
        .expect(0)?;
    assert_eq!(
        zone_tabs(&layout(&dir, &[]).await?, Zone::Bottom),
        vec![vec![panel_of(&view, "terminal")?]]
    );

    owner.stop().await
}

/// Отказ ядра `invalid_argument` с причиной `reason`: код 1 и кадр `core_error` в stdout `--json`.
fn refusal(run: Run, reason: &str) -> TestResult<CoreError> {
    let run = run.expect(1)?;
    match run.frame()? {
        Response::CoreError { error, .. }
            if error.code() == ErrorCode::InvalidArgument
                && error.detail_str("reason") == Some(reason) =>
        {
            Ok(error)
        }
        other => Err(format!("ожидался отказ {reason}, получено {other:?}").into()),
    }
}

/// Отказ ядра в текстовом режиме: код 1, stdout пуст, код ошибки в stderr.
fn assert_text_refusal(run: Run, code: &str) -> TestResult {
    let run = run.expect(1)?;
    assert!(run.stdout.is_empty(), "{run:?}");
    assert!(
        run.stderr.starts_with(&format!("shellioctl: {code}: ")),
        "{run:?}"
    );
    Ok(())
}

async fn panel_view(dir: &Dir, panel: &str, scope: &[&str]) -> TestResult<PanelView> {
    let mut args = vec!["panel", "get", panel, "--json"];
    args.extend_from_slice(scope);
    match dir.run(&args).await?.expect(0)?.reply()?.value {
        ReplyValue::Panel(view) => Ok(*view),
        other => Err(format!("ожидалась панель, получено {other:?}").into()),
    }
}

async fn plugins(dir: &Dir) -> TestResult<PluginsView> {
    match dir
        .run(&["plugin", "list", "--json"])
        .await?
        .expect(0)?
        .reply()?
        .value
    {
        ReplyValue::Plugins(view) => Ok(view),
        other => Err(format!("ожидались плагины, получено {other:?}").into()),
    }
}

fn plugin_ids(view: &PluginsView) -> Vec<&str> {
    view.plugins
        .iter()
        .map(|plugin| plugin.id.as_str())
        .collect()
}

async fn revision(dir: &Dir) -> TestResult<Revision> {
    Ok(dir
        .run(&["status", "--json"])
        .await?
        .expect(0)?
        .reply()?
        .revision)
}

/// Ответ `panel.open` в `--json`.
fn opened(run: &Run) -> TestResult<(PanelId, WorkbenchId, OpenOutcome)> {
    match run.command_value()? {
        CommandValue::Opened {
            panel,
            workbench,
            outcome,
        } => Ok((panel, workbench, outcome)),
        other => Err(format!("ожидался ответ panel.open, получено {other:?}").into()),
    }
}

/// `panel open|command|get` и `plugin list` против владельца с тестовым реестром: singleton
/// рабочей области `notes` с действиями и `solo` на всё приложение. Второй владелец над тем же
/// хранилищем получает другой реестр, и панели первого перестают быть `ready`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn panel_and_plugin_calls_against_test_registry() -> TestResult {
    let dir = Dir::new()?;
    let persist = MemoryPersist::new();
    let notes = || {
        TestPlugin::new("notes")
            .title("Заметки")
            .cardinality(Cardinality::PerWorkbench)
            .actions()
    };
    let owner = Owner::start_plugged(
        &dir,
        persist.clone(),
        registry_of(vec![
            notes(),
            TestPlugin::new("solo").cardinality(Cardinality::PerApp),
        ]),
    )
    .await?;
    let workbench = open_workbench(&dir).await?;
    let bench_id = workbench.to_string();
    let home = tree(&dir, &["--all"])
        .await?
        .active_workspace
        .ok_or("нет активного воркспейса")?
        .to_string();
    let created = dir
        .run(&["workspace", "new", "Чужой", "--all", "--json"])
        .await?
        .expect(0)?;
    let CommandValue::Workspace { workspace: other } = created.command_value()? else {
        return Err(format!("ожидался воркспейс: {created:?}").into());
    };
    let other = other.to_string();
    let terminal = panel_of(&layout(&dir, &[]).await?, "terminal")?.to_string();

    let catalog = plugins(&dir).await?;
    assert_eq!(
        plugin_ids(&catalog),
        ["terminal", "navigator", "notes", "solo"],
        "порядок регистрации"
    );
    let listed = &catalog.plugins[2];
    assert_eq!(listed.title, "Заметки");
    assert_eq!(listed.cardinality, Cardinality::PerWorkbench);
    assert_eq!(listed.commands, ["notes.add", "notes.pad"]);
    assert_eq!(catalog.plugins[3].cardinality, Cardinality::PerApp);
    let text = dir.run(&["plugin", "list"]).await?.expect(0)?;
    let parsed: PluginsView = serde_json::from_str(&text.stdout)?;
    assert_eq!(
        parsed, catalog,
        "текстовый вывод plugin list — JSON каталога"
    );

    let first = dir
        .run(&[
            "panel",
            "open",
            "notes",
            "--workbench",
            &bench_id,
            "--zone",
            "bottom",
            "--args",
            r#"{"count":2}"#,
            "--all",
            "--json",
        ])
        .await?
        .expect(0)?;
    let (panel, at, outcome) = opened(&first)?;
    assert_eq!((at, outcome), (workbench, OpenOutcome::Created));
    let id = panel.to_string();
    let view = layout(&dir, &[]).await?;
    assert_eq!(zone_tabs(&view, Zone::Bottom), vec![vec![panel]]);
    assert_eq!(view.layout.focus.last(), Some(&panel));

    let shown = panel_view(&dir, &id, &["--all"]).await?;
    assert_eq!(shown.panel.id, panel);
    assert_eq!(shown.panel.plugin.as_str(), "notes");
    assert_eq!(shown.panel.title.to_string(), "Заметки");
    assert_eq!(shown.status, PanelStatus::Ready);
    assert_eq!(shown.version, Some(1));
    assert_eq!(shown.data, json!({"count": 2}));
    assert!(shown.readable);
    assert_eq!(shown.canonical, None);
    assert_eq!(panel_view(&dir, &id, &["--workspace", &home]).await?, shown);
    let text = dir.run(&["panel", "get", &id, "--all"]).await?.expect(0)?;
    let parsed: PanelView = serde_json::from_str(&text.stdout)?;
    assert_eq!(parsed, shown, "текстовый вывод panel get — JSON панели");

    // Повтор singleton: та же панель, другая зона — только подсказка, контекст доходит до `reopen`.
    let again = dir
        .run(&[
            "panel",
            "open",
            "notes",
            "--zone",
            "right",
            "--args",
            r#"{"count":5}"#,
            "--workspace",
            &home,
            "--json",
        ])
        .await?
        .expect(0)?;
    assert_eq!(opened(&again)?, (panel, workbench, OpenOutcome::Focused));
    assert_eq!(
        panel_view(&dir, &id, &["--all"]).await?.data,
        json!({"count": 5})
    );
    assert_eq!(
        zone_tabs(&layout(&dir, &[]).await?, Zone::Bottom),
        vec![vec![panel]]
    );
    let before = revision(&dir).await?;
    let repeat = dir
        .run(&["panel", "open", "notes", "--all"])
        .await?
        .expect(0)?;
    assert_eq!(
        repeat.stdout,
        format!(
            "ревизия {}: панель {id} в рабочей области {bench_id}: в фокусе\n",
            before.0
        ),
        "полный повтор без изменений не пишет ревизию"
    );

    let acted = dir
        .run(&[
            "panel",
            "command",
            &id,
            "notes.add",
            "--args",
            r#"{"by":2}"#,
            "--all",
            "--json",
        ])
        .await?
        .expect(0)?;
    assert_eq!(acted.command_value()?, CommandValue::Panel { panel });
    assert_eq!(acted.reply()?.revision, Revision(before.0 + 1));
    assert_eq!(
        panel_view(&dir, &id, &["--all"]).await?.data,
        json!({"count": 7})
    );
    let acted = dir
        .run(&[
            "panel",
            "command",
            &id,
            "notes.add",
            "--args",
            r#"{"by":1}"#,
            "--workspace",
            &home,
        ])
        .await?
        .expect(0)?;
    assert_eq!(
        acted.stdout,
        format!("ревизия {}: панель {id}\n", before.0 + 2)
    );
    let idle = dir
        .run(&[
            "panel",
            "command",
            &id,
            "notes.add",
            "--args",
            r#"{"by":0}"#,
            "--all",
            "--json",
        ])
        .await?
        .expect(0)?;
    assert_eq!(
        idle.reply()?.revision,
        Revision(before.0 + 2),
        "действие без изменения данных не пишет ревизию"
    );
    let ready = panel_view(&dir, &id, &["--all"]).await?;
    assert_eq!(ready.data, json!({"count": 8}));

    // Отказы ядра: код 1, кадр `core_error` с причиной и полем.
    let settled = revision(&dir).await?;
    let cases: [(&[&str], &str, &str); 8] = [
        (
            &["panel", "command", &id, "notes.nope"],
            "command_unknown",
            "name",
        ),
        (
            &["panel", "command", &id, "notes.add"],
            "args_invalid",
            "args",
        ),
        (
            &[
                "panel",
                "command",
                &id,
                "notes.add",
                "--args",
                r#"{"by":1,"extra":1}"#,
            ],
            "args_invalid",
            "args",
        ),
        (
            &[
                "panel",
                "command",
                &id,
                "notes.add",
                "--args",
                r#"{"by":9}"#,
            ],
            "count_out_of_range",
            "by",
        ),
        (
            &["panel", "command", &terminal, "notes.add"],
            "command_unknown",
            "name",
        ),
        (&["panel", "open", "ghost"], "plugin_unknown", "plugin"),
        (&["panel", "open", "Bad Id"], "plugin_invalid", "plugin"),
        (
            &["panel", "open", "notes", "--args", r#"{"count":10}"#],
            "count_out_of_range",
            "count",
        ),
    ];
    for (args, reason, field) in cases {
        let mut args = args.to_vec();
        args.extend(["--all", "--json"]);
        let error = refusal(dir.run(&args).await?, reason)?;
        assert_eq!(
            error.detail_str("field"),
            Some(field),
            "{args:?}: {error:?}"
        );
    }
    assert_text_refusal(
        dir.run(&["panel", "open", "ghost", "--all"]).await?,
        "invalid_argument",
    )?;
    assert_text_refusal(
        dir.run(&["panel", "command", &id, "notes.nope", "--all"])
            .await?,
        "invalid_argument",
    )?;

    // Панель вне области и несуществующая панель неразличимы: `not_found`.
    let missing = "00000000-0000-0000-0000-0000000fffff";
    for args in [
        &["panel", "get", &id, "--workspace", &other][..],
        &["panel", "get", missing, "--all"],
        &[
            "panel",
            "command",
            &id,
            "notes.add",
            "--args",
            r#"{"by":1}"#,
            "--workspace",
            &other,
        ],
        &["panel", "command", missing, "notes.add", "--all"],
    ] {
        let mut args = args.to_vec();
        args.push("--json");
        let run = dir.run(&args).await?.expect(1)?;
        match run.frame()? {
            Response::CoreError { error, .. } => {
                assert_eq!(error.code(), ErrorCode::NotFound, "{args:?}");
            }
            other => return Err(format!("{args:?}: ожидался not_found, получено {other:?}").into()),
        }
    }
    assert_text_refusal(
        dir.run(&["panel", "get", missing, "--all"]).await?,
        "not_found",
    )?;

    // Плагин на всё приложение под областью воркспейса: отказ сервера, а не разбора аргументов.
    for args in [
        &["panel", "open", "solo"][..],
        &["panel", "open", "solo", "--workbench", &bench_id],
        &["layout", "split", &terminal, "--open", "solo"],
    ] {
        let mut args = args.to_vec();
        args.extend(["--workspace", &home, "--json"]);
        let error = refusal(dir.run(&args).await?, "scope_requires_all")?;
        assert_eq!(error.detail_str("field"), Some("scope"), "{args:?}");
    }
    assert_text_refusal(
        dir.run(&["panel", "open", "solo", "--workspace", &home])
            .await?,
        "invalid_argument",
    )?;
    assert_eq!(
        revision(&dir).await?,
        settled,
        "отказы ядра не пишут ревизию"
    );
    let solo = dir
        .run(&["panel", "open", "solo", "--all", "--json"])
        .await?
        .expect(0)?;
    let (solo, at, outcome) = opened(&solo)?;
    assert_eq!((at, outcome), (workbench, OpenOutcome::Created));
    let solo_id = solo.to_string();
    let error = refusal(
        dir.run(&["panel", "open", "solo", "--workspace", &home, "--json"])
            .await?,
        "scope_requires_all",
    )?;
    assert_eq!(
        error.detail("panel"),
        None,
        "отказ области не называет существующий экземпляр"
    );
    let repeat = dir
        .run(&["panel", "open", "solo", "--all"])
        .await?
        .expect(0)?;
    assert!(
        repeat.stdout.ends_with(&format!(
            ": панель {solo_id} в рабочей области {bench_id}: в фокусе\n"
        )),
        "{repeat:?}"
    );

    // Ошибки использования: код 2, владелец не вызван.
    let settled = revision(&dir).await?;
    for args in [
        &["panel", "open", "notes", "--args", "[1]", "--all"][..],
        &["panel", "open", "notes", "--args", "null", "--all"],
        &["panel", "open", "notes", "--args", "{", "--all"],
        &["panel", "open", "notes"],
        &["panel", "open", "--all"],
        &["panel", "open", "notes", "--zone", "top", "--all"],
        &["panel", "command", &id, "notes.add", "--args", "7", "--all"],
        &["panel", "command", &id, "--all"],
        &["panel", "command", &id, "notes.add"],
        &["panel", "get", &id],
        &["panel", "get", &id, "--workbench", &bench_id, "--all"],
        &["plugin", "list", "--all"],
        &["plugin", "list", "--workspace", &home],
        &["plugin", "list", "extra"],
        &["plugin"],
    ] {
        let refused = dir.run(args).await?.expect(2)?;
        assert!(refused.stdout.is_empty(), "{args:?}: {refused:?}");
        assert!(refused.stderr.contains("--help"), "{args:?}: {refused:?}");
    }
    assert_eq!(revision(&dir).await?, settled);
    owner.stop().await?;

    // Новый запуск: `notes` новой версии не переносит данные, `solo` не зарегистрирован.
    let owner = Owner::start_plugged(
        &dir,
        persist,
        registry_of(vec![notes().state_version(2).migrate(TestMigrate::Refuse)]),
    )
    .await?;
    let catalog = plugins(&dir).await?;
    assert_eq!(plugin_ids(&catalog), ["terminal", "navigator", "notes"]);
    assert_eq!(catalog.plugins[2].state_version, 2);

    let stale = panel_view(&dir, &id, &["--all"]).await?;
    assert_eq!(stale.status, PanelStatus::MigrateFailed);
    assert_eq!(stale.version, Some(1));
    assert!(stale.readable);
    let Value::String(raw) = &stale.data else {
        return Err(format!("данные не в ready — исходный текст: {stale:?}").into());
    };
    assert_eq!(serde_json::from_str::<Value>(raw)?, ready.data);
    let orphan = panel_view(&dir, &solo_id, &["--all"]).await?;
    assert_eq!(orphan.status, PanelStatus::PluginMissing);

    let started = revision(&dir).await?;
    let cases: [(&[&str], &str, &str, &str); 5] = [
        (
            &[
                "panel",
                "command",
                &id,
                "notes.add",
                "--args",
                r#"{"by":1}"#,
            ],
            "panel",
            &id,
            "migrate_failed",
        ),
        (
            &["panel", "command", &id, "notes.nope"],
            "panel",
            &id,
            "migrate_failed",
        ),
        (
            &["panel", "open", "notes", "--args", r#"{"count":1}"#],
            "plugin",
            &id,
            "migrate_failed",
        ),
        (
            &["panel", "open", "notes", "--workbench", &bench_id],
            "plugin",
            &id,
            "migrate_failed",
        ),
        (
            &["panel", "command", &solo_id, "solo.add"],
            "panel",
            &solo_id,
            "plugin_missing",
        ),
    ];
    for (args, field, unavailable, status) in cases {
        let mut args = args.to_vec();
        args.extend(["--all", "--json"]);
        let error = refusal(dir.run(&args).await?, "panel_unavailable")?;
        assert_eq!(error.detail_str("status"), Some(status), "{args:?}");
        assert_eq!(error.detail_str("field"), Some(field), "{args:?}");
        assert_eq!(error.detail_str("panel"), Some(unavailable), "{args:?}");
    }
    let text = dir
        .run(&["panel", "command", &id, "notes.add", "--all"])
        .await?
        .expect(1)?;
    assert!(text.stdout.is_empty(), "{text:?}");
    assert!(
        text.stderr.contains("статус migrate_failed"),
        "текст отказа называет статус: {text:?}"
    );
    let error = refusal(
        dir.run(&["panel", "open", "solo", "--all", "--json"])
            .await?,
        "plugin_unknown",
    )?;
    assert_eq!(error.detail_str("field"), Some("plugin"));
    assert_eq!(revision(&dir).await?, started, "отказы не пишут ревизию");
    assert_eq!(
        panel_view(&dir, &id, &["--all"]).await?,
        stale,
        "данные панели не в ready не тронуты"
    );

    owner.stop().await
}
