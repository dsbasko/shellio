//! Сценарий `chord`: режимы `Input` и `Navigation` и аккорд выхода `Ctrl-\` H4 на обвязке
//! `input` — A в центральной зоне, B в нижней, обе фикстуры в raw-режиме, проверки с оградой и
//! точными байтами. В левой и правой зонах — плитки-заглушки с видом стенда, чтобы `h` и `l`
//! было куда переводить фокус.
//!
//! Все клавиши, щелчки и прокрутка идут через `postEvent` и обычную доставку `NSApplication`,
//! маршрутизатор стоит в `sendEvent`. После каждой группы событий сценарий ждёт их возврата из
//! `sendEvent` и читает фактическое состояние: режим и ожидание автомата, first responder и
//! плитку, цвет полосы вкладок у её слоя, журнал вызовов `ghostty_surface_key` и приходов
//! клавиш у видов. Каждая проверка кончается в `Input`: ограда — обычная клавиша, в
//! `Navigation` её съел бы маршрутизатор.
//!
//! Открытие наложения при ожидании второго аккорда проверяет сценарий `palette` (задача 10),
//! аккорд в `:terminal` nvim — сценарий `nvim` и ручная приёмка.

use std::cell::Cell;
use std::rc::Rc;

use objc2::runtime::AnyObject;
use objc2::{MainThreadOnly, sel};
use objc2_app_kit::{NSEventModifierFlags, NSEventType, NSTextInputClient, NSView};
use objc2_foundation::{NSRange, NSRect, NSString};

use super::events::{self, Key};
use super::input::{
    self, A, B, FOCUS_TIMEOUT, Harness, Input, KEY_B, KEY_X, Plan, STATE_TIMEOUT, focused, key,
    only_a, view_center,
};
use super::stand::StandView;
use super::{Scenario, Stand, poll};
use crate::fixture::Command;
use crate::ghostty::runtime;
use crate::ghostty::surface::Phase;
use crate::keys::Action as KeyAction;
use crate::router::{self, Cancel, Mode, Snapshot};
use crate::scene::Zone;
use crate::utf16::{self, Range16};

const CONTROL: usize = NSEventModifierFlags::Control.0;
const SHIFT: usize = NSEventModifierFlags::Shift.0;

const CHORD: Key = key("Ctrl-\\", 42, "\u{1c}", "\\", CONTROL);
const KEY_I: Key = key("i", 34, "i", "i", 0);
/// Клавиши `Navigation` на русской раскладке: физические `h`, `j`, `k`, `l`, `o`, `i`.
const NAV_H: Key = key("р (h)", 4, "р", "р", 0);
const NAV_J: Key = key("о (j)", 38, "о", "о", 0);
const NAV_K: Key = key("л (k)", 40, "л", "л", 0);
const NAV_L: Key = key("д (l)", 37, "д", "д", 0);
const NAV_I: Key = key("ш (i)", 34, "ш", "ш", 0);
const NAV_CTRL_O: Key = key("Ctrl-щ (Ctrl-o)", 31, "\u{f}", "щ", CONTROL);
const CONTROL_KEY: u16 = 59;
const SHIFT_KEY: u16 = 56;

const LEGACY: &[u8] = &[0x1c];
/// kitty keyboard protocol: `CSI 92 ; 5 u` — `\` с `Ctrl`; отпускание `CSI 92 ; 5 : 3 u`.
const KITTY_PRESS: &[u8] = b"\x1b[92;5u";
const KITTY_RELEASE: &[u8] = b"\x1b[92;5:3u";
const COMPOSING: &str = "жу";

pub fn run(stand: Rc<Stand>) {
    input::run_harness(
        stand,
        Harness {
            scenario: Scenario::Chord,
            prefix: "h4",
            notes: &[
                (
                    "route_note",
                    "клавиши, щелчки и прокрутка идут через postEvent и обычную доставку NSApplication; маршрутизатор стоит в sendEvent после composition и перед поверхностью; ghostty_surface_key сценарий напрямую не вызывает",
                ),
                (
                    "chord_note",
                    "«сразу после» — следующее нажатие, которое не модификатор и не автоповтор первого аккорда; миллисекундного окна нет; второй аккорд передаёт сохранённое при первом логическое событие парой нажатие и отпускание",
                ),
                (
                    "navigation_note",
                    "Ctrl-o возвращает на один шаг к плитке до последнего перехода h/j/k/l, полной истории нет; после возврата режим остаётся Navigation",
                ),
                (
                    "pending_note",
                    "открытие наложения при ожидании второго аккорда проверяет сценарий palette; Ctrl-\\ Ctrl-\\ Ctrl-n в :terminal nvim — сценарий nvim и ручная приёмка",
                ),
            ],
            build,
        },
    );
}

fn build(plan: &mut Plan) {
    install(plan);
    first_chord(plan);
    second_chord(plan);
    held_control(plan);
    modifier_between(plan);
    autorepeat(plan);
    kitty(plan);
    composition_first(plan);
    navigation(plan);
    return_to_source(plan);
    other_key(plan);
    focus_change(plan);
    click(plan);
    scroll(plan);
    app_switch(plan);
    receiver_closed(plan);
}

// ---- чтение состояния ----

/// Счётчики журналов до действия проверки.
#[derive(Clone, Copy, Default)]
struct Marks {
    arrivals: [usize; 2],
    sent: [usize; 2],
    replays: usize,
    moves: usize,
    transitions: usize,
    repeats: u64,
    composing: u64,
    scrolls: u64,
}

fn state() -> Result<Snapshot, String> {
    router::snapshot().ok_or_else(|| "маршрутизатор не стоит".to_owned())
}

fn marks(input: &Input) -> Result<Marks, String> {
    let router = state()?;
    let (a, b) = (input.view(A).input_log(), input.view(B).input_log());
    Ok(Marks {
        arrivals: [a.arrivals.len(), b.arrivals.len()],
        sent: [a.sent.len(), b.sent.len()],
        replays: router.record.replays.len(),
        moves: router.record.moves.len(),
        transitions: router.transitions.len(),
        repeats: router.record.repeats_swallowed,
        composing: router.record.composing_passed,
        scrolls: a.scrolls,
    })
}

type Mark = Rc<Cell<Marks>>;

fn take_marks(plan: &mut Plan) -> Mark {
    let shared: Mark = Rc::new(Cell::new(Marks::default()));
    let set = Rc::clone(&shared);
    plan.act("отметки журналов", move |_, input| {
        set.set(marks(input)?);
        Ok(())
    });
    shared
}

/// Плитка first responder окна.
fn focus_in(stand: &Stand, input: &Input) -> Option<(Zone, usize)> {
    let responder = stand.window.firstResponder()?;
    let view = responder.downcast_ref::<NSView>()?;
    input.scene.locate(view)
}

fn verify_mode(input: &mut Input, label: &str, mode: Mode, waiting: bool) -> Result<(), String> {
    let router = state()?;
    input.verify(
        format!(
            "{label}: режим {mode:?}, ожидание {waiting} (прочитано {:?}, {})",
            router.mode, router.waiting
        ),
        router.mode == mode && router.waiting == waiting,
    );
    Ok(())
}

/// Плитка с фокусом и цвет её полосы вкладок, прочитанный у слоя, — тон режима.
fn verify_focus(stand: &Stand, input: &mut Input, label: &str, zone: Zone, mode: Mode) {
    let at = focus_in(stand, input);
    input.verify(
        format!(
            "{label}: фокус в {zone:?} ({at:?}, first responder {})",
            events::responder_name(&stand.window)
        ),
        at == Some((zone, 0)),
    );
    let Some(tile) = input.scene.tile(zone, 0) else {
        return input.verify(format!("{label}: нет плитки {zone:?}"), false);
    };
    let expected = tile.palette().strip(true, mode);
    let actual = tile.strip_rgb();
    input.verify(
        format!(
            "{label}: тон плитки {zone:?} — режим {mode:?} (слой {actual:?}, ожидался {expected:?}, выделена {})",
            tile.is_focused()
        ),
        actual == Some(expected) && tile.is_focused(),
    );
}

/// Ни один из видов не получил клавишу `code` после отметки.
fn verify_unseen(input: &mut Input, label: &str, mark: Marks, codes: &[u16]) {
    for which in [A, B] {
        let log = input.view(which).input_log();
        let seen: Vec<u16> = log.arrivals[mark.arrivals[which].min(log.arrivals.len())..]
            .iter()
            .filter(|arrival| codes.contains(&arrival.key_code))
            .map(|arrival| arrival.key_code)
            .collect();
        input.verify(
            format!(
                "{label}: вид {} клавиш маршрутизатора не получил ({seen:?})",
                ["A", "B"][which]
            ),
            seen.is_empty(),
        );
    }
}

fn last_cancel(router: &Snapshot, mark: Marks) -> Option<Cancel> {
    router.transitions[mark.transitions.min(router.transitions.len())..]
        .iter()
        .find_map(|transition| transition.cancel)
}

fn verify_cancel(
    input: &mut Input,
    label: &str,
    mark: Marks,
    reason: Cancel,
) -> Result<(), String> {
    let router = state()?;
    let cancel = last_cancel(&router, mark);
    input.verify(
        format!("{label}: ожидание снято по {reason:?} ({cancel:?})"),
        cancel == Some(reason),
    );
    Ok(())
}

fn verify_no_replay(input: &mut Input, label: &str, mark: Marks) -> Result<(), String> {
    let replays = state()?.record.replays.len() - mark.replays;
    input.verify(
        format!("{label}: повторов аккорда нет ({replays})"),
        replays == 0,
    );
    Ok(())
}

/// Повтор аккорда дошёл до A парой нажатие и отпускание физической клавиши `\`.
fn verify_replay(input: &mut Input, label: &str, mark: Marks) -> Result<(), String> {
    let router = state()?;
    let replays = &router.record.replays[mark.replays.min(router.record.replays.len())..];
    let delivered = matches!(replays, [only] if only.delivered && only.inputs.len() == 2);
    input.verify(
        format!(
            "{label}: один повтор, доставлен ({:?})",
            replays
                .iter()
                .map(|replay| (replay.delivered, replay.reason.clone()))
                .collect::<Vec<_>>()
        ),
        delivered,
    );
    let log = input.view(A).input_log();
    let sent: Vec<(KeyAction, u32, Option<String>)> = log.sent[mark.sent[A].min(log.sent.len())..]
        .iter()
        .filter(|sent| sent.from == "chord-replay")
        .map(|sent| {
            (
                sent.input.action,
                sent.input.keycode,
                sent.input.text.clone(),
            )
        })
        .collect();
    let pair = matches!(
        sent.as_slice(),
        [(KeyAction::Press, 42, _), (KeyAction::Release, 42, None)]
    );
    input.verify(
        format!("{label}: ghostty_surface_key A — нажатие и отпускание `\\` ({sent:?})"),
        pair,
    );
    Ok(())
}

// ---- доставка ----

/// События действия и ожидание их возврата из `sendEvent`.
fn post(
    plan: &mut Plan,
    name: &'static str,
    act: impl FnOnce(&Stand, &mut Input) -> Result<Vec<u64>, String> + 'static,
) {
    plan.act(name, move |stand, input| {
        input.posted = act(stand, input)?;
        Ok(())
    });
    plan.wait(
        "события прошли sendEvent",
        STATE_TIMEOUT,
        |_, input| input.dispatched(),
    );
}

fn press(plan: &mut Plan, name: &'static str, keys: &'static [Key]) {
    post(plan, name, move |_, _| {
        let mut numbers = Vec::new();
        for key in keys {
            numbers.extend(events::press(*key)?);
        }
        Ok(numbers)
    });
}

fn click_at(stand: &Stand, input: &Input, which: usize) -> Result<Vec<u64>, String> {
    let point = view_center(&input.view(which))?;
    let label = ["A", "B"][which];
    Ok(vec![
        events::post_mouse(NSEventType::LeftMouseDown, &stand.window, point, label)?,
        events::post_mouse(NSEventType::LeftMouseUp, &stand.window, point, label)?,
    ])
}

/// Щелчок в A и ожидание фокуса A в `Input`.
fn back_to_a(plan: &mut Plan) {
    post(plan, "щелчок в A", |stand, input| {
        click_at(stand, input, A)
    });
    plan.wait("фокус A", FOCUS_TIMEOUT, |stand, input| {
        focused(stand, &input.view(A))
    });
    plan.act("режим Input у A", |stand, input| {
        verify_mode(input, "после щелчка в A", Mode::Input, false)?;
        verify_focus(stand, input, "после щелчка в A", Zone::Center, Mode::Input);
        Ok(())
    });
}

// ---- проверки ----

fn install(plan: &mut Plan) {
    plan.act(
        "плитки-заглушки и маршрутизатор",
        |stand, input| {
            let mtm = stand.view.mtm();
            for (zone, title) in [(Zone::Left, "L"), (Zone::Right, "R")] {
                let view = StandView::new(mtm, NSRect::ZERO);
                input.scene.add_tab(zone, 0, title, &view)?;
            }
            router::install(Rc::clone(&input.scene), &stand.window);
            input.scene.sync_focus(&stand.window);
            Ok(())
        },
    );
    plan.wait("фокус A", FOCUS_TIMEOUT, |stand, input| {
        focused(stand, &input.view(A))
    });
}

fn first_chord(plan: &mut Plan) {
    plan.begin("first-chord", A, |_| only_a(&[]));
    plan.act("тон Input до аккорда", |stand, input| {
        verify_mode(input, "до аккорда", Mode::Input, false)?;
        verify_focus(stand, input, "до аккорда", Zone::Center, Mode::Input);
        Ok(())
    });
    let mark = take_marks(plan);
    press(plan, "первый Ctrl-\\", &[CHORD]);
    let after = Rc::clone(&mark);
    plan.act("Navigation и ожидание", move |stand, input| {
        verify_mode(input, "после первого Ctrl-\\", Mode::Navigation, true)?;
        verify_focus(
            stand,
            input,
            "после первого Ctrl-\\",
            Zone::Center,
            Mode::Navigation,
        );
        verify_unseen(input, "первый Ctrl-\\", after.get(), &[CHORD.code]);
        let saved = state()?.saved_press.ok_or("нет сохранённого события")?;
        input.note(format!("сохранённое событие: {saved:?}"));
        if saved.text.as_deref() != Some("\\") || saved.unshifted_codepoint != u32::from('\\') {
            return Err(format!(
                "предусловие: латинская раскладка — клавиша 42 дала {:?}, unshifted {}",
                saved.text, saved.unshifted_codepoint
            ));
        }
        Ok(())
    });
    press(plan, "i", &[KEY_I]);
    plan.act("Input после i", move |stand, input| {
        verify_mode(input, "после i", Mode::Input, false)?;
        verify_focus(stand, input, "после i", Zone::Center, Mode::Input);
        verify_unseen(
            input,
            "i в Navigation",
            mark.get(),
            &[CHORD.code, KEY_I.code],
        );
        Ok(())
    });
    plan.finish();
}

fn second_chord(plan: &mut Plan) {
    plan.begin("second-chord-legacy", A, |_| only_a(LEGACY));
    let mark = take_marks(plan);
    press(plan, "Ctrl-\\ Ctrl-\\", &[CHORD, CHORD]);
    plan.act("повтор и Input", move |stand, input| {
        verify_mode(input, "после второго Ctrl-\\", Mode::Input, false)?;
        verify_focus(
            stand,
            input,
            "после второго Ctrl-\\",
            Zone::Center,
            Mode::Input,
        );
        verify_replay(input, "второй Ctrl-\\", mark.get())?;
        verify_unseen(input, "Ctrl-\\ Ctrl-\\", mark.get(), &[CHORD.code]);
        Ok(())
    });
    plan.finish();
}

fn held_control(plan: &mut Plan) {
    plan.begin("held-control", A, |_| only_a(LEGACY));
    let mark = take_marks(plan);
    post(plan, "Ctrl удержан: \\ \\", |_, _| {
        let control = NSEventModifierFlags(CONTROL);
        Ok(vec![
            events::modifier(CONTROL_KEY, control)?,
            events::key_down(CHORD, false)?,
            events::key_up(CHORD)?,
            events::key_down(CHORD, false)?,
            events::key_up(CHORD)?,
            events::modifier(CONTROL_KEY, NSEventModifierFlags::empty())?,
        ])
    });
    plan.act(
        "повтор при удержанном Ctrl",
        move |_, input| {
            verify_mode(input, "после второго \\", Mode::Input, false)?;
            verify_replay(input, "Ctrl удержан", mark.get())
        },
    );
    plan.finish();
}

fn modifier_between(plan: &mut Plan) {
    plan.begin("modifier-between", A, |_| only_a(LEGACY));
    let mark = take_marks(plan);
    post(plan, "Ctrl-\\, Shift, Ctrl-\\", |_, _| {
        let mut numbers = events::press(CHORD)?;
        numbers.push(events::modifier(SHIFT_KEY, NSEventModifierFlags(SHIFT))?);
        numbers.push(events::modifier(SHIFT_KEY, NSEventModifierFlags::empty())?);
        numbers.extend(events::press(CHORD)?);
        Ok(numbers)
    });
    plan.act(
        "модификатор ожидание не снял",
        move |_, input| {
            verify_mode(input, "после второго Ctrl-\\", Mode::Input, false)?;
            verify_replay(input, "модификатор между аккордами", mark.get())?;
            let router = state()?;
            let modifiers: Vec<(bool, bool)> = router.transitions
                [mark.get().transitions.min(router.transitions.len())..]
                .iter()
                .filter(|transition| transition.signal == "modifier")
                .map(|transition| (transition.waiting_before, transition.waiting_after))
                .collect();
            let shift = modifiers
                .iter()
                .filter(|(before, after)| *before && *after)
                .count();
            input.verify(
                format!("Shift между аккордами при ожидании, ожидание осталось ({modifiers:?})"),
                shift >= 2,
            );
            Ok(())
        },
    );
    plan.finish();
}

fn autorepeat(plan: &mut Plan) {
    plan.begin("autorepeat", A, |_| only_a(LEGACY));
    let mark = take_marks(plan);
    post(plan, "Ctrl-\\ с автоповтором", |_, _| {
        let mut numbers = vec![events::key_down(CHORD, false)?];
        for _ in 0..3 {
            numbers.push(events::key_down(CHORD, true)?);
        }
        numbers.push(events::key_up(CHORD)?);
        Ok(numbers)
    });
    let after = Rc::clone(&mark);
    plan.act(
        "автоповтор не второй аккорд",
        move |_, input| {
            verify_mode(input, "после автоповтора", Mode::Navigation, true)?;
            let repeats = state()?.record.repeats_swallowed - after.get().repeats;
            input.verify(
                format!("три автоповтора съедены маршрутизатором ({repeats})"),
                repeats == 3,
            );
            verify_no_replay(input, "автоповтор", after.get())?;
            verify_unseen(input, "автоповтор", after.get(), &[CHORD.code]);
            Ok(())
        },
    );
    press(plan, "второй Ctrl-\\", &[CHORD]);
    plan.act(
        "повтор после автоповтора",
        move |_, input| {
            verify_mode(input, "после второго Ctrl-\\", Mode::Input, false)?;
            verify_replay(input, "второй после автоповтора", mark.get())
        },
    );
    plan.finish();
}

fn kitty(plan: &mut Plan) {
    plan.mode("kitty: disambiguate", A, Command::KittyPush { flags: 1 });
    plan.begin("kitty-disambiguate", A, |_| only_a(KITTY_PRESS));
    let mark = take_marks(plan);
    press(plan, "Ctrl-\\ Ctrl-\\", &[CHORD, CHORD]);
    plan.act("повтор в kitty 1", move |_, input| {
        verify_mode(input, "kitty 1", Mode::Input, false)?;
        verify_replay(input, "kitty 1", mark.get())
    });
    plan.finish();
    plan.mode("kitty снят", A, Command::KittyPop);
    // С типами событий отпускание ограды тоже даёт байты, поэтому режим снимается до ограды.
    plan.mode(
        "kitty: disambiguate и типы событий",
        A,
        Command::KittyPush { flags: 3 },
    );
    plan.begin("kitty-event-types", A, |_| {
        only_a(&[KITTY_PRESS, KITTY_RELEASE].concat())
    });
    let mark = take_marks(plan);
    press(plan, "Ctrl-\\ Ctrl-\\", &[CHORD, CHORD]);
    plan.act("повтор в kitty 3", move |_, input| {
        verify_mode(input, "kitty 3", Mode::Input, false)?;
        verify_replay(input, "kitty 3", mark.get())
    });
    plan.expect_bytes();
    plan.mode("kitty снят до ограды", A, Command::KittyPop);
    plan.fence();
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
            NSRange::new(Range16::NONE.location, Range16::NONE.length),
        )
    };
}

fn composition_first(plan: &mut Plan) {
    plan.begin("composition-first", A, |_| only_a(&[]));
    plan.act("marked text у A", |_, input| {
        set_marked(input, COMPOSING);
        input.verify(
            "у A идёт composition".to_owned(),
            input.view(A).is_composing(),
        );
        Ok(())
    });
    let mark = take_marks(plan);
    press(plan, "Ctrl-\\ при marked text", &[CHORD]);
    plan.act("Ctrl-\\ ушёл в composition", move |_, input| {
        let before = mark.get();
        verify_mode(input, "Ctrl-\\ при composition", Mode::Input, false)?;
        let passed = state()?.record.composing_passed - before.composing;
        input.verify(
            format!("маршрутизатор пропустил нажатие к composition ({passed})"),
            passed == 1,
        );
        let log = input.view(A).input_log();
        let reached = log.arrivals[before.arrivals[A]..]
            .iter()
            .any(|arrival| arrival.kind == "keyDown" && arrival.key_code == CHORD.code);
        let sent = log.sent.len() - before.sent[A];
        input.verify(
            format!(
                "keyDown Ctrl-\\ дошёл до вида A ({reached}), в ghostty не ушёл ({sent} вызовов)"
            ),
            reached && sent == 0,
        );
        // SAFETY: селектор команды отмены, как его шлёт метод ввода.
        unsafe { input.view(A).doCommandBySelector(sel!(cancelOperation:)) };
        input.verify(
            "composition снята".to_owned(),
            !input.view(A).is_composing(),
        );
        Ok(())
    });
    plan.finish();
}

fn navigation(plan: &mut Plan) {
    plan.begin("navigation-cyrillic", A, |_| only_a(&[]));
    let mark = take_marks(plan);
    press(plan, "Ctrl-\\", &[CHORD]);
    let moves: [(&'static str, Key, Zone); 6] = [
        ("о (j)", NAV_J, Zone::Bottom),
        ("л (k)", NAV_K, Zone::Center),
        ("р (h)", NAV_H, Zone::Left),
        ("д (l)", NAV_L, Zone::Center),
        ("д (l) ещё раз", NAV_L, Zone::Right),
        ("р (h) ещё раз", NAV_H, Zone::Center),
    ];
    for (name, key, zone) in moves {
        post(plan, name, move |_, _| events::press(key));
        plan.act(
            "фокус после перехода",
            move |stand, input| {
                verify_mode(input, name, Mode::Navigation, false)?;
                verify_focus(stand, input, name, zone, Mode::Navigation);
                Ok(())
            },
        );
    }
    press(plan, "Enter", &[Key::RETURN]);
    plan.act("Input после Enter", move |stand, input| {
        verify_mode(input, "после Enter", Mode::Input, false)?;
        verify_focus(stand, input, "после Enter", Zone::Center, Mode::Input);
        let router = state()?;
        let moved = router.record.moves[mark.get().moves..]
            .iter()
            .filter(|record| record.moved)
            .count();
        input.verify(format!("шесть переходов фокуса ({moved})"), moved == 6);
        verify_unseen(
            input,
            "клавиши Navigation",
            mark.get(),
            &[
                CHORD.code,
                NAV_H.code,
                NAV_J.code,
                NAV_K.code,
                NAV_L.code,
                Key::RETURN.code,
            ],
        );
        Ok(())
    });
    plan.finish();
}

fn return_to_source(plan: &mut Plan) {
    plan.begin("return-to-source", A, |_| Ok([Vec::new(), b"b".to_vec()]));
    let mark = take_marks(plan);
    press(plan, "Ctrl-\\ о (j) Enter", &[CHORD, NAV_J, Key::RETURN]);
    plan.act("Input в B", |stand, input| {
        verify_mode(input, "B после Enter", Mode::Input, false)?;
        verify_focus(stand, input, "B после Enter", Zone::Bottom, Mode::Input);
        Ok(())
    });
    press(plan, "b, Ctrl-\\ в B", &[KEY_B, CHORD]);
    plan.act(
        "ожидание с получателем B",
        |_, input| verify_mode(input, "Ctrl-\\ в B", Mode::Navigation, true),
    );
    press(plan, "Ctrl-щ (Ctrl-o)", &[NAV_CTRL_O]);
    let after = Rc::clone(&mark);
    plan.act("возврат к A", move |stand, input| {
        verify_mode(input, "после Ctrl-o", Mode::Navigation, false)?;
        verify_focus(stand, input, "после Ctrl-o", Zone::Center, Mode::Navigation);
        verify_cancel(input, "Ctrl-o", after.get(), Cancel::OtherKey)?;
        Ok(())
    });
    press(plan, "Ctrl-щ (Ctrl-o) ещё раз", &[NAV_CTRL_O]);
    plan.act(
        "источника больше нет",
        move |stand, input| {
            verify_focus(
                stand,
                input,
                "второй Ctrl-o",
                Zone::Center,
                Mode::Navigation,
            );
            let router = state()?;
            let returns: Vec<bool> = router.record.moves[mark.get().moves..]
                .iter()
                .filter(|record| record.action == "return-to-source")
                .map(|record| record.moved)
                .collect();
            input.verify(
                format!("первый Ctrl-o вернул, второй — нет ({returns:?})"),
                returns == [true, false],
            );
            Ok(())
        },
    );
    press(plan, "ш (i)", &[NAV_I]);
    plan.act("Input в A", |stand, input| {
        verify_mode(input, "после ш (i)", Mode::Input, false)?;
        verify_focus(stand, input, "после ш (i)", Zone::Center, Mode::Input);
        Ok(())
    });
    plan.finish();
}

fn other_key(plan: &mut Plan) {
    plan.begin("wait-other-key", A, |_| only_a(&[]));
    let mark = take_marks(plan);
    press(plan, "Ctrl-\\ x", &[CHORD, KEY_X]);
    let after = Rc::clone(&mark);
    plan.act("x снял ожидание", move |_, input| {
        verify_mode(input, "после x", Mode::Navigation, false)?;
        verify_cancel(input, "x", after.get(), Cancel::OtherKey)
    });
    press(plan, "Ctrl-\\ без ожидания", &[CHORD]);
    let after = Rc::clone(&mark);
    plan.act("Ctrl-\\ не второй аккорд", move |_, input| {
        verify_mode(input, "Ctrl-\\ после x", Mode::Navigation, false)?;
        verify_no_replay(input, "Ctrl-\\ после x", after.get())
    });
    press(plan, "i", &[KEY_I]);
    plan.act(
        "x не дошёл до поверхности",
        move |_, input| {
            verify_mode(input, "после i", Mode::Input, false)?;
            verify_unseen(
                input,
                "x в Navigation",
                mark.get(),
                &[CHORD.code, KEY_X.code, KEY_I.code],
            );
            Ok(())
        },
    );
    plan.finish();

    plan.begin("wait-other-key-repeat", A, |_| only_a(&[]));
    let mark = take_marks(plan);
    post(
        plan,
        "Ctrl-\\, x с автоповтором, Ctrl-\\",
        |_, _| {
            let mut numbers = events::press(CHORD)?;
            numbers.push(events::key_down(KEY_X, false)?);
            for _ in 0..2 {
                numbers.push(events::key_down(KEY_X, true)?);
            }
            numbers.push(events::key_up(KEY_X)?);
            numbers.extend(events::press(CHORD)?);
            Ok(numbers)
        },
    );
    plan.act(
        "автоповтор x второго аккорда не вернул",
        move |_, input| {
            verify_mode(input, "после x с автоповтором", Mode::Navigation, false)?;
            verify_cancel(input, "x с автоповтором", mark.get(), Cancel::OtherKey)?;
            verify_no_replay(input, "Ctrl-\\ после x с автоповтором", mark.get())?;
            verify_unseen(
                input,
                "x с автоповтором",
                mark.get(),
                &[CHORD.code, KEY_X.code],
            );
            Ok(())
        },
    );
    press(plan, "i", &[KEY_I]);
    plan.finish();
}

fn focus_change(plan: &mut Plan) {
    plan.begin("wait-focus-change", B, |_| Ok([Vec::new(), Vec::new()]));
    let mark = take_marks(plan);
    press(plan, "Ctrl-\\ в A", &[CHORD]);
    plan.act("фокус в B программно", |stand, input| {
        let b = input.view(B);
        if !input.scene.focus(&stand.window, &b) {
            return Err("окно не приняло B first responder".to_owned());
        }
        Ok(())
    });
    let after = Rc::clone(&mark);
    plan.act(
        "смена фокуса сняла ожидание",
        move |stand, input| {
            verify_mode(input, "после смены фокуса", Mode::Navigation, false)?;
            verify_focus(
                stand,
                input,
                "после смены фокуса",
                Zone::Bottom,
                Mode::Navigation,
            );
            verify_cancel(input, "смена фокуса", after.get(), Cancel::FocusChanged)
        },
    );
    press(plan, "Ctrl-\\ в B", &[CHORD]);
    plan.act(
        "Ctrl-\\ после смены фокуса не повтор",
        move |_, input| {
            verify_mode(input, "Ctrl-\\ после смены фокуса", Mode::Navigation, false)?;
            verify_no_replay(input, "смена фокуса", mark.get())
        },
    );
    press(plan, "i", &[KEY_I]);
    plan.act("Input в B", |stand, input| {
        verify_mode(input, "после i", Mode::Input, false)?;
        verify_focus(stand, input, "после i", Zone::Bottom, Mode::Input);
        Ok(())
    });
    plan.finish();
    back_to_a(plan);
}

fn click(plan: &mut Plan) {
    plan.begin("wait-click", B, |_| Ok([Vec::new(), b"b".to_vec()]));
    let mark = take_marks(plan);
    press(plan, "Ctrl-\\ в A", &[CHORD]);
    post(plan, "щелчок в B", |stand, input| {
        click_at(stand, input, B)
    });
    plan.act(
        "щелчок включил Input в B",
        move |stand, input| {
            verify_mode(input, "после щелчка в B", Mode::Input, false)?;
            verify_focus(stand, input, "после щелчка в B", Zone::Bottom, Mode::Input);
            verify_cancel(input, "щелчок", mark.get(), Cancel::Click)?;
            verify_no_replay(input, "щелчок", mark.get())
        },
    );
    press(plan, "b", &[KEY_B]);
    plan.finish();
    back_to_a(plan);
}

fn scroll(plan: &mut Plan) {
    plan.begin("wait-scroll", A, |_| only_a(&[]));
    let mark = take_marks(plan);
    press(plan, "Ctrl-\\", &[CHORD]);
    post(plan, "прокрутка над A", |_, input| {
        let point = view_center(&input.view(A))?;
        Ok(vec![events::post_scroll(point, "прокрутка A")?])
    });
    let after = Rc::clone(&mark);
    plan.act(
        "прокрутка сняла ожидание",
        move |_, input| {
            verify_mode(input, "после прокрутки", Mode::Navigation, false)?;
            verify_cancel(input, "прокрутка", after.get(), Cancel::Scroll)?;
            let scrolls = input.view(A).input_log().scrolls - after.get().scrolls;
            input.observe("scroll_reached_surface_a", scrolls);
            input.note(format!(
                "прокрутка дошла до ghostty_surface_mouse_scroll A: {scrolls}"
            ));
            Ok(())
        },
    );
    press(plan, "Ctrl-\\ после прокрутки", &[CHORD]);
    plan.act(
        "Ctrl-\\ после прокрутки не повтор",
        move |_, input| {
            verify_mode(input, "Ctrl-\\ после прокрутки", Mode::Navigation, false)?;
            verify_no_replay(input, "прокрутка", mark.get())
        },
    );
    press(plan, "i", &[KEY_I]);
    plan.finish();
}

fn app_switch(plan: &mut Plan) {
    plan.begin("wait-app-switch", A, |_| only_a(&[]));
    let mark = take_marks(plan);
    press(plan, "Ctrl-\\", &[CHORD]);
    plan.act("скрытие приложения", |stand, _| {
        stand.app.hide(None);
        Ok(())
    });
    let after = Rc::clone(&mark);
    plan.step(
        "приложение неактивно",
        STATE_TIMEOUT,
        |stand, _| poll(!stand.app.isActive(), || events::focus_state(&stand.app)),
        move |_, input| {
            verify_mode(input, "после переключения приложения", Mode::Navigation, false)?;
            let router = state()?;
            let cancel = last_cancel(&router, after.get());
            input.observe("app_switch_cancel", cancel);
            input.verify(
                format!(
                    "переключение приложения сняло ожидание ({cancel:?}; AppSwitch — WillResignActive, FocusChanged — уход key-окна)"
                ),
                matches!(cancel, Some(Cancel::AppSwitch | Cancel::FocusChanged)),
            );
            Ok(())
        },
    );
    plan.act(
        "показ и активация приложения",
        |stand, _| {
            stand.app.unhide(None);
            #[allow(deprecated)]
            stand.app.activateIgnoringOtherApps(true);
            stand.window.makeKeyAndOrderFront(None);
            Ok(())
        },
    );
    plan.wait(
        "фокус A после возврата",
        FOCUS_TIMEOUT,
        |stand, input| focused(stand, &input.view(A)),
    );
    press(plan, "Ctrl-\\ после возврата", &[CHORD]);
    plan.act(
        "Ctrl-\\ после переключения не повтор",
        move |_, input| {
            verify_mode(input, "Ctrl-\\ после возврата", Mode::Navigation, false)?;
            verify_no_replay(input, "переключение приложения", mark.get())
        },
    );
    press(plan, "i", &[KEY_I]);
    plan.finish();
}

fn receiver_closed(plan: &mut Plan) {
    plan.begin("wait-receiver-closed", B, |_| Ok([Vec::new(), Vec::new()]));
    let mark = take_marks(plan);
    press(plan, "Ctrl-\\ в A", &[CHORD]);
    plan.act("закрытие A", |_, input| {
        runtime::request_close(input.id(A)).map(|_| ())
    });
    let after = Rc::clone(&mark);
    plan.step(
        "A освобождена, фикстура вышла",
        STATE_TIMEOUT,
        |_, input| {
            let phase = runtime::phase(input.id(A));
            let closed = input.fixture_closed(A);
            poll(phase == Some(Phase::Freed) && closed, || {
                format!("фаза A {phase:?}, фикстура закрыта {closed}")
            })
        },
        move |_, input| {
            verify_mode(input, "после закрытия A", Mode::Navigation, false)?;
            verify_cancel(input, "закрытие A", after.get(), Cancel::ReceiverGone)
        },
    );
    press(plan, "Ctrl-\\ после закрытия", &[CHORD]);
    plan.act(
        "Ctrl-\\ после закрытия не повтор",
        move |_, input| {
            verify_mode(input, "Ctrl-\\ после закрытия", Mode::Navigation, false)?;
            verify_no_replay(input, "закрытие получателя", mark.get())
        },
    );
    press(plan, "о (j) i", &[NAV_J, KEY_I]);
    plan.act("Input в B", |stand, input| {
        verify_mode(input, "B после закрытия A", Mode::Input, false)?;
        verify_focus(
            stand,
            input,
            "B после закрытия A",
            Zone::Bottom,
            Mode::Input,
        );
        Ok(())
    });
    plan.finish();
}
