//! Сценарий `palette`: палитра поверх поверхности, скругление плитки и живая смена темы (H6, H1,
//! H2) на обвязке `input` — A в центральной зоне с контрастной сеткой фикстуры, B в нижней, обе
//! фикстуры в raw-режиме, проверки с оградой и точными байтами.
//!
//! Клавиши и щелчки идут через `postEvent` и обычную доставку `NSApplication`: палитра стоит в
//! `sendEvent` первой, за ней маршрутизатор режимов. Каждая проверка с оградой кончается
//! закрытой палитрой, режимом `Input` и фокусом A: ограда — обычная клавиша.
//!
//! Пиксельные проверки идут по снимкам композиции экрана (`screencapture -R`) и сравнивают
//! пиксели внутри одного снимка или пары снимков, а не с токенами. Без разрешения Screen
//! Recording они получают `pending-manual`, проверки иерархии, фокуса и байтов — свой итог.
//! Повтор снимка — предикат фактического состояния: кадр на экране отстаёт от модели терминала,
//! поэтому снимок повторяется до ожидаемого пикселя или предела попыток, число попыток
//! записывается.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use objc2::MainThreadOnly;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSAppearanceNameAqua, NSAppearanceNameDarkAqua,
    NSEventModifierFlags, NSEventType, NSTextInputClient, NSView, NSWorkspace,
};
use objc2_foundation::{NSPoint, NSRange, NSRect, NSSize, NSString};
use serde::Serialize;

use super::events::{self, Key};
use super::input::{
    self, A, B, Harness, Input, KEY_A, KEY_B, KEY_ESC, KEY_X, Plan, STATE_TIMEOUT, focused, key,
    only_a,
};
use super::snapshot::{self, Capture, Pending};
use super::surface_env::{self, Extra};
use super::{Poll, Scenario, Stand, poll};
use crate::fixture::{self, Command};
use crate::ghostty::config::{self, ColorScheme};
use crate::ghostty::runtime;
use crate::ghostty::surface::Phase;
use crate::palette::{self, Close};
use crate::results::{Requirement, SnapshotStatus, Verdict};
use crate::router::{self, Cancel, Mode};
use crate::scene::{self, Rgb, Zone};
use crate::utf16;
use crate::view::SurfaceView;

const COMMAND: usize = NSEventModifierFlags::Command.0;
const CONTROL: usize = NSEventModifierFlags::Control.0;

const CMD_K: Key = key("Cmd-K", 40, "k", "k", COMMAND);
const CHORD: Key = key("Ctrl-\\", 42, "\u{1c}", "\\", CONTROL);
const KEY_I: Key = key("i", 34, "i", "i", 0);
const KEY_C: Key = key("c", 8, "c", "c", 0);

/// Цвет углов и маркера возврата: насыщенные, не встречаются в сетке и в хроме.
const MAGENTA: Rgb = Rgb(255, 0, 255);
const GREEN: Rgb = Rgb(0, 200, 0);
/// Допуск совпадения цвета после управления цветом снимка.
const NEAR: u8 = 60;
/// Допуск совпадения двух пикселей одной заливки в одном снимке.
const SAME: u8 = 8;
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(20);
const CAPTURE_ATTEMPTS: u32 = 10;
const READY_TIMEOUT: Duration = Duration::from_secs(12);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(10);
const SCHEME_TIMEOUT: Duration = Duration::from_secs(5);
/// Открытий и закрытий для наблюдения задержки.
const CYCLES: usize = 20;
const COMPOSING: &str = "жу";

pub fn run(stand: Rc<Stand>) {
    input::run_harness(
        stand,
        Harness {
            scenario: Scenario::Palette,
            prefix: "h6",
            notes: &[
                (
                    "route_note",
                    "клавиши и щелчки идут через postEvent и обычную доставку NSApplication; палитра стоит в sendEvent первой, затем маршрутизатор режимов; composition IME раньше обоих",
                ),
                (
                    "pixels_note",
                    "пиксельные проверки — по снимкам композиции screencapture -R, сравнение внутри снимка или пары снимков; снимок повторяется до ожидаемого пикселя или предела попыток, попытки записаны",
                ),
                (
                    "latency_note",
                    "задержка открытия и закрытия палитры — наблюдение: t_post клавиши → конец первого прохода отображения после возврата sendEvent, по определению P0.2; время до фотона не измерено; отрицательный вывод P0.2 по открытию палитры здесь не чинится",
                ),
                (
                    "marked_note",
                    "открытие палитры при marked text — командой (как из меню): composition снимается без коммита; Cmd-K при marked text палитра и маршрутизатор пропускают к composition, что дальше делает вид (привязка ghostty super+k — clear_screen), автоматически не проверено",
                ),
            ],
            build,
        },
    );
}

/// Идентичность поверхности и её ребёнка.
#[derive(Clone, Debug, PartialEq, Serialize)]
struct Identity {
    view: usize,
    handle: usize,
    key: String,
    pid: Option<i32>,
    nonce: Option<String>,
    env_nonce: Option<String>,
    foreground_pid: u64,
    tty: Option<String>,
    closed: bool,
}

#[derive(Default)]
struct State {
    pending: Option<Pending>,
    attempts: u32,
    captures: Vec<(&'static str, Capture, u32)>,
    before: Vec<Identity>,
    after_dark: Vec<Identity>,
    after_light: Vec<Identity>,
    schemes: Vec<runtime::SchemeRecord>,
    /// Точек снимка от сценария; за ними идут точки контроля содержимого.
    user_points: usize,
    updates_mark: u64,
    c: Option<Extra>,
    cycles: Vec<(Vec<u64>, Vec<u64>)>,
    return_point: Option<NSPoint>,
}

type Shared = Rc<RefCell<State>>;

fn build(plan: &mut Plan) {
    let state: Shared = Rc::new(RefCell::new(State::default()));
    install(plan);
    open_close(plan);
    typing(plan);
    parent_keys(plan);
    outside_click(plan);
    chord_wait(plan);
    marked_text(plan);
    latency(plan, &state);
    composition(plan, &state);
    rounding(plan, &state);
    theme(plan, &state);
    conclude(plan, &state);
}

// ---- общие шаги ----

fn both(first: Poll<()>, second: impl FnOnce() -> Poll<()>) -> Poll<()> {
    match first {
        Poll::Ready(()) => second(),
        pending => pending,
    }
}

fn opened() -> Poll<()> {
    let snapshot = palette::snapshot();
    let ready = snapshot.as_ref().is_some_and(|palette| {
        palette.open && palette.panel_visible && palette.panel_key && palette.field_editing
    });
    poll(ready, || format!("палитра: {snapshot:?}"))
}

fn closed_to(stand: &Stand, input: &Input, mode: Mode) -> Poll<()> {
    let snapshot = palette::snapshot();
    let shut = snapshot
        .as_ref()
        .is_some_and(|palette| !palette.open && !palette.panel_visible);
    let current = router::snapshot().map(|router| router.mode);
    both(
        poll(shut && current == Some(mode), || {
            format!("палитра {snapshot:?}, режим {current:?}")
        }),
        || focused(stand, &input.view(A)),
    )
}

fn press(plan: &mut Plan, name: &'static str, key: Key) {
    plan.act(name, move |_, input| {
        input.posted = events::press(key)?;
        Ok(())
    });
}

fn open_by_key(plan: &mut Plan) {
    press(plan, "Cmd-K", CMD_K);
    plan.wait(
        "палитра открыта, key и поле редактируется",
        STATE_TIMEOUT,
        |_, input| both(input.dispatched(), opened),
    );
}

fn close_by_escape(plan: &mut Plan, mode: Mode) {
    press(plan, "Esc в палитре", KEY_ESC);
    plan.wait(
        "палитра закрыта, режим и фокус A",
        STATE_TIMEOUT,
        move |stand, input| both(input.dispatched(), || closed_to(stand, input, mode)),
    );
}

fn last_close() -> Option<palette::CloseRecord> {
    palette::snapshot().and_then(|palette| palette.record.closes.last().cloned())
}

fn verify_open(input: &mut Input) {
    let Some(palette) = palette::snapshot() else {
        input.verify("палитра поставлена".to_owned(), false);
        return;
    };
    input.verify(
        format!("панель {} непрозрачна", palette.panel_class),
        palette.panel_opaque,
    );
    input.verify(
        "панель — дочернее окно основного".to_owned(),
        palette.child_of_parent,
    );
    input.verify(
        format!("панелей создано {}", palette.panels_created),
        palette.panels_created == 1,
    );
    let parent_key = input
        .view(A)
        .window()
        .is_some_and(|window| window.isKeyWindow());
    input.verify("основное окно не key".to_owned(), !parent_key);
    input.verify(
        "A без фокуса ghostty при открытой палитре".to_owned(),
        !input.view(A).effective_focus(),
    );
}

fn verify_closed(stand: &Stand, input: &mut Input, reason: Close) {
    let close = last_close();
    input.verify(
        format!(
            "причина закрытия {:?}",
            close.as_ref().map(|close| close.reason)
        ),
        close.as_ref().is_some_and(|close| close.reason == reason),
    );
    input.verify(
        "first responder возвращён".to_owned(),
        close.as_ref().is_some_and(|close| close.restored)
            && input::is_responder(stand, &input.view(A)),
    );
    input.verify(
        "A с фокусом ghostty после закрытия".to_owned(),
        input.view(A).effective_focus(),
    );
}

// ---- установка ----

fn install(plan: &mut Plan) {
    plan.act("палитра и маршрутизатор", |stand, input| {
        palette::install(&stand.window, Some(Rc::clone(&input.scene)));
        router::install(Rc::clone(&input.scene), &stand.window);
        let workspace = NSWorkspace::sharedWorkspace();
        let contrast = workspace.accessibilityDisplayShouldIncreaseContrast();
        let motion = workspace.accessibilityDisplayShouldReduceMotion();
        input.observe("increase_contrast", contrast);
        input.observe("reduce_motion", motion);
        input.findings.push((
            "h6-accessibility-display".to_owned(),
            Requirement::Required,
            Verdict::Observed,
            format!(
                "Increase Contrast {contrast}, Reduce Motion {motion}: прочитаны у NSWorkspace; анимаций у палитры пробы нет, контраст — токены сцены"
            ),
        ));
        let path = stand.prelude.out.join(config::FILE);
        let text = std::fs::read_to_string(&path)
            .map_err(|err| format!("{}: {err}", path.display()))?;
        let expected = config::conditional_theme(config::LIGHT_THEME, config::DARK_THEME)?;
        let actual = config::theme_value(&text);
        input.findings.push((
            "h1-conditional-theme".to_owned(),
            Requirement::Required,
            if actual.as_deref() == Some(expected.as_str()) {
                Verdict::Pass
            } else {
                Verdict::Fail
            },
            format!(
                "theme в конфигурации {actual:?}, ожидалось {expected}; светлая схема — {}, тёмная — {}",
                config::theme_for(ColorScheme::Light, config::LIGHT_THEME, config::DARK_THEME),
                config::theme_for(ColorScheme::Dark, config::LIGHT_THEME, config::DARK_THEME)
            ),
        ));
        input.send(
            A,
            Command::Grid {
                block_cols: 4,
                block_rows: 2,
            },
        )
    });
    plan.wait(
        "сетка A записана в PTY",
        STATE_TIMEOUT,
        |_, input| input.acked(),
    );
    tag(plan, "сетка A в модели");
}

/// Метка в первой строке A без прокрутки: когда она видна, всё записанное раньше разобрано.
fn tag(plan: &mut Plan, name: &'static str) {
    plan.act(name, |_, input| {
        let text = format!(
            "pt{}",
            input.posted.len() + events::elapsed_ms().unwrap_or(0.0) as usize
        );
        input.observe("last_tag", &text);
        input.send(
            A,
            Command::Print {
                text: format!("\x1b7\x1b[1;1H\x1b[0m{text}\x1b8"),
            },
        )?;
        TAG.with(|cell| *cell.borrow_mut() = text);
        Ok(())
    });
    plan.wait("метка видна у A", STATE_TIMEOUT, |_, input| {
        both(input.acked(), || {
            let tag = TAG.with(|cell| cell.borrow().clone());
            let text = runtime::viewport_text(input.id(A)).unwrap_or_default();
            poll(text.contains(&tag), || format!("метки {tag} нет у A"))
        })
    });
}

thread_local! {
    static TAG: RefCell<String> = const { RefCell::new(String::new()) };
}

// ---- проверки с оградой ----

fn open_close(plan: &mut Plan) {
    plan.begin("open-close", A, |_| only_a(b""));
    open_by_key(plan);
    plan.act(
        "сверка открытой палитры",
        |_, input| {
            verify_open(input);
            let mode = router::snapshot().map(|router| router.mode);
            input.verify(
                format!("режим при открытой палитре {mode:?}"),
                mode == Some(Mode::Input),
            );
            Ok(())
        },
    );
    close_by_escape(plan, Mode::Input);
    plan.act(
        "сверка после закрытия",
        |stand, input| {
            verify_closed(stand, input, Close::Escape);
            Ok(())
        },
    );
    plan.finish();
}

fn typing(plan: &mut Plan) {
    plan.begin("typing-stays-in-palette", A, |_| only_a(b""));
    open_by_key(plan);
    plan.act("печать в палитру", |_, input| {
        input.posted = Vec::new();
        for key in [KEY_A, KEY_B, KEY_C] {
            input.posted.extend(events::press(key)?);
        }
        Ok(())
    });
    plan.wait(
        "текст в поле палитры",
        STATE_TIMEOUT,
        |_, input| {
            both(input.dispatched(), || {
                let text = palette::snapshot().map(|palette| palette.field_text);
                poll(text.as_deref() == Some("abc"), || format!("поле {text:?}"))
            })
        },
    );
    close_by_escape(plan, Mode::Input);
    plan.act(
        "сверка текста закрытия",
        |stand, input| {
            let close = last_close();
            input.verify(
                format!(
                    "поле при закрытии {:?}",
                    close.as_ref().map(|close| close.text.clone())
                ),
                close.as_ref().is_some_and(|close| close.text == "abc"),
            );
            verify_closed(stand, input, Close::Escape);
            Ok(())
        },
    );
    plan.finish();
}

fn parent_keys(plan: &mut Plan) {
    plan.begin("parent-keys-swallowed", A, |_| only_a(b""));
    open_by_key(plan);
    plan.act(
        "клавиша, адресованная основному окну",
        |stand, input| {
            let before =
                palette::snapshot().map_or(0, |palette| palette.record.parent_keys_swallowed);
            input.observe("parent_keys_before", before);
            input.posted = events::press_in(KEY_X, &stand.window)?;
            Ok(())
        },
    );
    plan.wait(
        "клавиша вернулась из sendEvent",
        STATE_TIMEOUT,
        |_, input| input.dispatched(),
    );
    plan.act(
        "сверка съеденной клавиши",
        |_, input| {
            let palette = palette::snapshot();
            let swallowed = palette
                .as_ref()
                .map_or(0, |palette| palette.record.parent_keys_swallowed);
            input.verify(
                format!("съедено нажатий родителя {swallowed}"),
                swallowed >= 1,
            );
            let text = palette.map(|palette| palette.field_text);
            input.verify(format!("поле {text:?}"), text.as_deref() == Some(""));
            Ok(())
        },
    );
    close_by_escape(plan, Mode::Input);
    plan.finish();
}

fn outside_click(plan: &mut Plan) {
    plan.begin("outside-click", A, |_| only_a(b""));
    open_by_key(plan);
    plan.act("щелчок в A вне палитры", |stand, input| {
        let view = input.view(A);
        let before = view.input_log().mouse_buttons;
        input.observe("mouse_buttons_before_click", before);
        let size = view.bounds().size;
        let point = input::screen_point(&view, 40.0, size.height - 30.0)?;
        input.posted = vec![
            events::post_mouse(
                NSEventType::LeftMouseDown,
                &stand.window,
                point,
                "щелчок наружу",
            )?,
            events::post_mouse(
                NSEventType::LeftMouseUp,
                &stand.window,
                point,
                "щелчок наружу",
            )?,
        ];
        Ok(())
    });
    plan.wait(
        "палитра закрыта щелчком",
        STATE_TIMEOUT,
        |stand, input| both(input.dispatched(), || closed_to(stand, input, Mode::Input)),
    );
    plan.act("сверка щелчка", |stand, input| {
        verify_closed(stand, input, Close::OutsideClick);
        let buttons = input.view(A).input_log().mouse_buttons;
        input.observe("mouse_buttons_after_click", buttons);
        input.note(format!("кнопок мыши у A после щелчка {buttons}"));
        Ok(())
    });
    plan.finish();
}

fn chord_wait(plan: &mut Plan) {
    plan.begin("overlay-cancels-chord-wait", A, |_| only_a(b""));
    press(plan, "первый Ctrl-\\", CHORD);
    plan.wait(
        "ожидание второго аккорда",
        STATE_TIMEOUT,
        |_, input| {
            both(input.dispatched(), || {
                let router = router::snapshot();
                poll(
                    router
                        .as_ref()
                        .is_some_and(|router| router.waiting && router.mode == Mode::Navigation),
                    || {
                        format!(
                            "маршрутизатор {:?}",
                            router.map(|router| (router.mode, router.waiting))
                        )
                    },
                )
            })
        },
    );
    open_by_key(plan);
    plan.act(
        "сверка снятого ожидания",
        |_, input| {
            let router = router::snapshot();
            let cancel = router
                .as_ref()
                .and_then(|router| router.transitions.iter().rev().find_map(|step| step.cancel));
            input.verify(
                format!("последняя отмена {cancel:?}"),
                cancel == Some(Cancel::Overlay),
            );
            let waiting = router.as_ref().is_some_and(|router| router.waiting);
            input.verify("ожидания нет".to_owned(), !waiting);
            verify_open(input);
            Ok(())
        },
    );
    close_by_escape(plan, Mode::Navigation);
    press(plan, "i — Input", KEY_I);
    plan.wait(
        "режим Input и фокус A",
        STATE_TIMEOUT,
        |stand, input| {
            both(input.dispatched(), || {
                let mode = router::snapshot().map(|router| router.mode);
                both(
                    poll(mode == Some(Mode::Input), || format!("режим {mode:?}")),
                    || focused(stand, &input.view(A)),
                )
            })
        },
    );
    plan.finish();
}

fn set_marked(input: &Input, text: &str) {
    let view = input.view(A);
    let string = NSString::from_str(text);
    let string: &AnyObject = string.as_ref();
    let end = utf16::len(text);
    // SAFETY: строка — `NSString`, как требует протокол.
    unsafe {
        view.setMarkedText_selectedRange_replacementRange(
            string,
            NSRange::new(end, 0),
            NSRange::new(usize::MAX >> 1, 0),
        );
    }
}

fn marked_text(plan: &mut Plan) {
    plan.begin("open-with-marked-text", A, |_| only_a(b""));
    plan.act(
        "marked text у A и открытие командой",
        |_, input| {
            set_marked(input, COMPOSING);
            input.verify("A с marked text".to_owned(), input.view(A).is_composing());
            palette::open("command")
        },
    );
    plan.wait("палитра открыта", STATE_TIMEOUT, |_, _| {
        opened()
    });
    plan.act("сверка снятой composition", |_, input| {
        let open = palette::snapshot().and_then(|palette| palette.record.opens.last().cloned());
        input.verify(
            "composition снята при открытии".to_owned(),
            open.as_ref().is_some_and(|open| open.composition_discarded),
        );
        input.verify("marked text нет".to_owned(), !input.view(A).is_composing());
        Ok(())
    });
    close_by_escape(plan, Mode::Input);
    plan.act(
        "сверка после закрытия",
        |stand, input| {
            verify_closed(stand, input, Close::Escape);
            input.verify(
                "позднего marked text нет".to_owned(),
                !input.view(A).is_composing(),
            );
            Ok(())
        },
    );
    plan.finish();
}

// ---- задержка ----

fn latency(plan: &mut Plan, state: &Shared) {
    plan.begin("open-close-cycles", A, |_| only_a(b""));
    for _ in 0..CYCLES {
        let opening = Rc::clone(state);
        plan.act("Cmd-K цикла", move |_, input| {
            input.posted = events::press(CMD_K)?;
            opening
                .borrow_mut()
                .cycles
                .push((input.posted.clone(), Vec::new()));
            Ok(())
        });
        plan.wait(
            "палитра открыта",
            STATE_TIMEOUT,
            |_, input| both(input.dispatched(), opened),
        );
        let closing = Rc::clone(state);
        plan.act("Esc цикла", move |_, input| {
            input.posted = events::press(KEY_ESC)?;
            if let Some(cycle) = closing.borrow_mut().cycles.last_mut() {
                cycle.1 = input.posted.clone();
            }
            Ok(())
        });
        plan.wait(
            "палитра закрыта",
            STATE_TIMEOUT,
            |stand, input| both(input.dispatched(), || closed_to(stand, input, Mode::Input)),
        );
    }
    let judged = Rc::clone(state);
    plan.act("задержка открытия и закрытия", move |_, input| {
        let state = judged.borrow();
        let interval = |numbers: &[u64], index: usize| -> Option<f64> {
            let record = events::record(*numbers.get(index)?)?;
            let pass = events::display_pass_after(record.send_return?)?;
            Some(crate::clock::interval_ms(record.t_post, pass))
        };
        let open: Vec<f64> = state
            .cycles
            .iter()
            .filter_map(|(open, _)| interval(open, 1))
            .collect();
        let close: Vec<f64> = state
            .cycles
            .iter()
            .filter_map(|(_, close)| interval(close, 0))
            .collect();
        let records = palette::snapshot().map(|palette| palette.record);
        let open_sync: Vec<f64> = records
            .as_ref()
            .map(|record| record.opens.iter().map(|open| open.sync_ms).collect())
            .unwrap_or_default();
        let close_sync: Vec<f64> = records
            .as_ref()
            .map(|record| record.closes.iter().map(|close| close.sync_ms).collect())
            .unwrap_or_default();
        input.verify(
            format!("циклов с задержкой открытия {} и закрытия {} из {CYCLES}", open.len(), close.len()),
            open.len() == CYCLES && close.len() == CYCLES,
        );
        let summary = |values: &[f64]| match crate::stats::summarize(values) {
            Ok(summary) => format!(
                "p50 {:.2}, p95 {:.2}, max {:.2} мс ({} значений)",
                summary.p50, summary.p95, summary.max, summary.count
            ),
            Err(err) => format!("нет сводки: {err:?}"),
        };
        input.findings.push((
            "h6-palette-latency".to_owned(),
            Requirement::Required,
            Verdict::Observed,
            format!(
                "открытие Cmd-K: {}; закрытие Esc: {}; синхронная часть открытия {}, закрытия {}; бюджет 16 мс не сравнивается: P0.2 уже записал отрицательный вывод по палитре, проба его не чинит",
                summary(&open),
                summary(&close),
                summary(&open_sync),
                summary(&close_sync)
            ),
        ));
        input.metrics.push(("palette_open_to_display_pass".to_owned(), "ms", open));
        input.metrics.push(("palette_close_to_display_pass".to_owned(), "ms", close));
        input.metrics.push(("palette_open_sync".to_owned(), "ms", open_sync));
        input.metrics.push(("palette_close_sync".to_owned(), "ms", close_sync));
        Ok(())
    });
    plan.finish();
}

// ---- снимки ----

/// Рамка вида в координатах экрана.
fn screen_frame(view: &NSView) -> Option<NSRect> {
    let window = view.window()?;
    Some(window.convertRectToScreen(view.convertRect_toView(view.bounds(), None)))
}

fn point(rect: NSRect, dx: f64, dy: f64) -> NSPoint {
    NSPoint::new(rect.origin.x + dx, rect.origin.y + dy)
}

/// Снимок окна с повтором до `accept` или предела попыток. `points` — точки экрана.
fn capture(
    plan: &mut Plan,
    state: &Shared,
    name: &'static str,
    points: impl Fn(&Stand, &mut Input, &State) -> Result<Vec<NSPoint>, String> + 'static,
    accept: impl Fn(&[Option<Rgb>]) -> bool + 'static,
) {
    let starting = Rc::clone(state);
    plan.act(name, move |_, _| {
        let mut state = starting.borrow_mut();
        state.pending = None;
        state.attempts = 0;
        Ok(())
    });
    let polling = Rc::clone(state);
    plan.wait(
        "снимок композиции",
        CAPTURE_TIMEOUT,
        move |stand, input| {
            let mut state = polling.borrow_mut();
            if let Some(mut capture) = state.pending.as_ref().and_then(Pending::take) {
                state.pending = None;
                let controls = capture.samples.split_off(state.user_points.min(capture.samples.len()));
                let shows_window = shows_window(&controls);
                if capture.ok() && !shows_window {
                    capture.snapshot.status = SnapshotStatus::PendingManual;
                    capture.snapshot.reason = Some(format!(
                        "снимок не показывает окно пробы: соседние блоки сетки A {controls:?} неразличимы; Screen Recording не даёт окна (CGPreflightScreenCaptureAccess {})",
                        snapshot::allowed()
                    ));
                }
                let done = !capture.ok()
                    || accept(&capture.samples)
                    || state.attempts >= CAPTURE_ATTEMPTS;
                if done {
                    let attempts = state.attempts;
                    state.captures.push((name, capture, attempts));
                    return Poll::Ready(());
                }
            }
            if state.pending.is_none() {
                let mut points = match points(stand, input, &state) {
                    Ok(points) => points,
                    Err(reason) => return Poll::Pending(format!("точки снимка: {reason}")),
                };
                state.user_points = points.len();
                match control_points(input) {
                    Ok(controls) => points.extend(controls),
                    Err(reason) => return Poll::Pending(format!("контроль снимка: {reason}")),
                }
                state.attempts += 1;
                let area = stand.window.frame();
                state.pending = Some(snapshot::capture(
                    stand.view.mtm(),
                    area,
                    points,
                    &stand.prelude.out,
                    name,
                ));
            }
            Poll::Pending(format!("снимок {name}, попытка {}", state.attempts))
        },
    );
}

/// Контроль содержимого снимка: две клетки соседних блоков сетки A вне палитры, чёрная и белая.
/// Снимок без доступа к окнам (только обои) их не различит.
fn control_points(input: &mut Input) -> Result<Vec<NSPoint>, String> {
    Ok(vec![
        input::cell_point(input, A, 1, 4, 0.5)?,
        input::cell_point(input, A, 5, 4, 0.5)?,
    ])
}

fn shows_window(controls: &[Option<Rgb>]) -> bool {
    match controls {
        [Some(first), Some(second)] => snapshot::distance(*first, *second) > 150,
        _ => false,
    }
}

fn find<'a>(state: &'a State, name: &str) -> Option<&'a (&'static str, Capture, u32)> {
    state
        .captures
        .iter()
        .rev()
        .find(|(captured, _, _)| *captured == name)
}

fn near(sample: Option<Rgb>, color: Rgb, limit: u8) -> bool {
    sample.is_some_and(|sample| snapshot::distance(sample, color) <= limit)
}

/// Итог пиксельной проверки: без разрешения — `pending-manual`.
fn pixel_verdict(captures: &[&Capture], pass: bool) -> Verdict {
    if captures
        .iter()
        .any(|capture| capture.snapshot.status == crate::results::SnapshotStatus::PendingManual)
    {
        Verdict::PendingManual
    } else if pass && captures.iter().all(|capture| capture.ok()) {
        Verdict::Pass
    } else {
        Verdict::Fail
    }
}

// ---- композиция палитры поверх сетки и кадр возврата ----

/// Точки внутри будущей или открытой палитры над A: между полем и подписью.
fn palette_points(stand: &Stand, input: &Input) -> Result<Vec<NSPoint>, String> {
    let content = stand
        .window
        .convertRectToScreen(stand.window.contentLayoutRect());
    let panel = palette::frame_in(content);
    let a = screen_frame(&input.view(A)).ok_or("A вне окна")?;
    let left = panel.origin.x.max(a.origin.x) + 12.0;
    let right = (panel.origin.x + panel.size.width).min(a.origin.x + a.size.width) - 12.0;
    if right <= left {
        return Err("палитра не перекрывает A".to_owned());
    }
    let y = panel.origin.y + panel.size.height * 0.45;
    Ok((0..5)
        .map(|index| NSPoint::new(left + (right - left) * f64::from(index) / 4.0, y))
        .collect())
}

/// Опорный пиксель заливки панели: пустое тело плитки левой зоны той же темы.
fn reference_point(input: &Input) -> Result<NSPoint, String> {
    let tile = input
        .scene
        .tile(Zone::Left, 0)
        .ok_or("нет плитки левой зоны")?;
    let body = screen_frame(&tile.content()).ok_or("плитка вне окна")?;
    Ok(point(body, body.size.width / 2.0, body.size.height / 2.0))
}

fn composition(plan: &mut Plan, state: &Shared) {
    capture(
        plan,
        state,
        "grid-under-palette",
        |stand, input, _| {
            let mut points = palette_points(stand, input)?;
            points.push(reference_point(input)?);
            Ok(points)
        },
        |_| true,
    );
    plan.begin("composition-and-return-frame", A, |_| only_a(b""));
    open_by_key(plan);
    capture(
        plan,
        state,
        "palette-over-grid",
        |stand, input, _| {
            let mut points = palette_points(stand, input)?;
            points.push(reference_point(input)?);
            Ok(points)
        },
        |samples| {
            let reference = samples.last().copied().flatten();
            reference.is_some_and(|reference| {
                samples[..samples.len() - 1]
                    .iter()
                    .all(|sample| near(*sample, reference, SAME))
            })
        },
    );
    let marking = Rc::clone(state);
    plan.act(
        "зелёный маркер под палитрой",
        move |stand, input| {
            let reading = runtime::read(input.id(A)).ok_or("A не живая")?;
            let scale = input.view(A).backing_scale();
            let cell_w = f64::from(reading.size.cell_width_px) / scale;
            let a = screen_frame(&input.view(A)).ok_or("A вне окна")?;
            let content = stand
                .window
                .convertRectToScreen(stand.window.contentLayoutRect());
            let panel = palette::frame_in(content);
            let first =
                (((panel.origin.x.max(a.origin.x) - a.origin.x) / cell_w).ceil() as u16 + 2).max(1);
            let last = ((((panel.origin.x + panel.size.width).min(a.origin.x + a.size.width)
                - a.origin.x)
                / cell_w)
                .floor() as u16)
                .saturating_sub(2)
                .min(reading.size.columns);
            if last <= first {
                return Err(format!("клетки под палитрой {first}…{last}"));
            }
            let mid = (first + last) / 2;
            let point = input::cell_point(input, A, u32::from(mid), 6, 0.5)?;
            let inside = point.x > panel.origin.x
                && point.x < panel.origin.x + panel.size.width
                && point.y > panel.origin.y
                && point.y < panel.origin.y + panel.size.height;
            if !inside {
                return Err(format!("точка маркера {point:?} не под палитрой {panel:?}"));
            }
            marking.borrow_mut().return_point = Some(point);
            input.send(
                A,
                Command::Marker {
                    row: 4,
                    col: first,
                    rows: 6,
                    cols: last - first + 1,
                    rgb: [GREEN.0, GREEN.1, GREEN.2],
                },
            )
        },
    );
    plan.wait(
        "маркер записан в PTY",
        STATE_TIMEOUT,
        |_, input| input.acked(),
    );
    tag(plan, "маркер в модели");
    close_by_escape(plan, Mode::Input);
    capture(
        plan,
        state,
        "return-frame",
        |_, _, state| {
            state
                .return_point
                .map(|point| vec![point])
                .ok_or("нет точки маркера".to_owned())
        },
        |samples| near(samples.first().copied().flatten(), GREEN, NEAR),
    );
    plan.act(
        "сверка после закрытия",
        |stand, input| {
            verify_closed(stand, input, Close::Escape);
            Ok(())
        },
    );
    plan.finish();
}

// ---- скругление ----

/// Углы плитки A: точка за дугой скругления и точка внутри у каждого нижнего угла.
fn corner_points(input: &Input) -> Result<Vec<NSPoint>, String> {
    let tile = input.scene.tile(Zone::Center, 0).ok_or("нет плитки A")?;
    let frame = screen_frame(&tile).ok_or("плитка вне окна")?;
    let right = frame.size.width;
    Ok(vec![
        point(frame, 1.0, 1.0),
        point(frame, 5.0, 5.0),
        point(frame, right - 1.0, 1.0),
        point(frame, right - 5.0, 5.0),
    ])
}

fn set_tile_mask(input: &Input, mask: bool) -> Result<(), String> {
    let tile = input.scene.tile(Zone::Center, 0).ok_or("нет плитки A")?;
    let layer = tile.layer().ok_or("у плитки нет слоя")?;
    layer.setMasksToBounds(mask);
    tile.setNeedsDisplay(true);
    Ok(())
}

fn rounding(plan: &mut Plan, state: &Shared) {
    plan.act("углы A пурпурные", |_, input| {
        let reading = runtime::read(input.id(A)).ok_or("A не живая")?;
        let (rows, columns) = (reading.size.rows, reading.size.columns);
        if rows < 2 || columns < 8 {
            return Err(format!("сетка A {columns}×{rows}"));
        }
        let rgb = [MAGENTA.0, MAGENTA.1, MAGENTA.2];
        input.send(
            A,
            Command::Marker {
                row: rows,
                col: 1,
                rows: 1,
                cols: 4,
                rgb,
            },
        )?;
        Ok(())
    });
    plan.wait(
        "левый угол записан",
        STATE_TIMEOUT,
        |_, input| input.acked(),
    );
    plan.act("правый угол", |_, input| {
        let reading = runtime::read(input.id(A)).ok_or("A не живая")?;
        let (rows, columns) = (reading.size.rows, reading.size.columns);
        let rgb = [MAGENTA.0, MAGENTA.1, MAGENTA.2];
        input.send(
            A,
            Command::Marker {
                row: rows,
                col: columns - 3,
                rows: 1,
                cols: 4,
                rgb,
            },
        )
    });
    plan.wait(
        "правый угол записан",
        STATE_TIMEOUT,
        |_, input| input.acked(),
    );
    tag(plan, "углы в модели");
    plan.act("иерархия слоёв скругления", |_, input| {
        let tile = input.scene.tile(Zone::Center, 0).ok_or("нет плитки A")?;
        let tile_layer = tile.layer().ok_or("у плитки нет слоя")?;
        let view = input.view(A);
        let mut layer = view.layer();
        let mut chain = Vec::new();
        let mut clipped = false;
        while let Some(current) = layer {
            let object: &AnyObject = &current;
            chain.push(format!(
                "{} radius {} masks {}",
                object.class().name().to_string_lossy(),
                current.cornerRadius(),
                current.masksToBounds()
            ));
            if std::ptr::eq(&*current, &*tile_layer)
                && current.masksToBounds()
                && (current.cornerRadius() - scene::RADIUS).abs() < 0.01
            {
                clipped = true;
            }
            layer = current.superlayer();
        }
        input.observe("surface_layer_chain", &chain);
        input.findings.push((
            "h6-rounding-hierarchy".to_owned(),
            Requirement::Required,
            if clipped { Verdict::Pass } else { Verdict::Fail },
            format!(
                "слой плитки со скруглением {} и masksToBounds — предок слоя поверхности: {clipped}; цепочка: {}",
                scene::RADIUS,
                chain.join(" → ")
            ),
        ));
        Ok(())
    });
    capture(
        plan,
        state,
        "corners-masked",
        |_, input, _| corner_points(input),
        |samples| near(samples[1], MAGENTA, NEAR) && near(samples[3], MAGENTA, NEAR),
    );
    plan.act(
        "отрицательный контроль: без маски",
        |_, input| set_tile_mask(input, false),
    );
    capture(
        plan,
        state,
        "corners-unmasked",
        |_, input, _| corner_points(input),
        |samples| near(samples[0], MAGENTA, NEAR) && near(samples[2], MAGENTA, NEAR),
    );
    let judged = Rc::clone(state);
    plan.act("маска плитки возвращена", move |_, input| {
        set_tile_mask(input, true)?;
        let state = judged.borrow();
        let (Some(masked), Some(unmasked)) = (find(&state, "corners-masked"), find(&state, "corners-unmasked")) else {
            return Err("нет снимков углов".to_owned());
        };
        let m = &masked.1.samples;
        let u = &unmasked.1.samples;
        let content = near(m[1], MAGENTA, NEAR) && near(m[3], MAGENTA, NEAR);
        let clipped = !near(m[0], MAGENTA, NEAR) && !near(m[2], MAGENTA, NEAR);
        let control = near(u[0], MAGENTA, NEAR) && near(u[2], MAGENTA, NEAR);
        let verdict = pixel_verdict(&[&masked.1, &unmasked.1], content && clipped && control);
        input.findings.push((
            "h6-rounding-pixels".to_owned(),
            Requirement::Required,
            verdict,
            format!(
                "с маской: углы {:?} и {:?}, внутри {:?} и {:?}; без маски углы {:?} и {:?}; содержимое в углу {content}, угол срезан {clipped}, отрицательный контроль показал квадратный слой {control}; попыток {} и {}",
                m[0], m[2], m[1], m[3], u[0], u[2], masked.2, unmasked.2
            ),
        ));
        Ok(())
    });
}

// ---- тема ----

fn identity(input: &mut Input, which: usize) -> Result<Identity, String> {
    let view = input.view(which);
    let (key, handle) = runtime::current(input.id(which)).ok_or("поверхность не живая")?;
    let reading = runtime::read(input.id(which)).ok_or("поверхность не живая")?;
    let nonce = if which == A {
        input.env.fixture_nonce.clone()
    } else {
        input.b.fixture_nonce.clone()
    };
    input.env.hub.poll();
    let conn = input.env.hub.by_nonce(&nonce);
    let ready = conn.as_ref().and_then(|conn| conn.ready.clone());
    let closed = conn.is_some_and(|conn| conn.closed);
    Ok(Identity {
        view: Retained::as_ptr(&view) as usize,
        handle: handle.addr(),
        key: key.to_string(),
        pid: ready.as_ref().map(|ready| ready.pid),
        nonce: ready.as_ref().map(|ready| ready.nonce.clone()),
        env_nonce: ready.and_then(|ready| ready.env_nonce),
        foreground_pid: reading.foreground_pid,
        tty: reading.tty_name,
        closed,
    })
}

fn b_center(input: &Input) -> Result<Vec<NSPoint>, String> {
    let frame = screen_frame(&input.view(B)).ok_or("B вне окна")?;
    Ok(vec![point(
        frame,
        frame.size.width / 2.0,
        frame.size.height / 2.0,
    )])
}

fn set_scheme(
    stand: &Stand,
    input: &Input,
    state: &Shared,
    scheme: ColorScheme,
) -> Result<(), String> {
    let name = match scheme {
        ColorScheme::Light => unsafe { NSAppearanceNameAqua },
        ColorScheme::Dark => unsafe { NSAppearanceNameDarkAqua },
    };
    let appearance = NSAppearance::appearanceNamed(name).ok_or("нет оформления")?;
    stand.window.setAppearance(Some(&appearance));
    input
        .scene
        .apply_appearance(&stand.window, scheme.is_dark());
    let mark = runtime::stats().map_or(0, |stats| stats.surface_config_updates);
    let record = runtime::set_color_scheme(scheme)?;
    let mut state = state.borrow_mut();
    state.updates_mark = mark;
    state.schemes.push(record);
    Ok(())
}

fn scheme_applied(state: &Shared) -> Poll<()> {
    let mark = state.borrow().updates_mark;
    let updates = runtime::stats().map_or(0, |stats| stats.surface_config_updates);
    poll(updates >= mark + 2, || {
        format!(
            "ghostty_surface_update_config {updates}, ожидалось не меньше {}",
            mark + 2
        )
    })
}

fn theme(plan: &mut Plan, state: &Shared) {
    let before = Rc::clone(state);
    plan.act(
        "идентичность до смены темы",
        move |_, input| {
            let identities = vec![identity(input, A)?, identity(input, B)?];
            before.borrow_mut().before = identities;
            Ok(())
        },
    );
    capture(
        plan,
        state,
        "theme-light",
        |_, input, _| b_center(input),
        |_| true,
    );
    let switching = Rc::clone(state);
    plan.act("тёмная тема", move |stand, input| {
        set_scheme(stand, input, &switching, ColorScheme::Dark)
    });
    let applied = Rc::clone(state);
    plan.wait(
        "мягкая перезагрузка обеих поверхностей",
        SCHEME_TIMEOUT,
        move |_, _| scheme_applied(&applied),
    );
    capture(
        plan,
        state,
        "theme-dark",
        |_, input, _| b_center(input),
        |samples| samples[0].is_some_and(|rgb| snapshot::luma(rgb) < 0.3),
    );
    let after = Rc::clone(state);
    plan.act(
        "идентичность после тёмной темы",
        move |_, input| {
            let identities = vec![identity(input, A)?, identity(input, B)?];
            after.borrow_mut().after_dark = identities;
            Ok(())
        },
    );
    new_surface_after_switch(plan, state);
    let switching = Rc::clone(state);
    plan.act("светлая тема", move |stand, input| {
        set_scheme(stand, input, &switching, ColorScheme::Light)
    });
    let applied = Rc::clone(state);
    plan.wait(
        "мягкая перезагрузка обеих поверхностей",
        SCHEME_TIMEOUT,
        move |_, _| scheme_applied(&applied),
    );
    capture(
        plan,
        state,
        "theme-light-again",
        |_, input, _| b_center(input),
        |samples| samples[0].is_some_and(|rgb| snapshot::luma(rgb) > 0.7),
    );
    let judged = Rc::clone(state);
    plan.act("сверка смены темы", move |_, input| {
        let identities = vec![identity(input, A)?, identity(input, B)?];
        let mut state = judged.borrow_mut();
        state.after_light = identities;
        let same = state.before == state.after_dark && state.before == state.after_light;
        let alive = state.after_light.iter().all(|identity| !identity.closed && identity.pid.is_some());
        input.observe("identity_before", &state.before);
        input.observe("identity_after_light", &state.after_light);
        input.observe("scheme_changes", &state.schemes);
        input.findings.push((
            "h1-live-scheme-identity".to_owned(),
            Requirement::Required,
            if same && alive { Verdict::Pass } else { Verdict::Fail },
            format!(
                "поверхности, ручки, поколения, PID, TTY и nonce command и окружения A и B после смены на тёмную и обратно совпадают: {same}; фикстуры живы {alive}; ghostty_surface_update_config {}",
                runtime::stats().map_or(0, |stats| stats.surface_config_updates)
            ),
        ));
        let light = find(&state, "theme-light").map(|capture| (capture.1.clone(), capture.2));
        let dark = find(&state, "theme-dark").map(|capture| (capture.1.clone(), capture.2));
        let again = find(&state, "theme-light-again").map(|capture| (capture.1.clone(), capture.2));
        let (Some(light), Some(dark), Some(again)) = (light, dark, again) else {
            return Err("нет снимков темы".to_owned());
        };
        let luma = |capture: &Capture| capture.samples[0].map(snapshot::luma);
        let pass = luma(&light.0).is_some_and(|value| value > 0.7)
            && luma(&dark.0).is_some_and(|value| value < 0.3)
            && luma(&again.0).is_some_and(|value| value > 0.7);
        input.findings.push((
            "h6-live-scheme-colors".to_owned(),
            Requirement::Required,
            pixel_verdict(&[&light.0, &dark.0, &again.0], pass),
            format!(
                "фон B на снимке: светлая {:?}, тёмная {:?} (попыток {}), снова светлая {:?} (попыток {})",
                light.0.samples[0], dark.0.samples[0], dark.1, again.0.samples[0], again.1
            ),
        ));
        Ok(())
    });
}

/// Новая поверхность после смены темы получает `command` и окружение (H1).
fn new_surface_after_switch(plan: &mut Plan, state: &Shared) {
    let adding = Rc::clone(state);
    plan.act(
        "поверхность C после смены темы",
        move |stand, input| {
            let view = SurfaceView::new(
                stand.view.mtm(),
                NSRect::new(NSPoint::ZERO, NSSize::new(10.0, 10.0)),
            );
            input.scene.add_tab(Zone::Right, 0, "C", &view)?;
            let c = input.env.add_fixture(stand, "s3", view)?;
            surface_env::create_extra(&c)?;
            adding.borrow_mut().c = Some(c);
            Ok(())
        },
    );
    let waiting = Rc::clone(state);
    plan.wait(
        "готовность фикстуры C",
        READY_TIMEOUT,
        move |_, input| {
            let state = waiting.borrow();
            let Some(c) = state.c.as_ref() else {
                return Poll::Pending("нет C".to_owned());
            };
            input.env.hub.poll();
            let ready = input
                .env
                .hub
                .by_nonce(&c.fixture_nonce)
                .and_then(|conn| conn.ready.clone());
            poll(ready.is_some(), || {
                format!("нет готовности {}", c.fixture_nonce)
            })
        },
    );
    let checking = Rc::clone(state);
    plan.act("сверка C и закрытие", move |_, input| {
        let state = checking.borrow();
        let c = state.c.as_ref().ok_or("нет C")?;
        let ready = input
            .env
            .hub
            .by_nonce(&c.fixture_nonce)
            .and_then(|conn| conn.ready.clone())
            .ok_or("нет готовности C")?;
        let pass = ready.nonce == c.fixture_nonce
            && ready.env_nonce.as_deref() == Some(c.env_nonce.as_str())
            && ready.term.as_deref() == Some("xterm-ghostty");
        input.findings.push((
            "h1-nonce-after-live-scheme".to_owned(),
            Requirement::Required,
            if pass { Verdict::Pass } else { Verdict::Fail },
            format!(
                "поверхность после смены на тёмную: nonce command {} (ожидался {}), {} {:?} (ожидался {}), TERM {:?}",
                ready.nonce,
                c.fixture_nonce,
                fixture::ENV_NONCE,
                ready.env_nonce,
                c.env_nonce,
                ready.term
            ),
        ));
        runtime::request_close(c.id).map(|_| ())
    });
    let closing = Rc::clone(state);
    plan.wait(
        "освобождение C и выход фикстуры",
        CLOSE_TIMEOUT,
        move |_, input| {
            let state = closing.borrow();
            let Some(c) = state.c.as_ref() else {
                return Poll::Ready(());
            };
            input.env.hub.poll();
            let closed = input
                .env
                .hub
                .by_nonce(&c.fixture_nonce)
                .is_some_and(|conn| conn.closed);
            let phase = runtime::phase(c.id);
            poll(phase == Some(Phase::Freed) && closed, || {
                format!("C {phase:?}, сокет закрыт {closed}")
            })
        },
    );
    let removing = Rc::clone(state);
    plan.act("вкладка C снята", move |_, input| {
        if let Some(c) = removing.borrow().c.as_ref() {
            input.scene.take_tab(&c.view)?;
            c.view.removeFromSuperview();
        }
        Ok(())
    });
}

// ---- итог композиции ----

fn conclude(plan: &mut Plan, state: &Shared) {
    let judged = Rc::clone(state);
    plan.act("выводы композиции", move |_, input| {
        let state = judged.borrow();
        let (Some(under), Some(over)) = (find(&state, "grid-under-palette"), find(&state, "palette-over-grid")) else {
            return Err("нет снимков композиции".to_owned());
        };
        let reference = over.1.samples.last().copied().flatten();
        let samples = &over.1.samples[..over.1.samples.len().saturating_sub(1)];
        let opaque = reference.is_some_and(|reference| samples.iter().all(|sample| near(*sample, reference, SAME)));
        let under_reference = under.1.samples.last().copied().flatten();
        let grid_seen = under_reference.is_some_and(|reference| {
            under.1.samples[..under.1.samples.len().saturating_sub(1)]
                .iter()
                .any(|sample| sample.is_some_and(|sample| snapshot::distance(sample, reference) > NEAR))
        });
        input.findings.push((
            "h6-palette-opaque".to_owned(),
            Requirement::Required,
            pixel_verdict(&[&under.1, &over.1], opaque && grid_seen),
            format!(
                "под палитрой без неё: {:?}, опорная заливка {:?}, сетка видна {grid_seen}; с палитрой: {samples:?}, опорная {reference:?}, без пробоя {opaque} (попыток {})",
                under.1.samples, under_reference, over.2
            ),
        ));
        let Some(back) = find(&state, "return-frame") else {
            return Err("нет снимка возврата".to_owned());
        };
        let seen = near(back.1.samples.first().copied().flatten(), GREEN, NEAR);
        input.findings.push((
            "h6-return-frame".to_owned(),
            Requirement::Required,
            pixel_verdict(&[&back.1], seen),
            format!(
                "маркер, нарисованный под открытой палитрой, после закрытия {:?} (ожидался зелёный), попыток {}; первая попытка — первый снимок после возврата первого прохода отображения",
                back.1.samples.first(),
                back.2
            ),
        ));
        input.snapshots = state
            .captures
            .iter()
            .map(|(_, capture, _)| capture.snapshot.clone())
            .collect();
        Ok(())
    });
}
