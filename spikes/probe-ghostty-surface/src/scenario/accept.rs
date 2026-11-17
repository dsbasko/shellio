//! Сценарий `accept`: приёмочный проход исполнителя P0.3 (ADR-0036) на обвязке `input` с
//! установленными палитрой и маршрутизатором режимов.
//!
//! Клавиши задаются кодом и модификаторами: `CGEvent` клавиатуры, `NSEvent` из него без
//! пересборки и `NSApp.postEvent` (`events::press_translated`); метод ввода получает событие
//! через обычную доставку процесса, а не через поток событий WindowServer. Эквивалентность физической
//! клавиатуре этим не доказана: японский ввод сначала проходит контроль на штатном `NSTextView`
//! с тем же источником и теми же событиями. Части:
//!
//! - `layout-ru` — «Русская», коды клавиш слова `привет`;
//! - `dead-key` — ABC, `Option-e e` даёт `é` через мёртвую клавишу раскладки;
//! - `ja-control` — японский коммит по Return на `NSTextView` (исследование, основание отнесения);
//! - `ja-commit`, `ja-esc` — коммит `か` по Return и отмена Esc в поверхности;
//! - `ja-candidate` — преобразование, окно кандидатов процесса метода ввода у строки marked text,
//!   коммит выбранного кандидата;
//! - `cmdk-marked` — `Cmd-K` при marked text: палитра не открывается, composition и экран целы;
//! - `scroll-report` и `scroll-history` — событие прокрутки из `CGEvent` отдаётся `scrollWheel:`
//!   вида: отправленное, оно приходит без окна и до вида не доходит (`events::post_scroll`).
//!
//! Вариант `x1` идёт со встроенным дисплеем в режиме 1x на время процесса (`main.rs`): окно
//! кандидатов и composition на другом масштабе.
//!
//! Правила до прогона: проверка, которую стенд не смог провести (источник не выбран, метод ввода
//! не начал composition, контроль `NSTextView` тоже не коммитит, прокрутка не дошла до вида),
//! получает `not_run` с причиной. Контроль проходит те же стимулы, что и части: коммит по Return,
//! Esc и коммит преобразования; отказ поверхности при отказе контроля — `not_run`. Наблюдённое нарушение (лишние байты, расхождение сверки,
//! коммит контроля без коммита поверхности) — `fail`.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use objc2::MainThreadOnly;
use objc2::rc::Retained;
use objc2_app_kit::{NSEventModifierFlags, NSEventType, NSTextInputClient, NSTextView};
use objc2_foundation::{NSPoint, NSRange, NSRect, NSSize, NSString};

use super::events;
use super::input::{self, A, B, Harness, Input, Plan, STATE_TIMEOUT, cell_point, only_a};
use super::visibility::{ForeignWindow, foreign_windows};
use super::{Poll, Scenario, Stand, poll};
use crate::fixture::{Command, MouseMode};
use crate::ghostty::runtime;
use crate::results::{Requirement, Verdict};
use crate::sources::{self, Sources};
use crate::{palette, router};

const NONE: NSEventModifierFlags = NSEventModifierFlags::empty();
const KEY_RETURN: u16 = 36;
const KEY_SPACE: u16 = 49;
const KEY_ESC: u16 = 53;
const KEY_E: u16 = 14;
const KEY_K: u16 = 40;
const KEY_A: u16 = 0;
const KEY_N: u16 = 45;
/// `привет` на раскладке «Русская»: п р и в е т — клавиши g h b d t n.
const PRIVET: [(u16, NSEventModifierFlags); 6] = [
    (5, NONE),
    (4, NONE),
    (11, NONE),
    (2, NONE),
    (17, NONE),
    (45, NONE),
];
const KA: [(u16, NSEventModifierFlags); 2] = [(KEY_K, NONE), (KEY_A, NONE)];
const KANA: [(u16, NSEventModifierFlags); 4] =
    [(KEY_K, NONE), (KEY_A, NONE), (KEY_N, NONE), (KEY_A, NONE)];
/// Срок, за который метод ввода начинает composition или коммитит после клавиши.
const IME_LIMIT: Duration = Duration::from_secs(3);
/// Пауза между повторными Esc при снятии composition.
const ESC_SPACING: Duration = Duration::from_millis(400);
/// Клетка прокрутки (столбец, строка с нуля).
const SCROLL_CELL: (u32, u32) = (10, 4);
/// `mouse-scroll-multiplier` закреплённой ghostty для дискретной прокрутки
/// (`src/config/Config.zig`, discrete = 3): один щелчок колеса — три отчёта.
const DISCRETE_SCROLL_REPORTS: usize = 3;
/// Допуск окна кандидатов по горизонтали от начала marked text и по вертикали от строки, точки.
const CANDIDATE_SLACK: f64 = 60.0;

/// Вариант со встроенным дисплеем в режиме 1x.
pub const X1: &str = "x1";

thread_local! {
    static SOURCES: RefCell<Option<Sources>> = const { RefCell::new(None) };
}

/// Состояние частей, которым нужен результат прежних шагов.
#[derive(Default)]
struct Accept {
    /// Итог контроля японского ввода на `NSTextView`.
    control: Control,
    control_view: Option<Retained<NSTextView>>,
    windows_before: Vec<ForeignWindow>,
    screen_before: String,
    preedit_before: Option<String>,
    opens_before: usize,
    composing_before: u64,
    scrolls_before: u64,
}

type Shared = Rc<RefCell<Accept>>;

/// Контроль японского ввода на штатном `NSTextView` тем же источником и теми же событиями.
#[derive(Clone, Debug, Default, serde::Serialize)]
struct Control {
    /// Прогон тех же клавиш до контроля: первый японский ввод процесса теряется при соединении с
    /// IMK («error messaging the mach port for IMKCFRunLoopWakeUpReliable»).
    warmup: Option<String>,
    source_selected: bool,
    focused: bool,
    composed: bool,
    marked_before_return: Option<String>,
    text: String,
    committed: bool,
    /// Esc при marked text `か` снял composition; `None` — composition не началась.
    esc_cancels: Option<bool>,
    /// Marked text после `kana` и двух Space.
    converted: Option<String>,
    /// Return закоммитил преобразованный текст; `None` — преобразования не было.
    convert_commits: Option<bool>,
    /// Второй Esc снял composition, оставшуюся после первого; `None` — второго не было.
    second_esc_cancels: Option<bool>,
    /// Второй Return закоммитил преобразование, оставшееся после первого.
    second_return_commits: Option<bool>,
}

impl Control {
    /// Контроль проведён и composition у него началась: отказ поверхности тогда — её дефект.
    fn composes(&self) -> bool {
        self.source_selected && self.focused && self.composed
    }

    fn commits(&self) -> bool {
        self.composes() && self.committed
    }
}

pub fn run(stand: Rc<Stand>) {
    let out = stand.prelude.out.clone();
    SOURCES.with(|cell| *cell.borrow_mut() = Some(Sources::new(&out)));
    input::run_harness(
        stand,
        Harness {
            scenario: Scenario::Accept,
            prefix: "acc",
            notes: &[(
                "accept_note",
                "приёмочный проход исполнителя (ADR-0036): клавиши по коду с символами текущего источника ввода через postEvent и обычную доставку процесса; источник выбирается через Text Input Sources и возвращается после сценария; японский ввод сверяется с контролем на NSTextView",
            )],
            build,
        },
    );
}

fn build(plan: &mut Plan) {
    let state: Shared = Rc::new(RefCell::new(Accept::default()));
    plan.act(
        "палитра и маршрутизатор",
        |stand, input| {
            palette::install(&stand.window, Some(Rc::clone(&input.scene)));
            router::install(Rc::clone(&input.scene), &stand.window);
            let scale = input.view(A).backing_scale();
            let display = stand
                .window
                .screen()
                .map_or(0, |screen| screen.CGDirectDisplayID());
            let mode = crate::display::current(display);
            let (verdict, reason) = match (stand.prelude.variant == X1, scale == 1.0) {
                (true, true) => (Verdict::Pass, "режим 1x встроенного дисплея"),
                (true, false) => (
                    Verdict::NotRun,
                    "режим 1x встроенного дисплея не установлен: части прогона идут на прежнем масштабе",
                ),
                (false, _) => (Verdict::Observed, "масштаб дисплея как есть"),
            };
            input.findings.push((
                "acc-display-scale".to_owned(),
                Requirement::Required,
                verdict,
                format!("{reason}: backingScaleFactor вида {scale}, режим {mode:?}"),
            ));
            Ok(())
        },
    );
    layout_ru(plan);
    dead_key(plan);
    ja_control(plan, &state);
    ja_commit(plan, &state);
    ja_esc(plan, &state);
    ja_candidate(plan, &state);
    cmdk_marked(plan, &state);
    source(plan, None, sources::ABC);
    scroll_report(plan, &state);
    scroll_history(plan, &state);
    plan.act(
        "возврат источника ввода",
        |_, input| {
            let back = SOURCES.with(|cell| cell.borrow_mut().as_mut().and_then(Sources::restore));
            input.observe("input_source_restored", format!("{back:?}"));
            match back {
                Some(Err(reason)) => Err(format!("стенд: источник ввода не возвращён: {reason}")),
                _ => Ok(()),
            }
        },
    );
}

fn mark_not_run(input: &mut Input, part: &'static str, reason: String) {
    if !input.not_run.iter().any(|(name, _)| *name == part) {
        input.not_run.push((part, reason));
    }
}

fn is_not_run(input: &Input, part: &str) -> bool {
    input.not_run.iter().any(|(name, _)| *name == part)
}

/// Выбор источника ввода. `part` — часть, которая без него не проводится; возврат ABC после
/// части (`None`) вердикт проведённой проверки не меняет: отказ — ошибка исполнения, потому что
/// ограда с другим источником не дойдёт.
fn source(plan: &mut Plan, part: Option<&'static str>, id: &'static str) {
    plan.act("источник ввода", move |_, input| {
        let selected = SOURCES.with(|cell| {
            cell.borrow_mut().as_mut().map_or(
                Err("источники не подготовлены".to_owned()),
                |sources| sources.select(id),
            )
        });
        let failure = match selected {
            Ok(true) => None,
            Ok(false) => Some(format!("стенд: источник {id} не выбран за срок")),
            Err(reason) => Some(format!("стенд: {reason}")),
        };
        input.note(format!(
            "источник ввода {id}: {} (текущий {:?})",
            failure.as_deref().unwrap_or("выбран"),
            sources::current()
        ));
        match (failure, part) {
            (None, _) => Ok(()),
            (Some(reason), Some(part)) => {
                mark_not_run(input, part, reason);
                Ok(())
            }
            (Some(reason), None) => Err(reason),
        }
    });
}

fn keys(plan: &mut Plan, label: &'static str, codes: &'static [(u16, NSEventModifierFlags)]) {
    keys_if(plan, label, codes, |_| true);
}

/// Клавиши части, предусловие которой выполнено: стимул непроведённой части не посылается.
fn part_keys(
    plan: &mut Plan,
    part: &'static str,
    label: &'static str,
    codes: &'static [(u16, NSEventModifierFlags)],
) {
    keys_if(plan, label, codes, move |input| !is_not_run(input, part));
}

fn keys_if(
    plan: &mut Plan,
    label: &'static str,
    codes: &'static [(u16, NSEventModifierFlags)],
    when: impl Fn(&Input) -> bool + 'static,
) {
    plan.act(label, move |_, input| {
        if !when(input) {
            input.note(format!("{label}: не посылается, предусловие не выполнено"));
            input.posted = Vec::new();
            return Ok(());
        }
        let mut posted = Vec::new();
        for (code, flags) in codes {
            posted.extend(events::press_translated(*code, *flags, label)?);
        }
        input.posted = posted;
        Ok(())
    });
    plan.wait(
        "клавиши прошли sendEvent",
        STATE_TIMEOUT,
        |_, input| input.dispatched(),
    );
}

fn has_marked(input: &Input) -> bool {
    input.view(A).hasMarkedText()
}

fn marked_text(input: &Input) -> Option<String> {
    input
        .view(A)
        .hasMarkedText()
        .then(|| input.view(A).feeds().preedits.last().cloned())
        .flatten()
}

/// Ожидание условия не дольше [`IME_LIMIT`] без ошибки исполнения: итог решает `then`.
fn within(
    plan: &mut Plan,
    name: &'static str,
    done: impl Fn(&Stand, &mut Input) -> bool + 'static,
    then: impl FnOnce(&Stand, &mut Input, bool) -> Result<(), String> + 'static,
) {
    let started = Rc::new(Cell::new(None::<Instant>));
    let done = Rc::new(done);
    let check = Rc::clone(&done);
    plan.step(
        name,
        IME_LIMIT + Duration::from_secs(2),
        move |stand, input| {
            let since = started.get().unwrap_or_else(|| {
                let now = Instant::now();
                started.set(Some(now));
                now
            });
            poll(check(stand, input) || since.elapsed() >= IME_LIMIT, || {
                format!("{name}: ещё нет")
            })
        },
        move |stand, input| {
            let ok = done(stand, input);
            then(stand, input, ok)
        },
    );
}

/// Composition после клавиш. Без неё — `fail`, если контроль `NSTextView` с тем же источником
/// composition начал, иначе `not_run`. До коммита байтов в PTY нет.
fn await_marked(plan: &mut Plan, part: &'static str, state: &Shared) {
    let control = Rc::clone(state);
    within(
        plan,
        "composition метода ввода",
        |_, input| has_marked(input),
        move |_, input, marked| {
            if is_not_run(input, part) {
                return Ok(());
            }
            input.note(format!(
                "marked text после клавиш: {marked}, preedit {:?}",
                marked_text(input)
            ));
            let early = input.pending_bytes(A);
            input.verify(
                format!("до коммита в PTY ничего: {}", crate::fixture::hex(&early)),
                early.is_empty() || !marked,
            );
            if !marked {
                let control = control.borrow().control.clone();
                if control.composes() {
                    input.verify(
                        format!(
                            "composition поверхности не началась, а у NSTextView с тем же источником и событиями началась ({control:?})"
                        ),
                        false,
                    );
                } else {
                    mark_not_run(
                        input,
                        part,
                        format!(
                            "стенд: метод ввода не начал composition на событиях стенда, контроль NSTextView тоже ({control:?})"
                        ),
                    );
                }
            }
            Ok(())
        },
    );
}

/// Снимает composition: Esc, пока marked text есть, не больше четырёх раз, каждый после
/// доставки прежнего; если не снялась — смена first responder на B и обратно (снятие без
/// коммита, как в `ime`). Открытая палитра закрывается.
fn clear_marked(plan: &mut Plan) {
    let last = Rc::new(Cell::new(None::<Instant>));
    let tries = Rc::new(Cell::new(0_u32));
    let failure = Rc::new(RefCell::new(None::<String>));
    let failed = Rc::clone(&failure);
    plan.step(
        "снятие composition",
        Duration::from_secs(6),
        move |_, input| {
            if let Poll::Pending(waiting) = input.dispatched() {
                return Poll::Pending(waiting);
            }
            if !has_marked(input) || tries.get() >= 4 || failure.borrow().is_some() {
                return Poll::Ready(());
            }
            if last.get().is_none_or(|at| at.elapsed() >= ESC_SPACING) {
                match events::press_translated(KEY_ESC, NONE, "Esc снятия") {
                    Ok(numbers) => input.posted = numbers,
                    Err(reason) => {
                        *failure.borrow_mut() = Some(reason);
                        return Poll::Ready(());
                    }
                }
                last.set(Some(Instant::now()));
                tries.set(tries.get() + 1);
            }
            Poll::Pending(format!("marked text после {} Esc", tries.get()))
        },
        move |stand, input| {
            if let Some(reason) = failed.borrow_mut().take() {
                return Err(format!("стенд: Esc снятия не создан: {reason}"));
            }
            if has_marked(input) {
                input.note("composition не снята Esc: смена first responder".to_owned());
                stand.window.makeFirstResponder(Some(&input.view(B)));
                stand.window.makeFirstResponder(Some(&input.view(A)));
            }
            if has_marked(input) {
                return Err("стенд: composition не снята ни Esc, ни сменой фокуса".to_owned());
            }
            if palette::is_open() {
                input.note("палитра открыта: закрытие".to_owned());
                palette::close(palette::Close::Escape);
            }
            Ok(())
        },
    );
    plan.wait(
        "фокус A после снятия",
        STATE_TIMEOUT,
        |stand, input| input::focused(stand, &input.view(A)),
    );
}

/// Ожидаемые байты, возврат ABC и ограда: ограда идёт с раскладкой ABC.
fn finish_on_abc(plan: &mut Plan) {
    plan.expect_bytes();
    source(plan, None, sources::ABC);
    plan.fence();
}

fn layout_ru(plan: &mut Plan) {
    const PART: &str = "layout-ru";
    source(plan, Some(PART), sources::RUSSIAN);
    plan.begin(PART, A, |_| only_a("привет".as_bytes()));
    keys(plan, "привет по кодам клавиш", &PRIVET);
    finish_on_abc(plan);
}

fn dead_key(plan: &mut Plan) {
    const PART: &str = "dead-key";
    source(plan, Some(PART), sources::ABC);
    plan.begin(PART, A, |_| only_a("é".as_bytes()));
    keys(
        plan,
        "Option-e e",
        &[(KEY_E, NSEventModifierFlags::Option), (KEY_E, NONE)],
    );
    finish_on_abc(plan);
}

/// Контроль японского ввода на штатном `NSTextView` с тем же источником и теми же событиями:
/// основание отнесения отказа поверхности к стенду или к её клиенту ввода. Сначала прогрев теми
/// же клавишами, затем коммит `か` по Return, отмена Esc и коммит преобразования.
fn ja_control(plan: &mut Plan, state: &Shared) {
    let making = Rc::clone(state);
    plan.act("контроль NSTextView", move |stand, _| {
        let mtm = stand.view.mtm();
        let frame = NSRect::new(NSPoint::new(20.0, 20.0), NSSize::new(240.0, 40.0));
        let text = NSTextView::initWithFrame(NSTextView::alloc(mtm), frame);
        stand.view.addSubview(&text);
        let made = stand.window.makeFirstResponder(Some(&text));
        let focused = made
            && stand
                .window
                .firstResponder()
                .as_deref()
                .is_some_and(|responder| std::ptr::addr_eq(responder, &*text));
        let selected = SOURCES.with(|cell| {
            cell.borrow_mut()
                .as_mut()
                .is_some_and(|sources| sources.select(sources::ABC).unwrap_or(false))
        });
        let mut accept = making.borrow_mut();
        accept.control.focused = focused;
        accept.control.source_selected = selected;
        accept.control_view = Some(text);
        Ok(())
    });

    // Доставка клавиш контролю без метода ввода: ABC, `k a` дают текст `ka`.
    control_keys(plan, state, "контроль, ABC: k a", &KA);
    within_control(
        plan,
        state,
        "контроль, ABC: текст",
        |c| c.text == "ka",
        |control, seen, ok| {
            control.warmup = Some(format!("ABC: {ok}, текст {:?}", seen.text));
        },
    );
    control_reset(plan, state);
    let selecting = Rc::clone(state);
    plan.act(
        "контроль: японский источник",
        move |_, _| {
            let selected = SOURCES.with(|cell| {
                cell.borrow_mut()
                    .as_mut()
                    .is_some_and(|sources| sources.select(sources::JAPANESE).unwrap_or(false))
            });
            let mut accept = selecting.borrow_mut();
            accept.control.source_selected &= selected;
            Ok(())
        },
    );

    control_keys(plan, state, "контроль: k a", &KA);
    within_control(
        plan,
        state,
        "контроль: composition",
        |c| c.marked,
        |control, seen, ok| {
            control.composed = ok;
            control.marked_before_return = ok.then(|| seen.text.clone());
        },
    );
    control_keys(plan, state, "контроль: Return", &[(KEY_RETURN, NONE)]);
    within_control(
        plan,
        state,
        "контроль: коммит",
        |c| c.text == "か" && !c.marked,
        |control, seen, ok| {
            control.text = seen.text.clone();
            control.committed = ok;
        },
    );
    control_reset(plan, state);

    control_keys(plan, state, "контроль: k a перед Esc", &KA);
    within_control(
        plan,
        state,
        "контроль: composition перед Esc",
        |c| c.marked,
        |_, _, _| {},
    );
    let esc_from = Rc::new(Cell::new(false));
    let marking = Rc::clone(&esc_from);
    let marked_now = Rc::clone(state);
    plan.act("контроль: marked перед Esc", move |_, _| {
        marking.set(control_marked(&marked_now));
        Ok(())
    });
    control_keys(plan, state, "контроль: Esc", &[(KEY_ESC, NONE)]);
    within_control(
        plan,
        state,
        "контроль: снятие Esc",
        |c| !c.marked,
        move |control, _, ok| {
            control.esc_cancels = esc_from.get().then_some(ok);
        },
    );
    control_second(
        plan,
        state,
        "контроль: второй Esc",
        KEY_ESC,
        |c| !c.marked,
        |control, ok| {
            control.second_esc_cancels = Some(ok);
        },
    );
    control_reset(plan, state);

    control_keys(plan, state, "контроль: k a n a", &KANA);
    within_control(
        plan,
        state,
        "контроль: composition かな",
        |c| c.marked,
        |_, _, _| {},
    );
    control_keys(
        plan,
        state,
        "контроль: Space Space",
        &[(KEY_SPACE, NONE), (KEY_SPACE, NONE)],
    );
    within_control(
        plan,
        state,
        "контроль: преобразование",
        |c| c.marked && c.text != "かな",
        |control, seen, ok| {
            control.converted = ok.then(|| seen.text.clone());
        },
    );
    control_keys(
        plan,
        state,
        "контроль: Return преобразования",
        &[(KEY_RETURN, NONE)],
    );
    within_control(
        plan,
        state,
        "контроль: коммит преобразования",
        |c| !c.marked,
        |control, seen, ok| {
            control.convert_commits = control
                .converted
                .as_ref()
                .map(|converted| ok && seen.text == *converted);
        },
    );
    control_second(
        plan,
        state,
        "контроль: второй Return",
        KEY_RETURN,
        |c| !c.marked,
        |control, ok| {
            control.second_return_commits = Some(ok);
        },
    );

    let judging = Rc::clone(state);
    plan.act("контроль: итог", move |stand, input| {
        let mut accept = judging.borrow_mut();
        if let Some(view) = accept.control_view.take() {
            if view.hasMarkedText() {
                view.unmarkText();
            }
            stand.window.makeFirstResponder(Some(&input.view(A)));
            view.removeFromSuperview();
        }
        input.findings.push((
            "acc-ja-control".to_owned(),
            Requirement::Research,
            Verdict::Observed,
            format!(
                "NSTextView, источник {}: {:?}",
                sources::JAPANESE,
                accept.control
            ),
        ));
        Ok(())
    });
    source(plan, None, sources::ABC);
    plan.wait(
        "фокус A после контроля",
        STATE_TIMEOUT,
        |stand, input| input::focused(stand, &input.view(A)),
    );
}

/// Состояние контрольного `NSTextView`.
struct Seen {
    text: String,
    marked: bool,
}

fn seen(state: &Shared) -> Seen {
    Seen {
        text: control_text(state),
        marked: control_marked(state),
    }
}

fn control_keys(
    plan: &mut Plan,
    state: &Shared,
    label: &'static str,
    codes: &'static [(u16, NSEventModifierFlags)],
) {
    let ready = Rc::clone(state);
    keys_if(plan, label, codes, move |_| {
        let accept = ready.borrow();
        accept.control_view.is_some() && accept.control.focused && accept.control.source_selected
    });
}

fn within_control(
    plan: &mut Plan,
    state: &Shared,
    name: &'static str,
    done: impl Fn(&Seen) -> bool + 'static,
    record: impl FnOnce(&mut Control, &Seen, bool) + 'static,
) {
    let checking = Rc::clone(state);
    let recording = Rc::clone(state);
    within(
        plan,
        name,
        move |_, _| done(&seen(&checking)),
        move |_, input, ok| {
            let now = seen(&recording);
            input.note(format!(
                "{name}: {ok}, текст {:?}, marked {}",
                now.text, now.marked
            ));
            record(&mut recording.borrow_mut().control, &now, ok);
            Ok(())
        },
    );
}

/// Вторая клавиша контролю, если после первой composition осталась.
fn control_second(
    plan: &mut Plan,
    state: &Shared,
    label: &'static str,
    code: u16,
    done: impl Fn(&Seen) -> bool + 'static,
    record: impl FnOnce(&mut Control, bool) + 'static,
) {
    let ready = Rc::clone(state);
    let codes: &'static [(u16, NSEventModifierFlags)] = match code {
        KEY_ESC => &[(KEY_ESC, NONE)],
        _ => &[(KEY_RETURN, NONE)],
    };
    keys_if(plan, label, codes, move |_| {
        let accept = ready.borrow();
        accept.control.focused
            && accept.control.source_selected
            && accept
                .control_view
                .as_ref()
                .is_some_and(|view| view.hasMarkedText())
    });
    let sent = Rc::new(Cell::new(false));
    let noting = Rc::clone(&sent);
    plan.act(
        "вторая клавиша послана",
        move |_, input| {
            noting.set(!input.posted.is_empty());
            Ok(())
        },
    );
    within_control(plan, state, label, done, move |control, _, ok| {
        if sent.get() {
            record(control, ok);
        }
    });
}

/// Вторая клавиша поверхности, если composition осталась после первой: наблюдение без вердикта
/// части, рядом с тем же у контроля.
fn surface_second(
    plan: &mut Plan,
    state: &Shared,
    name: &'static str,
    code: u16,
    control_of: fn(&Control) -> Option<bool>,
) {
    let codes: &'static [(u16, NSEventModifierFlags)] = match code {
        KEY_ESC => &[(KEY_ESC, NONE)],
        _ => &[(KEY_RETURN, NONE)],
    };
    keys_if(plan, "вторая клавиша", codes, has_marked);
    let sent = Rc::new(Cell::new(false));
    let noting = Rc::clone(&sent);
    plan.act(
        "вторая клавиша послана",
        move |_, input| {
            noting.set(!input.posted.is_empty());
            Ok(())
        },
    );
    let control = Rc::clone(state);
    within(
        plan,
        "composition после второй клавиши",
        |_, input| !has_marked(input),
        move |_, input, cleared| {
            if sent.get() {
                let control = control_of(&control.borrow().control);
                input.findings.push((
                    format!("acc-{name}"),
                    Requirement::Research,
                    Verdict::Observed,
                    format!(
                        "вторая клавиша сняла composition поверхности: {cleared}; у контроля NSTextView: {control:?}"
                    ),
                ));
            }
            Ok(())
        },
    );
}

/// Контроль с чистого листа: composition снимается сменой first responder, текст очищается.
fn control_reset(plan: &mut Plan, state: &Shared) {
    let resetting = Rc::clone(state);
    plan.act("контроль: сброс", move |stand, input| {
        let accept = resetting.borrow();
        if let Some(view) = accept.control_view.as_ref() {
            if view.hasMarkedText() {
                stand.window.makeFirstResponder(Some(&input.view(A)));
                stand.window.makeFirstResponder(Some(view));
            }
            view.setString(&NSString::from_str(""));
        }
        Ok(())
    });
}

fn control_marked(state: &Shared) -> bool {
    state
        .borrow()
        .control_view
        .as_ref()
        .is_some_and(|view| view.hasMarkedText())
}

fn control_text(state: &Shared) -> String {
    state
        .borrow()
        .control_view
        .as_ref()
        .map(|view| view.string().to_string())
        .unwrap_or_default()
}

fn ja_commit(plan: &mut Plan, state: &Shared) {
    const PART: &str = "ja-commit";
    source(plan, Some(PART), sources::JAPANESE);
    plan.begin(PART, A, |_| only_a("か".as_bytes()));
    part_keys(plan, PART, "k a", &KA);
    await_marked(plan, PART, state);
    part_keys(plan, PART, "Return", &[(KEY_RETURN, NONE)]);
    let control = Rc::clone(state);
    within(
        plan,
        "коммит по Return",
        |_, input| !has_marked(input),
        move |_, input, committed| {
            if committed || is_not_run(input, PART) {
                return Ok(());
            }
            let control = control.borrow().control.clone();
            if control.commits() {
                input.verify(
                    format!(
                        "коммит по Return: у NSTextView с тем же источником и событиями коммит есть, у поверхности нет ({control:?})"
                    ),
                    false,
                );
            } else {
                mark_not_run(
                    input,
                    PART,
                    format!(
                        "стенд: Return не коммитит composition на событиях стенда и в контроле NSTextView ({control:?})"
                    ),
                );
            }
            Ok(())
        },
    );
    clear_marked(plan);
    finish_on_abc(plan);
}

fn ja_esc(plan: &mut Plan, state: &Shared) {
    const PART: &str = "ja-esc";
    source(plan, Some(PART), sources::JAPANESE);
    plan.begin(PART, A, |_| only_a(&[]));
    part_keys(plan, PART, "k a", &KA);
    await_marked(plan, PART, state);
    part_keys(plan, PART, "Esc", &[(KEY_ESC, NONE)]);
    let control = Rc::clone(state);
    within(
        plan,
        "composition снята Esc",
        |_, input| !has_marked(input),
        move |_, input, cleared| {
            if is_not_run(input, PART) {
                return Ok(());
            }
            let control = control.borrow().control.clone();
            match (cleared, control.esc_cancels) {
                (true, _) | (false, Some(true)) => {
                    input.verify(format!("composition снята одним Esc: {cleared}"), cleared);
                }
                (false, _) => mark_not_run(
                    input,
                    PART,
                    format!(
                        "стенд: Esc не снимает composition на событиях стенда и в контроле NSTextView (esc_cancels {:?})",
                        control.esc_cancels
                    ),
                ),
            }
            Ok(())
        },
    );
    surface_second(plan, state, "ja-esc-second", KEY_ESC, |c| {
        c.second_esc_cancels
    });
    clear_marked(plan);
    finish_on_abc(plan);
}

/// Пути исполняемых файлов: окно кандидатов принадлежит процессу метода ввода.
fn process_path(pid: i64) -> Option<String> {
    let pid = i32::try_from(pid).ok()?;
    let mut buffer = vec![0_u8; 4096];
    // SAFETY: буфер локальный, его длина передана явно.
    let len = unsafe { libc::proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    (len > 0).then(|| String::from_utf8_lossy(&buffer[..len as usize]).into_owned())
}

fn is_input_method(window: &ForeignWindow) -> bool {
    process_path(window.pid).is_some_and(|path| {
        path.contains("/InputMethods/") || path.contains("TextInput") || path.contains("Kotoeri")
    })
}

/// Окна процессов метода ввода, новые или сменившие рамку после `before`.
fn candidate_windows(before: &[ForeignWindow]) -> Vec<ForeignWindow> {
    foreign_windows()
        .into_iter()
        .filter(is_input_method)
        .filter(|window| {
            !before
                .iter()
                .any(|old| old.number == window.number && old.bounds == window.bounds)
        })
        .collect()
}

fn ja_candidate(plan: &mut Plan, state: &Shared) {
    const PART: &str = "ja-candidate";
    source(plan, Some(PART), sources::JAPANESE);
    let recording = Rc::clone(state);
    plan.act("окна до ввода", move |_, _| {
        recording.borrow_mut().windows_before = foreign_windows();
        Ok(())
    });
    plan.begin(PART, A, |_| only_a(&[]));
    part_keys(plan, PART, "k a n a", &KANA);
    await_marked(plan, PART, state);
    part_keys(
        plan,
        PART,
        "Space Space",
        &[(KEY_SPACE, NONE), (KEY_SPACE, NONE)],
    );
    let (seen, judged) = (Rc::clone(state), Rc::clone(state));
    within(
        plan,
        "окно кандидатов",
        move |_, _| !candidate_windows(&seen.borrow().windows_before).is_empty(),
        move |_, input, _| {
            candidate_verdict(input, PART, &judged.borrow().windows_before);
            if is_not_run(input, PART) {
                return Ok(());
            }
            // Текущий preedit коммитится Return: ожидание — он, а не байты PTY.
            match marked_text(input) {
                Some(chosen) => {
                    input.note(format!("preedit перед коммитом {chosen:?}"));
                    input.set_expect([chosen.into_bytes(), Vec::new()]);
                }
                None => input.verify("preedit перед коммитом есть".to_owned(), false),
            }
            Ok(())
        },
    );
    part_keys(plan, PART, "Return", &[(KEY_RETURN, NONE)]);
    let commit_control = Rc::clone(state);
    within(
        plan,
        "коммит кандидата",
        |_, input| !has_marked(input),
        move |_, input, committed| {
            if committed || is_not_run(input, PART) {
                return Ok(());
            }
            let control = commit_control.borrow().control.clone();
            if control.convert_commits == Some(true) {
                input.verify(
                    format!(
                        "коммит преобразования по Return: у NSTextView есть, у поверхности нет ({control:?})"
                    ),
                    false,
                );
            } else {
                mark_not_run(
                    input,
                    PART,
                    format!(
                        "стенд: Return не коммитит преобразование на событиях стенда и в контроле NSTextView (convert_commits {:?})",
                        control.convert_commits
                    ),
                );
            }
            Ok(())
        },
    );
    surface_second(plan, state, "ja-candidate-second-return", KEY_RETURN, |c| {
        c.second_return_commits
    });
    clear_marked(plan);
    finish_on_abc(plan);
}

/// Окно процесса метода ввода (по пути исполняемого файла) у строки marked text: под ней, а у
/// нижнего края экрана — над ней. Роль окна «выбор кандидата» этим не доказана: вывод —
/// геометрия окна метода ввода относительно `firstRectForCharacterRange` с известной ошибкой
/// ширины `ime_point` (ADR-0003).
fn candidate_verdict(input: &mut Input, part: &'static str, before: &[ForeignWindow]) {
    let fresh = candidate_windows(before);
    let view = input.view(A);
    let mut actual = NSRange::new(0, 0);
    let len = view.markedRange().length;
    // SAFETY: указатель на локальный `NSRange`, живой на время вызова.
    let rect: NSRect = unsafe {
        view.firstRectForCharacterRange_actualRange(NSRange::new(0, len.max(1)), &mut actual)
    };
    let primary = objc2_core_graphics::CGDisplayBounds(objc2_core_graphics::CGMainDisplayID());
    // Строка marked text в координатах Core Graphics (начало сверху).
    let line_top = primary.size.height - (rect.origin.y + rect.size.height);
    let line_bottom = primary.size.height - rect.origin.y;
    let near = fresh.iter().find(|window| {
        let [x, y, _, height] = window.bounds;
        let below = y >= line_bottom - 4.0 && y - line_bottom <= CANDIDATE_SLACK;
        let above = y + height <= line_top + 4.0 && line_top - (y + height) <= CANDIDATE_SLACK;
        (x - rect.origin.x).abs() <= CANDIDATE_SLACK && (below || above)
    });
    input.observe(
        "candidate",
        serde_json::json!({
            "marked": marked_text(input),
            "first_rect": [rect.origin.x, rect.origin.y, rect.size.width, rect.size.height],
            "line_cg": [line_top, line_bottom],
            "input_method_windows": fresh,
            "paths": fresh.iter().map(|window| process_path(window.pid)).collect::<Vec<_>>(),
            "near": near,
            "scale": view.backing_scale(),
        }),
    );
    let (verdict, reason) = if is_not_run(input, part) {
        (
            Verdict::NotRun,
            "стенд: composition части ja-candidate не началась".to_owned(),
        )
    } else {
        match (fresh.is_empty(), near) {
            (true, _) => (
                Verdict::NotRun,
                "стенд: окно процесса метода ввода не появилось и не сменило рамку".to_owned(),
            ),
            (false, Some(window)) => (
                Verdict::Pass,
                format!(
                    "окно кандидатов {} ({}) у строки marked text: рамка {:?}, firstRect {rect:?}, scale {}",
                    window.number,
                    window.owner,
                    window.bounds,
                    view.backing_scale()
                ),
            ),
            (false, None) => (
                Verdict::Fail,
                format!(
                    "окна метода ввода {fresh:?} не у строки marked text: firstRect {rect:?}, строка {line_top:.1}..{line_bottom:.1}"
                ),
            ),
        }
    };
    input.findings.push((
        "acc-ime-window-geometry".to_owned(),
        Requirement::Required,
        verdict,
        reason,
    ));
}

/// `Cmd-K` при marked text: палитра не открывается (её перехват пропускает нажатие при
/// composition), composition с тем же текстом жива, экран не очищен, фокус у A, байтов нет.
fn cmdk_marked(plan: &mut Plan, state: &Shared) {
    const PART: &str = "cmdk-marked";
    source(plan, Some(PART), sources::JAPANESE);
    plan.begin(PART, A, |_| only_a(&[]));
    part_keys(plan, PART, "k a", &KA);
    await_marked(plan, PART, state);
    let shot = Rc::clone(state);
    plan.act("состояние до Cmd-K", move |_, input| {
        let palette = palette::snapshot();
        let mut accept = shot.borrow_mut();
        accept.screen_before = runtime::viewport_text(input.id(A)).unwrap_or_default();
        accept.preedit_before = marked_text(input);
        accept.opens_before = palette.as_ref().map_or(0, |p| p.record.opens.len());
        accept.composing_before = palette.as_ref().map_or(0, |p| p.record.composing_passed);
        Ok(())
    });
    // Без composition Cmd-K открыл бы палитру: стимул только при выполненном предусловии.
    keys_if(
        plan,
        "Cmd-K",
        &[(KEY_K, NSEventModifierFlags::Command)],
        |input| !is_not_run(input, PART) && has_marked(input),
    );
    let after = Rc::clone(state);
    plan.act(
        "состояние после Cmd-K",
        move |stand, input| {
            if is_not_run(input, PART) || after.borrow().preedit_before.is_none() {
                return Ok(());
            }
            let accept = after.borrow();
            let palette = palette::snapshot();
            let opens = palette.as_ref().map_or(0, |p| p.record.opens.len());
            let composing = palette.as_ref().map_or(0, |p| p.record.composing_passed);
            let preedit = marked_text(input);
            let screen = runtime::viewport_text(input.id(A)).unwrap_or_default();
            input.verify(
                format!(
                    "палитра не открылась: открытий {} → {opens}, открыта {}",
                    accept.opens_before,
                    palette::is_open()
                ),
                opens == accept.opens_before && !palette::is_open(),
            );
            input.verify(
                format!(
                    "перехват палитры пропустил нажатие при composition: {} → {composing}",
                    accept.composing_before
                ),
                composing == accept.composing_before + 1,
            );
            input.verify(
                format!(
                    "composition с тем же текстом: {:?} → {preedit:?}",
                    accept.preedit_before
                ),
                preedit.is_some() && preedit == accept.preedit_before,
            );
            input.verify(
                "экран не очищен привязкой super+k".to_owned(),
                !accept.screen_before.trim().is_empty() && screen == accept.screen_before,
            );
            input.verify(
                "фокус у A".to_owned(),
                input::is_responder(stand, &input.view(A)),
            );
            Ok(())
        },
    );
    clear_marked(plan);
    finish_on_abc(plan);
}

fn scroll_report(plan: &mut Plan, state: &Shared) {
    const PART: &str = "scroll-report";
    // Позицию указателя поверхность получает от кнопок и движения мыши, не от прокрутки:
    // щелчок в клетке до включения отчёта.
    let positions = Rc::new(Cell::new(0_u64));
    let mark = Rc::clone(&positions);
    plan.act(
        "щелчок в клетке прокрутки",
        move |stand, input| {
            mark.set(input.view(A).input_log().mouse_positions);
            let point = cell_point(input, A, SCROLL_CELL.0, SCROLL_CELL.1, 0.5)?;
            input.posted = vec![
                events::post_mouse(
                    NSEventType::LeftMouseDown,
                    &stand.window,
                    point,
                    "клетка прокрутки",
                )?,
                events::post_mouse(
                    NSEventType::LeftMouseUp,
                    &stand.window,
                    point,
                    "клетка прокрутки",
                )?,
            ];
            Ok(())
        },
    );
    plan.wait(
        "щелчок прошёл sendEvent и задал позицию",
        STATE_TIMEOUT,
        move |_, input| match input.dispatched() {
            Poll::Ready(()) => poll(
                input.view(A).input_log().mouse_positions > positions.get(),
                || "позиция указателя у поверхности не обновилась".to_owned(),
            ),
            pending => pending,
        },
    );
    plan.mode(
        "отчёт мыши включён",
        A,
        Command::Mouse {
            mode: MouseMode::Click,
        },
    );
    let (column, row) = (SCROLL_CELL.0 + 1, SCROLL_CELL.1 + 1);
    plan.begin(PART, A, move |_| {
        only_a(
            format!("\x1b[<65;{column};{row}M")
                .repeat(DISCRETE_SCROLL_REPORTS)
                .as_bytes(),
        )
    });
    let mark = Rc::clone(state);
    plan.act(
        "прокрутка вниз на щелчок колеса",
        move |_, input| {
            mark.borrow_mut().scrolls_before = input.view(A).input_log().scrolls;
            let point = cell_point(input, A, SCROLL_CELL.0, SCROLL_CELL.1, 0.5)?;
            let event = events::scroll_event(point, -1, "прокрутка")?;
            input.view(A).scrollWheel(&event);
            input.note(
                "прокрутка: scrollWheel: вида напрямую, маршрутизация AppKit не проверяется"
                    .to_owned(),
            );
            input.posted = Vec::new();
            Ok(())
        },
    );
    plan.wait(
        "прокрутка прошла sendEvent",
        STATE_TIMEOUT,
        |_, input| input.dispatched(),
    );
    plan.expect_bytes();
    let reached = Rc::clone(state);
    plan.act("прокрутка у вида", move |_, input| {
        let count = input.view(A).input_log().scrolls - reached.borrow().scrolls_before;
        input.note(format!("прокруток у вида: {count}"));
        if count == 0 {
            mark_not_run(
                input,
                PART,
                "стенд: событие прокрутки не дошло до вида поверхности".to_owned(),
            );
        }
        Ok(())
    });
    plan.fence();
    plan.mode(
        "отчёт мыши выключен",
        A,
        Command::Mouse {
            mode: MouseMode::Off,
        },
    );
}

/// Прокрутка истории без отчёта мыши: видимый текст сдвигается, байтов в PTY нет.
fn scroll_history(plan: &mut Plan, state: &Shared) {
    const PART: &str = "scroll-history";
    plan.act("история в 200 строк", |_, input| {
        let text: String = (0..200).map(|line| format!("hist{line:03}\r\n")).collect();
        input.send(A, Command::Print { text })
    });
    plan.wait(
        "история на экране",
        STATE_TIMEOUT,
        |_, input| match input.acked() {
            Poll::Ready(()) => {
                let text = runtime::viewport_text(input.id(A)).unwrap_or_default();
                poll(text.contains("hist199"), || {
                    "hist199 не на экране".to_owned()
                })
            }
            pending => pending,
        },
    );
    plan.begin(PART, A, |_| only_a(&[]));
    let shot = Rc::clone(state);
    plan.act("прокрутка вверх", move |_, input| {
        let mut accept = shot.borrow_mut();
        accept.screen_before = runtime::viewport_text(input.id(A)).unwrap_or_default();
        accept.scrolls_before = input.view(A).input_log().scrolls;
        let point = cell_point(input, A, SCROLL_CELL.0, SCROLL_CELL.1, 0.5)?;
        let event = events::scroll_event(point, 5, "прокрутка истории")?;
        input.view(A).scrollWheel(&event);
        input.note(
            "прокрутка истории: scrollWheel: вида напрямую, маршрутизация AppKit не проверяется"
                .to_owned(),
        );
        input.posted = Vec::new();
        Ok(())
    });
    plan.wait(
        "прокрутка прошла sendEvent",
        STATE_TIMEOUT,
        |_, input| input.dispatched(),
    );
    let (moving, judging) = (Rc::clone(state), Rc::clone(state));
    within(
        plan,
        "видимый текст сдвинулся",
        move |_, input| {
            runtime::viewport_text(input.id(A)).unwrap_or_default() != moving.borrow().screen_before
        },
        move |_, input, moved| {
            let reached = input.view(A).input_log().scrolls - judging.borrow().scrolls_before;
            input.note(format!(
                "прокруток у вида {reached}, текст сдвинулся {moved}"
            ));
            if reached == 0 {
                mark_not_run(
                    input,
                    PART,
                    "стенд: событие прокрутки не дошло до вида поверхности".to_owned(),
                );
            } else {
                input.verify(format!("видимый текст сдвинулся: {moved}"), moved);
            }
            Ok(())
        },
    );
    plan.act("прокрутка обратно", |_, input| {
        let point = cell_point(input, A, SCROLL_CELL.0, SCROLL_CELL.1, 0.5)?;
        let event = events::scroll_event(point, -5, "прокрутка истории обратно")?;
        input.view(A).scrollWheel(&event);
        input.note("прокрутка истории обратно: scrollWheel: вида напрямую, маршрутизация AppKit не проверяется".to_owned());
        input.posted = Vec::new();
        Ok(())
    });
    plan.wait(
        "прокрутка обратно прошла sendEvent",
        STATE_TIMEOUT,
        |_, input| input.dispatched(),
    );
    plan.finish();
}
