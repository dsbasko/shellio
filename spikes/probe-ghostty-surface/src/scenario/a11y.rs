//! Сценарий `a11y`: доступность вида терминала H13 на обвязке `input` — две поверхности с
//! фикстурами в raw-режиме, A в центральной зоне, B в нижней.
//!
//! Атрибуты читает настоящий клиент доступности ([`super::ax`]): отдельный процесс `--ax-client`
//! с AXUIElement API, который рабочий поток запускает на каждый запрос, — элемент в центре A, его
//! атрибуты текста и параметрические атрибуты, обход окон приложения по `AXChildren`. Запрос идёт
//! межпроцессным путём, как у VoiceOver, и отвечает на него главный поток пробы. Известный текст фикстуры с кириллицей,
//! широким иероглифом и эмодзи разводит байты, клетки и единицы UTF-16, ожидаемые диапазоны
//! считаются по этому тексту независимо от кода вида. Выделение делает перетаскивание мышью через
//! обычную доставку `NSApplication`.
//!
//! Уведомления об изменении отправляет стенд после квитанции фикстуры
//! ([`SurfaceView::a11y_refresh`]), чтения экрана для доступности считает журнал вида. Процесс
//! клиента должен быть доверен Accessibility (доверие наследуется от терминала, из которого идёт
//! `make`), иначе это ошибка предусловия и процесс заканчивается ошибкой исполнения. Чтение
//! VoiceOver — ручная приёмка задачи 19.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use objc2::rc::Retained;
use objc2::{MainThreadOnly, msg_send};
use objc2_app_kit::{NSEventType, NSScreen, NSView};
use serde::Serialize;

use super::ax::{self, Param};
use super::events;
use super::input::{
    self, A, B, Harness, Input, KEY_X, Plan, STATE_TIMEOUT, cell_point, only_a, press_all,
    view_center,
};
use super::{Poll, Scenario, Stand, poll};
use crate::a11y;
use crate::fixture::{Command, Event};
use crate::ghostty::runtime;
use crate::results::{Requirement, Verdict};
use crate::scene::Zone;
use crate::utf16::{self, Range16};
use crate::view::SurfaceView;

/// Вторая строка: кириллица, широкий иероглиф и эмодзи перед словом выделения.
const MIXED: &str = "жук 中 🙂 тест";
/// Начало `MIXED` до слова выделения и само слово; столбцы слова в клетках.
const BEFORE_WORD: &str = "жук 中 🙂 ";
const WORD: &str = "тест";
const WORD_CELLS: (u32, u32) = (10, 13);
/// Третья строка с явными пробелами в конце: `AXValue` их срезает.
const TAIL: &str = "конец";
const TAIL_SPACES: &str = "   ";
/// Начало многострочного выделения после «a11y-» в первой строке и конец на «к» второй.
const ACROSS_FROM: u32 = 5;
const ACROSS_TO: u32 = 2;
/// Поток вывода без запросов доступности.
const BURST_LINES: u32 = 300;
const BURST_INTERVAL_US: u64 = 1000;
/// Ответ клиента доступности: срок запроса к процессу 5 с и обход дерева.
const AX_TIMEOUT: Duration = Duration::from_secs(12);
const LABEL_A: &str = "Терминал A";
const LABEL_B: &str = "Терминал B";

const TEXT_ATTRIBUTES: [&str; 8] = [
    "AXRole",
    "AXDescription",
    "AXValue",
    "AXNumberOfCharacters",
    "AXVisibleCharacterRange",
    "AXSelectedText",
    "AXSelectedTextRange",
    "AXInsertionPointLineNumber",
];
const PARAMETERIZED: [&str; 5] = [
    "AXLineForIndex",
    "AXRangeForLine",
    "AXStringForRange",
    "AXAttributedStringForRange",
    "AXBoundsForRange",
];

pub fn run(stand: Rc<Stand>) {
    input::run_harness(
        stand,
        Harness {
            scenario: Scenario::A11y,
            prefix: "h13",
            notes: &[
                (
                    "a11y_note",
                    "атрибуты читает настоящий клиент AXUIElement в отдельном процессе --ax-client — тот же межпроцессный путь, что у VoiceOver, ответ собирает главный поток пробы; в процессе пробы оба пути не годятся: прямой accessibilityAttributeValue: у NSView к методам протокола NSAccessibility не переходит (роль AXUnknown), а AXUIElement API к собственному процессу зовёт методы вида в потоке клиента, а не на главном; выделение — перетаскивание мышью через обычную доставку NSApplication",
                ),
                (
                    "value_note",
                    "AXValue — логические строки видимой области (мягкий перенос склеен) без пробелов в конце строк и пустых строк внизу; без выделения точка вставки стоит в конце текста, курсор терминала не переводится; рамка диапазона — рамка вида",
                ),
                (
                    "manual_note",
                    "чтение VoiceOver в демонстрации с настоящим shell и следование за сменой фокуса проверяются вручную в задаче 19",
                ),
            ],
            build,
        },
    );
}

/// Известный текст и ожидаемые значения, посчитанные по нему без кода вида.
#[derive(Clone, Debug, Serialize)]
struct Known {
    tag: String,
    value: String,
    len16: usize,
    word: Range16,
    across_text: String,
    across: Range16,
    line_one: Range16,
    emoji: Range16,
    wide: Range16,
}

impl Known {
    fn new(nonce: &str) -> Known {
        let short: String = nonce
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .rev()
            .take(6)
            .collect();
        let tag = format!("a11y-{short}");
        let value = format!("{tag}\n{MIXED}\n{TAIL}");
        let line_start = utf16::len(&tag) + 1;
        let across_text = format!("{short}\nжук");
        Known {
            len16: utf16::len(&value),
            word: Range16::new(line_start + utf16::len(BEFORE_WORD), utf16::len(WORD)),
            across: Range16::new(utf16::len("a11y-"), utf16::len(&across_text)),
            line_one: Range16::new(line_start, utf16::len(MIXED)),
            emoji: Range16::new(line_start + utf16::len("жук 中 "), utf16::len("🙂")),
            wide: Range16::new(line_start + utf16::len("жук "), utf16::len("中")),
            tag,
            value,
            across_text,
        }
    }
}

#[derive(Default)]
struct State {
    known: Option<Known>,
    placeholder: Option<Retained<NSView>>,
    /// Сборки снимков и уведомления A до действия: журнал вида после сравнивается с ними.
    builds: usize,
    external_builds: usize,
    posts: usize,
    stream: Option<u64>,
    client: Option<ax::Pending>,
    /// Сборки снимков A до запроса клиента.
    ask_builds: usize,
    response: Option<Result<ax::Response, String>>,
    responses: Vec<(&'static str, ax::Response)>,
}

type Shared = Rc<RefCell<State>>;

fn known(state: &Shared) -> Result<Known, String> {
    state
        .borrow()
        .known
        .clone()
        .ok_or_else(|| "известный текст не напечатан".to_owned())
}

fn mark(state: &Shared, view: &SurfaceView) {
    let log = view.a11y_log();
    let mut state = state.borrow_mut();
    state.builds = log.builds.len();
    state.external_builds = log.external_builds();
    state.posts = log.posts.len();
}

fn new_posts(state: &Shared, view: &SurfaceView) -> Vec<&'static str> {
    let from = state.borrow().posts;
    view.a11y_log()
        .posts
        .get(from..)
        .unwrap_or_default()
        .iter()
        .map(|post| post.notification)
        .collect()
}

fn build(plan: &mut Plan) {
    let state: Shared = Rc::new(RefCell::new(State::default()));
    labels(plan);
    known_text(plan, &state);
    attributes(plan, &state);
    selection(plan, &state);
    insertion_point(plan, &state);
    on_demand(plan, &state);
    hidden(plan, &state);
    conclude(plan, &state);
}

fn labels(plan: &mut Plan) {
    plan.act("метки доступности", |_, input| {
        input.view(A).set_a11y_label(LABEL_A);
        input.view(B).set_a11y_label(LABEL_B);
        let before = input.view(A).a11y_log();
        input.observe("a11y_log_a_before", &before);
        Ok(())
    });
}

fn known_text(plan: &mut Plan, state: &Shared) {
    let printed = Rc::clone(state);
    plan.act("известный текст у A", move |_, input| {
        let known = Known::new(&input.env.fixture_nonce);
        let text = format!(
            "\x1b[2J\x1b[H{}\r\n{MIXED}\r\n{TAIL}{TAIL_SPACES}\r\n\r\n",
            known.tag
        );
        input.observe("a11y_known", &known);
        printed.borrow_mut().known = Some(known);
        input.send(A, Command::Print { text })
    });
    let shown = Rc::clone(state);
    plan.wait(
        "известный текст в модели A",
        STATE_TIMEOUT,
        move |_, input| {
            let Ok(known) = known(&shown) else {
                return Poll::Pending("известный текст не напечатан".to_owned());
            };
            match input.acked() {
                Poll::Ready(()) => {
                    let text = runtime::viewport_text(input.id(A)).unwrap_or_default();
                    poll(text.starts_with(&known.tag) && text.contains(TAIL), || {
                        format!("известного текста нет в видимой области A: {text:?}")
                    })
                }
                pending => pending,
            }
        },
    );
}

// ---- клиент доступности ----

/// Центр A в координатах AX API: от левого верхнего угла главного экрана.
fn ax_point(view: &SurfaceView) -> Result<(f64, f64), String> {
    let point = view_center(view)?;
    let screens = NSScreen::screens(view.mtm());
    let primary = screens.firstObject().ok_or("нет экранов")?;
    Ok((point.x, primary.frame().size.height - point.y))
}

/// Запрос клиента доступности к элементу в центре A; ответ ждёт [`answered`].
fn ask(state: &Shared, input: &Input, params: Vec<Param>, tree: bool) -> Result<(), String> {
    let request = ax::Request {
        pid: libc::pid_t::try_from(std::process::id()).map_err(|error| error.to_string())?,
        point: ax_point(&input.view(A))?,
        params,
        tree,
    };
    let view = input.view(A);
    view.a11y_forget();
    let ask_builds = view.a11y_log().builds.len();
    a11y::stand_client(true);
    let pending = ax::start(&request);
    if pending.is_err() {
        a11y::stand_client(false);
    }
    let mut state = state.borrow_mut();
    state.client = Some(pending?);
    state.ask_builds = ask_builds;
    state.response = None;
    Ok(())
}

fn answered(state: &Shared) -> Poll<()> {
    let mut state = state.borrow_mut();
    let Some(client) = state.client.as_ref() else {
        return poll(state.response.is_some(), || {
            "запроса клиента нет".to_owned()
        });
    };
    match client.poll() {
        ax::Poll::Ready(response) => {
            a11y::stand_client(false);
            state.client = None;
            state.response = Some(*response);
            Poll::Ready(())
        }
        ax::Poll::Waiting => Poll::Pending("клиент доступности ещё ждёт ответа".to_owned()),
        ax::Poll::Lost => Poll::Pending("поток клиента доступности потерян".to_owned()),
    }
}

/// Ответ клиента: ошибка запуска — ошибка исполнения, отказ доверия — ошибка предусловия.
fn take_response(state: &Shared) -> Result<ax::Response, String> {
    let response = state
        .borrow_mut()
        .response
        .take()
        .ok_or("нет ответа клиента доступности")??;
    if !response.trusted {
        return Err("предусловие: процесс клиента доступности не доверен Accessibility (AXIsProcessTrusted = false): выдайте Accessibility терминалу, из которого идёт make".to_owned());
    }
    Ok(response)
}

/// Ответ клиента с элементом в центре A: метка элемента — метка A.
fn take_element(
    state: &Shared,
    input: &mut Input,
    label: &'static str,
) -> Result<ax::Element, String> {
    let response = take_response(state)?;
    let from = state.borrow().ask_builds;
    let causes: Vec<(String, bool)> = input
        .view(A)
        .a11y_log()
        .builds
        .get(from..)
        .unwrap_or_default()
        .iter()
        .map(|build| (build.cause.clone(), build.external))
        .collect();
    input.verify(
        format!("{label}: снимок для клиента собран по его запросу атрибута ({causes:?})"),
        !causes.is_empty()
            && causes
                .iter()
                .all(|(cause, external)| cause.starts_with("AX") && !external),
    );
    input.verify(
        format!(
            "{label}: клиент — отдельный процесс (PID клиента {}, пробы {})",
            response.client_pid,
            std::process::id()
        ),
        response.client_pid != std::process::id(),
    );
    let element = response.element.clone();
    input.verify(
        format!(
            "{label}: клиент нашёл в центре A элемент с меткой «{LABEL_A}» за {:.1} мс ({:?}, ошибка {:?})",
            response.ms,
            element.as_ref().map(|element| &element.description),
            response.hit_error
        ),
        element
            .as_ref()
            .is_some_and(|element| element.description.text() == Some(LABEL_A)),
    );
    state.borrow_mut().responses.push((label, response));
    element.ok_or_else(|| format!("{label}: клиент не нашёл элемент в центре A"))
}

fn verify_refresh(
    input: &mut Input,
    state: &Shared,
    label: &str,
    reason: &str,
    expect: (bool, bool),
) -> a11y::Refresh {
    let view = input.view(A);
    mark(state, &view);
    let refresh = view.a11y_refresh(reason);
    let posts = new_posts(state, &view);
    let mut wanted = Vec::new();
    if expect.0 {
        wanted.push("AXValueChanged");
    }
    if expect.1 {
        wanted.push("AXSelectedTextChanged");
    }
    input.verify(
        format!("{label}: обновление по квитанции отправило {wanted:?} ({posts:?}, {refresh:?})"),
        refresh.present && posts == wanted,
    );
    refresh
}

fn missing(names: &[String], wanted: &[&'static str]) -> Vec<&'static str> {
    wanted
        .iter()
        .copied()
        .filter(|name| !names.iter().any(|have| have == name))
        .collect()
}

fn param(element: &ax::Element, index: usize) -> Option<&ax::Value> {
    element.params.get(index).map(|(_, value)| value)
}

fn attributes(plan: &mut Plan, state: &Shared) {
    plan.begin("attributes", A, |_| only_a(&[]));
    let asking = Rc::clone(state);
    plan.act(
        "обновление и запрос клиента",
        move |_, input| {
            let known = known(&asking)?;
            verify_refresh(
                input,
                &asking,
                "известный текст",
                "known-text",
                (true, false),
            );
            let params = vec![
                Param::LineForIndex(known.word.location),
                Param::RangeForLine(1),
                Param::StringForRange(known.emoji),
                Param::AttributedStringForRange(known.wide),
            ];
            ask(&asking, input, params, false)
        },
    );
    let waiting = Rc::clone(state);
    let checking = Rc::clone(state);
    plan.step(
        "ответ клиента доступности",
        AX_TIMEOUT,
        move |_, _| answered(&waiting),
        move |_, input| {
            let known = known(&checking)?;
            let element = take_element(&checking, input, "attributes")?;
            let absent = missing(&element.names, &TEXT_ATTRIBUTES);
            input.verify(
                format!("атрибуты текста в AXUIElementCopyAttributeNames (нет {absent:?})"),
                absent.is_empty(),
            );
            let absent = missing(&element.parameterized_names, &PARAMETERIZED);
            input.verify(
                format!(
                    "параметрические атрибуты в AXUIElementCopyParameterizedAttributeNames (нет {absent:?})"
                ),
                absent.is_empty(),
            );
            input.verify(
                format!("AXRole AXTextArea ({:?})", element.role),
                element.role.text() == Some("AXTextArea"),
            );
            input.verify(
                format!("AXValue равен известному тексту ({:?})", element.value),
                element.value.text() == Some(known.value.as_str()),
            );
            input.verify(
                format!(
                    "AXNumberOfCharacters {} ({:?})",
                    known.len16, element.characters
                ),
                element.characters.integer() == isize::try_from(known.len16).ok(),
            );
            input.verify(
                format!(
                    "AXVisibleCharacterRange {{0, {}}} ({:?})",
                    known.len16, element.visible
                ),
                element.visible.range() == Some(Range16::new(0, known.len16)),
            );
            input.verify(
                format!(
                    "без выделения AXSelectedTextRange {{{}, 0}} и AXSelectedText пуст ({:?}, {:?})",
                    known.len16, element.selected, element.selected_text
                ),
                element.selected.range() == Some(Range16::new(known.len16, 0))
                    && element.selected_text.text().is_none_or(str::is_empty),
            );
            input.verify(
                format!("AXInsertionPointLineNumber 2 ({:?})", element.insertion_line),
                element.insertion_line.integer() == Some(2),
            );
            let line = param(&element, 0);
            input.verify(
                format!("AXLineForIndex слова — строка 1 ({line:?})"),
                line.and_then(ax::Value::integer) == Some(1),
            );
            let range = param(&element, 1);
            input.verify(
                format!("AXRangeForLine 1 — {:?} ({range:?})", known.line_one),
                range.and_then(ax::Value::range) == Some(known.line_one),
            );
            let emoji = param(&element, 2);
            input.verify(
                format!(
                    "AXStringForRange {:?} — эмодзи в двух единицах UTF-16 ({emoji:?})",
                    known.emoji
                ),
                emoji.and_then(ax::Value::text) == Some("🙂"),
            );
            let wide = param(&element, 3);
            input.verify(
                format!("AXAttributedStringForRange широкого иероглифа ({wide:?})"),
                wide.and_then(ax::Value::text) == Some("中"),
            );
            Ok(())
        },
    );
    plan.finish();
}

/// Перетаскивание от клетки к клетке A, как в сценарии `input`.
fn drag(stand: &Stand, input: &mut Input, from: (u32, u32), to: (u32, u32)) -> Result<(), String> {
    let start = cell_point(input, A, from.0, from.1, 0.25)?;
    let end = cell_point(input, A, to.0, to.1, 0.75)?;
    events::post_mouse(
        NSEventType::LeftMouseDown,
        &stand.window,
        start,
        "выделение",
    )?;
    events::post_mouse(
        NSEventType::LeftMouseDragged,
        &stand.window,
        end,
        "выделение",
    )?;
    events::post_mouse(NSEventType::LeftMouseUp, &stand.window, end, "выделение")?;
    Ok(())
}

/// Выделение у libghostty с ожидаемым текстом.
fn selected(input: &mut Input, text: &str) -> Poll<()> {
    let view = input.view(A);
    let Some(handle) = view.handle() else {
        return Poll::Pending("A не живая".to_owned());
    };
    // SAFETY: ручка живой поверхности, чтение на главном потоке.
    let selection = unsafe { handle.selection() }.map(|selection| selection.text);
    poll(selection.as_deref() == Some(text), || {
        format!("выделение A {selection:?}, ожидалось {text:?}")
    })
}

/// Выделение: обвязка проверки, перетаскивание, обновление по выделению у libghostty, запрос
/// клиента и сверка диапазона.
fn selection_check(
    plan: &mut Plan,
    state: &Shared,
    name: &'static str,
    cells: ((u32, u32), (u32, u32)),
    expect: fn(&Known) -> (String, Range16),
) {
    plan.begin(name, A, |_| only_a(&[]));
    plan.act(
        "выделение перетаскиванием",
        move |stand, input| drag(stand, input, cells.0, cells.1),
    );
    let shown = Rc::clone(state);
    let asking = Rc::clone(state);
    plan.step(
        "выделение у A",
        STATE_TIMEOUT,
        move |_, input| match known(&shown) {
            Ok(known) => selected(input, &expect(&known).0),
            Err(reason) => Poll::Pending(reason),
        },
        move |_, input| {
            verify_refresh(input, &asking, name, name, (false, true));
            ask(&asking, input, Vec::new(), false)
        },
    );
    let waiting = Rc::clone(state);
    let checking = Rc::clone(state);
    plan.step(
        "ответ клиента доступности",
        AX_TIMEOUT,
        move |_, _| answered(&waiting),
        move |_, input| {
            let (text, range) = expect(&known(&checking)?);
            let element = take_element(&checking, input, name)?;
            input.verify(
                format!(
                    "AXSelectedTextRange {range:?} в единицах UTF-16 ({:?})",
                    element.selected
                ),
                element.selected.range() == Some(range),
            );
            input.verify(
                format!("AXSelectedText {text:?} ({:?})", element.selected_text),
                element.selected_text.text() == Some(text.as_str()),
            );
            let inside = element.value.text().and_then(|value| {
                utf16::range_to_utf8(value, range).map(|bytes| value[bytes].to_owned())
            });
            input.verify(
                format!("AXValue в диапазоне выделения — {text:?} ({inside:?})"),
                inside.as_deref() == Some(text.as_str()),
            );
            Ok(())
        },
    );
    plan.finish();
}

fn selection(plan: &mut Plan, state: &Shared) {
    selection_check(
        plan,
        state,
        "selection-word",
        ((WORD_CELLS.0, 1), (WORD_CELLS.1, 1)),
        |known| (WORD.to_owned(), known.word),
    );
    selection_check(
        plan,
        state,
        "selection-across-lines",
        ((ACROSS_FROM, 0), (ACROSS_TO, 1)),
        |known| (known.across_text.clone(), known.across),
    );
}

fn insertion_point(plan: &mut Plan, state: &Shared) {
    plan.check(
        "selection-cleared-by-key",
        A,
        |_| only_a(b"x"),
        |_, _| press_all(&[KEY_X]),
    );
    plan.begin("insertion-point", A, |_| only_a(&[]));
    let asking = Rc::clone(state);
    plan.act(
        "обновление и запрос клиента",
        move |_, input| {
            let view = input.view(A);
            // SAFETY: ручка живой поверхности, чтение на главном потоке.
            let has = view
                .handle()
                .is_some_and(|handle| unsafe { handle.has_selection() });
            input.verify(format!("клавиша сняла выделение у A ({has})"), !has);
            verify_refresh(
                input,
                &asking,
                "снятое выделение",
                "selection-cleared",
                (false, true),
            );
            ask(&asking, input, Vec::new(), false)
        },
    );
    let waiting = Rc::clone(state);
    let checking = Rc::clone(state);
    plan.step(
        "ответ клиента доступности",
        AX_TIMEOUT,
        move |_, _| answered(&waiting),
        move |_, input| {
            let known = known(&checking)?;
            let element = take_element(&checking, input, "insertion-point")?;
            input.verify(
                format!(
                    "AXSelectedTextRange {{{}, 0}} и AXValue без изменений ({:?})",
                    known.len16, element.selected
                ),
                element.selected.range() == Some(Range16::new(known.len16, 0))
                    && element.value.text() == Some(known.value.as_str()),
            );
            verify_refresh(
                input,
                &checking,
                "без изменений",
                "unchanged",
                (false, false),
            );
            Ok(())
        },
    );
    plan.finish();
}

fn on_demand(plan: &mut Plan, state: &Shared) {
    let start = Rc::clone(state);
    plan.begin("reads-on-demand", A, |_| only_a(&[]));
    plan.act(
        "поток вывода у A без запросов доступности",
        move |_, input| {
            mark(&start, &input.view(A));
            let id = input.send_id(
                A,
                Command::Stream {
                    lines: BURST_LINES,
                    interval_us: BURST_INTERVAL_US,
                    columns: 60,
                    osc_every: 0,
                    report_every: 0,
                },
            )?;
            start.borrow_mut().stream = Some(id);
            Ok(())
        },
    );
    let waiting = Rc::clone(state);
    let checked = Rc::clone(state);
    plan.step(
        "поток A закончен",
        STATE_TIMEOUT,
        move |_, input| {
            let Some(id) = waiting.borrow().stream else {
                return Poll::Pending("поток не начат".to_owned());
            };
            let done = input.receipts(A).iter().any(|receipt| {
                matches!(receipt.event, Event::StreamDone { id: got, .. } if got == id)
            });
            poll(done, || format!("нет stream-done #{id} от A"))
        },
        move |_, input| {
            let log = input.view(A).a11y_log();
            let (builds, external) = {
                let state = checked.borrow();
                (
                    log.builds.len() - state.builds,
                    log.external_builds() - state.external_builds,
                )
            };
            input.verify(
                format!(
                    "за поток в {BURST_LINES} строк экран для доступности читался только по запросам клиента вне стенда: сборок {builds}, из них по внешним запросам {external}"
                ),
                builds == external,
            );
            // Без выделения точка вставки стоит в конце текста: новый текст сдвигает и её.
            verify_refresh(input, &checked, "после потока", "stream-done", (true, true));
            Ok(())
        },
    );
    plan.finish();
}

/// Обход дерева клиентом: A и B среди текстовых областей окон приложения или нет.
fn tree_check(plan: &mut Plan, state: &Shared, name: &'static str, b_present: bool) {
    plan.begin(name, A, |_| only_a(&[]));
    let asking = Rc::clone(state);
    plan.act("обход дерева клиентом", move |_, input| {
        let b = input.view(B);
        // SAFETY: вызов метода протокола доступности без аргументов на главном потоке.
        let element: bool = unsafe { msg_send![&*b, isAccessibilityElement] };
        input.verify(
            format!("B isAccessibilityElement {element}, ожидалось {b_present}"),
            element == b_present,
        );
        if !b_present {
            let before = b.a11y_log();
            let refresh = b.a11y_refresh("hidden");
            let after = b.a11y_log();
            input.verify(
                format!(
                    "скрытая B: обновление без уведомлений и без чтения экрана ({refresh:?}, сборок {} → {})",
                    before.builds.len(),
                    after.builds.len()
                ),
                !refresh.present
                    && after.posts.len() == before.posts.len()
                    && after.builds.len() == before.builds.len(),
            );
        }
        ask(&asking, input, Vec::new(), true)
    });
    let waiting = Rc::clone(state);
    let checking = Rc::clone(state);
    plan.step(
        "ответ клиента доступности",
        AX_TIMEOUT,
        move |_, _| answered(&waiting),
        move |_, input| {
            take_element(&checking, input, name)?;
            let (labels, nodes, roles, cut) = checking
                .borrow()
                .responses
                .last()
                .map(|(_, response)| {
                    (
                        response.text_areas.clone(),
                        response.tree_nodes,
                        response.tree_roles.clone(),
                        response.tree_cut,
                    )
                })
                .unwrap_or_default();
            input.verify(
                format!(
                    "A в дереве окон приложения ({labels:?}, {nodes} узлов, обход обрезан {cut}, роли {roles:?})"
                ),
                labels.iter().any(|label| label == LABEL_A),
            );
            input.verify(
                format!(
                    "B {} дереве окон приложения ({labels:?})",
                    if b_present { "в" } else { "не в" }
                ),
                labels.iter().any(|label| label == LABEL_B) == b_present,
            );
            Ok(())
        },
    );
    plan.finish();
}

fn hidden(plan: &mut Plan, state: &Shared) {
    let hide = Rc::clone(state);
    plan.act(
        "B за вкладкой-заглушкой",
        move |stand, input| {
            let placeholder = NSView::new(stand.view.mtm());
            input
                .scene
                .add_tab(Zone::Bottom, 0, "заглушка", &placeholder)?;
            hide.borrow_mut().placeholder = Some(placeholder);
            Ok(())
        },
    );
    plan.wait("B вне окна", STATE_TIMEOUT, |_, input| {
        poll(input.view(B).window().is_none(), || {
            "B осталась в окне за вкладкой-заглушкой".to_owned()
        })
    });
    tree_check(plan, state, "hidden-not-in-tree", false);
    let show = Rc::clone(state);
    plan.act("B снова в плитке", move |_, input| {
        let placeholder = show
            .borrow_mut()
            .placeholder
            .take()
            .ok_or("нет вкладки-заглушки")?;
        input.scene.take_tab(&placeholder)?;
        placeholder.removeFromSuperview();
        Ok(())
    });
    plan.wait("B в окне", STATE_TIMEOUT, |_, input| {
        poll(input.view(B).window().is_some(), || {
            "B не вернулась в окно".to_owned()
        })
    });
    tree_check(plan, state, "restored-in-tree", true);
}

/// Стоимость чтений, журналы, ответы клиента и ручная часть.
fn conclude(plan: &mut Plan, state: &Shared) {
    let shared = Rc::clone(state);
    plan.act("стоимость и журналы доступности", move |_, input| {
        let a = input.view(A).a11y_log();
        let b = input.view(B).a11y_log();
        let ms: Vec<f64> = a.builds.iter().map(|build| build.ms).collect();
        let calls: Vec<f64> = a
            .builds
            .iter()
            .map(|build| f64::from(build.read_calls))
            .collect();
        let summary = crate::stats::summarize(&ms).ok();
        let responses = std::mem::take(&mut shared.borrow_mut().responses);
        let client_ms: Vec<f64> = responses.iter().map(|(_, response)| response.ms).collect();
        input.findings.push((
            "h13-cost".to_owned(),
            Requirement::Research,
            Verdict::Observed,
            format!(
                "снимок доступности A: сборок {}, попаданий в кэш {}, запросов атрибутов {} (вне стенда {}), чтений ghostty {}; сборка p50 {:?} мс, max {:?} мс; срок снимка {} мс; ответ клиента AX (с обходом дерева) {:?} мс",
                a.builds.len(),
                a.cache_hits,
                a.requests,
                a.external_requests,
                a.read_calls,
                summary.as_ref().map(|s| s.p50),
                summary.as_ref().map(|s| s.max),
                a11y::SNAPSHOT_TTL_NS / 1_000_000,
                client_ms
            ),
        ));
        let off_main = a11y::off_main_requests();
        input.findings.push((
            "h13-main-thread".to_owned(),
            Requirement::Required,
            if off_main == 0 {
                Verdict::Pass
            } else {
                Verdict::Fail
            },
            format!(
                "запросы клиента в отдельном процессе пришли на главный поток пробы: вне главного потока {off_main}, ответов клиента {}",
                responses.len()
            ),
        ));
        input.findings.push((
            "h13-voiceover".to_owned(),
            Requirement::Required,
            Verdict::PendingManual,
            "VoiceOver по запросу читает видимый текст терминала в демонстрации с настоящим shell и следует за сменой фокуса — ручная приёмка задачи 19".to_owned(),
        ));
        input.metrics.push(("a11y.build_ms".to_owned(), "ms", ms));
        input
            .metrics
            .push(("a11y.read_calls".to_owned(), "count", calls));
        input
            .metrics
            .push(("a11y.client_ms".to_owned(), "ms", client_ms));
        input.observe("a11y_off_main_requests", off_main);
        input.observe("a11y_log_a", &a);
        input.observe("a11y_log_b", &b);
        input.observe("a11y_client_responses", &responses);
        Ok(())
    });
}
