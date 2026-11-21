//! Владелец профиля и второй запуск (ADR-0006).
//!
//! Главный поток только создаёт runtime и крутит цикл UI. Всё остальное делает задача
//! runtime: профиль, выбор роли, хранилище и ядро, публикация сокета после готовности ядра и
//! UI, сервер управления, пересылка аргументов второго запуска и порядок завершения. Файловый
//! ввод-вывод идёт через `spawn_blocking`, не на главном потоке и не на рабочих потоках.
//!
//! Ни один выход не выполняет деструкторов: владелец выходит внутри UI после
//! [`Ui::finish_terminate`], второй запуск и отказ старта — через [`Ui::exit`]. Поэтому
//! сервер, ядро и блокировка закрываются явно, до вызова порта.

use std::ffi::OsString;
use std::fmt::Display;
use std::future::{Future, pending};
use std::io::{self, Write};
use std::panic::resume_unwind;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use shellio_control::{
    Activation, ClientError, Clock, ControlErrorCode, Exit, InstanceError, Launch, LaunchHandler,
    LaunchReceipt, OkValue, OwnerState, Ownership, RefusedArg, RequestBody, Response, Server,
    ServerConfig, Session, TokioClock, socket_path,
};
use shellio_core::{Core, CoreHandle, FsRootResolver, SystemClock, SystemIds};
use shellio_store::{Database, Profile, ProfileError, SqlitePersist};
use tokio::runtime::{Builder, Handle, Runtime};
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::task::spawn_blocking;

use crate::launch::{self, Outgoing};

/// Срок второго запуска: подключение, `hello` и отправка `launch`.
pub const SECOND_LAUNCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Версия сервера в ответе `hello`.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Имя второго запуска в `hello`.
const CLIENT: &str = concat!("shellio ", env!("CARGO_PKG_VERSION"));

/// Порт UI владельца. Методы зовутся из задач runtime и не ждут главного потока синхронно.
pub trait Ui: Send + Sync + 'static {
    /// Готовность главного потока (`applicationDidFinishLaunching`). Готовность, наступившая
    /// до вызова, не теряется.
    fn ready(&self) -> impl Future<Output = ()> + Send;

    /// Роль выбрана: владелец переводит приложение в `.regular`.
    fn become_owner(&self) -> impl Future<Output = ()> + Send;

    /// Выполняет активацию на главном потоке и возвращается после вызова.
    fn activate(&self) -> impl Future<Output = Activation> + Send;

    /// Второй запуск перед отправкой `launch`: уступить активацию владельцу `pid`, чтобы его
    /// [`Ui::activate`] получил передний план. Возвращается после вызова.
    fn yield_activation(&self, pid: u32) -> impl Future<Output = ()> + Send;

    /// Запрос завершения от UI (`applicationShouldTerminate`). UI без такого запроса не
    /// завершает future никогда.
    fn terminate_requested(&self) -> impl Future<Output = ()> + Send;

    /// Порядок завершения выполнен: UI отпускает приложение, процесс выходит с кодом 0.
    fn finish_terminate(&self);

    /// Второй запуск или отказ старта: процесс выходит с кодом `code` после сброса вывода.
    fn exit(&self, code: u8);
}

/// Строка в stdout. Закрытый вывод не роняет процесс.
pub fn say(line: impl Display) {
    let _ = writeln!(io::stdout().lock(), "{line}");
}

/// Строка в stderr. Закрытый вывод не роняет процесс.
pub fn say_err(line: impl Display) {
    let _ = writeln!(io::stderr().lock(), "{line}");
}

/// Сбросить stdout и stderr: перед выходом без деструкторов.
pub fn flush_output() {
    let _ = io::stdout().lock().flush();
    let _ = io::stderr().lock().flush();
}

/// Выйти с кодом `code` после сброса вывода.
pub fn exit_process(code: u8) -> ! {
    flush_output();
    std::process::exit(i32::from(code))
}

/// Создать runtime и запустить на нём задачу процесса с аргументами `args` без имени
/// программы. Runtime должен жить до выхода процесса. Главный поток после вызова крутит цикл
/// UI или ждёт [`Ui::exit`] и [`Ui::finish_terminate`].
pub fn start<U: Ui>(
    ui: Arc<U>,
    args: Vec<OsString>,
    profile: Result<Profile, ProfileError>,
) -> io::Result<Runtime> {
    let runtime = Builder::new_multi_thread()
        .enable_all()
        .thread_name("shellio-owner")
        .build()?;
    let main = runtime.spawn(run(Arc::clone(&ui), args, profile));
    runtime.spawn(async move {
        match main.await {
            Ok(Finish::Terminate) => ui.finish_terminate(),
            Ok(Finish::Exit(exit)) => ui.exit(exit.code()),
            Err(error) => {
                say_err(format_args!("shellio: задача процесса упала: {error}"));
                ui.exit(Exit::Failure.code());
            }
        }
    });
    Ok(runtime)
}

/// Чем кончилась задача процесса.
enum Finish {
    /// Владелец выполнил порядок завершения.
    Terminate,
    /// Второй запуск или отказ старта.
    Exit(Exit),
}

/// Отказ старта: сообщение и код 1.
fn fail(reason: impl Display) -> Finish {
    say_err(format_args!("shellio: запуск не удался: {reason}"));
    Finish::Exit(Exit::Failure)
}

/// Синхронная работа на потоке `spawn_blocking`. Паника пробрасывается.
async fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    match spawn_blocking(work).await {
        Ok(value) => value,
        Err(error) => match error.try_into_panic() {
            Ok(panic) => resume_unwind(panic),
            // Отмена бывает только при остановке runtime, а он живёт до выхода процесса.
            Err(_) => pending().await,
        },
    }
}

async fn run<U: Ui>(
    ui: Arc<U>,
    args: Vec<OsString>,
    profile: Result<Profile, ProfileError>,
) -> Finish {
    let profile = match profile {
        Ok(profile) => profile,
        Err(error) => return fail(format_args!("профиль не вычислен: {error}")),
    };
    let state = profile.clone();
    let cwd = blocking(move || {
        let cwd = std::env::current_dir()
            .map_err(|error| format!("рабочий каталог не прочитан: {error}"))?;
        state
            .ensure_state_dir()
            .map_err(|error| error.to_string())?;
        Ok::<_, String>(cwd)
    })
    .await;
    let outgoing = match cwd {
        Ok(cwd) => launch::outgoing(args, &cwd),
        Err(reason) => return fail(reason),
    };
    let ui_ref = &*ui;
    match claim(
        &profile,
        TokioClock::default(),
        SECOND_LAUNCH_TIMEOUT,
        &outgoing.launch,
        &|pid| ui_ref.yield_activation(pid),
    )
    .await
    {
        Ok(Claim::Owner(ownership)) => serve(ui, profile, ownership, outgoing).await,
        Ok(Claim::Second(second)) => Finish::Exit(second.report(&outgoing.local)),
        Err(error) => fail(error),
    }
}

/// Исход выбора роли.
#[derive(Debug)]
pub enum Claim {
    /// Блокировка взята: процесс — владелец.
    Owner(Ownership),
    /// Второй запуск закончен.
    Second(Second),
}

/// Исход второго запуска.
#[derive(Debug)]
pub enum Second {
    /// Владелец принял аргументы и ответил квитанцией. Код 0.
    Delivered {
        /// PID владельца из `hello`.
        pid: u32,
        /// Квитанция владельца.
        receipt: LaunchReceipt,
    },
    /// Владелец ответил определённым отказом: код по кадру.
    Refused(Response),
    /// После полной отправки `launch` исход неизвестен. Код 74.
    Unknown(ClientError),
    /// Неповторяемый отказ без ответа владельца. Код 1.
    Failed(String),
    /// Срок истёк до отправки `launch`: владелец запускается или перезапускается. Код 75.
    /// Внутри — причина последней попытки.
    Expired(String),
}

impl Second {
    /// Код выхода второго запуска.
    pub fn exit(&self) -> Exit {
        match self {
            Self::Delivered { .. } => Exit::Success,
            Self::Refused(frame) => Exit::of_response(frame),
            Self::Unknown(_) => Exit::OutcomeUnknown,
            Self::Failed(_) => Exit::Failure,
            Self::Expired(_) => Exit::Retry,
        }
    }

    /// Напечатать исход: квитанцию с локальными отказами `local` в stdout, отказ в stderr.
    fn report(&self, local: &[RefusedArg]) -> Exit {
        match self {
            Self::Delivered { pid, receipt } => say(launch::receipt_text(*pid, receipt, local)),
            Self::Refused(frame) => say_err(format_args!(
                "shellio: владелец отказал: {}",
                describe(frame)
            )),
            Self::Unknown(error) => say_err(format_args!(
                "shellio: исход неизвестен, повтор не делается: {error}"
            )),
            Self::Failed(reason) => say_err(format_args!("shellio: {reason}")),
            Self::Expired(last) => say_err(format_args!(
                "shellio: владелец запускается или перезапускается: {last}"
            )),
        }
        self.exit()
    }
}

fn describe(frame: &Response) -> String {
    match frame {
        Response::Error { error, .. } => error.to_string(),
        Response::CoreError { error, .. } => error.to_string(),
        Response::Ok { .. }
        | Response::Snapshot { .. }
        | Response::Event { .. }
        | Response::Reset { .. } => serde_json::to_string(frame).unwrap_or_default(),
    }
}

/// Выбрать роль до срока `timeout`: взять блокировку профиля или передать `launch` владельцу.
///
/// Блокировка занята — подключение к сокету. Сокета нет, его никто не слушает, владелец
/// уходит или отказ пришёл до полной отправки `launch` — ждать попытки по `clock` и снова
/// пробовать блокировку: освободилась — процесс становится владельцем. Каталог состояния уже
/// создан. `yield_to` получает PID готового владельца из `hello` и завершается до отправки
/// `launch`.
pub async fn claim<C, Y, F>(
    profile: &Profile,
    clock: C,
    timeout: Duration,
    launch: &Launch,
    yield_to: &Y,
) -> Result<Claim, InstanceError>
where
    C: Clock + Clone,
    Y: Fn(u32) -> F + Sync,
    F: Future<Output = ()> + Send,
{
    let deadline = clock.deadline(timeout);
    let socket = socket_path(profile);
    loop {
        let candidate = profile.clone();
        if let Some(ownership) = blocking(move || Ownership::try_acquire(&candidate)).await? {
            return Ok(Claim::Owner(ownership));
        }
        let reason = match forward(&clock, deadline, &socket, launch, yield_to).await {
            Attempt::Done(second) => return Ok(Claim::Second(second)),
            Attempt::Retry(reason) => reason,
        };
        if clock.wait_retry(deadline).await.is_err() {
            return Ok(Claim::Second(Second::Expired(reason)));
        }
    }
}

/// Исход одной попытки передачи.
enum Attempt {
    Done(Second),
    /// `launch` не исполнен: повторить с блокировки.
    Retry(String),
}

fn after_error(error: ClientError) -> Attempt {
    match Exit::of_error(&error) {
        Exit::Retry => Attempt::Retry(error.to_string()),
        Exit::OutcomeUnknown => Attempt::Done(Second::Unknown(error)),
        Exit::Success | Exit::Failure | Exit::Usage | Exit::NotRunning => {
            Attempt::Done(Second::Failed(error.to_string()))
        }
    }
}

async fn forward<C, Y, F>(
    clock: &C,
    deadline: C::Deadline,
    socket: &Path,
    launch: &Launch,
    yield_to: &Y,
) -> Attempt
where
    C: Clock + Clone,
    Y: Fn(u32) -> F,
    F: Future<Output = ()>,
{
    let mut session = match Session::connect(clock.clone(), deadline, socket).await {
        Ok(session) => session,
        Err(error) => return Attempt::Retry(error.to_string()),
    };
    let pid = match session.hello(CLIENT).await {
        Ok(Response::Ok {
            value: OkValue::Hello(hello),
            ..
        }) => match hello.owner {
            OwnerState::Ready => hello.pid,
            OwnerState::Leaving => return Attempt::Retry("владелец уходит".to_owned()),
        },
        Ok(frame) => return Attempt::Done(Second::Refused(frame)),
        Err(error) => return after_error(error),
    };
    yield_to(pid).await;
    match session.request(RequestBody::Launch(launch.clone())).await {
        Ok(Response::Ok {
            value: OkValue::Launch(receipt),
            ..
        }) => Attempt::Done(Second::Delivered { pid, receipt }),
        // Отказ `owner_leaving` даётся до допуска: `launch` не исполнен.
        Ok(Response::Error { error, .. }) if error.code() == ControlErrorCode::OwnerLeaving => {
            Attempt::Retry(error.to_string())
        }
        Ok(frame) => Attempt::Done(Second::Refused(frame)),
        Err(error) => after_error(error),
    }
}

/// Обработчик запуска владельца: и пересланные аргументы, и собственные.
struct Launcher<U> {
    ui: Arc<U>,
}

impl<U> Clone for Launcher<U> {
    fn clone(&self) -> Self {
        Self {
            ui: Arc::clone(&self.ui),
        }
    }
}

impl<U: Ui> LaunchHandler for Launcher<U> {
    fn launch(&self, launch: Launch) -> impl Future<Output = LaunchReceipt> + Send {
        let ui = Arc::clone(&self.ui);
        async move {
            let judged = launch.clone();
            let refused = blocking(move || launch::judge(&judged)).await;
            let activation = ui.activate().await;
            let receipt = LaunchReceipt {
                activation,
                refused,
            };
            // Строка пишется до квитанции: второй запуск выходит позже неё.
            say(launch::handled_line(&launch, &receipt));
            receipt
        }
    }
}

/// Сигналы завершения владельца.
struct Signals {
    term: Signal,
    int: Signal,
}

impl Signals {
    /// Обработчики ставятся до публикации сокета: сигнал, пришедший раньше, иначе убил бы
    /// процесс и оставил сокет.
    fn install() -> io::Result<Self> {
        Ok(Self {
            term: signal(SignalKind::terminate())?,
            int: signal(SignalKind::interrupt())?,
        })
    }

    /// Дождаться `SIGTERM`, `SIGINT` или запроса UI и назвать причину.
    async fn wait(mut self, ui: &impl Ui) -> &'static str {
        tokio::select! {
            _ = self.term.recv() => "SIGTERM",
            _ = self.int.recv() => "SIGINT",
            () = ui.terminate_requested() => "запрос UI",
        }
    }
}

async fn load_core(profile: Profile) -> Result<CoreHandle, String> {
    let runtime = Handle::current();
    blocking(move || {
        let db = Database::open(&profile, &SystemClock)
            .map_err(|error| format!("хранилище не открыто: {error}"))?;
        Core::start(
            &runtime,
            Box::new(SqlitePersist::new(db)),
            Box::new(SystemClock),
            Box::new(SystemIds),
            Arc::new(FsRootResolver),
        )
        .map_err(|error| format!("ядро не запущено: {error}"))
    })
    .await
}

/// Шаг 5 порядка завершения и отказа старта: свой сокет удаляется, блокировка отпускается
/// последней.
async fn release(ownership: Ownership) {
    if let Err(error) = blocking(move || ownership.release()).await {
        say_err(format_args!(
            "shellio: блокировка отпущена с ошибкой: {error}"
        ));
    }
}

/// Отказ старта после загрузки ядра: ядро останавливается, блокировка отпускается.
async fn abort(core: CoreHandle, ownership: Ownership, reason: String) -> Finish {
    core.shutdown().await;
    release(ownership).await;
    fail(reason)
}

async fn serve<U: Ui>(
    ui: Arc<U>,
    profile: Profile,
    ownership: Ownership,
    outgoing: Outgoing,
) -> Finish {
    let signals = match Signals::install() {
        Ok(signals) => signals,
        Err(error) => {
            release(ownership).await;
            return fail(format_args!("обработчики сигналов не поставлены: {error}"));
        }
    };
    ui.become_owner().await;
    // Разбор незавершённого обновления встанет здесь, до открытия `state.db` (P5.2).
    let core = match load_core(profile.clone()).await {
        Ok(core) => core,
        Err(reason) => {
            release(ownership).await;
            return fail(reason);
        }
    };
    ui.ready().await;

    let (ownership, published) = blocking(move || {
        let mut ownership = ownership;
        let published = ownership.publish();
        (ownership, published)
    })
    .await;
    let listener = match published {
        Ok(listener) => listener,
        Err(error) => return abort(core, ownership, error.to_string()).await,
    };
    let launcher = Launcher {
        ui: Arc::clone(&ui),
    };
    let server = match Server::start(
        &Handle::current(),
        listener,
        core.clone(),
        launcher.clone(),
        TokioClock::default(),
        ServerConfig::new(VERSION),
    ) {
        Ok(server) => server,
        Err(error) => {
            let reason = format!("сервер управления не запущен: {error}");
            return abort(core, ownership, reason).await;
        }
    };
    say(format_args!(
        "shellio: владелец готов, pid {}, сокет {}",
        std::process::id(),
        socket_path(&profile).display()
    ));

    for refused in &outgoing.local {
        say(launch::refusal_line(refused));
    }
    launcher.launch(outgoing.launch).await;

    let cause = signals.wait(&*ui).await;
    say(format_args!("shellio: завершение: {cause}"));
    server.shutdown().await;
    release(ownership).await;
    Finish::Terminate
}
