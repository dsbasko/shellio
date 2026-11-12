//! Проба P0.2 `probe-appkit-shell` (ADR-0034). Без `--scenario` показывает демонстрационную
//! сцену, со `--scenario` сценарий ведёт процесс изнутри и печатает итог.

mod args;
mod chrome;
mod fixtures;
mod list;
mod menu;
mod palette;
mod results;
mod scenario;
mod scene;
mod split;
mod stats;
mod tabs;
mod tiles;
mod tokens;
mod tree;

use std::cell::{OnceCell, RefCell};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::thread;

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
use results::ResultFile;
use scenario::events::{SharedTrace, TraceRing};
use scenario::{
    EXIT_EXECUTION, Finisher, Outcome, Report, Scenario, Stand, events, readback, watchdog,
};
use scene::Scene;

const PROBE_NAME: &str = "probe-appkit-shell";
const EXIT_USAGE: u8 = 2;
pub const WINDOW_SIZE: NSSize = NSSize::new(1180.0, 720.0);
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
        Mode::Aggregate { run_dir } => aggregate(&run_dir),
        Mode::Demo(scene) => match run_app(None, scene) {
            None => ExitCode::SUCCESS,
            Some(outcome) => print_outcome(&outcome),
        },
        Mode::Scenario {
            scenario,
            scene,
            out,
        } => {
            if !out.is_dir() {
                return usage_error(&format!("--out {}: каталога нет", out.display()));
            }
            run_scenario(scenario, scene, out)
        }
    }
}

/// `summary.json` прогона и сводка в stdout. Ненулевой код — только при ошибке исполнения
/// хотя бы одного процесса или самого агрегирования; выводы на код не влияют.
fn aggregate(run_dir: &Path) -> ExitCode {
    let written = results::aggregate(run_dir)
        .and_then(|summary| results::write_summary(run_dir, &summary).map(|path| (summary, path)));
    let (summary, path) = match written {
        Ok(written) => written,
        Err(reason) => {
            let _ = writeln!(io::stderr().lock(), "{PROBE_NAME}: --aggregate: {reason}");
            return ExitCode::from(EXIT_EXECUTION);
        }
    };
    let mut stdout = io::stdout().lock();
    let _ = write!(stdout, "{}", results::render_summary(&summary));
    let _ = writeln!(stdout, "сводка: {}", path.display());
    if summary.totals.execution_errors > 0 {
        ExitCode::from(EXIT_EXECUTION)
    } else {
        ExitCode::SUCCESS
    }
}

/// Процесс сценария: сторож, сцена, затем `result.json` в `--out` и итог в stdout.
fn run_scenario(scenario: Scenario, scene: SceneOptions, out: PathBuf) -> ExitCode {
    let environment = readback::static_environment(scene);
    let trace: SharedTrace = Arc::new(Mutex::new(TraceRing::default()));
    // Сторож снимается только после записи и печати итога: зависнуть может и выход из цикла.
    let watchdog = watchdog::arm(
        scenario.name(),
        scenario.deadline(),
        out.clone(),
        trace.clone(),
    );
    let mut outcome = match &watchdog {
        Ok(_) => run_app(
            Some(ScenarioRun {
                scenario,
                out: out.clone(),
                trace: trace.clone(),
            }),
            scene,
        )
        .unwrap_or_else(|| {
            Outcome::error(
                "основной цикл завершился без итога сценария",
                Report::default(),
            )
        }),
        Err(err) => Outcome::error(format!("сторож не запущен: {err}"), Report::default()),
    };
    outcome.report.diagnostics.trace = trace
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .lines();
    let code = print_outcome(&outcome);
    let result = ResultFile::new(
        scenario.name(),
        scene.split.name(),
        environment,
        outcome.execution,
        outcome.report,
    );
    // Окно уже не обслуживается; запись идёт в рабочем потоке, главный получает только итог.
    let written = thread::Builder::new()
        .name("probe-result".to_owned())
        .spawn(move || results::write_result(&out, &result))
        .map_err(|err| format!("рабочий поток не запущен: {err}"))
        .and_then(|writer| {
            writer
                .join()
                .unwrap_or_else(|_| Err("поток записи упал".to_owned()))
        });
    match written {
        Ok(_) => code,
        Err(reason) => {
            let _ = writeln!(
                io::stdout().lock(),
                "execution=error\nreason={} не записан: {reason}",
                results::RESULT_FILE
            );
            ExitCode::from(EXIT_EXECUTION)
        }
    }
}

fn usage_error(message: &str) -> ExitCode {
    let _ = writeln!(io::stderr().lock(), "{PROBE_NAME}: {message}\n\n{USAGE}");
    ExitCode::from(EXIT_USAGE)
}

/// Сценарий процесса: его имя, `--out` и след для сторожа.
struct ScenarioRun {
    scenario: Scenario,
    out: PathBuf,
    trace: SharedTrace,
}

/// Крутит основной цикл. `None` — демонстрация закончилась без итога, как и должна.
fn run_app(scenario: Option<ScenarioRun>, scene: SceneOptions) -> Option<Outcome> {
    let Some(mtm) = MainThreadMarker::new() else {
        return Some(Outcome::error(
            "запуск не на главном потоке",
            Report::default(),
        ));
    };
    // Демонстрация работает без стенда, сценарий — с подклассом, отмечающим `sendEvent`.
    let app = match scenario {
        Some(_) => match events::shared_application(mtm) {
            Ok(app) => app,
            Err(reason) => return Some(Outcome::error(reason, Report::default())),
        },
        None => NSApplication::sharedApplication(mtm),
    };
    // У исполняемого файла без Info.plist исходная политика `prohibited`: такое окно не получит
    // ни активности, ни key-статуса.
    if !app.setActivationPolicy(NSApplicationActivationPolicy::Regular) {
        return Some(Outcome::error(
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

    slot.borrow_mut().take()
}

/// Печатает итог после остановки основного цикла, когда окно уже не обслуживается.
fn print_outcome(outcome: &Outcome) -> ExitCode {
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

/// Прямоугольник окна размера `size` по центру видимой области дисплея. С заданным экраном
/// начало отсчитывается от начала этого экрана. Дробное начало AppKit округляет, и содержимое
/// вырастает на точку, поэтому оно целое.
pub fn window_frame(display: &DisplayChoice, size: NSSize) -> NSRect {
    let screen = display.screen.frame();
    let visible = display.screen.visibleFrame();
    let origin = NSPoint::new(
        (visible.origin.x - screen.origin.x + ((visible.size.width - size.width) / 2.0).max(0.0))
            .floor(),
        (visible.origin.y - screen.origin.y + ((visible.size.height - size.height) / 2.0).max(0.0))
            .floor(),
    );
    NSRect::new(origin, size)
}

/// Голое окно сценариев стенда, которым сцена не нужна.
fn main_window(mtm: MainThreadMarker, display: &DisplayChoice) -> Retained<NSWindow> {
    let style = NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::Miniaturizable
        | NSWindowStyleMask::Resizable;
    // SAFETY: обычная инициализация окна с существующим экраном.
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer_screen(
            NSWindow::alloc(mtm),
            window_frame(display, WINDOW_SIZE),
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
    scenario: Option<ScenarioRun>,
    scene: SceneOptions,
    finisher: Finisher,
    window: OnceCell<Retained<NSWindow>>,
    /// Собранная сцена держится до конца процесса.
    built: OnceCell<Rc<Scene>>,
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

        /// Сценарий заканчивается своим итогом: закрытие окна сценарием процесс не завершает.
        #[unsafe(method(applicationShouldTerminateAfterLastWindowClosed:))]
        fn should_terminate_after_last_window_closed(&self, _sender: &NSApplication) -> bool {
            self.ivars().scenario.is_none()
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
        scenario: Option<ScenarioRun>,
        scene: SceneOptions,
        finisher: Finisher,
    ) -> Retained<AppDelegate> {
        let this = AppDelegate::alloc(mtm).set_ivars(DelegateIvars {
            scenario,
            scene,
            finisher,
            window: OnceCell::new(),
            built: OnceCell::new(),
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
        let with_scene = ivars
            .scenario
            .as_ref()
            .is_none_or(|run| run.scenario.needs_scene());
        let scene = with_scene.then(|| Rc::new(Scene::build(mtm, &display, ivars.scene)));
        let window = match &scene {
            Some(scene) => scene.window.clone(),
            None => main_window(mtm, &display),
        };
        if let Some(scene) = &scene {
            let _ = ivars.built.set(Rc::clone(scene));
        }
        window.makeKeyAndOrderFront(None);
        // Кооперативный `activate()` (macOS 14) отклоняется, пока активен терминал, из которого
        // запущен процесс; устаревший запрос с игнорированием других приложений проходит.
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
        let _ = ivars.window.set(window.clone());

        if let Some(run) = &ivars.scenario {
            let stand = Stand {
                app,
                window,
                display,
                scene: ivars.scene,
                built_scene: scene,
                activation: ACTIVATION,
                finisher: ivars.finisher.clone(),
                trace: run.trace.clone(),
                out: run.out.clone(),
            };
            scenario::start(run.scenario, Rc::new(stand));
        }
    }
}
