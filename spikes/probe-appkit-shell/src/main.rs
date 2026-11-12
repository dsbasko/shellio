//! Проба P0.2 `probe-appkit-shell` (ADR-0034). Без `--scenario` показывает демонстрационную
//! сцену, со `--scenario` сценарий ведёт процесс изнутри и печатает итог.

mod args;
mod scenario;

use std::cell::{OnceCell, RefCell};
use std::io::{self, Write};
use std::process::ExitCode;
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate,
    NSApplicationTerminateReply, NSBackingStoreType, NSMenu, NSMenuItem, NSScreen, NSWindow,
    NSWindowStyleMask,
};
use objc2_core_graphics::CGDisplayIsBuiltin;
use objc2_foundation::{
    NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, ns_string,
};

use args::{Mode, SceneOptions, USAGE};
use scenario::{EXIT_EXECUTION, Finisher, Outcome, Report, Scenario, Stand};

const PROBE_NAME: &str = "probe-appkit-shell";
const EXIT_USAGE: u8 = 2;
const WINDOW_SIZE: NSSize = NSSize::new(1180.0, 720.0);
const ACTIVATION: &str =
    "NSApplication.activateIgnoringOtherApps(true) в applicationDidFinishLaunching";

fn main() -> ExitCode {
    let args = match std::env::args_os()
        .skip(1)
        .map(|arg| arg.into_string())
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(args) => args,
        Err(arg) => return usage_error(&format!("аргумент не в UTF-8: {arg:?}")),
    };
    let mode = match args::parse(&args) {
        Ok(mode) => mode,
        Err(err) => return usage_error(&err.to_string()),
    };
    match mode {
        Mode::Help => {
            let _ = writeln!(io::stdout().lock(), "{USAGE}");
            ExitCode::SUCCESS
        }
        Mode::Aggregate { run_dir } => usage_error(&format!(
            "--aggregate {}: агрегирование ещё не реализовано",
            run_dir.display()
        )),
        Mode::Demo(scene) => run_app(None, scene),
        Mode::Scenario {
            scenario,
            scene,
            out,
        } => {
            if !out.is_dir() {
                return usage_error(&format!("--out {}: каталога нет", out.display()));
            }
            run_app(Some(scenario), scene)
        }
    }
}

fn usage_error(message: &str) -> ExitCode {
    let _ = writeln!(io::stderr().lock(), "{PROBE_NAME}: {message}\n\n{USAGE}");
    ExitCode::from(EXIT_USAGE)
}

fn run_app(scenario: Option<Scenario>, scene: SceneOptions) -> ExitCode {
    let Some(mtm) = MainThreadMarker::new() else {
        return report_outcome(Outcome::error(
            "запуск не на главном потоке",
            Report::default(),
        ));
    };
    let app = NSApplication::sharedApplication(mtm);
    // У исполняемого файла без Info.plist исходная политика `prohibited`: такое окно не получит
    // ни активности, ни key-статуса.
    if !app.setActivationPolicy(NSApplicationActivationPolicy::Regular) {
        return report_outcome(Outcome::error(
            "setActivationPolicy(.regular) отклонён",
            Report::default(),
        ));
    }
    app.setMainMenu(Some(&main_menu(mtm)));

    let slot = Rc::new(RefCell::new(None));
    let delegate = AppDelegate::new(mtm, scenario, scene, Finisher::new(Rc::clone(&slot)));
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    app.run();
    app.setDelegate(None);
    drop(delegate);

    let outcome = slot.borrow_mut().take();
    match outcome {
        Some(outcome) => report_outcome(outcome),
        None if scenario.is_none() => ExitCode::SUCCESS,
        None => report_outcome(Outcome::error(
            "основной цикл завершился без итога сценария",
            Report::default(),
        )),
    }
}

/// Печатает итог после остановки основного цикла, когда окно уже не обслуживается.
fn report_outcome(outcome: Outcome) -> ExitCode {
    let code = outcome.exit_code();
    let mut stdout = io::stdout().lock();
    if stdout.write_all(outcome.render().as_bytes()).is_err() {
        return ExitCode::from(EXIT_EXECUTION);
    }
    code
}

fn main_menu(mtm: MainThreadMarker) -> Retained<NSMenu> {
    let menu_bar = NSMenu::new(mtm);
    let app_item = NSMenuItem::new(mtm);
    let app_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str(PROBE_NAME));
    app_menu.addItem(&menu_item(
        mtm,
        &format!("Скрыть {PROBE_NAME}"),
        sel!(hide:),
        "h",
    ));
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    app_menu.addItem(&menu_item(
        mtm,
        &format!("Завершить {PROBE_NAME}"),
        sel!(terminate:),
        "q",
    ));
    app_item.setSubmenu(Some(&app_menu));
    menu_bar.addItem(&app_item);
    menu_bar
}

fn menu_item(
    mtm: MainThreadMarker,
    title: &str,
    action: objc2::runtime::Sel,
    key: &str,
) -> Retained<NSMenuItem> {
    // SAFETY: действия `hide:` и `terminate:` реализует NSApplication в цепочке ответчиков.
    unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            Some(action),
            &NSString::from_str(key),
        )
    }
}

/// Дисплей окна: встроенный, если он есть, иначе главный.
pub struct DisplayChoice {
    pub screen: Retained<NSScreen>,
    pub builtin: bool,
}

fn choose_display(mtm: MainThreadMarker) -> Option<DisplayChoice> {
    let builtin = NSScreen::screens(mtm)
        .iter()
        .find(|screen| CGDisplayIsBuiltin(screen.CGDirectDisplayID()));
    match builtin {
        Some(screen) => Some(DisplayChoice {
            screen,
            builtin: true,
        }),
        None => NSScreen::mainScreen(mtm).map(|screen| DisplayChoice {
            screen,
            builtin: false,
        }),
    }
}

fn main_window(mtm: MainThreadMarker, display: &DisplayChoice) -> Retained<NSWindow> {
    let screen = display.screen.frame();
    let visible = display.screen.visibleFrame();
    // С заданным экраном начало прямоугольника отсчитывается от начала этого экрана. Дробное
    // начало AppKit округляет, и содержимое вырастает на точку, поэтому оно целое.
    let origin = NSPoint::new(
        (visible.origin.x - screen.origin.x
            + ((visible.size.width - WINDOW_SIZE.width) / 2.0).max(0.0))
        .floor(),
        (visible.origin.y - screen.origin.y
            + ((visible.size.height - WINDOW_SIZE.height) / 2.0).max(0.0))
        .floor(),
    );
    let style = NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::Miniaturizable
        | NSWindowStyleMask::Resizable;
    // SAFETY: обычная инициализация окна с существующим экраном.
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer_screen(
            NSWindow::alloc(mtm),
            NSRect::new(origin, WINDOW_SIZE),
            style,
            NSBackingStoreType::Buffered,
            false,
            Some(&display.screen),
        )
    };
    // SAFETY: окно держит Rust, поэтому AppKit не должен освобождать его при закрытии.
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(ns_string!("probe-appkit-shell"));
    window.setContentMinSize(WINDOW_SIZE);
    window
}

struct DelegateIvars {
    scenario: Option<Scenario>,
    scene: SceneOptions,
    finisher: Finisher,
    window: OnceCell<Retained<NSWindow>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeAppKitShellDelegate"]
    #[ivars = DelegateIvars]
    struct AppDelegate;

    unsafe impl NSObjectProtocol for AppDelegate {}

    unsafe impl NSApplicationDelegate for AppDelegate {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _notification: &NSNotification) {
            self.show_scene();
        }

        #[unsafe(method(applicationShouldTerminateAfterLastWindowClosed:))]
        fn should_terminate_after_last_window_closed(&self, _sender: &NSApplication) -> bool {
            true
        }

        #[unsafe(method(applicationShouldTerminate:))]
        fn should_terminate(&self, sender: &NSApplication) -> NSApplicationTerminateReply {
            if self.ivars().scenario.is_none() {
                return NSApplicationTerminateReply::TerminateNow;
            }
            // Сценарий заканчивается только своим итогом, внешнее завершение — ошибка исполнения.
            self.ivars().finisher.finish(
                sender,
                Outcome::error(
                    "процесс завершён извне до конца сценария",
                    Report::default(),
                ),
            );
            NSApplicationTerminateReply::TerminateCancel
        }
    }
);

impl AppDelegate {
    fn new(
        mtm: MainThreadMarker,
        scenario: Option<Scenario>,
        scene: SceneOptions,
        finisher: Finisher,
    ) -> Retained<AppDelegate> {
        let this = AppDelegate::alloc(mtm).set_ivars(DelegateIvars {
            scenario,
            scene,
            finisher,
            window: OnceCell::new(),
        });
        // SAFETY: `init` у NSObject без аргументов.
        unsafe { msg_send![super(this), init] }
    }

    fn show_scene(&self) {
        let mtm = self.mtm();
        let app = NSApplication::sharedApplication(mtm);
        let ivars = self.ivars();
        let Some(display) = choose_display(mtm) else {
            ivars.finisher.finish(
                &app,
                Outcome::error("предусловие: нет дисплея", Report::default()),
            );
            return;
        };
        let window = main_window(mtm, &display);
        window.makeKeyAndOrderFront(None);
        // Кооперативный `activate()` (macOS 14) отклоняется, пока активен терминал, из которого
        // запущен процесс; устаревший запрос с игнорированием других приложений проходит.
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
        let _ = ivars.window.set(window.clone());

        if let Some(scenario) = ivars.scenario {
            let stand = Stand {
                app,
                window,
                display,
                scene: ivars.scene,
                activation: ACTIVATION,
                finisher: ivars.finisher.clone(),
            };
            scenario::start(scenario, Rc::new(stand));
        }
    }
}
