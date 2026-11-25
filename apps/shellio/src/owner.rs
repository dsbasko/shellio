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
//!
//! Зеркало состояния UI (ADR-0020) ведёт задача владельца: подписка на ядро, снимок и события
//! уходят порту [`MirrorSink`], разрыв ревизий даёт новый снимок.
//!
//! Запросы `ui` управляющего API сервер передаёт порту [`Ui::ui_request`] через [`UiPort`].

use std::fmt::Display;
use std::future::{Future, pending};
use std::io::{self, Write};
use std::panic::resume_unwind;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use shellio_control::{
    Activation, ClientError, Clock, ControlErrorCode, Exit, Expired, InstanceError, Launch,
    LaunchHandler, LaunchReceipt, OkValue, OwnerState, Ownership, RefusedArg, RequestBody,
    Response, Server, ServerConfig, Session, TokioClock, UiGate, UiHandler, UiRefusal, UiReply,
    UiRequest, socket_path,
};
use shellio_core::{
    Core, CoreHandle, Delivery, Event, FsRootResolver, MirrorStep, SystemClock, SystemIds,
};
use shellio_store::{Database, Profile, ProfileError, SqlitePersist};
use tokio::runtime::{Builder, Handle, Runtime};
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::task::{JoinHandle, spawn_blocking};

use crate::keymap_service::{Actions, KeymapService};
use crate::launch::{self, Outgoing, Run, WindowMode};
use crate::plugins;

/// Срок второго запуска: подключение, `hello` и отправка `launch`.
pub const SECOND_LAUNCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Срок передачи активации от получения `hello`, но не позже [`SECOND_LAUNCH_TIMEOUT`].
pub const HANDOFF_TIMEOUT: Duration = Duration::from_secs(1);

/// Версия сервера в ответе `hello`.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Имя второго запуска в `hello`.
const CLIENT: &str = concat!("shellio ", env!("CARGO_PKG_VERSION"));

/// Получатель зеркала состояния на главном потоке.
pub trait MirrorSink: Send + Sync + 'static {
    /// Применить доставку к зеркалу и вернуть исход. `None` — главный поток её не выполнил: UI
    /// уже нет.
    fn deliver(&self, delivery: Delivery) -> impl Future<Output = Option<MirrorStep>> + Send;
}

/// Порт UI владельца. Методы зовутся из задач runtime и не ждут главного потока синхронно.
pub trait Ui: MirrorSink {
    /// Готовность главного потока (`applicationDidFinishLaunching`). Готовность, наступившая
    /// до вызова, не теряется.
    fn ready(&self) -> impl Future<Output = ()> + Send;

    /// Роль выбрана: владелец переводит приложение в `.regular` и создаёт окно в режиме
    /// `window`. Возвращает `windowNumber` окна, `None` — окна нет.
    fn become_owner(&self, window: WindowMode) -> impl Future<Output = Option<i64>> + Send;

    /// Ядро загружено, сокет ещё не опубликован: главный поток получает ручку ядра и передаёт ему
    /// первый размер окна внутренним входом (ADR-0009, «Размер окна»). Возвращается после вызова.
    fn attach_core(&self, core: CoreHandle) -> impl Future<Output = ()> + Send;

    /// Выполняет активацию на главном потоке и возвращается после вызова.
    fn activate(&self) -> impl Future<Output = Activation> + Send;

    /// Запрос `ui` управляющего API на главном потоке. Работа начинается только после
    /// [`UiGate::begin`] с ответом `true`; срок накладывает сервер.
    fn ui_request(
        &self,
        request: UiRequest,
        gate: UiGate,
    ) -> impl Future<Output = Result<UiReply, UiRefusal>> + Send;

    /// Второй запуск перед отправкой `launch`: передать активацию владельцу `pid`, чтобы его
    /// [`Ui::activate`] получил передний план, и вернуть факты передачи. Срок накладывает
    /// [`claim`]: future, не завершившийся в срок, отбрасывается.
    fn hand_off_activation(&self, pid: u32) -> impl Future<Output = Handoff> + Send;

    /// Запрос завершения от UI (`applicationShouldTerminate`). UI без такого запроса не
    /// завершает future никогда.
    fn terminate_requested(&self) -> impl Future<Output = ()> + Send;

    /// Порядок завершения выполнен: UI отпускает приложение, процесс выходит с кодом 0.
    fn finish_terminate(&self);

    /// Второй запуск или отказ старта: процесс выходит с кодом `code` после сброса вывода.
    fn exit(&self, code: u8);
}

/// Факты передачи активации вторым запуском.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Handoff {
    /// Второй запуск был активен перед передачей.
    pub self_active: bool,
    /// Приложение владельца найдено по PID.
    pub owner_found: bool,
    /// Ответ запроса активации владельца, `None` — запрос не делался.
    pub activated: Option<bool>,
    /// Ответ LaunchServices на открытие бандла владельца.
    pub reopen: Reopen,
}

/// Ответ LaunchServices на открытие бандла владельца вторым запуском.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reopen {
    /// Запрос не делался: нет UI, приложения владельца или его бандла.
    NotRequested,
    /// LaunchServices открыл экземпляр с этим PID: владельца или другой.
    Opened(u32),
    /// LaunchServices ответил ошибкой с этим кодом; `None` — ни экземпляра, ни ошибки.
    Failed(Option<i64>),
}

/// Исход передачи активации.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandoffOutcome {
    /// Порт вернул факты в срок.
    Done(Handoff),
    /// Срок передачи истёк: `launch` отправляется без неё, если не истёк и срок второго
    /// запуска.
    Expired,
}

/// Строка исхода передачи владельцу `pid` для stderr второго запуска.
pub fn handoff_line(pid: u32, outcome: HandoffOutcome) -> String {
    let prefix = format!("shellio: передача активации pid {pid}");
    match outcome {
        HandoffOutcome::Expired => format!("{prefix}: срок передачи истёк"),
        HandoffOutcome::Done(handoff) => {
            let yes_no = |fact: bool| if fact { "да" } else { "нет" };
            let activated = match handoff.activated {
                Some(true) => "принята",
                Some(false) => "отклонена",
                None => "не запрашивалась",
            };
            let reopen = reopen_text(pid, handoff.reopen);
            format!(
                "{prefix}: второй запуск активен — {}, приложение владельца найдено — {}, \
                 активация владельца — {activated}, LaunchServices — {reopen}",
                yes_no(handoff.self_active),
                yes_no(handoff.owner_found),
            )
        }
    }
}

/// Строка stderr второго запуска перед запросом к LaunchServices: запрос не отменяется, а
/// истёкший срок передачи иначе скрыл бы, что он был.
pub fn reopen_start_line(pid: u32) -> String {
    format!(
        "shellio: передача активации pid {pid}: своей активности нет, открытие владельца через \
         LaunchServices"
    )
}

/// Строка ответа LaunchServices для stderr второго запуска, когда за ним следует ожидание
/// собственной активности: истёкший срок передачи иначе скрыл бы этот факт.
pub fn reopen_line(pid: u32, reopen: Reopen) -> String {
    format!(
        "shellio: передача активации pid {pid}: LaunchServices — {}",
        reopen_text(pid, reopen)
    )
}

fn reopen_text(pid: u32, reopen: Reopen) -> String {
    match reopen {
        Reopen::NotRequested => "не запрашивался".to_owned(),
        Reopen::Opened(opened) if opened == pid => "открыл владельца".to_owned(),
        Reopen::Opened(opened) => format!("открыл pid {opened}, не владельца"),
        Reopen::Failed(Some(code)) => format!("ошибка {code}"),
        Reopen::Failed(None) => "без экземпляра и ошибки".to_owned(),
    }
}

/// Передача активации вторым запуском в [`claim`]: вызов порта и запись исхода.
pub trait ActivationHandoff: Sync {
    /// Передать активацию владельцу `pid`. Срок накладывает [`claim`].
    fn hand_off(&self, pid: u32) -> impl Future<Output = Handoff> + Send;

    /// Исход передачи владельцу `pid`. Вызывается до отправки `launch`.
    fn outcome(&self, pid: u32, outcome: HandoffOutcome);
}

/// Передача портом [`Ui`]: исход печатается строкой в stderr.
struct UiHandoff<'a, U>(&'a U);

impl<U: Ui> ActivationHandoff for UiHandoff<'_, U> {
    fn hand_off(&self, pid: u32) -> impl Future<Output = Handoff> + Send {
        self.0.hand_off_activation(pid)
    }

    fn outcome(&self, pid: u32, outcome: HandoffOutcome) {
        say_err(handoff_line(pid, outcome));
    }
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

/// Создать runtime и запустить на нём задачу процесса с разобранными аргументами `run`
/// ([`launch::mode`]). Отказ галерее на профиле по умолчанию ([`launch::gallery_refusal`])
/// вызывающий проверяет до этого вызова. Runtime должен жить до выхода процесса. Главный поток
/// после вызова крутит цикл UI или ждёт [`Ui::exit`] и [`Ui::finish_terminate`].
pub fn start<U: Ui>(
    ui: Arc<U>,
    run: Run,
    profile: Result<Profile, ProfileError>,
) -> io::Result<Runtime> {
    let runtime = Builder::new_multi_thread()
        .enable_all()
        .thread_name("shellio-owner")
        .build()?;
    let main = runtime.spawn(process(Arc::clone(&ui), run, profile));
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

async fn process<U: Ui>(ui: Arc<U>, run: Run, profile: Result<Profile, ProfileError>) -> Finish {
    let profile = match profile {
        Ok(profile) => profile,
        Err(error) => return fail(format_args!("профиль не вычислен: {error}")),
    };
    let state = profile.clone();
    let args = run.args.clone();
    let prepared = blocking(move || {
        state
            .ensure_state_dir()
            .map_err(|error| error.to_string())?;
        launch::outgoing_from(args, std::env::current_dir())
    })
    .await;
    let outgoing = match prepared {
        Ok(outgoing) => outgoing,
        Err(reason) => return fail(reason),
    };
    let forwarded = run.forwarded(&outgoing.launch);
    match claim(
        &profile,
        TokioClock,
        SECOND_LAUNCH_TIMEOUT,
        &forwarded,
        &UiHandoff(&*ui),
    )
    .await
    {
        Ok(Claim::Owner(ownership)) => serve(ui, profile, ownership, outgoing, run.window).await,
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
        | Response::Reset { .. }
        | Response::Keymap { .. } => serde_json::to_string(frame).unwrap_or_default(),
    }
}

/// Выбрать роль до срока `timeout`: взять блокировку профиля или передать `launch` владельцу.
///
/// Блокировка занята — подключение к сокету. Сокета нет, его никто не слушает, владелец
/// уходит или отказ пришёл до полной отправки `launch` — ждать попытки по `clock` и снова
/// пробовать блокировку: освободилась — процесс становится владельцем. Каталог состояния уже
/// создан. `handoff` получает PID готового владельца из `hello` и передаёт ему активацию до
/// отправки `launch` под сроком [`HANDOFF_TIMEOUT`] внутри `timeout`. Срок передачи истёк —
/// `launch` всё равно отправляется.
pub async fn claim<C, H>(
    profile: &Profile,
    clock: C,
    timeout: Duration,
    launch: &Launch,
    handoff: &H,
) -> Result<Claim, InstanceError>
where
    C: Clock + Clone,
    H: ActivationHandoff,
{
    let deadline = clock.deadline(timeout);
    let socket = socket_path(profile);
    loop {
        let candidate = profile.clone();
        if let Some(ownership) = blocking(move || Ownership::try_acquire(&candidate)).await? {
            return Ok(Claim::Owner(ownership));
        }
        let reason = match forward(&clock, deadline, &socket, launch, handoff).await {
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

async fn forward<C, H>(
    clock: &C,
    deadline: C::Deadline,
    socket: &Path,
    launch: &Launch,
    handoff: &H,
) -> Attempt
where
    C: Clock + Clone,
    H: ActivationHandoff,
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
    // Срок передачи выдаётся до вызова порта, а внешний `within` не даёт ему пережить срок
    // второго запуска: тот истёк — `launch` получит отказ до отправки и повтор по сроку.
    let cutoff = clock.deadline(HANDOFF_TIMEOUT);
    let outcome = match clock
        .within(deadline, clock.within(cutoff, handoff.hand_off(pid)))
        .await
    {
        Ok(Ok(facts)) => HandoffOutcome::Done(facts),
        Ok(Err(Expired)) | Err(Expired) => HandoffOutcome::Expired,
    };
    handoff.outcome(pid, outcome);
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

/// Порт `ui` сервера над портом [`Ui`] владельца.
struct UiPort<U> {
    ui: Arc<U>,
    /// Каталог состояния владельца задан явно.
    explicit: bool,
}

impl<U: Ui> UiHandler for UiPort<U> {
    fn ui(
        &self,
        request: UiRequest,
        gate: UiGate,
    ) -> impl Future<Output = Result<UiReply, UiRefusal>> + Send {
        let admitted = admit_ui(&request, self.explicit);
        let work = admitted.is_ok().then(|| self.ui.ui_request(request, gate));
        async move {
            admitted?;
            match work {
                Some(work) => work.await,
                None => Err(UiRefusal::NoUi),
            }
        }
    }
}

/// Допуск запроса `ui` до главного потока: `ui.cell` и `ui.window.size` принимает только владелец
/// с явным каталогом состояния (ADR-0007, «Уточнено в `prototype-conformance`»). Отказ приходит до
/// изменения.
pub const fn admit_ui(request: &UiRequest, explicit: bool) -> Result<(), UiRefusal> {
    match request {
        UiRequest::Cell { .. } | UiRequest::WindowSize { .. } if !explicit => {
            Err(UiRefusal::IsolatedProfileOnly)
        }
        UiRequest::Cell { .. }
        | UiRequest::WindowSize { .. }
        | UiRequest::Window
        | UiRequest::Inspect
        | UiRequest::Stats
        | UiRequest::Receipts { .. }
        | UiRequest::Command(_)
        | UiRequest::Filter { .. }
        | UiRequest::Gallery(_) => Ok(()),
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
        // Реестр собирается до открытия базы: с неверным списком плагинов база не трогается.
        let registry = plugins::shared_registry().map_err(|error| error.to_string())?;
        // Плагин без фабрики вида остался бы пустой панелью: это ошибка запуска, а не окна.
        plugins::factories(&registry).map_err(|error| error.to_string())?;
        let db = Database::open(&profile, &SystemClock)
            .map_err(|error| format!("хранилище не открыто: {error}"))?;
        Core::start(
            &runtime,
            Box::new(SqlitePersist::new(db)),
            Box::new(SystemClock),
            Box::new(SystemIds),
            Arc::new(FsRootResolver),
            registry,
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
    mode: WindowMode,
) -> Finish {
    let signals = match Signals::install() {
        Ok(signals) => signals,
        Err(error) => {
            release(ownership).await;
            return fail(format_args!("обработчики сигналов не поставлены: {error}"));
        }
    };
    let explicit = profile.is_explicit();
    let window = ui.become_owner(mode).await;
    // Разбор незавершённого обновления встанет здесь, до открытия `state.db` (P5.2).
    let core = match load_core(profile.clone()).await {
        Ok(core) => core,
        Err(reason) => {
            release(ownership).await;
            return fail(reason);
        }
    };
    ui.ready().await;
    ui.attach_core(core.clone()).await;

    // Служба keymap отдаёт первичный снимок до публикации сокета: первый `query.keymap` уже видит
    // таблицу по файлу профиля.
    let keymap = match start_keymap(&profile).await {
        Ok(keymap) => keymap,
        Err(reason) => return abort(core, ownership, reason).await,
    };

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
    let server = match Server::start_with_keymap(
        &Handle::current(),
        listener,
        core.clone(),
        launcher.clone(),
        UiPort {
            ui: Arc::clone(&ui),
            explicit,
        },
        keymap.port(),
        TokioClock,
        ServerConfig::new(VERSION),
    ) {
        Ok(server) => server,
        Err(error) => {
            let reason = format!("сервер управления не запущен: {error}");
            return abort(core, ownership, reason).await;
        }
    };
    // Первый снимок зеркала встаёт в очередь главного потока раньше активации собственного
    // запуска: первая отрисовка окна key уже содержит его.
    let _mirror = start_mirror(Arc::clone(&ui), core.clone()).await;
    say(ready_line(
        std::process::id(),
        &socket_path(&profile),
        mode,
        window,
    ));

    for refused in &outgoing.local {
        say(launch::refusal_line(refused));
    }
    launcher.launch(outgoing.launch).await;

    let cause = signals.wait(&*ui).await;
    say(format_args!("shellio: завершение: {cause}"));
    server.shutdown().await;
    drop(keymap);
    release(ownership).await;
    Finish::Terminate
}

/// Служба keymap профиля: действия берутся из реестра плагинов процесса, наблюдение включено.
async fn start_keymap(profile: &Profile) -> Result<KeymapService, String> {
    let registry = plugins::shared_registry().map_err(|error| error.to_string())?;
    KeymapService::start(
        &Handle::current(),
        profile.config_dir().to_owned(),
        Actions::of(registry),
        true,
    )
    .await
    .map_err(|reason| format!("служба keymap не запущена: {reason}"))
}

/// Строка готовности владельца: PID, сокет, режим окна и `windowNumber`. Сценарии берут PID из
/// начала строки, поэтому новые поля только дописываются в конец.
pub fn ready_line(pid: u32, socket: &Path, mode: WindowMode, window: Option<i64>) -> String {
    let window = window.map_or_else(|| "нет".to_owned(), |number| number.to_string());
    format!(
        "shellio: владелец готов, pid {pid}, сокет {}, режим {}, окно {window}",
        socket.display(),
        mode.as_str()
    )
}

/// Запустить зеркало UI: первый снимок ядра доставляется до возврата, события и новые снимки —
/// задачей. Задача заканчивается, когда ядро закрыто или UI больше не выполняет доставок.
pub async fn start_mirror<S: MirrorSink>(sink: Arc<S>, core: CoreHandle) -> JoinHandle<()> {
    let first = deliver_snapshot(&*sink, &core).await;
    tokio::spawn(async move {
        let Some(mut events) = first else {
            return;
        };
        while forward_events(&*sink, &mut events).await {
            match deliver_snapshot(&*sink, &core).await {
                Some(next) => events = next,
                None => return,
            }
        }
    })
}

/// Новая подписка и её снимок зеркалу. `None` — ядро закрыто или UI нет.
async fn deliver_snapshot<S: MirrorSink>(
    sink: &S,
    core: &CoreHandle,
) -> Option<broadcast::Receiver<Event>> {
    let (snapshot, events) = core.subscribe().await.ok()?;
    // Снимок свежей подписки не старее зеркала: исход только подтверждает доставку.
    let _applied: MirrorStep = sink.deliver(Delivery::Snapshot(snapshot)).await?;
    Some(events)
}

/// События подписки зеркалу до разрыва. `true` — зеркалу нужен новый снимок, `false` — ядро
/// закрыто или UI нет.
async fn forward_events<S: MirrorSink>(sink: &S, events: &mut broadcast::Receiver<Event>) -> bool {
    loop {
        let delivery = match events.recv().await {
            Ok(event) => Delivery::Event(event),
            Err(RecvError::Lagged(_)) => Delivery::Reset,
            Err(RecvError::Closed) => return false,
        };
        match sink.deliver(delivery).await {
            None => return false,
            Some(MirrorStep::NeedSnapshot) => return true,
            Some(MirrorStep::Applied | MirrorStep::Ignored) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use shellio_core::test_support::{
        FakeRootResolver, ManualClock as CoreClock, MemoryPersist, SequentialIds, registry,
    };
    use shellio_core::{Command, Mirror, Revision, Snapshot, UnixMillis, workspace_name};

    use super::*;

    /// `ui.cell` и `ui.window.size` принимает только владелец с явным каталогом состояния;
    /// остальные запросы `ui` от профиля не зависят.
    #[test]
    fn cell_is_admitted_only_on_an_explicit_profile() {
        use shellio_control::{CellContrast, CellMotion, GalleryCommand};
        use shellio_core::UiCommand;

        let isolated = Profile::isolated("/tmp/shellio-state");
        assert!(isolated.is_explicit());
        let cells = [
            UiRequest::Cell {
                contrast: Some(CellContrast::Increased),
                motion: None,
            },
            UiRequest::Cell {
                contrast: Some(CellContrast::System),
                motion: Some(CellMotion::System),
            },
            UiRequest::Cell {
                contrast: None,
                motion: None,
            },
            UiRequest::WindowSize {
                width: 1600,
                height: 720,
            },
        ];
        for cell in cells {
            assert_eq!(admit_ui(&cell, isolated.is_explicit()), Ok(()));
            assert_eq!(
                admit_ui(&cell, false),
                Err(UiRefusal::IsolatedProfileOnly),
                "{cell:?}"
            );
        }
        for other in [
            UiRequest::Window,
            UiRequest::Inspect,
            UiRequest::Stats,
            UiRequest::Receipts { after: None },
            UiRequest::Command(UiCommand::WindowClose),
            UiRequest::Command(UiCommand::PanelFilter),
            UiRequest::Filter {
                text: "вид".into()
            },
            UiRequest::Gallery(GalleryCommand::Reset),
        ] {
            for explicit in [false, true] {
                assert_eq!(admit_ui(&other, explicit), Ok(()), "{other:?}");
            }
        }
    }

    #[test]
    fn ready_line_keeps_pid_prefix_and_names_mode_and_window() {
        let socket = Path::new("/tmp/p/shellio.sock");
        assert_eq!(
            ready_line(7, socket, WindowMode::Gallery, Some(4242)),
            "shellio: владелец готов, pid 7, сокет /tmp/p/shellio.sock, режим gallery, окно 4242"
        );
        assert_eq!(
            ready_line(8, socket, WindowMode::Normal, None),
            "shellio: владелец готов, pid 8, сокет /tmp/p/shellio.sock, режим normal, окно нет"
        );
    }

    /// Зеркало без UI: применяет доставки, а событие ревизии `lose` один раз теряет, как разрыв.
    struct LossyMirror {
        mirror: Mutex<Option<Mirror>>,
        lose: Revision,
        lost: Mutex<bool>,
        snapshots: Mutex<u32>,
        revision: tokio::sync::watch::Sender<Option<Revision>>,
    }

    impl MirrorSink for LossyMirror {
        fn deliver(&self, delivery: Delivery) -> impl Future<Output = Option<MirrorStep>> + Send {
            let mut mirror = self.mirror.lock().unwrap();
            let step = match delivery {
                Delivery::Event(event)
                    if event.revision == self.lose && !*self.lost.lock().unwrap() =>
                {
                    *self.lost.lock().unwrap() = true;
                    MirrorStep::Ignored
                }
                delivery => {
                    if matches!(delivery, Delivery::Snapshot(_)) {
                        *self.snapshots.lock().unwrap() += 1;
                    }
                    Mirror::deliver_to(&mut mirror, delivery)
                }
            };
            self.revision
                .send_replace(mirror.as_ref().map(Mirror::revision));
            std::future::ready(Some(step))
        }
    }

    impl LossyMirror {
        fn revision(&self) -> Option<Revision> {
            *self.revision.borrow()
        }
    }

    async fn snapshot(core: &CoreHandle) -> Snapshot {
        core.subscribe().await.unwrap().0
    }

    /// Снимок и события держат зеркало равным ядру. Потерянное событие даёт разрыв на
    /// следующем, задача берёт новый снимок, затем закрытое ядро её завершает.
    #[tokio::test(flavor = "multi_thread")]
    async fn mirror_follows_core_and_takes_new_snapshot_after_gap() {
        let core = Core::start(
            &Handle::current(),
            Box::new(MemoryPersist::new()),
            Box::new(CoreClock::new(UnixMillis(1_700_000_000_000))),
            Box::new(SequentialIds::new()),
            Arc::new(FakeRootResolver::new()),
            registry(),
        )
        .unwrap();
        let start = snapshot(&core).await.revision;
        let sink = Arc::new(LossyMirror {
            mirror: Mutex::new(None),
            lose: start.next().next(),
            lost: Mutex::new(false),
            snapshots: Mutex::new(0),
            revision: tokio::sync::watch::Sender::new(None),
        });
        let pump = start_mirror(Arc::clone(&sink), core.clone()).await;
        assert_eq!(
            sink.revision(),
            Some(start),
            "первый снимок доставлен до возврата"
        );

        let mut last = start;
        for title in ["Один", "Два", "Три"] {
            let reply = core
                .request(Command::WorkspaceCreate {
                    title: title.to_owned(),
                })
                .await
                .unwrap();
            last = reply.revision;
        }
        let expected = snapshot(&core).await;
        assert_eq!(expected.revision, last);
        let mut revision = sink.revision.subscribe();
        revision
            .wait_for(|revision| *revision == Some(last))
            .await
            .unwrap();
        let mirror = sink.mirror.lock().unwrap().clone().unwrap();
        assert_eq!(*mirror.state(), *expected.state);
        assert_eq!(
            workspace_name(mirror.state()),
            workspace_name(&expected.state)
        );
        assert!(*sink.lost.lock().unwrap());
        assert_eq!(
            *sink.snapshots.lock().unwrap(),
            2,
            "разрыв дал второй снимок"
        );

        core.shutdown().await;
        pump.await.unwrap();
    }

    fn done(self_active: bool, owner_found: bool, activated: Option<bool>) -> HandoffOutcome {
        reopened(self_active, owner_found, activated, Reopen::NotRequested)
    }

    fn reopened(
        self_active: bool,
        owner_found: bool,
        activated: Option<bool>,
        reopen: Reopen,
    ) -> HandoffOutcome {
        HandoffOutcome::Done(Handoff {
            self_active,
            owner_found,
            activated,
            reopen,
        })
    }

    #[test]
    fn handoff_line_names_every_fact() {
        assert_eq!(
            handoff_line(7, done(true, true, Some(true))),
            "shellio: передача активации pid 7: второй запуск активен — да, приложение владельца \
             найдено — да, активация владельца — принята, LaunchServices — не запрашивался"
        );
        assert_eq!(
            handoff_line(7, done(false, true, Some(false))),
            "shellio: передача активации pid 7: второй запуск активен — нет, приложение владельца \
             найдено — да, активация владельца — отклонена, LaunchServices — не запрашивался"
        );
        assert_eq!(
            handoff_line(7, done(true, false, None)),
            "shellio: передача активации pid 7: второй запуск активен — да, приложение владельца \
             найдено — нет, активация владельца — не запрашивалась, LaunchServices — не \
             запрашивался"
        );
    }

    #[test]
    fn handoff_line_tells_whether_launch_services_opened_the_owner() {
        let line = |reopen| handoff_line(7, reopened(false, true, Some(true), reopen));
        assert!(line(Reopen::Opened(7)).ends_with("LaunchServices — открыл владельца"));
        assert!(line(Reopen::Opened(9)).ends_with("LaunchServices — открыл pid 9, не владельца"));
        assert!(line(Reopen::Failed(Some(-600))).ends_with("LaunchServices — ошибка -600"));
        assert!(line(Reopen::Failed(None)).ends_with("LaunchServices — без экземпляра и ошибки"));
        assert_eq!(
            reopen_line(7, Reopen::Opened(9)),
            "shellio: передача активации pid 7: LaunchServices — открыл pid 9, не владельца"
        );
        assert_eq!(
            reopen_start_line(7),
            "shellio: передача активации pid 7: своей активности нет, открытие владельца через \
             LaunchServices"
        );
    }

    #[test]
    fn handoff_line_reports_expired_handoff() {
        assert_eq!(
            handoff_line(42, HandoffOutcome::Expired),
            "shellio: передача активации pid 42: срок передачи истёк"
        );
    }
}
