//! Проба P0.3 `probe-ghostty-surface` (ADR-0034). Без `--scenario` показывает демонстрационную
//! сцену, со `--scenario` сценарий ведёт процесс изнутри и печатает итог. `--launch`,
//! `--aggregate` и `--fixture` работают без AppKit.

mod args;
mod clock;
mod env;
mod fixture;
mod ghostty;
mod gpu;
mod keys;
mod launch;
mod palette;
mod results;
mod router;
mod scenario;
mod scene;
mod stats;
mod utf16;
mod view;
mod visibility;

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
    NSApplicationTerminateReply, NSAutoresizingMaskOptions, NSBackingStoreType, NSMenu, NSMenuItem,
    NSScreen, NSWindow, NSWindowStyleMask,
};
use objc2_core_graphics::CGDisplayIsBuiltin;
use objc2_foundation::{
    NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, ns_string,
};

use args::{Mode, Route, Run, USAGE};
use results::ResultFile;
use scenario::events::{SharedTrace, TraceRing};
use scenario::stand::StandView;
use scenario::{
    EXIT_EXECUTION, Finisher, Outcome, Prelude, Report, Scenario, Stand, events, watchdog,
};

pub const PROBE_NAME: &str = "probe-ghostty-surface";
const EXIT_USAGE: u8 = 2;
pub const WINDOW_SIZE: NSSize = NSSize::new(1180.0, 720.0);
const ACTIVATION: &str =
    "NSApplication.activateIgnoringOtherApps(true) в applicationDidFinishLaunching";

fn main() -> ExitCode {
    // Вход в `main` — граница сценариев старта, затем окружение: позже его меняют AppKit и
    // ghostty.
    clock::mark_main_entry();
    let entry_env: Vec<(String, String)> = std::env::vars_os()
        .map(|(name, value)| {
            (
                name.to_string_lossy().into_owned(),
                value.to_string_lossy().into_owned(),
            )
        })
        .collect();
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
        Mode::Launch(launch) => launch::run(launch),
        Mode::Fixture { channel, nonce } => fixture::main(&channel, &nonce, entry_env),
        Mode::Demo(run) => {
            let check = env::check(&entry_env, env::Lang::of(run.route, None, None));
            if !check.violations.is_empty() {
                let _ = writeln!(
                    io::stderr().lock(),
                    "{PROBE_NAME}: предусловие: окружение: {}\nдемонстрация запускается через make probe-ghostty-demo",
                    check.violations.join("; ")
                );
                return ExitCode::from(EXIT_EXECUTION);
            }
            match run_app(None, run.route) {
                None => ExitCode::SUCCESS,
                Some(outcome) => print_outcome(&outcome),
            }
        }
        Mode::Scenario {
            scenario,
            variant,
            out,
            run,
        } => {
            if !out.is_dir() {
                return usage_error(&format!("--out {}: каталога нет", out.display()));
            }
            run_scenario(scenario, &variant, out, run, &entry_env)
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

/// Процесс сценария: проверка окружения, сторож, окно, затем `result.json` в `--out` и итог в
/// stdout.
fn run_scenario(
    scenario: Scenario,
    variant: &str,
    out: PathBuf,
    run: Run,
    entry_env: &[(String, String)],
) -> ExitCode {
    let check = env::check(
        entry_env,
        env::Lang::of(run.route, Some(scenario), Some(variant)),
    );
    let process = scenario::window::process_info(
        run.route.name(),
        run.nonce.clone(),
        entry_env,
        check.extra.clone(),
    );
    let environment = scenario::window::static_environment(process);
    let trace: SharedTrace = Arc::new(Mutex::new(TraceRing::default()));
    // Сокет фикстур и `ghostty_init` — до первого рабочего потока: сторож ниже уже поток. Зависание
    // в `ghostty_init` снимает внешний сторож `--launch`.
    let prelude = if check.violations.is_empty() {
        Prelude::prepare(scenario, variant, &out, run.nonce.clone())
    } else {
        Err(format!(
            "предусловие: окружение процесса: {}",
            check.violations.join("; ")
        ))
    };
    // Сторож снимается только после записи и печати итога: зависнуть может и выход из цикла.
    let watchdog = watchdog::arm(
        scenario.name(),
        scenario.deadline(),
        out.clone(),
        trace.clone(),
    );
    let mut outcome = match (&watchdog, prelude) {
        (Err(err), _) => Outcome::error(format!("сторож не запущен: {err}"), Report::default()),
        (Ok(_), Err(reason)) => Outcome::error(reason, Report::default()),
        (Ok(_), Ok(prelude)) => run_app(
            Some(ScenarioRun {
                scenario,
                trace: trace.clone(),
                prelude: RefCell::new(Some(prelude)),
            }),
            run.route,
        )
        .unwrap_or_else(|| {
            Outcome::error(
                "основной цикл завершился без итога сценария",
                Report::default(),
            )
        }),
    };
    outcome.report.diagnostics.trace = trace
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .lines();
    let code = print_outcome(&outcome);
    let result = ResultFile::new(
        scenario.name(),
        variant,
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
    drop(watchdog);
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

/// Сценарий процесса, след для сторожа и то, что сделано до `NSApplication`.
struct ScenarioRun {
    scenario: Scenario,
    trace: SharedTrace,
    prelude: RefCell<Option<Prelude>>,
}

/// Крутит основной цикл. `None` — демонстрация закончилась без итога, как и должна.
fn run_app(scenario: Option<ScenarioRun>, route: Route) -> Option<Outcome> {
    let Some(mtm) = MainThreadMarker::new() else {
        return Some(Outcome::error(
            "запуск не на главном потоке",
            Report::default(),
        ));
    };
    // Подкласс `sendEvent` доставляет `keyUp` и автоповтор владельцу нажатия; сценарию он ещё и
    // отмечает события, демонстрация работает без стенда.
    let app = match events::shared_application(mtm) {
        Ok(app) => app,
        Err(reason) => return Some(Outcome::error(reason, Report::default())),
    };
    // У бандла без LSUIElement политика уже `regular`, и повторная установка возвращает `false`.
    if app.activationPolicy() != NSApplicationActivationPolicy::Regular
        && !app.setActivationPolicy(NSApplicationActivationPolicy::Regular)
    {
        return Some(Outcome::error(
            format!(
                "setActivationPolicy(.regular) отклонён при политике {:?}",
                app.activationPolicy()
            ),
            Report::default(),
        ));
    }
    app.setMainMenu(Some(&main_menu(mtm)));

    let slot = Rc::new(RefCell::new(None));
    let delegate = AppDelegate::new(mtm, scenario, route, Finisher::new(Rc::clone(&slot)));
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

/// Прямоугольник окна размера `size` по центру видимой области дисплея, от начала этого экрана.
/// Дробное начало AppKit округляет, и содержимое вырастает на точку, поэтому оно целое.
fn window_frame(display: &DisplayChoice, size: NSSize) -> NSRect {
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

/// Окно пробы с видом стенда в содержимом, который принимает фокус.
fn main_window(
    mtm: MainThreadMarker,
    display: &DisplayChoice,
) -> (Retained<NSWindow>, Retained<StandView>) {
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
    window.setTitle(ns_string!("probe-ghostty-surface"));
    window.setContentMinSize(WINDOW_SIZE);
    let view = StandView::new(mtm, NSRect::new(NSPoint::ZERO, WINDOW_SIZE));
    view.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    window.setContentView(Some(&view));
    window.makeFirstResponder(Some(&view));
    (window, view)
}

struct DelegateIvars {
    scenario: Option<ScenarioRun>,
    route: Route,
    finisher: Finisher,
    /// Окно держится до конца процесса.
    window: OnceCell<Retained<NSWindow>>,
    /// Сцена четырёх зон демонстрации; сценарий строит сцену сам.
    scene: OnceCell<scene::Scene>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeGhosttySurfaceDelegate"]
    #[ivars = DelegateIvars]
    struct AppDelegate;

    unsafe impl NSObjectProtocol for AppDelegate {}

    unsafe impl NSApplicationDelegate for AppDelegate {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _notification: &NSNotification) {
            self.show_window();
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
        route: Route,
        finisher: Finisher,
    ) -> Retained<AppDelegate> {
        let this = AppDelegate::alloc(mtm).set_ivars(DelegateIvars {
            scenario,
            route,
            finisher,
            window: OnceCell::new(),
            scene: OnceCell::new(),
        });
        // SAFETY: `init` у NSObject без аргументов.
        unsafe { msg_send![super(this), init] }
    }

    fn show_window(&self) {
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
        let (window, view) = main_window(mtm, &display);
        window.makeKeyAndOrderFront(None);
        // Кооперативный `activate()` (macOS 14) отклоняется, пока активен терминал, из которого
        // запущен процесс; устаревший запрос с игнорированием других приложений проходит.
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
        let _ = ivars.window.set(window.clone());

        if ivars.scenario.is_none() {
            let dark = scene::is_dark(&app.effectiveAppearance());
            let demo = scene::Scene::new(mtm, dark);
            demo.install(&view, &window);
            let _ = ivars.scene.set(demo);
        }
        if let Some(run) = &ivars.scenario {
            let Some(prelude) = run.prelude.borrow_mut().take() else {
                ivars.finisher.finish(
                    &app,
                    Outcome::error("окно сценария открыто второй раз", Report::default()),
                );
                return;
            };
            let stand = Stand {
                app,
                window,
                view,
                display,
                route: ivars.route,
                activation: ACTIVATION,
                finisher: ivars.finisher.clone(),
                trace: run.trace.clone(),
                prelude,
            };
            scenario::start(run.scenario, Rc::new(stand));
        }
    }
}
