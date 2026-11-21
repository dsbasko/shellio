//! `NSApplication` без окна: цикл главного потока, политика активации, готовность и завершение.

use std::cell::Cell;
use std::error::Error;
use std::fmt;

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationOptions, NSApplicationActivationPolicy,
    NSApplicationDelegate, NSApplicationTerminateReply, NSRunningApplication,
};
use objc2_foundation::NSNotification;

/// События приложения. Методы вызываются на главном потоке из делегата `NSApplication` и не должны
/// ждать: пока метод не вернулся, цикл главного потока стоит.
pub trait Delegate: 'static {
    /// `applicationDidFinishLaunching`: цикл главного потока запущен и выполняет работу [`MainThread`].
    fn did_finish_launching(&self, app: &App);

    /// `applicationDidBecomeActive`: приложение стало активным, то есть получило передний план.
    fn did_become_active(&self, app: &App);

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
    /// владельцу ([`App::hand_off_activation_to`]). Политика ставится явно, потому что приложение из
    /// бандла без неё стартует `.regular`.
    ///
    /// Штатное завершение через [`App::finish_terminate`] выходит из процесса внутри AppKit, и тогда этот
    /// вызов не возвращается: код после него и деструкторы не выполняются. `Ok` — цикл остановлен без
    /// выхода из процесса.
    pub fn run(delegate: impl Delegate) -> Result<(), RunError> {
        let mtm = MainThreadMarker::new().ok_or(RunError::NotMainThread)?;
        let app = NSApplication::sharedApplication(mtm);
        if !app.setActivationPolicy(policy(Role::Launch)) {
            return Err(RunError::PolicyRefused);
        }
        // Делегат у NSApplication слабый, поэтому его держит этот кадр до конца цикла.
        let delegate = AppDelegate::new(mtm, Box::new(delegate));
        app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        app.run();
        app.setDelegate(None);
        Ok(())
    }

    /// Политика владельца `.regular`: иконка в Dock и право на передний план. `false` — AppKit отказал.
    pub fn become_regular(&self) -> bool {
        self.ns_app().setActivationPolicy(policy(Role::Owner))
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
        self.ns_app().activate();
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
    delegate: Box<dyn Delegate>,
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

impl AppDelegate {
    fn new(mtm: MainThreadMarker, delegate: Box<dyn Delegate>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(Ivars { delegate });
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
