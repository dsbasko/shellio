//! Проба P0.6 `probe-diff-view` (ADR-0034). Без `--scenario` показывает демонстрацию, со
//! `--scenario` сценарий ведёт процесс изнутри, пишет `result.json` в `--out` и печатает итог.
//! `--aggregate` сводит прогон, `--reference` пишет эталоны подсветки, оба без `NSApplication`.
//!
//! Активация без бандла, меню приложения, выбор дисплея и сведения о системе скопированы из
//! `spikes/probe-appkit-shell/src/main.rs` и `scenario/readback.rs` (P0.2) и упрощены.

mod args;
mod blocks;
mod draft_field;
mod paint;
mod results;
mod scenario;
mod scene;
mod stats;
mod text;
mod view_a;
mod view_b;

use std::cell::{OnceCell, RefCell};
use std::ffi::CString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::thread;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate,
    NSApplicationTerminateReply, NSBackingStoreType, NSEvent, NSEventModifierFlags, NSEventType,
    NSMenu, NSMenuItem, NSRunningApplication, NSScreen, NSWindow, NSWindowStyleMask,
};
use objc2_core_graphics::CGDisplayIsBuiltin;
use objc2_foundation::{
    NSDate, NSNotification, NSObject, NSObjectProtocol, NSPoint, NSProcessInfo, NSRect, NSRunLoop,
    NSRunLoopCommonModes, NSSize, NSString, NSTimer,
};

use args::{Command, FramesPath, Scenario, USAGE, Variant, ViewMode, ViewOptions};
use results::{
    Build, DisplayInfo, Environment, Execution, Hardware, Rect, Report, ResultFile, Size, Verdict,
    WindowInfo,
};

const PROBE_NAME: &str = "probe-diff-view";
const EXIT_USAGE: u8 = 2;
/// Код выхода при ошибке исполнения: стенд не дошёл до конца, предусловие не выполнено,
/// тайм-аут. Отрицательный вывод гипотезы ошибкой исполнения не считается.
const EXIT_EXECUTION: u8 = 3;
const WINDOW_SIZE: NSSize = NSSize::new(1180.0, 720.0);
/// Менеджер окон, при котором окно стенда не держит запрошенную рамку.
const AEROSPACE_BUNDLE_ID: &str = "bobko.aerospace";
/// Срок `smoke` от `applicationDidFinishLaunching` до готового окна.
const SMOKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Срок сторожа `smoke` из отчёта, раздел «Сроки и размеры»: окно, два ожидания отметки по 2 с и
/// снимок до 10 с.
const SMOKE_DEADLINE: Duration = Duration::from_secs(60);
/// Срок сторожа `open-smoke` из отчёта, раздел «Сроки и размеры»: окно, генерация фикстуры и
/// сроки всех шагов вместе.
const OPEN_SMOKE_DEADLINE: Duration = Duration::from_secs(90);
/// Срок сторожа `ax` из отчёта, раздел «Сроки и размеры»: окно, генерация фикстуры, готовность
/// клиента, открытие, четыре шага и пять снимков.
const AX_DEADLINE: Duration = Duration::from_secs(150);
/// Срок сторожа `open` из отчёта, раздел «Сроки и размеры»: окно до 10 с, загрузка до 30 с,
/// готовность до 10 с, открытие до 30 с.
const OPEN_DEADLINE: Duration = Duration::from_secs(120);
/// Срок сторожа `open-warm`: окно, загрузка и готовность, как у `open`, и 20 открытий до 30 с.
const OPEN_WARM_DEADLINE: Duration = Duration::from_secs(11 * 60);
/// Частота встроенного дисплея, которую подтверждает `calibrate` (предусловие плана).
const REQUIRED_FPS: isize = 120;
/// Срок сторожа `calibrate` из отчёта, раздел «Сроки и размеры»: четыре записи по 5 с, у каждой
/// сохранение до 3 мин.
const CALIBRATE_DEADLINE: Duration = Duration::from_secs(20 * 60);
/// Срок сторожа `frames` при пути `none` из отчёта, раздел «Сроки и размеры»: окно, загрузка,
/// готовность, открытие, раскрытие тредов, три отрезка S1 с ожиданием покрытия, S2 и S3.
const FRAMES_DEADLINE: Duration = Duration::from_secs(4 * 60);
/// Сторожа `memory`, `keys` и `idle` (отчёт, строка «Сроки `memory`, `keys`, `idle`»).
const MEMORY_DEADLINE: Duration = Duration::from_secs(5 * 60);
const KEYS_DEADLINE: Duration = Duration::from_secs(10 * 60);
const IDLE_DEADLINE: Duration = Duration::from_secs(2 * 60);
/// Период проверки готовности окна. Поток не засыпает: между проверками цикл обрабатывает события.
const POLL_INTERVAL_S: f64 = 0.005;

fn main() -> ExitCode {
    let args = match std::env::args_os()
        .skip(1)
        .map(|arg| arg.into_string())
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(args) => args,
        Err(arg) => return usage_error(&format!("аргумент не в UTF-8: {arg:?}")),
    };
    let command = match args::parse(&args) {
        Ok(command) => command,
        Err(err) => return usage_error(&err.to_string()),
    };
    match command {
        Command::Help => {
            let _ = writeln!(io::stdout().lock(), "{USAGE}");
            ExitCode::SUCCESS
        }
        Command::Aggregate { run_dir } => aggregate(&run_dir),
        Command::Reference { dir } => reference(&dir),
        Command::Neighbor => neighbor(),
        Command::AxClient { pid } => ax_client(pid),
        Command::Demo(view) => match run_app(None, view, None, None, Default::default()) {
            None => ExitCode::SUCCESS,
            Some(outcome) => print_outcome(&outcome),
        },
        Command::Scenario {
            scenario,
            view,
            out,
            fixtures,
            frames_path,
        } => {
            if !out.is_dir() {
                return usage_error(&format!("--out {}: каталога нет", out.display()));
            }
            run_scenario(
                scenario,
                view,
                out,
                fixtures.as_deref(),
                frames_path.unwrap_or_default(),
            )
        }
    }
}

fn usage_error(message: &str) -> ExitCode {
    let _ = writeln!(io::stderr().lock(), "{PROBE_NAME}: {message}\n\n{USAGE}");
    ExitCode::from(EXIT_USAGE)
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

/// Эталоны подсветки фикстур вне измеряемого процесса. Ненулевой код — ошибка исполнения.
fn reference(dir: &Path) -> ExitCode {
    let started = Instant::now();
    match probe_diff_view::highlight::reference::write_all(dir) {
        Ok(paths) => {
            let mut stdout = io::stdout().lock();
            for path in paths {
                let _ = writeln!(stdout, "эталон: {}", path.display());
            }
            let _ = writeln!(
                stdout,
                "эталоны записаны за {:.1} с",
                started.elapsed().as_secs_f64()
            );
            ExitCode::SUCCESS
        }
        Err(reason) => {
            let _ = writeln!(io::stderr().lock(), "{PROBE_NAME}: --reference: {reason}");
            ExitCode::from(EXIT_EXECUTION)
        }
    }
}

/// Служебный сосед калибровки. Выходит сам при закрытии stdin, сюда возвращается только отказ.
fn neighbor() -> ExitCode {
    let Some(mtm) = MainThreadMarker::new() else {
        let _ = writeln!(
            io::stderr().lock(),
            "{PROBE_NAME}: --neighbor не на главном потоке"
        );
        return ExitCode::from(EXIT_EXECUTION);
    };
    let Some(screen) = choose_display(mtm) else {
        let _ = writeln!(io::stderr().lock(), "{PROBE_NAME}: --neighbor: нет дисплея");
        return ExitCode::from(EXIT_EXECUTION);
    };
    let reason = scenario::neighbor::run(mtm, &screen)
        .err()
        .unwrap_or_else(|| "основной цикл соседа завершился".to_owned());
    let _ = writeln!(io::stderr().lock(), "{PROBE_NAME}: --neighbor: {reason}");
    ExitCode::from(EXIT_EXECUTION)
}

/// Внешний клиент доступности сценария `ax`. Отказ до готовности — строка `ax_client_error=`.
fn ax_client(pid: i32) -> ExitCode {
    match scenario::ax_client::run(pid) {
        Ok(()) => ExitCode::SUCCESS,
        Err(reason) => {
            let mut stdout = io::stdout().lock();
            let _ = writeln!(stdout, "{}{reason}", scenario::ax_client::ERROR_LINE);
            let _ = stdout.flush();
            ExitCode::from(EXIT_EXECUTION)
        }
    }
}

/// Итог процесса сценария.
struct Outcome {
    execution: Execution,
    report: Report,
}

impl Outcome {
    fn ok(report: Report) -> Outcome {
        Outcome {
            execution: Execution::Ok,
            report,
        }
    }

    fn error(reason: impl Into<String>, last_state: Option<String>, report: Report) -> Outcome {
        Outcome {
            execution: Execution::Error {
                reason: reason.into(),
                last_state,
            },
            report,
        }
    }

    fn render(&self) -> String {
        let mut text = match &self.execution {
            Execution::Ok => "execution=ok\n".to_owned(),
            Execution::Error { reason, last_state } => {
                let mut text = format!("execution=error\nreason={reason}\n");
                if let Some(state) = last_state {
                    text.push_str(&format!("last_state={state}\n"));
                }
                text
            }
        };
        for finding in &self.report.findings {
            text.push_str(&format!(
                "finding={} {}: {}\n",
                finding.verdict.name(),
                finding.name,
                finding.reason
            ));
        }
        text
    }

    fn exit_code(&self) -> ExitCode {
        if self.execution.is_ok() {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(EXIT_EXECUTION)
        }
    }
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

/// Процесс сценария: сцена или сценарий без окна, затем `result.json` в `--out` и итог в stdout.
fn run_scenario(
    scenario: Scenario,
    view: ViewOptions,
    out: PathBuf,
    fixtures: Option<&Path>,
    frames_path: FramesPath,
) -> ExitCode {
    if scenario == Scenario::Memory {
        scenario::memory_run::install_counter();
    }
    let environment = static_environment();
    // Грамматика проверяется до окна: разбор строки из одного выражения, без AppKit.
    let grammar = check_grammar();
    // Вставший `displayLink`, запись `xctrace` или потерянное событие иначе повесили бы процесс
    // без итога. След стенда событий идёт в кольцо сторожа.
    let trace = scenario::watchdog::SharedTrace::default();
    let deadline = match scenario {
        Scenario::Calibrate => Some(("calibrate", CALIBRATE_DEADLINE)),
        Scenario::Smoke => Some(("smoke", SMOKE_DEADLINE)),
        Scenario::OpenSmoke => Some(("open-smoke", OPEN_SMOKE_DEADLINE)),
        Scenario::Ax => Some(("ax", AX_DEADLINE)),
        Scenario::Open => Some(("open", OPEN_DEADLINE)),
        Scenario::OpenWarm => Some(("open-warm", OPEN_WARM_DEADLINE)),
        Scenario::Frames => Some(("frames", FRAMES_DEADLINE)),
        Scenario::Memory => Some(("memory", MEMORY_DEADLINE)),
        Scenario::Keys => Some(("keys", KEYS_DEADLINE)),
        Scenario::Idle => Some(("idle", IDLE_DEADLINE)),
        Scenario::Highlight => None,
    };
    let _watchdog = match deadline {
        Some((name, deadline)) => {
            match scenario::watchdog::arm(name, deadline, out.clone(), trace.clone()) {
                Ok(watchdog) => Some(watchdog),
                Err(err) => {
                    let _ = writeln!(
                        io::stdout().lock(),
                        "execution=error\nreason=сторож не взведён: {err}"
                    );
                    return ExitCode::from(EXIT_EXECUTION);
                }
            }
        }
        None => None,
    };
    let mut outcome = match scenario {
        Scenario::Smoke
        | Scenario::Calibrate
        | Scenario::OpenSmoke
        | Scenario::Ax
        | Scenario::Keys
        | Scenario::Memory
        | Scenario::Idle => run_app(Some(scenario), view, Some(out.clone()), None, trace)
            .unwrap_or_else(|| {
                Outcome::error(
                    "основной цикл завершился без итога сценария",
                    None,
                    Report::default(),
                )
            }),
        Scenario::Highlight => run_headless(fixtures),
        Scenario::Frames => match scenario::frames_run::unsupported_path(frames_path) {
            Some(reason) => Outcome::error(reason, None, Report::default()),
            None => {
                run_app(Some(scenario), view, Some(out.clone()), None, trace).unwrap_or_else(|| {
                    Outcome::error(
                        "основной цикл завершился без итога сценария",
                        None,
                        Report::default(),
                    )
                })
            }
        },
        Scenario::Open | Scenario::OpenWarm => {
            match fixtures.and_then(|dir| scenario::open::not_run_reason(view, dir)) {
                Some(reason) => {
                    let mut outcome = Outcome::ok(scenario::open::not_run_report(&reason));
                    if let Some(mtm) = MainThreadMarker::new() {
                        outcome.report.displays = display_infos(mtm);
                    }
                    outcome
                }
                None => run_app(
                    Some(scenario),
                    view,
                    Some(out.clone()),
                    fixtures.map(Path::to_path_buf),
                    trace,
                )
                .unwrap_or_else(|| {
                    Outcome::error(
                        "основной цикл завершился без итога сценария",
                        None,
                        Report::default(),
                    )
                }),
            }
        }
    };
    if outcome.execution.is_ok() {
        match grammar {
            Ok(reason) => outcome
                .report
                .finding("grammar_loaded", Verdict::Pass, reason),
            Err(reason) => outcome
                .report
                .finding("grammar_loaded", Verdict::Fail, reason),
        }
        let mut complete = environment.clone();
        complete.displays.clone_from(&outcome.report.displays);
        let missing = complete.missing();
        if missing.is_empty() {
            outcome.report.finding(
                "environment_complete",
                Verdict::Pass,
                "все поля окружения есть",
            );
        } else {
            outcome.report.finding(
                "environment_complete",
                Verdict::Fail,
                format!("нет полей окружения: {}", missing.join(", ")),
            );
        }
    }
    let code = print_outcome(&outcome);
    let result = ResultFile::new(
        scenario.name(),
        view.variant,
        view.mode,
        view.fixture,
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

/// Сценарий без окна и без `NSApplication`. Дисплеи для окружения читаются через `NSScreen`.
fn run_headless(fixtures: Option<&Path>) -> Outcome {
    let Some(fixtures) = fixtures else {
        return Outcome::error("сценарию нужен --fixtures", None, Report::default());
    };
    let mut outcome = match scenario::highlight::run(fixtures) {
        (report, Ok(())) => Outcome::ok(report),
        (report, Err(reason)) => Outcome::error(reason, None, report),
    };
    if let Some(mtm) = MainThreadMarker::new() {
        outcome.report.displays = display_infos(mtm);
    }
    outcome
}

/// Грамматика `tree-sitter-rust` загружается в разборщик этой версии `tree-sitter` и разбирает
/// строку без ошибок.
fn check_grammar() -> Result<String, String> {
    let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&language)
        .map_err(|err| format!("грамматика не загружена: {err}"))?;
    let tree = parser
        .parse("fn main() { let x = r#\"y\"#; }", None)
        .ok_or("разбор не вернул дерево")?;
    let root = tree.root_node();
    if root.kind() != "source_file" || root.has_error() {
        return Err(format!(
            "корень {} с ошибкой разбора: {}",
            root.kind(),
            root.has_error()
        ));
    }
    Ok(format!(
        "ABI грамматики {}, корень {}",
        language.abi_version(),
        root.kind()
    ))
}

/// Крутит основной цикл. `None` — демонстрация закончилась без итога, как и должна.
fn run_app(
    scenario: Option<Scenario>,
    view: ViewOptions,
    out: Option<PathBuf>,
    fixtures: Option<PathBuf>,
    trace: scenario::watchdog::SharedTrace,
) -> Option<Outcome> {
    let Some(mtm) = MainThreadMarker::new() else {
        return Some(Outcome::error(
            "запуск не на главном потоке",
            None,
            Report::default(),
        ));
    };
    // Демонстрация работает без стенда, сценарий — с подклассом, отмечающим `sendEvent` (P0.2).
    // Без установленного стенда событий подкласс пропускает события без отметок.
    let app = match scenario {
        Some(_) => match scenario::events::shared_application(mtm) {
            Ok(app) => app,
            Err(reason) => return Some(Outcome::error(reason, None, Report::default())),
        },
        None => NSApplication::sharedApplication(mtm),
    };
    // У исполняемого файла без Info.plist исходная политика `prohibited`: такое окно не получит
    // ни активности, ни key-статуса.
    if !app.setActivationPolicy(NSApplicationActivationPolicy::Regular) {
        return Some(Outcome::error(
            "setActivationPolicy(.regular) отклонён",
            None,
            Report::default(),
        ));
    }
    app.setMainMenu(Some(&main_menu(mtm)));

    let slot = Rc::new(RefCell::new(None));
    let delegate = AppDelegate::new(mtm, scenario, view, out, fixtures, trace, Rc::clone(&slot));
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    app.run();
    app.setDelegate(None);
    drop(delegate);

    slot.borrow_mut().take()
}

fn main_menu(mtm: MainThreadMarker) -> Retained<NSMenu> {
    let menu_bar = NSMenu::new(mtm);
    let app_item = NSMenuItem::new(mtm);
    let app_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str(PROBE_NAME));
    // SAFETY: действие `terminate:` реализует NSApplication в цепочке ответчиков.
    let quit = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(&format!("Завершить {PROBE_NAME}")),
            Some(sel!(terminate:)),
            &NSString::from_str("q"),
        )
    };
    app_menu.addItem(&quit);
    app_item.setSubmenu(Some(&app_menu));
    menu_bar.addItem(&app_item);
    menu_bar
}

/// Дисплей окна: встроенный, если он есть, иначе главный.
fn choose_display(mtm: MainThreadMarker) -> Option<Retained<NSScreen>> {
    NSScreen::screens(mtm)
        .iter()
        .find(|screen| CGDisplayIsBuiltin(screen.CGDirectDisplayID()))
        .or_else(|| NSScreen::mainScreen(mtm))
}

/// Прямоугольник окна по центру видимой области дисплея в координатах этого дисплея. Дробное
/// начало AppKit округляет, и содержимое вырастает на точку, поэтому оно целое.
fn window_frame(screen: &NSScreen, size: NSSize) -> NSRect {
    let full = screen.frame();
    let visible = screen.visibleFrame();
    let origin = NSPoint::new(
        (visible.origin.x - full.origin.x + ((visible.size.width - size.width) / 2.0).max(0.0))
            .floor(),
        (visible.origin.y - full.origin.y + ((visible.size.height - size.height) / 2.0).max(0.0))
            .floor(),
    );
    NSRect::new(origin, size)
}

/// Прямоугольник относительно дисплея в глобальных координатах AppKit.
fn global_rect(local: NSRect, screen_origin: NSPoint) -> NSRect {
    NSRect::new(
        NSPoint::new(
            local.origin.x + screen_origin.x,
            local.origin.y + screen_origin.y,
        ),
        local.size,
    )
}

fn describe_rect(rect: NSRect) -> String {
    format!(
        "{},{} {}x{}",
        rect.origin.x, rect.origin.y, rect.size.width, rect.size.height
    )
}

/// Рамка содержимого, отличная от запрошенной: окно разложил менеджер окон или система, и
/// сценарий меряет не ту геометрию.
fn frame_mismatch(actual: NSRect, requested: NSRect) -> Option<String> {
    (actual != requested).then(|| {
        format!(
            "предусловие: рамка содержимого окна {} вместо запрошенной {}",
            describe_rect(actual),
            describe_rect(requested)
        )
    })
}

/// Физические предусловия окна стенда из плана, раздел «Предусловия»: AeroSpace выключен
/// полностью, окно держит запрошенную рамку. Нарушение — ошибка исполнения.
fn precondition_violation(window: &NSWindow, requested: Option<NSRect>) -> Option<String> {
    let aerospace = NSRunningApplication::runningApplicationsWithBundleIdentifier(
        &NSString::from_str(AEROSPACE_BUNDLE_ID),
    );
    if aerospace.count() > 0 {
        return Some(format!(
            "предусловие: AeroSpace запущен ({AEROSPACE_BUNDLE_ID}), выключить его может только владелец"
        ));
    }
    let Some(requested) = requested else {
        return Some("запрошенная рамка окна не записана".to_owned());
    };
    frame_mismatch(window.contentRectForFrameRect(window.frame()), requested)
}

/// Предусловие `calibrate` из плана: встроенный дисплей 120 Гц (ProMotion) подключён и включён, и
/// окно стоит на нём. При закрытой крышке встроенного дисплея в списке нет, и окно уходит на
/// главный дисплей.
fn refresh_violation(displays: &[DisplayInfo], window: Option<&WindowInfo>) -> Option<String> {
    let listed = displays
        .iter()
        .map(|display| format!("«{}» до {} Гц", display.name, display.max_fps))
        .collect::<Vec<_>>()
        .join(", ");
    let Some(builtin) = displays.iter().find(|display| display.builtin) else {
        return Some(format!(
            "предусловие: встроенный дисплей не подключён или выключен (крышка закрыта?), дисплеи: {listed}"
        ));
    };
    if builtin.max_fps < REQUIRED_FPS {
        return Some(format!(
            "предусловие: встроенный дисплей «{}» до {} Гц, нужен {REQUIRED_FPS} Гц",
            builtin.name, builtin.max_fps
        ));
    }
    let on = window.and_then(|window| window.display_id);
    if on != Some(builtin.id) {
        return Some(format!(
            "предусловие: окно на дисплее {on:?}, а не на встроенном {}",
            builtin.id
        ));
    }
    None
}

fn main_window(mtm: MainThreadMarker, screen: &NSScreen, title: &str) -> Retained<NSWindow> {
    let style = NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::Miniaturizable
        | NSWindowStyleMask::Resizable;
    // SAFETY: обычная инициализация окна с существующим экраном.
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer_screen(
            NSWindow::alloc(mtm),
            window_frame(screen, WINDOW_SIZE),
            style,
            NSBackingStoreType::Buffered,
            false,
            Some(screen),
        )
    };
    // SAFETY: окно держит Rust, поэтому AppKit не должен освобождать его при закрытии.
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&NSString::from_str(title));
    window
}

fn demo_title(view: ViewOptions) -> String {
    format!(
        "{PROBE_NAME} — вариант {}, {} (каркас)",
        view.variant.unwrap_or(Variant::A).name(),
        view.mode.unwrap_or(ViewMode::Unified).name()
    )
}

struct DelegateIvars {
    scenario: Option<Scenario>,
    view: ViewOptions,
    /// Каталог процесса сценария: в нём `calibrate` пишет трассы отрезков.
    out: Option<PathBuf>,
    /// Каталог фикстур с эталонами подсветки сценариев открытия.
    fixtures: Option<PathBuf>,
    slot: Rc<RefCell<Option<Outcome>>>,
    window: OnceCell<Retained<NSWindow>>,
    timer: RefCell<Option<Retained<NSTimer>>>,
    /// Конец `applicationDidFinishLaunching`: начало ожидания готового окна.
    shown_at: OnceCell<Instant>,
    /// Запрошенная рамка содержимого окна в глобальных координатах AppKit.
    requested: OnceCell<NSRect>,
    /// Идущий сценарий с окном.
    running: RefCell<Option<Box<dyn scenario::Running>>>,
    /// Кольцо следа сторожа процесса, его же получает стенд событий.
    trace: scenario::watchdog::SharedTrace,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeDiffViewDelegate"]
    #[ivars = DelegateIvars]
    struct AppDelegate;

    unsafe impl NSObjectProtocol for AppDelegate {}

    unsafe impl NSApplicationDelegate for AppDelegate {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _notification: &NSNotification) {
            self.show_window();
        }

        /// Сценарий заканчивается своим итогом: закрытие окна процесс не завершает.
        #[unsafe(method(applicationShouldTerminateAfterLastWindowClosed:))]
        fn should_terminate_after_last_window_closed(&self, _sender: &NSApplication) -> bool {
            self.ivars().scenario.is_none()
        }

        #[unsafe(method(applicationShouldTerminate:))]
        fn should_terminate(&self, _sender: &NSApplication) -> NSApplicationTerminateReply {
            if self.ivars().scenario.is_none() {
                return NSApplicationTerminateReply::TerminateNow;
            }
            // Внешнее завершение до итога сценария — ошибка исполнения.
            self.finish(Outcome::error(
                "процесс завершён извне до конца сценария",
                None,
                Report::default(),
            ));
            NSApplicationTerminateReply::TerminateCancel
        }
    }

    impl AppDelegate {
        #[unsafe(method(pollWindow:))]
        fn poll_window(&self, _timer: &NSTimer) {
            let polled = self.ivars().running.borrow().as_ref().map(|run| run.poll());
            match polled {
                None => self.check_window(),
                Some(None) => {
                    // Сценарий попросил не опрашивать его до конца окна: одно срабатывание вместо
                    // опроса раз в 5 мс (отчёт, строка «Таймер опроса в окнах `idle`»).
                    if let Some(pause) = scenario::take_pause()
                        && let Some(timer) = self.ivars().timer.borrow().as_ref()
                    {
                        timer.setFireDate(&NSDate::dateWithTimeIntervalSinceNow(
                            pause.as_secs_f64(),
                        ));
                    }
                }
                Some(Some(result)) => {
                    drop(self.ivars().running.borrow_mut().take());
                    self.finish(match result {
                        Ok(report) => Outcome::ok(report),
                        Err(failed) => Outcome::error(failed.0, None, failed.1),
                    });
                }
            }
        }
    }
);

impl AppDelegate {
    fn new(
        mtm: MainThreadMarker,
        scenario: Option<Scenario>,
        view: ViewOptions,
        out: Option<PathBuf>,
        fixtures: Option<PathBuf>,
        trace: scenario::watchdog::SharedTrace,
        slot: Rc<RefCell<Option<Outcome>>>,
    ) -> Retained<AppDelegate> {
        let this = AppDelegate::alloc(mtm).set_ivars(DelegateIvars {
            scenario,
            view,
            out,
            fixtures,
            slot,
            window: OnceCell::new(),
            timer: RefCell::new(None),
            shown_at: OnceCell::new(),
            requested: OnceCell::new(),
            running: RefCell::new(None),
            trace,
        });
        // SAFETY: `init` у NSObject без аргументов.
        unsafe { msg_send![super(this), init] }
    }

    fn show_window(&self) {
        let mtm = self.mtm();
        let app = NSApplication::sharedApplication(mtm);
        let ivars = self.ivars();
        let Some(screen) = choose_display(mtm) else {
            self.finish(Outcome::error(
                "предусловие: нет дисплея",
                None,
                Report::default(),
            ));
            return;
        };
        let title = match ivars.scenario {
            Some(scenario) => format!("{PROBE_NAME} — {}", scenario.name()),
            None => demo_title(ivars.view),
        };
        let window = main_window(mtm, &screen, &title);
        let _ = ivars.requested.set(global_rect(
            window_frame(&screen, WINDOW_SIZE),
            screen.frame().origin,
        ));
        window.makeKeyAndOrderFront(None);
        // Кооперативный `activate()` (macOS 14) отклоняется, пока активен терминал, из которого
        // запущен процесс; устаревший запрос с игнорированием других приложений проходит.
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
        let _ = ivars.window.set(window);
        if ivars.scenario.is_none() {
            return;
        }
        let _ = ivars.shown_at.set(Instant::now());
        // SAFETY: цель — этот делегат, он отвечает на `pollWindow:` с аргументом-таймером; таймер
        // держит делегат до `invalidate` в `finish`.
        let timer = unsafe {
            NSTimer::timerWithTimeInterval_target_selector_userInfo_repeats(
                POLL_INTERVAL_S,
                self as &AnyObject,
                sel!(pollWindow:),
                None,
                true,
            )
        };
        // SAFETY: `NSRunLoopCommonModes` — неизменяемая константа Foundation.
        unsafe { NSRunLoop::currentRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };
        *ivars.timer.borrow_mut() = Some(timer);
    }

    /// Окно готово, когда приложение активно, окно видимо и key и ничего не ждёт отображения.
    fn check_window(&self) {
        let ivars = self.ivars();
        let app = NSApplication::sharedApplication(self.mtm());
        let Some(window) = ivars.window.get() else {
            return;
        };
        let waited = ivars
            .shown_at
            .get()
            .map_or(Duration::ZERO, Instant::elapsed);
        let state = format!(
            "isActive={} isVisible={} isKeyWindow={} viewsNeedDisplay={}",
            app.isActive(),
            window.isVisible(),
            window.isKeyWindow(),
            window.viewsNeedDisplay()
        );
        let ready = app.isActive()
            && window.isVisible()
            && window.isKeyWindow()
            && !window.viewsNeedDisplay();
        if ready {
            let mut report = Report::default();
            let waited_ms = waited.as_secs_f64() * 1e3;
            report.metric("window_ready_ms", "ms", vec![waited_ms]);
            report.finding(
                "window_ready",
                Verdict::Pass,
                format!("приложение активно, окно видимо и key, отображение не ждёт через {waited_ms:.1} мс"),
            );
            report.push("window_state", &state);
            report.displays = display_infos(self.mtm());
            report.window = Some(window_info(window));
            let builtin = report.displays.iter().find(|display| display.builtin);
            report.push(
                "builtin_display",
                builtin.map_or_else(
                    || "нет встроенного дисплея".to_owned(),
                    |display| format!("{}, до {} Гц", display.name, display.max_fps),
                ),
            );
            let requested = ivars.requested.get().copied();
            if let Some(reason) = precondition_violation(window, requested) {
                self.finish(Outcome::error(reason, Some(state), report));
                return;
            }
            let Some(out) = ivars.out.as_deref() else {
                self.finish(Outcome::error("сценарий без --out", None, report));
                return;
            };
            match ivars.scenario {
                Some(Scenario::Calibrate) => {
                    if let Some(reason) =
                        refresh_violation(&report.displays, report.window.as_ref())
                    {
                        self.finish(Outcome::error(reason, Some(state), report));
                        return;
                    }
                    match scenario::calibrate_run::Calibration::start(
                        self.mtm(),
                        window,
                        report,
                        out,
                    ) {
                        Ok(run) => *ivars.running.borrow_mut() = Some(Box::new(run)),
                        Err(failed) => self.finish(Outcome::error(failed.0, None, failed.1)),
                    }
                }
                Some(Scenario::Smoke) => match scenario::smoke::Smoke::start(
                    self.mtm(),
                    window,
                    report,
                    out,
                    ivars.trace.clone(),
                ) {
                    Ok(run) => *ivars.running.borrow_mut() = Some(Box::new(run)),
                    Err(failed) => self.finish(Outcome::error(failed.0, None, failed.1)),
                },
                Some(Scenario::OpenSmoke) => match scenario::open_smoke::OpenSmoke::start(
                    self.mtm(),
                    window,
                    report,
                    ivars.trace.clone(),
                    ivars.view,
                ) {
                    Ok(run) => *ivars.running.borrow_mut() = Some(Box::new(run)),
                    Err(failed) => self.finish(Outcome::error(failed.0, None, failed.1)),
                },
                Some(Scenario::Ax) => match scenario::ax::Ax::start(
                    self.mtm(),
                    window,
                    report,
                    ivars.trace.clone(),
                    ivars.view,
                    out,
                ) {
                    Ok(run) => *ivars.running.borrow_mut() = Some(Box::new(run)),
                    Err(failed) => self.finish(Outcome::error(failed.0, None, failed.1)),
                },
                Some(scenario @ (Scenario::Open | Scenario::OpenWarm)) => {
                    match scenario::open::Open::start(
                        self.mtm(),
                        window,
                        report,
                        ivars.trace.clone(),
                        ivars.view,
                        ivars.fixtures.as_deref(),
                        scenario == Scenario::OpenWarm,
                    ) {
                        Ok(run) => *ivars.running.borrow_mut() = Some(Box::new(run)),
                        Err(failed) => self.finish(Outcome::error(failed.0, None, failed.1)),
                    }
                }
                Some(Scenario::Frames) => match scenario::frames_run::FramesRun::start(
                    self.mtm(),
                    window,
                    report,
                    ivars.trace.clone(),
                    ivars.view,
                ) {
                    Ok(run) => *ivars.running.borrow_mut() = Some(Box::new(run)),
                    Err(failed) => self.finish(Outcome::error(failed.0, None, failed.1)),
                },
                Some(Scenario::Idle) => match scenario::idle_run::IdleRun::start(
                    self.mtm(),
                    window,
                    report,
                    ivars.trace.clone(),
                    ivars.view,
                ) {
                    Ok(run) => *ivars.running.borrow_mut() = Some(Box::new(run)),
                    Err(failed) => self.finish(Outcome::error(failed.0, None, failed.1)),
                },
                Some(Scenario::Keys) => match scenario::keys_run::KeysRun::start(
                    self.mtm(),
                    window,
                    report,
                    ivars.trace.clone(),
                    ivars.view,
                ) {
                    Ok(run) => *ivars.running.borrow_mut() = Some(Box::new(run)),
                    Err(failed) => self.finish(Outcome::error(failed.0, None, failed.1)),
                },
                Some(Scenario::Memory) => match scenario::memory_run::MemoryRun::start(
                    self.mtm(),
                    window,
                    report,
                    ivars.trace.clone(),
                    ivars.view,
                ) {
                    Ok(run) => *ivars.running.borrow_mut() = Some(Box::new(run)),
                    Err(failed) => self.finish(Outcome::error(failed.0, None, failed.1)),
                },
                Some(Scenario::Highlight) | None => self.finish(Outcome::ok(report)),
            }
        } else if waited >= SMOKE_TIMEOUT {
            self.finish(Outcome::error(
                format!("окно не готово за {} мс", SMOKE_TIMEOUT.as_millis()),
                Some(state),
                Report::default(),
            ));
        }
    }

    /// Первый итог остаётся, остальные отбрасываются. Цикл останавливается служебным событием:
    /// `stop:` срабатывает после очередного события, а таймер событием не является.
    fn finish(&self, outcome: Outcome) {
        let ivars = self.ivars();
        {
            let mut slot = ivars.slot.borrow_mut();
            if slot.is_some() {
                return;
            }
            *slot = Some(outcome);
        }
        if let Some(timer) = ivars.timer.borrow_mut().take() {
            timer.invalidate();
        }
        let app = NSApplication::sharedApplication(self.mtm());
        app.stop(None);
        let wake = NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
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
        if let Some(wake) = wake {
            app.postEvent_atStart(&wake, false);
        }
    }
}

fn rect(rect: NSRect) -> Rect {
    Rect {
        x: rect.origin.x,
        y: rect.origin.y,
        width: rect.size.width,
        height: rect.size.height,
    }
}

fn display_infos(mtm: MainThreadMarker) -> Vec<DisplayInfo> {
    NSScreen::screens(mtm)
        .iter()
        .map(|screen| {
            let id = screen.CGDirectDisplayID();
            DisplayInfo {
                id,
                name: screen.localizedName().to_string(),
                builtin: CGDisplayIsBuiltin(id),
                frame: rect(screen.frame()),
                visible_frame: rect(screen.visibleFrame()),
                scale: screen.backingScaleFactor(),
                max_fps: screen.maximumFramesPerSecond(),
            }
        })
        .collect()
}

fn window_info(window: &NSWindow) -> WindowInfo {
    let content = window.contentLayoutRect();
    WindowInfo {
        display_id: window.screen().map(|screen| screen.CGDirectDisplayID()),
        frame: rect(window.frame()),
        content_points: Size {
            width: content.size.width,
            height: content.size.height,
        },
        backing_scale: window.backingScaleFactor(),
    }
}

/// Постоянная часть окружения процесса; дисплеи и окно добавляет сценарий.
fn static_environment() -> Environment {
    Environment {
        os: NSProcessInfo::processInfo()
            .operatingSystemVersionString()
            .to_string(),
        hardware: Hardware {
            model: sysctl_string("hw.model"),
            cpu: sysctl_string("machdep.cpu.brand_string"),
            memory_bytes: sysctl_u64("hw.memsize"),
        },
        build: Build::from_env(|name| std::env::var(name).ok()),
        displays: Vec::new(),
        window: None,
    }
}

fn sysctl_raw(name: &str) -> Option<Vec<u8>> {
    let name = CString::new(name).ok()?;
    let mut len = 0usize;
    // SAFETY: первый вызов узнаёт размер значения, буфер не передаётся.
    let status = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if status != 0 || len == 0 {
        return None;
    }
    let mut buffer = vec![0u8; len];
    // SAFETY: буфер длины `len` жив на время вызова.
    let status = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            buffer.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if status != 0 {
        return None;
    }
    buffer.truncate(len);
    Some(buffer)
}

fn sysctl_string(name: &str) -> Option<String> {
    let bytes = sysctl_raw(name)?;
    let text = bytes.split(|byte| *byte == 0).next().unwrap_or_default();
    Some(String::from_utf8_lossy(text).trim().to_owned()).filter(|text| !text.is_empty())
}

fn sysctl_u64(name: &str) -> Option<u64> {
    let bytes: [u8; 8] = sysctl_raw(name)?.try_into().ok()?;
    Some(u64::from_ne_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grammar_loads_into_parser() {
        let reason = check_grammar().unwrap();
        assert!(reason.contains("source_file"), "{reason}");
    }

    #[test]
    fn requested_frame_is_global_and_exact() {
        let local = NSRect::new(NSPoint::new(10.0, 20.0), WINDOW_SIZE);
        let requested = global_rect(local, NSPoint::new(-1512.0, 300.0));
        assert_eq!(requested.origin, NSPoint::new(-1502.0, 320.0));
        assert_eq!(requested.size, WINDOW_SIZE);
        assert_eq!(frame_mismatch(requested, requested), None);
        // Плитка AeroSpace итерации 7: 750 × 948 точек вместо 1180 × 720.
        let tiled = NSRect::new(NSPoint::new(8.0, 8.0), NSSize::new(750.0, 948.0));
        let reason = frame_mismatch(tiled, requested).unwrap_or_default();
        assert!(reason.starts_with("предусловие: "), "{reason}");
        assert!(reason.contains("8,8 750x948"), "{reason}");
        assert!(reason.contains("-1502,320 1180x720"), "{reason}");
        let shifted = NSRect::new(NSPoint::new(-1501.0, 320.0), WINDOW_SIZE);
        assert!(frame_mismatch(shifted, requested).is_some());
    }

    fn display(id: u32, name: &str, builtin: bool, max_fps: isize) -> DisplayInfo {
        let rect = Rect {
            x: 0.0,
            y: 0.0,
            width: 1512.0,
            height: 982.0,
        };
        DisplayInfo {
            id,
            name: name.to_owned(),
            builtin,
            frame: rect,
            visible_frame: rect,
            scale: 2.0,
            max_fps,
        }
    }

    fn window_on(display_id: u32) -> WindowInfo {
        WindowInfo {
            display_id: Some(display_id),
            frame: Rect {
                x: 0.0,
                y: 0.0,
                width: 1180.0,
                height: 752.0,
            },
            content_points: Size {
                width: 1180.0,
                height: 720.0,
            },
            backing_scale: 2.0,
        }
    }

    #[test]
    fn calibrate_needs_the_builtin_120_hz_display() {
        let builtin = display(1, "Built-in Retina Display", true, 120);
        let external = display(3, "Redmi 27 NU", false, 60);
        assert_eq!(
            refresh_violation(&[builtin.clone(), external.clone()], Some(&window_on(1))),
            None
        );
        // Прогон 20261119-102151-33264: крышка закрыта, окно на внешнем дисплее 60 Гц.
        let closed = refresh_violation(std::slice::from_ref(&external), Some(&window_on(3)))
            .unwrap_or_default();
        assert!(closed.starts_with("предусловие: "), "{closed}");
        assert!(closed.contains("«Redmi 27 NU» до 60 Гц"), "{closed}");
        let slow = display(1, "Built-in Retina Display", true, 60);
        assert!(refresh_violation(&[slow], Some(&window_on(1))).is_some());
        assert!(refresh_violation(&[builtin.clone(), external], Some(&window_on(3))).is_some());
        assert!(refresh_violation(&[builtin], None).is_some());
    }

    #[test]
    fn demo_title_uses_defaults() {
        assert_eq!(
            demo_title(ViewOptions::default()),
            "probe-diff-view — вариант a, unified (каркас)"
        );
        assert_eq!(
            demo_title(ViewOptions {
                variant: Some(Variant::B),
                mode: Some(ViewMode::Side),
                fixture: None,
            }),
            "probe-diff-view — вариант b, side (каркас)"
        );
    }

    #[test]
    fn outcome_render_and_code() {
        let mut report = Report::default();
        report.finding("window_ready", Verdict::Pass, "готово");
        let ok = Outcome::ok(report);
        assert_eq!(
            ok.render(),
            "execution=ok\nfinding=pass window_ready: готово\n"
        );
        assert_eq!(ok.exit_code(), ExitCode::SUCCESS);
        let error = Outcome::error(
            "тайм-аут",
            Some("isActive=false".to_owned()),
            Report::default(),
        );
        assert_eq!(
            error.render(),
            "execution=error\nreason=тайм-аут\nlast_state=isActive=false\n"
        );
        assert_eq!(error.exit_code(), ExitCode::from(EXIT_EXECUTION));
    }

    #[test]
    fn sysctl_reads_known_keys() {
        assert!(sysctl_string("hw.model").is_some());
        assert!(sysctl_u64("hw.memsize").is_some_and(|bytes| bytes > 0));
        assert_eq!(sysctl_string("no.such.key"), None);
    }
}
