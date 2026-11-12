//! Сценарий `launch`: исполняемый файл без бандла, запущенный из терминала, получает
//! активность и key window без вмешательства стенда.

use std::rc::Rc;
use std::time::Duration;

use objc2::rc::Retained;
use objc2_app_kit::NSWindow;
use objc2_core_graphics::CGDisplayIsBuiltin;

use super::{Poll, Report, Scenario, Stand, Verdict, wait_until};

const TIMEOUT: Duration = Duration::from_secs(5);

pub fn run(stand: Rc<Stand>) {
    let mut report = stand.report(Scenario::Launch);
    if let Err(reason) = stand.check_session(&mut report) {
        return stand.conclude(report, Err(reason));
    }

    let observed = Rc::clone(&stand);
    wait_until(
        TIMEOUT,
        move || {
            let focus = Focus::read(&observed);
            if focus.active && focus.key == WindowRef::Main {
                Poll::Ready(focus)
            } else {
                Poll::Pending(focus.describe())
            }
        },
        move |result| match result {
            Ok((focus, waited)) => {
                let wait_ms = waited.as_secs_f64() * 1e3;
                report.push("wait_ms", format!("{wait_ms:.1}"));
                report.metric("activation_wait", "ms", vec![wait_ms]);
                report.finding(
                    "activation",
                    Verdict::Pass,
                    format!(
                        "isActive и key window — основное окно через {wait_ms:.1} мс после показа, способ: {}",
                        stand.activation
                    ),
                );
                focus.add_to(&mut report);
                display_to(&stand, &mut report);
                match input_source(&stand.window) {
                    Some(source) => {
                        report.push("input_source", source);
                        stand.conclude(report, Ok(()));
                    }
                    None => stand.conclude(
                        report,
                        Err("предусловие: источник ввода не прочитан".to_owned()),
                    ),
                }
            }
            Err(timeout) => {
                report.push(
                    "wait_ms",
                    format!("{:.1}", timeout.waited.as_secs_f64() * 1e3),
                );
                display_to(&stand, &mut report);
                stand.conclude(
                    report,
                    Err(format!(
                        "нет активности и key window за {} мс; последнее состояние: {}",
                        TIMEOUT.as_millis(),
                        timeout.last_state
                    )),
                );
            }
        },
    );
}

#[derive(PartialEq, Eq)]
enum WindowRef {
    None,
    Main,
    Other(isize),
}

impl WindowRef {
    fn of(window: Option<Retained<NSWindow>>, main: &NSWindow) -> WindowRef {
        match window {
            None => WindowRef::None,
            Some(window) if std::ptr::eq(&*window, main) => WindowRef::Main,
            Some(window) => WindowRef::Other(window.windowNumber()),
        }
    }

    fn describe(&self) -> String {
        match self {
            WindowRef::None => "none".to_owned(),
            WindowRef::Main => "main".to_owned(),
            WindowRef::Other(number) => format!("other#{number}"),
        }
    }
}

/// Фактическое состояние активности, прочитанное у AppKit.
struct Focus {
    active: bool,
    key: WindowRef,
    main: WindowRef,
}

impl Focus {
    fn read(stand: &Stand) -> Focus {
        Focus {
            active: stand.app.isActive(),
            key: WindowRef::of(stand.app.keyWindow(), &stand.window),
            main: WindowRef::of(stand.app.mainWindow(), &stand.window),
        }
    }

    fn describe(&self) -> String {
        format!(
            "isActive={} keyWindow={} mainWindow={}",
            self.active,
            self.key.describe(),
            self.main.describe()
        )
    }

    fn add_to(&self, report: &mut Report) {
        report.push("is_active", self.active);
        report.push("key_window_is_main", self.key == WindowRef::Main);
        report.push("key_window", self.key.describe());
        report.push("main_window", self.main.describe());
    }
}

fn display_to(stand: &Stand, report: &mut Report) {
    let window = &stand.window;
    report.push("backing_scale_factor", window.backingScaleFactor());
    if let Some(view) = window.contentView() {
        let points = view.bounds().size;
        let pixels = view.convertRectToBacking(view.bounds()).size;
        report.push(
            "content_points",
            format!("{}x{}", points.width, points.height),
        );
        report.push(
            "bitmap_pixels",
            format!("{}x{}", pixels.width, pixels.height),
        );
    }
    report.push(
        "display_chosen",
        if stand.display.builtin {
            "builtin"
        } else {
            "main"
        },
    );
    let Some(screen) = window.screen() else {
        report.push("display", "none");
        return;
    };
    let id = screen.CGDirectDisplayID();
    let frame = screen.frame();
    report.push("display_id", id);
    report.push("display_name", screen.localizedName());
    report.push("display_builtin", CGDisplayIsBuiltin(id));
    report.push(
        "display_frame",
        format!(
            "{},{} {}x{}",
            frame.origin.x, frame.origin.y, frame.size.width, frame.size.height
        ),
    );
    report.push("display_scale", screen.backingScaleFactor());
    report.push("display_max_fps", screen.maximumFramesPerSecond());
    report.push(
        "display_is_chosen",
        id == stand.display.screen.CGDirectDisplayID(),
    );
}

/// Источник ввода по field editor окна: у вида без `NSTextInputClient` контекста ввода нет.
pub fn input_source(window: &NSWindow) -> Option<String> {
    // SAFETY: `nil` вместо объекта — документированный запрос общего field editor окна.
    let editor = unsafe { window.fieldEditor_forObject(true, None) }?;
    let source = editor.inputContext()?.selectedKeyboardInputSource()?;
    Some(source.to_string())
}
