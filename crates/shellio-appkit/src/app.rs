//! `NSApplication`: цикл главного потока, политика активации, передача активации вторым запуском,
//! готовность, повторное открытие и завершение. Окно владельца — [`crate::MainWindow`].

use std::cell::{Cell, RefCell};
use std::error::Error;
use std::fmt;
use std::sync::Mutex;

use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationOptions, NSApplicationActivationPolicy,
    NSApplicationDelegate, NSApplicationTerminateReply, NSBackingStoreType, NSEvent,
    NSEventModifierFlags, NSEventType, NSResponder, NSRunningApplication, NSScreen, NSWindow,
    NSWindowStyleMask, NSWorkspace, NSWorkspaceOpenConfiguration,
};
use objc2_foundation::{
    NSArray, NSError, NSNotification, NSObjectNSDelayedPerforming, NSPoint, NSRect,
    NSRunLoopCommonModes, NSSize,
};

use crate::counters::{Live, counter};

counter!(APP_DELEGATE, Class, "ShellioAppDelegate");
counter!(SERVICE_WINDOW_CLASS, Class, "ShellioServiceWindow");

/// События приложения. Методы вызываются на главном потоке из делегата `NSApplication` и не должны
/// ждать: пока метод не вернулся, цикл главного потока стоит.
pub trait Delegate: 'static {
    /// `applicationDidFinishLaunching`: цикл главного потока запущен и выполняет работу [`MainThread`].
    fn did_finish_launching(&self, app: &App);

    /// `applicationDidBecomeActive`: приложение стало активным, то есть получило передний план.
    fn did_become_active(&self, app: &App);

    /// `applicationDidResignActive`: приложение ушло с переднего плана. Маршрутизатор клавиш снимает
    /// по нему ожидание второго аккорда выхода.
    fn did_resign_active(&self, _app: &App) {}

    /// `applicationShouldHandleReopen:hasVisibleWindows:`: приложение открыли снова (обычный
    /// `open` бандла, щелчок по значку в Dock). Делегат сам показывает окно, AppKit больше ничего
    /// не делает.
    fn reopen(&self, app: &App, has_visible_windows: bool);

    /// `applicationShouldTerminate`: пользователь или система просят завершить приложение. AppKit уже
    /// получил `TerminateLater` и ждёт [`App::finish_terminate`], цикл главного потока при этом идёт.
    /// Повторный запрос, пока первый ждёт квитанции, сюда не доходит.
    fn terminate_requested(&self, app: &App);
}

/// Приложение на главном потоке. Значение существует только там: его получают делегат из [`App::run`]
/// и замыкание из [`MainThread::exec`], в другой поток оно не передаётся.
#[derive(Debug, Clone, Copy)]
pub struct App {
    mtm: MainThreadMarker,
}

impl App {
    /// Запускает `NSApplication` на главном потоке с политикой `.accessory`: без иконки в Dock и строки
    /// меню, но с правом стать активным. Второй запуск так получает передний план, чтобы передать его
    /// владельцу ([`App::hand_off_activation_to`]). Бандл объявляет `LSUIElement`, поэтому LaunchServices
    /// регистрирует процесс без Dock с самого старта, ещё до этого вызова; явная политика даёт то же
    /// бинарю вне бандла.
    ///
    /// Штатное завершение через [`App::finish_terminate`] выходит из процесса внутри AppKit, и тогда этот
    /// вызов не возвращается: код после него и деструкторы не выполняются. `Ok` — цикл остановлен без
    /// выхода из процесса.
    pub fn run(delegate: impl Delegate) -> Result<(), RunError> {
        let mtm = MainThreadMarker::new().ok_or(RunError::NotMainThread)?;
        let app = NSApplication::sharedApplication(mtm);
        if !set_policy(&app, policy(Role::Launch)) {
            return Err(RunError::PolicyRefused);
        }
        // Делегат у NSApplication слабый, поэтому его держит этот кадр до конца цикла.
        let delegate = AppDelegate::new(mtm, Box::new(delegate));
        app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        app.run();
        app.setDelegate(None);
        Ok(())
    }

    /// Просит AppKit завершить приложение (`terminate:`): путь `applicationShouldTerminate`, ответ
    /// `TerminateLater` и порядок завершения владельца. Возвращается сразу, до `terminate:`.
    ///
    /// `terminate:` встаёт таймером цикла главного потока, а не выполняется на месте. Под
    /// `TerminateLater` AppKit крутит вложенный цикл до квитанции, а вызванный из работы
    /// [`MainThread`] он стоял бы внутри блока главной очереди: она последовательная и вложенным
    /// циклом не обслуживается, поэтому квитанция [`App::finish_terminate`] через неё не пришла бы.
    pub fn terminate(&self) {
        let app = self.ns_app();
        let object: &NSObject = &app;
        // SAFETY: `NSRunLoopCommonModes` — неизменяемая строка Foundation, живущая весь процесс.
        let common = unsafe { NSRunLoopCommonModes };
        let modes = NSArray::from_slice(&[common]);
        // SAFETY: `terminate:` реализован `NSApplication` и принимает отправителя или nil.
        unsafe {
            object.performSelector_withObject_afterDelay_inModes(
                sel!(terminate:),
                None,
                0.0,
                &modes,
            );
        }
    }

    /// Ставит в конец очереди событий пустое событие приложения: `-[NSApplication run]` забирает
    /// его, заканчивает свой оборот и сливает свой пул автоосвобождения. Этот пул сливается только
    /// после очередного события, а снятие окна и field editor AppKit доделывает асинхронно, уже
    /// после работы, и автоосвобождает их виды туда же: без ввода они живут до следующего события
    /// (`window-cycle` (в) P1.3).
    pub fn drain_event_pool(&self) {
        post_empty_event(&self.ns_app());
    }

    /// Политика владельца `.regular`: иконка в Dock и право на передний план. `false` — AppKit отказал.
    pub fn become_regular(&self) -> bool {
        set_policy(&self.ns_app(), policy(Role::Owner))
    }

    /// Владелец просит передний план после обработки запуска. Возврат значит, что вызов выполнен, а
    /// не что приложение активно: факт переднего плана проверяет `make e2e`.
    pub fn activate(&self) {
        // Кооперативный запрос: система судит его по тому, кто передал активацию. Её передаёт второй
        // запуск до отправки `launch` ([`App::hand_off_activation_to`]).
        self.ns_app().activate();
    }

    /// Второй запуск просит передний план себе, чтобы затем передать его владельцу. Активность
    /// приходит позже событием [`Delegate::did_become_active`] или не приходит вовсе.
    pub fn request_activation(&self) {
        // Кооперативный `activate()` агентское приложение не активирует ни из `open -n`, ни из
        // терминала. Устаревший вызов — по P0.2 (20 из 20 при старте из терминала) и варианту 2
        // плана `single-instance-activation`.
        self.activate_ignoring_others();
    }

    /// Владелец с показанным окном просит передний план некооперативно: под `LSUIElement` первый
    /// запуск от Finder кооперативным `activate()` вперёд не выходит (ADR-0006, «Уточнено в P1.2:
    /// доводка активации»). Правило «Первый передний план» плана `appkit-foundation`, вариант 2:
    /// P0.2 — 20 из 20 при старте с окном.
    pub fn activate_ignoring_others(&self) {
        #[allow(deprecated)]
        self.ns_app().activateIgnoringOtherApps(true);
    }

    /// Показывает служебное окно второго запуска перед [`App::request_activation`]: без рамки,
    /// 1×1, за пределами всех экранов, ключевое. Приложению с окном система даёт передний план
    /// охотнее (P0.2). Пользователь его не видит, живёт оно до [`App::close_service_window`].
    pub fn show_service_window(&self) {
        SERVICE_WINDOW.with(|slot| {
            let mut slot = slot.borrow_mut();
            let window =
                slot.get_or_insert_with(|| ServiceWindow::new(self.mtm, offscreen(self.mtm)));
            window.makeKeyAndOrderFront(None);
        });
    }

    /// Закрывает служебное окно, если оно есть. Повторный вызов ничего не делает.
    pub fn close_service_window(&self) {
        if let Some(window) = SERVICE_WINDOW.with(|slot| slot.borrow_mut().take()) {
            window.close();
        }
    }

    /// Активно ли приложение, то есть на переднем плане ли оно.
    pub fn is_active(&self) -> bool {
        self.ns_app().isActive()
    }

    /// Передаёт активацию приложению с PID `pid`: уступает её ему (`yieldActivationToApplication`) и
    /// просит систему активировать его от имени этого процесса (`activateFromApplication_options`).
    /// Возвращает факты передачи; переход переднего плана они не доказывают, его проверяет `make e2e`.
    pub fn hand_off_activation_to(&self, pid: u32) -> Transfer {
        let app = self.ns_app();
        // Активность читается до уступки: после неё система может уже отдать передний план.
        let self_active = app.isActive();
        let owner = i32::try_from(pid)
            .ok()
            .and_then(NSRunningApplication::runningApplicationWithProcessIdentifier);
        let activated = owner.as_deref().map(|owner| {
            // Сначала уступка, потом запрос: система принимает запрос, только если активный процесс
            // уже уступил активацию тому, кого просят активировать.
            app.yieldActivationToApplication(owner);
            owner.activateFromApplication_options(
                &NSRunningApplication::currentApplication(),
                NSApplicationActivationOptions::empty(),
            )
        });
        Transfer {
            self_active,
            owner_found: owner.is_some(),
            activated,
        }
    }

    /// Просит LaunchServices открыть приложение по бандлу приложения с PID `pid` и активировать
    /// его, не заводя нового экземпляра. LaunchServices адресует бандл, а не PID: из нескольких
    /// экземпляров он выбирает сам, поэтому `done` получает PID выбранного. `done` вызывается один
    /// раз на любом потоке; без приложения или бандла — сразу, без запроса.
    pub fn reopen_bundle_of(&self, pid: u32, done: impl FnOnce(Reopened) + Send + 'static) {
        let bundle = i32::try_from(pid)
            .ok()
            .and_then(NSRunningApplication::runningApplicationWithProcessIdentifier)
            .and_then(|owner| owner.bundleURL());
        let Some(bundle) = bundle else {
            done(Reopened::NoBundle);
            return;
        };
        let config = NSWorkspaceOpenConfiguration::configuration();
        config.setActivates(true);
        config.setCreatesNewApplicationInstance(false);
        let done = Mutex::new(Some(done));
        let handler = RcBlock::new(move |app: *mut NSRunningApplication, error: *mut NSError| {
            // SAFETY: LaunchServices передаёт обработчику живые объекты или nil на время вызова.
            let (app, error) = unsafe { (app.as_ref(), error.as_ref()) };
            let reopened = match (app, error) {
                (Some(app), _) => Reopened::Opened(app.processIdentifier()),
                (None, Some(error)) => Reopened::Failed(Some(error.code())),
                (None, None) => Reopened::Failed(None),
            };
            if let Some(done) = done.lock().ok().and_then(|mut done| done.take()) {
                done(reopened);
            }
        });
        NSWorkspace::sharedWorkspace().openApplicationAtURL_configuration_completionHandler(
            &bundle,
            &config,
            Some(&handler),
        );
    }

    /// Порядок завершения выполнен: приложение отпускается, процесс выходит внутри AppKit с кодом 0.
    /// Если запрос `applicationShouldTerminate` ждёт квитанции, AppKit получает
    /// `replyToApplicationShouldTerminate(true)`. Иначе завершение начинается вызовом `terminate:`, и
    /// делегат отвечает `TerminateNow`. Повторный вызов ничего не делает. Вывод сбрасывается до вызова:
    /// после него процесс завершается без деструкторов.
    pub fn finish_terminate(&self) {
        match transition(Termination::finish) {
            // Через очередь, а не сразу: квитанция, отданная из самого `terminate_requested`, иначе пришла
            // бы раньше ответа `TerminateLater`.
            Release::Reply => {
                MainThread::new().exec(|app| app.ns_app().replyToApplicationShouldTerminate(true))
            }
            Release::Terminate => self.ns_app().terminate(None),
            Release::Nothing => {}
        }
    }

    fn ns_app(&self) -> Retained<NSApplication> {
        NSApplication::sharedApplication(self.mtm)
    }

    /// Доказательство главного потока для кода crate.
    pub(crate) fn mtm(&self) -> MainThreadMarker {
        self.mtm
    }
}

/// Факты передачи активации вторым запуском ([`App::hand_off_activation_to`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transfer {
    /// Приложение было активно перед передачей.
    pub self_active: bool,
    /// Приложение владельца найдено по PID.
    pub owner_found: bool,
    /// Ответ `activateFromApplication_options`; `None` — запрос не делался.
    pub activated: Option<bool>,
}

/// Кадр 1×1 правее и выше всех экранов: окно там не видно ни на одном из них.
fn offscreen(mtm: MainThreadMarker) -> NSRect {
    beyond(NSScreen::screens(mtm).iter().map(|screen| screen.frame()))
}

/// Кадр 1×1 правее и выше всех кадров `screens` с отступом [`SERVICE_WINDOW_GAP`].
fn beyond(screens: impl IntoIterator<Item = NSRect>) -> NSRect {
    let (right, top) = screens
        .into_iter()
        .fold((0.0_f64, 0.0_f64), |(right, top), frame| {
            (
                right.max(frame.origin.x + frame.size.width),
                top.max(frame.origin.y + frame.size.height),
            )
        });
    NSRect::new(
        NSPoint::new(right + SERVICE_WINDOW_GAP, top + SERVICE_WINDOW_GAP),
        NSSize::new(1.0, 1.0),
    )
}

/// Отступ служебного окна от края экранов: за ним не видна и его тень.
const SERVICE_WINDOW_GAP: f64 = 1000.0;

/// Ответ LaunchServices на открытие бандла ([`App::reopen_bundle_of`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reopened {
    /// Приложение по PID не найдено или у него нет бандла: запрос не делался.
    NoBundle,
    /// LaunchServices открыл экземпляр с этим PID.
    Opened(i32),
    /// LaunchServices вернул ошибку с этим кодом; `None` — ни приложения, ни ошибки.
    Failed(Option<isize>),
}

/// Роль процесса для политики активации.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// Старт до выбора роли и второй запуск.
    Launch,
    /// Владелец профиля.
    Owner,
}

/// Политика активации роли. Второму запуску нужна `.accessory`, а не `.prohibited`: под `.prohibited`
/// приложение не бывает активным, и передавать владельцу ему нечего (ADR-0006).
const fn policy(role: Role) -> NSApplicationActivationPolicy {
    match role {
        Role::Launch => NSApplicationActivationPolicy::Accessory,
        Role::Owner => NSApplicationActivationPolicy::Regular,
    }
}

/// Пустое событие приложения в конец очереди событий: `sendEvent:` его никуда не доставляет.
fn post_empty_event(app: &NSApplication) {
    let event = NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
        NSEventType::ApplicationDefined,
        NSPoint::ZERO,
        NSEventModifierFlags::empty(),
        0.0,
        0,
        None,
        0,
        0,
        0,
    );
    if let Some(event) = event {
        app.postEvent_atStart(&event, false);
    }
}

/// Ставит политику `wanted`. Уже стоящая политика — успех: `setActivationPolicy` отказывает в
/// `.accessory` процессу, который `LSUIElement` бандла и так запустил с ней.
fn set_policy(app: &NSApplication, wanted: NSApplicationActivationPolicy) -> bool {
    app.activationPolicy() == wanted || app.setActivationPolicy(wanted)
}

/// Отправка работы на главный поток из любого потока. Замыкание встаёт в главную очередь dispatch и
/// выполняется циклом [`App::run`], отправитель не ждёт. Пока цикл не запущен, работа копится в очереди.
#[derive(Debug, Clone, Copy, Default)]
pub struct MainThread {
    _private: (),
}

impl MainThread {
    pub const fn new() -> Self {
        Self { _private: () }
    }

    pub fn exec(self, work: impl FnOnce(&App) + Send + 'static) {
        DispatchQueue::main().exec_async(move || {
            // Главная очередь выполняется только главным потоком, поэтому `None` здесь не наступает.
            if let Some(mtm) = MainThreadMarker::new() {
                work(&App { mtm });
            }
        });
    }
}

/// Отказ запуска [`App::run`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunError {
    NotMainThread,
    PolicyRefused,
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotMainThread => "NSApplication запускается только на главном потоке",
            Self::PolicyRefused => "AppKit отказал в политике активации .accessory",
        })
    }
}

impl Error for RunError {}

thread_local! {
    /// Служебное окно второго запуска; его создаёт и закрывает только главный поток.
    static SERVICE_WINDOW: RefCell<Option<Retained<ServiceWindow>>> = const { RefCell::new(None) };
}

thread_local! {
    /// Завершение приложения. Его меняют только делегат и [`App`], то есть главный поток.
    static TERMINATION: Cell<Termination> = const { Cell::new(Termination::Running) };
}

fn transition<R>(step: impl FnOnce(&mut Termination) -> R) -> R {
    TERMINATION.with(|cell| {
        let mut state = cell.get();
        let result = step(&mut state);
        cell.set(state);
        result
    })
}

/// Два входа в завершение: запрос AppKit (`applicationShouldTerminate`) и порядок завершения владельца по
/// сигналу. Оба сходятся в одном выходе через AppKit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Termination {
    Running,
    /// AppKit получил `TerminateLater` и ждёт `replyToApplicationShouldTerminate`.
    Pending,
    /// Порядок завершения выполнен, приложение отпущено.
    Finished,
}

/// Ответ на `applicationShouldTerminate`.
#[derive(Debug, PartialEq, Eq)]
enum Request {
    /// `TerminateLater`; `notify` — передать запрос делегату, только у первого.
    Later { notify: bool },
    /// `TerminateNow`: порядок завершения уже выполнен.
    Now,
}

/// Действие [`App::finish_terminate`].
#[derive(Debug, PartialEq, Eq)]
enum Release {
    Reply,
    Terminate,
    Nothing,
}

impl Termination {
    fn request(&mut self) -> Request {
        match *self {
            Self::Running => {
                *self = Self::Pending;
                Request::Later { notify: true }
            }
            Self::Pending => Request::Later { notify: false },
            Self::Finished => Request::Now,
        }
    }

    fn finish(&mut self) -> Release {
        let release = match *self {
            Self::Running => Release::Terminate,
            Self::Pending => Release::Reply,
            Self::Finished => Release::Nothing,
        };
        *self = Self::Finished;
        release
    }
}

struct Ivars {
    _live: Live,
    delegate: Box<dyn Delegate>,
}

struct ServiceIvars {
    _live: Live,
}

define_class!(
    // SAFETY: NSObject не требует от подкласса ничего сверх `init`, `Drop` у класса нет.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioAppDelegate"]
    #[ivars = Ivars]
    struct AppDelegate;

    // SAFETY: протокол NSObject реализует суперкласс NSObject.
    unsafe impl NSObjectProtocol for AppDelegate {}

    // SAFETY: протокол требует только необязательные методы, ниже они совпадают с его объявлениями.
    unsafe impl NSApplicationDelegate for AppDelegate {
        // SAFETY: селектор, аргумент и результат — как у `applicationDidFinishLaunching:` протокола.
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _notification: &NSNotification) {
            self.ivars()
                .delegate
                .did_finish_launching(&App { mtm: self.mtm() });
        }

        // SAFETY: селектор, аргумент и результат — как у `applicationDidBecomeActive:` протокола.
        #[unsafe(method(applicationDidBecomeActive:))]
        fn did_become_active(&self, _notification: &NSNotification) {
            self.ivars()
                .delegate
                .did_become_active(&App { mtm: self.mtm() });
        }

        // SAFETY: селектор, аргумент и результат — как у `applicationDidResignActive:` протокола.
        #[unsafe(method(applicationDidResignActive:))]
        fn did_resign_active(&self, _notification: &NSNotification) {
            self.ivars()
                .delegate
                .did_resign_active(&App { mtm: self.mtm() });
        }

        // SAFETY: селектор, аргументы и результат — как у
        // `applicationShouldHandleReopen:hasVisibleWindows:` протокола. `NO` — делегат обработал
        // открытие сам.
        #[unsafe(method(applicationShouldHandleReopen:hasVisibleWindows:))]
        fn should_handle_reopen(&self, _sender: &NSApplication, has_visible_windows: bool) -> bool {
            self.ivars()
                .delegate
                .reopen(&App { mtm: self.mtm() }, has_visible_windows);
            false
        }

        // SAFETY: селектор, аргумент и результат — как у `applicationShouldTerminate:` протокола.
        #[unsafe(method(applicationShouldTerminate:))]
        fn should_terminate(&self, _sender: &NSApplication) -> NSApplicationTerminateReply {
            match transition(Termination::request) {
                Request::Later { notify } => {
                    if notify {
                        self.ivars()
                            .delegate
                            .terminate_requested(&App { mtm: self.mtm() });
                    }
                    NSApplicationTerminateReply::TerminateLater
                }
                Request::Now => NSApplicationTerminateReply::TerminateNow,
            }
        }
    }
);

define_class!(
    // SAFETY: NSWindow не требует от подкласса ничего сверх переопределённых ниже методов, `Drop` у
    // класса нет.
    #[unsafe(super(NSWindow, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioServiceWindow"]
    #[ivars = ServiceIvars]
    struct ServiceWindow;

    impl ServiceWindow {
        // SAFETY: селектор и результат — как у `canBecomeKeyWindow` NSWindow.
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key_window(&self) -> bool {
            true
        }

        // SAFETY: селектор и результат — как у `canBecomeMainWindow` NSWindow.
        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main_window(&self) -> bool {
            false
        }

        // SAFETY: селектор, аргументы и результат — как у `constrainFrameRect:toScreen:` NSWindow.
        // Кадр не сдвигается на экран: окно должно остаться за его пределами.
        #[unsafe(method(constrainFrameRect:toScreen:))]
        fn constrain_frame_rect(&self, frame: NSRect, _screen: Option<&NSScreen>) -> NSRect {
            frame
        }
    }
);

impl ServiceWindow {
    fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ServiceIvars {
            _live: SERVICE_WINDOW_CLASS.live(),
        });
        // SAFETY: назначенный инициализатор NSWindow с окном без рамки.
        let window: Retained<Self> = unsafe {
            msg_send![
                super(this),
                initWithContentRect: frame,
                styleMask: NSWindowStyleMask::Borderless,
                backing: NSBackingStoreType::Buffered,
                defer: false
            ]
        };
        // SAFETY: окно держит `Retained` из SERVICE_WINDOW, `close` не должен его освобождать.
        unsafe { window.setReleasedWhenClosed(false) };
        window
    }
}

impl AppDelegate {
    fn new(mtm: MainThreadMarker, delegate: Box<dyn Delegate>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(Ivars {
            _live: APP_DELEGATE.live(),
            delegate,
        });
        // SAFETY: `init` у NSObject без аргументов, ivars уже заданы.
        unsafe { msg_send![super(this), init] }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    use super::*;

    struct Counting(Arc<AtomicUsize>);

    impl Delegate for Counting {
        fn did_finish_launching(&self, _app: &App) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }

        fn did_become_active(&self, _app: &App) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }

        fn reopen(&self, _app: &App, _has_visible_windows: bool) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }

        fn terminate_requested(&self, _app: &App) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn run_off_main_thread_is_refused_before_appkit() {
        let calls = Arc::new(AtomicUsize::new(0));
        let delegate = Counting(Arc::clone(&calls));
        // Свой поток: тест не зависит от того, где библиотека тестов выполняет тело.
        let result = thread::spawn(move || App::run(delegate)).join().unwrap();
        assert_eq!(result, Err(RunError::NotMainThread));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(Arc::strong_count(&calls), 1, "делегат отпущен");
    }

    #[test]
    fn ui_request_waits_for_owner_and_is_released_once() {
        let mut state = Termination::Running;
        assert_eq!(state.request(), Request::Later { notify: true });
        assert_eq!(state.request(), Request::Later { notify: false });
        assert_eq!(state.finish(), Release::Reply);
        assert_eq!(state.finish(), Release::Nothing);
        assert_eq!(state.request(), Request::Now);
    }

    #[test]
    fn owner_finish_without_request_starts_terminate_answered_now() {
        let mut state = Termination::Running;
        assert_eq!(state.finish(), Release::Terminate);
        assert_eq!(state.request(), Request::Now);
        assert_eq!(state.finish(), Release::Nothing);
    }

    #[test]
    fn main_thread_handle_crosses_threads() {
        fn assert_handle<T: Send + Sync + Copy + 'static>() {}
        assert_handle::<MainThread>();
    }

    #[test]
    fn run_error_texts_name_the_cause() {
        assert!(
            RunError::NotMainThread
                .to_string()
                .contains("главном потоке")
        );
        assert!(RunError::PolicyRefused.to_string().contains(".accessory"));
    }

    #[test]
    fn service_window_lies_beyond_every_screen() {
        let rect = |x, y, w, h| NSRect::new(NSPoint::new(x, y), NSSize::new(w, h));
        let screens = [
            rect(0.0, 0.0, 1512.0, 982.0),
            rect(-1920.0, 200.0, 1920.0, 1080.0),
            rect(1512.0, -300.0, 2560.0, 1440.0),
        ];
        let window = beyond(screens);
        assert_eq!(window.size, NSSize::new(1.0, 1.0));
        for screen in screens {
            assert!(
                window.origin.x >= screen.origin.x + screen.size.width + SERVICE_WINDOW_GAP
                    || window.origin.y >= screen.origin.y + screen.size.height + SERVICE_WINDOW_GAP,
                "окно {window:?} задевает экран {screen:?}"
            );
        }
        assert_eq!(
            window.origin,
            NSPoint::new(4072.0 + 1000.0, 1280.0 + 1000.0)
        );
    }

    #[test]
    fn service_window_without_screens_still_lies_off_the_origin() {
        let window = beyond([]);
        assert_eq!(
            window.origin,
            NSPoint::new(SERVICE_WINDOW_GAP, SERVICE_WINDOW_GAP)
        );
    }

    #[test]
    fn launch_may_become_active_without_dock_and_owner_is_regular() {
        assert_eq!(
            policy(Role::Launch),
            NSApplicationActivationPolicy::Accessory
        );
        assert_eq!(policy(Role::Owner), NSApplicationActivationPolicy::Regular);
        assert_ne!(
            policy(Role::Launch),
            NSApplicationActivationPolicy::Prohibited,
            "под .prohibited второму запуску нечего передать владельцу"
        );
    }
}
