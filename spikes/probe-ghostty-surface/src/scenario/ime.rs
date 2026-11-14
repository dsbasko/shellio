//! Сценарий `ime`: граница протокола `NSTextInputClient` H3 на обвязке `input` — две поверхности
//! с фикстурами в raw-режиме, проверки с оградой и точными байтами.
//!
//! Методы протокола вызываются у вида поверхности диспетчеризацией Objective-C, как их зовёт
//! метод ввода, и после каждого вызова состояние читается теми же методами протокола
//! (`hasMarkedText`, `markedRange`, `selectedRange`, `attributedSubstringForProposedRange`,
//! `firstRectForCharacterRange`), а также у libghostty: подача `ghostty_surface_preedit` и ширина
//! `ghostty_surface_ime_point`, которая равна ширине preedit в клетках. Клавиши `a` и `Esc` и
//! щелчок идут через обычную доставку `NSApplication`.
//!
//! Настоящий метод ввода здесь не участвует: кириллица, dead keys, настоящая composition и окно
//! кандидатов на экране — ручная приёмка задачи 19.

use std::cell::Cell;
use std::rc::Rc;

use objc2::runtime::AnyObject;
use objc2::sel;
use objc2_app_kit::NSTextInputClient;
use objc2_foundation::{NSRange, NSRect, NSString};
use serde::Serialize;

use super::events;
use super::input::{
    self, A, B, FOCUS_TIMEOUT, Harness, Input, KEY_A, KEY_B, KEY_ESC, KEY_X, Plan, STATE_TIMEOUT,
    click, focused, is_responder, only_a, press_all, view_center,
};
use super::{Poll, Scenario, poll};
use crate::fixture::Command;
use crate::ghostty::runtime;
use crate::keys::NO_PHYSICAL_KEY;
use crate::utf16::{self, Range16};
use crate::view::SurfaceView;

/// Курсор A для проверки окна кандидатов: строка и столбец с единицы.
const CURSOR: (u16, u16) = (5, 10);
/// Marked text с суррогатной парой: «か» — одна единица UTF-16, эмодзи — две.
const MARKED: &str = "か🙂";
const COMPOSING: &str = "жу";
const COMMIT: &str = "かな🙂";

pub fn run(stand: Rc<super::Stand>) {
    input::run_harness(
        stand,
        Harness {
            scenario: Scenario::Ime,
            prefix: "h3",
            notes: &[
                (
                    "protocol_note",
                    "методы NSTextInputClient вызываются у вида диспетчеризацией Objective-C, состояние читается теми же методами протокола и у libghostty после каждого вызова; клавиши a и Esc и щелчок идут через обычную доставку NSApplication",
                ),
                (
                    "manual_note",
                    "кириллица, dead keys, настоящая composition с выбором кандидата и окно кандидатов на экране проверяются вручную в задаче 19",
                ),
            ],
            build,
        },
    );
}

fn build(plan: &mut Plan) {
    cursor(plan);
    empty_preedit(plan);
    marked_text(plan);
    commit_outside(plan);
    commit_in_key_down(plan);
    esc_cancels(plan);
    focus_change(plan);
}

fn to_ns(range: Range16) -> NSRange {
    NSRange::new(range.location, range.length)
}

fn from_ns(range: NSRange) -> Range16 {
    Range16::new(range.location, range.length)
}

fn set_marked(view: &SurfaceView, text: &str, selected: Range16) {
    let string = NSString::from_str(text);
    let string: &AnyObject = string.as_ref();
    // SAFETY: строка — `NSString`, как требует протокол.
    unsafe {
        view.setMarkedText_selectedRange_replacementRange(
            string,
            to_ns(selected),
            to_ns(Range16::NONE),
        )
    };
}

fn insert(view: &SurfaceView, text: &str) {
    let string = NSString::from_str(text);
    let string: &AnyObject = string.as_ref();
    // SAFETY: строка — `NSString`, как требует протокол.
    unsafe { view.insertText_replacementRange(string, to_ns(Range16::NONE)) };
}

fn substring(view: &SurfaceView, range: Range16) -> (Option<String>, Range16) {
    let mut actual = to_ns(Range16::NONE);
    // SAFETY: указатель на локальный `NSRange`, живой на время вызова.
    let text =
        unsafe { view.attributedSubstringForProposedRange_actualRange(to_ns(range), &mut actual) };
    (text.map(|text| text.string().to_string()), from_ns(actual))
}

/// Состояние клиента ввода, прочитанное методами протокола, и состояние libghostty.
#[derive(Debug, Serialize)]
struct Reading {
    has_marked_text: bool,
    marked_range: Range16,
    selected_range: Range16,
    preedit_feeds: usize,
    last_preedit: Option<String>,
    ime_width: Option<f64>,
}

fn read(view: &SurfaceView) -> Reading {
    let preedits = view.feeds().preedits;
    Reading {
        has_marked_text: view.hasMarkedText(),
        marked_range: from_ns(view.markedRange()),
        selected_range: from_ns(view.selectedRange()),
        preedit_feeds: preedits.len(),
        last_preedit: preedits.last().cloned(),
        // SAFETY: ручка живой поверхности, чтение на главном потоке.
        ime_width: view
            .handle()
            .map(|handle| unsafe { handle.ime_point() }.width),
    }
}

/// Marked text нет ни у клиента, ни у libghostty.
fn verify_clear(input: &mut Input, which: usize, label: &str) {
    let state = read(&input.view(which));
    input.verify(
        format!("{label}: hasMarkedText false"),
        !state.has_marked_text,
    );
    input.verify(
        format!(
            "{label}: markedRange и selectedRange {{NSNotFound, 0}} ({:?}, {:?})",
            state.marked_range, state.selected_range
        ),
        state.marked_range == Range16::NONE && state.selected_range == Range16::NONE,
    );
    input.verify(
        format!(
            "{label}: последний preedit пустой ({:?})",
            state.last_preedit
        ),
        state.last_preedit.as_deref() == Some(""),
    );
    input.verify(
        format!(
            "{label}: ширина ghostty_surface_ime_point 0 ({:?})",
            state.ime_width
        ),
        state.ime_width == Some(0.0),
    );
}

/// Курсор A в известной клетке: окно кандидатов сверяется с ней.
fn cursor(plan: &mut Plan) {
    let marker = Rc::new(Cell::new(0u64));
    let shown = Rc::clone(&marker);
    plan.act(
        "курсор A в известной клетке",
        move |_, input| {
            let nonce = u64::from(std::process::id());
            marker.set(nonce);
            let text = format!("\r\nimecursor{nonce}\x1b[{};{}H", CURSOR.0, CURSOR.1);
            input.send(A, Command::Print { text })
        },
    );
    plan.wait(
        "курсор A на месте",
        STATE_TIMEOUT,
        move |_, input| {
            let word = format!("imecursor{}", shown.get());
            match input.acked() {
                Poll::Ready(()) => {
                    let text = runtime::viewport_text(input.id(A)).unwrap_or_default();
                    poll(text.contains(&word), || format!("{word} не виден у A"))
                }
                pending => pending,
            }
        },
    );
}

fn empty_preedit(plan: &mut Plan) {
    plan.check(
        "empty-preedit",
        A,
        |_| only_a(&[]),
        |_, input| {
            let view = input.view(A);
            let before = view.feeds().preedits.len();
            set_marked(&view, "", Range16::new(0, 0));
            let after = view.feeds().preedits.len();
            input.verify(
                format!("пустой setMarkedText подал ghostty_surface_preedit ({before} → {after})"),
                after == before + 1,
            );
            verify_clear(input, A, "пустой setMarkedText");
            Ok(())
        },
    );
}

fn marked_text(plan: &mut Plan) {
    plan.check(
        "marked-text",
        A,
        |_| only_a(&[]),
        |_, input| {
            let view = input.view(A);
            let end = utf16::range_from_utf8(MARKED, MARKED.len()..MARKED.len())
                .ok_or("диапазон конца marked text")?;
            set_marked(&view, MARKED, end);
            let state = read(&view);
            let len16 = utf16::len(MARKED);
            input.verify("hasMarkedText true".to_owned(), state.has_marked_text);
            input.verify(
                format!(
                    "markedRange {{0, {len16}}} в единицах UTF-16 ({:?})",
                    state.marked_range
                ),
                state.marked_range == Range16::new(0, len16),
            );
            input.verify(
                format!("selectedRange {end:?} ({:?})", state.selected_range),
                state.selected_range == end,
            );
            input.verify(
                format!("последний preedit {MARKED} ({:?})", state.last_preedit),
                state.last_preedit.as_deref() == Some(MARKED),
            );
            input.verify(
                format!(
                    "ширина ghostty_surface_ime_point больше 0 ({:?})",
                    state.ime_width
                ),
                state.ime_width.is_some_and(|width| width > 0.0),
            );
            let emoji = utf16::range_from_utf8(MARKED, "か".len()..MARKED.len())
                .ok_or("диапазон эмодзи")?;
            let (text, actual) = substring(&view, emoji);
            input.verify(
                format!("подстрока {emoji:?} — эмодзи ({text:?}, actualRange {actual:?})"),
                text.as_deref() == Some("🙂") && actual == emoji,
            );
            let (half, _) = substring(&view, Range16::new(emoji.location, 1));
            input.verify(
                format!("половина суррогатной пары не отдаётся ({half:?})"),
                half.is_none(),
            );
            first_rect(input, &view, len16)?;
            input.observe("marked_text_reading", &state);
            // SAFETY: селектор команды отмены, как его шлёт метод ввода.
            unsafe { view.doCommandBySelector(sel!(cancelOperation:)) };
            verify_clear(input, A, "doCommandBySelector cancelOperation:");
            Ok(())
        },
    );
}

/// Окно кандидатов: прямоугольник в окне на экране, у клетки курсора, высотой в клетку.
fn first_rect(input: &mut Input, view: &SurfaceView, len16: usize) -> Result<(), String> {
    let mut actual = to_ns(Range16::NONE);
    // SAFETY: указатель на локальный `NSRange`, живой на время вызова.
    let rect = unsafe {
        view.firstRectForCharacterRange_actualRange(to_ns(Range16::new(0, len16)), &mut actual)
    };
    let handle = view.handle().ok_or("A не живая")?;
    let (column, row) = (u32::from(CURSOR.1) - 1, u32::from(CURSOR.0) - 1);
    // SAFETY: ручка живой поверхности, чтение на главном потоке.
    let cell = unsafe { handle.cell(column, row) }.ok_or("клетка курсора не прочитана")?;
    let size = runtime::read(input.id(A)).ok_or("A не живая")?.size;
    let scale = view.backing_scale();
    let width = f64::from(size.cell_width_px) / scale;
    let height = f64::from(size.cell_height_px) / scale;
    let window = view.window().ok_or("вид вне окна")?;
    let on_screen = window.convertRectToScreen(view.convertRect_toView(view.bounds(), None));
    let x = rect.origin.x - on_screen.origin.x;
    let bottom = on_screen.size.height - (rect.origin.y - on_screen.origin.y);
    let frame = window.frame();
    let inside = contains(frame, rect);
    input.verify(
        format!("прямоугольник кандидатов {rect:?} внутри окна {frame:?}"),
        inside,
    );
    input.verify(
        format!(
            "x {x:.1} в клетке курсора {column},{row} [{:.1}, {:.1}]",
            cell.x,
            cell.x + width
        ),
        x >= cell.x && x <= cell.x + width,
    );
    input.verify(
        format!(
            "низ {bottom:.1} у строки курсора: базовая линия {:.1}, клетка {height:.1}",
            cell.baseline_y
        ),
        bottom >= cell.baseline_y && bottom <= cell.baseline_y + height,
    );
    input.verify(
        format!("высота {:.1} — высота клетки {height:.1}", rect.size.height),
        (rect.size.height - height).abs() < 0.5,
    );
    input.observe(
        "first_rect",
        serde_json::json!({
            "rect": [rect.origin.x, rect.origin.y, rect.size.width, rect.size.height],
            "actual_range": [actual.location, actual.length],
            "cell": cell,
            "cell_points": [width, height],
            "view_on_screen": [on_screen.origin.x, on_screen.origin.y, on_screen.size.width, on_screen.size.height],
        }),
    );
    Ok(())
}

fn contains(outer: NSRect, inner: NSRect) -> bool {
    inner.origin.x >= outer.origin.x
        && inner.origin.y >= outer.origin.y
        && inner.origin.x + inner.size.width <= outer.origin.x + outer.size.width
        && inner.origin.y + inner.size.height <= outer.origin.y + outer.size.height
}

fn commit_outside(plan: &mut Plan) {
    plan.check(
        "commit-outside-keydown",
        A,
        |_| only_a(COMMIT.as_bytes()),
        |_, input| {
            let view = input.view(A);
            set_marked(&view, MARKED, Range16::new(utf16::len(MARKED), 0));
            let before = view.input_log().sent.len();
            insert(&view, COMMIT);
            let sent = view.input_log().sent.split_off(before);
            let single = matches!(sent.as_slice(), [only]
                if only.from == "insertText"
                    && only.input.keycode == NO_PHYSICAL_KEY
                    && only.input.text.as_deref() == Some(COMMIT));
            input.verify(
                format!(
                    "insertText вне keyDown — одно текстовое событие без физической клавиши ({} вызовов)",
                    sent.len()
                ),
                single,
            );
            verify_clear(input, A, "после insertText");
            Ok(())
        },
    );
}

/// Счётчики журнала вида до действия проверки.
#[derive(Clone, Copy, Default)]
struct Marks {
    sent: usize,
    commands: usize,
    discarded: u64,
}

fn marks(view: &SurfaceView) -> Marks {
    let log = view.input_log();
    Marks {
        sent: log.sent.len(),
        commands: log.commands.len(),
        discarded: log.marked_discarded,
    }
}

fn commit_in_key_down(plan: &mut Plan) {
    let mark = Rc::new(Cell::new(Marks::default()));
    let after = Rc::clone(&mark);
    plan.begin("commit-in-keydown", A, |_| only_a(b"a"));
    plan.act("marked text и клавиша a", move |_, input| {
        let view = input.view(A);
        set_marked(&view, COMPOSING, Range16::new(utf16::len(COMPOSING), 0));
        mark.set(marks(&view));
        input.posted = events::press(KEY_A)?;
        Ok(())
    });
    plan.step(
        "клавиша a прошла sendEvent",
        STATE_TIMEOUT,
        |_, input| input.dispatched(),
        move |_, input| {
            let sent = input.view(A).input_log().sent.split_off(after.get().sent);
            let downs: Vec<_> = sent.iter().filter(|sent| sent.from == "keyDown").collect();
            let committed = matches!(downs.as_slice(), [only]
                if only.input.keycode == NO_PHYSICAL_KEY && only.input.text.as_deref() == Some("a"));
            input.verify(
                format!(
                    "текст insertText во время keyDown при marked text ушёл одним текстовым событием без физической клавиши ({} из keyDown)",
                    downs.len()
                ),
                committed,
            );
            verify_clear(input, A, "после keyDown");
            Ok(())
        },
    );
    plan.finish();
}

fn esc_cancels(plan: &mut Plan) {
    let mark = Rc::new(Cell::new(Marks::default()));
    let after = Rc::clone(&mark);
    plan.begin("esc-cancels", A, |_| only_a(&[]));
    plan.act("marked text и Esc", move |_, input| {
        let view = input.view(A);
        set_marked(&view, COMPOSING, Range16::new(utf16::len(COMPOSING), 0));
        mark.set(marks(&view));
        input.posted = events::press(KEY_ESC)?;
        Ok(())
    });
    plan.step(
        "Esc прошёл sendEvent",
        STATE_TIMEOUT,
        |_, input| input.dispatched(),
        move |_, input| {
            let before = after.get();
            let log = input.view(A).input_log();
            let sent = log.sent.len() - before.sent;
            let commands = log.commands[before.commands..].to_vec();
            input.verify(
                format!("Esc в ghostty не отправлен ({sent} вызовов)"),
                sent == 0,
            );
            input.verify(
                format!("Esc дал cancelOperation: ({commands:?})"),
                commands.iter().any(|command| command == "cancelOperation:"),
            );
            input.verify(
                format!(
                    "composition отменена через discardMarkedText ({} → {})",
                    before.discarded, log.marked_discarded
                ),
                log.marked_discarded == before.discarded + 1,
            );
            verify_clear(input, A, "после Esc");
            Ok(())
        },
    );
    plan.finish();
}

fn focus_change(plan: &mut Plan) {
    let mark = Rc::new(Cell::new(Marks::default()));
    let after = Rc::clone(&mark);
    plan.begin("focus-change", B, |_| Ok([Vec::new(), b"b".to_vec()]));
    plan.act(
        "marked text у A и щелчок в B",
        move |stand, input| {
            let view = input.view(A);
            set_marked(&view, COMPOSING, Range16::new(utf16::len(COMPOSING), 0));
            mark.set(marks(&view));
            click(stand, view_center(&input.view(B))?, "B")
        },
    );
    plan.step(
        "B first responder",
        STATE_TIMEOUT,
        |stand, input| {
            poll(is_responder(stand, &input.view(B)), || {
                events::focus_state(&stand.app)
            })
        },
        move |_, input| {
            let before = after.get();
            let view = input.view(A);
            let log = view.input_log();
            let focus = view.feeds().focus;
            input.verify(
                format!(
                    "A сняла marked text без коммита ({} вызовов ghostty_surface_key, discardMarkedText {} → {})",
                    log.sent.len() - before.sent,
                    before.discarded,
                    log.marked_discarded
                ),
                log.sent.len() == before.sent && log.marked_discarded == before.discarded + 1,
            );
            input.verify(
                format!("A потеряла фокус ghostty ({focus:?})"),
                focus.last() == Some(&false),
            );
            verify_clear(input, A, "A после смены фокуса");
            press_all(&[KEY_B])
        },
    );
    plan.finish();
    // Позднего коммита нет: после возврата фокуса A получает только новую клавишу.
    plan.act("щелчок в A", |stand, input| {
        click(stand, view_center(&input.view(A))?, "A")
    });
    plan.wait("фокус A", FOCUS_TIMEOUT, |stand, input| {
        focused(stand, &input.view(A))
    });
    plan.check(
        "no-late-commit",
        A,
        |_| only_a(b"x"),
        |_, input| {
            verify_clear(input, A, "A после возврата фокуса");
            press_all(&[KEY_X])
        },
    );
}
