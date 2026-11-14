//! Сценарий `input`: маршрут ввода H2 через обычную доставку `NSApplication`. Две поверхности с
//! фикстурами в raw-режиме: A в центральной зоне, B в нижней. Каждая проверка отмечает число байтов
//! у обеих фикстур, отправляет события через очередь приложения, ждёт ожидаемые байты, затем
//! печатает ограду — клавишу `` ` `` — получателю и ждёт её квитанцию. Байты до ограды у
//! получателя и все байты другой фикстуры сравниваются с ожидаемыми точно. В конце весь поток
//! байтов каждой фикстуры сравнивается со склейкой ожидаемых отрезков: лишний или потерянный байт
//! в любом месте виден.
//!
//! `ghostty_surface_key` сценарий напрямую не вызывает: прямой вызов маршрут не доказывает.
//! Сочетания `Cmd` проходят `performKeyEquivalent`, вставка — привязку `Cmd-V` и выдачу с
//! именованной доски на следующем проходе, мышь — события окна с положением курсора.
//!
//! Обвязку — сцену, две фикстуры в raw-режиме, проверки с оградой и итог — переиспользуют
//! сценарии `ime` и `chord` ([`run_harness`]). Маршрутизатор режимов ставит только `chord`; итог
//! обвязки снимает его в любом случае.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use objc2::MainThreadOnly;
use objc2::rc::Retained;
use objc2_app_kit::{
    NSBackingStoreType, NSEventModifierFlags, NSEventType, NSView, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSRect, NSSize};
use serde::Serialize;

use super::events::{self, Key};
use super::stand::StandView;
use super::surface_env::{self, Extra, State};
use super::{Poll, Report, Scenario, Stand, Step, poll, run_steps, window};
use crate::fixture::{self, Command, Event, MouseMode};
use crate::ghostty::clipboard::{self, Board};
use crate::ghostty::config::ColorScheme;
use crate::ghostty::runtime;
use crate::ghostty::surface::{Phase, SurfaceId};
use crate::results::{Requirement, Verdict};
use crate::scene::{Scene, Zone};
use crate::view::SurfaceView;

pub(super) const FOCUS_TIMEOUT: Duration = Duration::from_secs(5);
const READY_TIMEOUT: Duration = Duration::from_secs(12);
pub(super) const STATE_TIMEOUT: Duration = Duration::from_secs(5);
const ACT_TIMEOUT: Duration = Duration::from_secs(1);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(10);
/// Шаг ожидания байтов; внутренний предел короче, по нему ограда печатается всё равно, и
/// расхождение записывает итог проверки, а не тайм-аут.
const BYTES_TIMEOUT: Duration = Duration::from_secs(6);
const BYTES_LIMIT: Duration = Duration::from_secs(3);
const FENCE_TIMEOUT: Duration = Duration::from_secs(6);
/// Другая фикстура считается тихой, если её байты не менялись столько после ограды.
const QUIET: Duration = Duration::from_millis(150);

pub(super) const A: usize = 0;
pub(super) const B: usize = 1;
const NAMES: [&str; 2] = ["A", "B"];
const FENCE_BYTE: u8 = b'`';

const SHIFT: usize = NSEventModifierFlags::Shift.0;
const CONTROL: usize = NSEventModifierFlags::Control.0;
const OPTION: usize = NSEventModifierFlags::Option.0;
const COMMAND: usize = NSEventModifierFlags::Command.0;
const ARROW: usize = NSEventModifierFlags::NumericPad.0 | NSEventModifierFlags::Function.0;

pub(super) const fn key(
    label: &'static str,
    code: u16,
    characters: &'static str,
    ignoring: &'static str,
    flags: usize,
) -> Key {
    Key {
        label,
        code,
        characters,
        ignoring,
        flags: NSEventModifierFlags(flags),
    }
}

const FENCE: Key = key("`", 50, "`", "`", 0);
pub(super) const KEY_A: Key = key("a", 0, "a", "a", 0);
const KEY_SHIFT_Z: Key = key("Shift-Z", 6, "Z", "z", SHIFT);
/// Символы вне латинской раскладки: клавиши `;` и `e` с текстом события.
const KEY_ZHE: Key = key("ж", 41, "ж", "ж", 0);
const KEY_E_ACUTE: Key = key("é", 14, "é", "é", 0);
const KEY_CTRL_C: Key = key("Ctrl-C", 8, "\u{3}", "c", CONTROL);
const KEY_CTRL_A: Key = key("Ctrl-A", 0, "\u{1}", "a", CONTROL);
const KEY_ALT_A: Key = key("Alt-a", 0, "å", "a", OPTION);
/// Привязка ghostty по умолчанию: `super+arrow_left=text:\x01`.
const KEY_CMD_LEFT: Key = key("Cmd-Left", 123, "\u{f702}", "\u{f702}", COMMAND | ARROW);
/// Без привязки и без пункта меню.
const KEY_CMD_Y: Key = key("Cmd-y", 16, "y", "y", COMMAND);
const KEY_LEFT: Key = key("Left", 123, "\u{f702}", "\u{f702}", ARROW);
const KEY_UP: Key = key("Up", 126, "\u{f700}", "\u{f700}", ARROW);
pub(super) const KEY_ESC: Key = key("Esc", 53, "\u{1b}", "\u{1b}", 0);
pub(super) const KEY_X: Key = key("x", 7, "x", "x", 0);
const KEY_CMD_C: Key = key("Cmd-C", 8, "c", "c", COMMAND);
const KEY_CMD_V: Key = key("Cmd-V", 9, "v", "v", COMMAND);
pub(super) const KEY_B: Key = key("b", 11, "b", "b", 0);
const KEY_Q: Key = key("q", 12, "q", "q", 0);
const KEY_K: Key = key("k", 40, "k", "k", 0);
const KEY_I: Key = key("i", 34, "i", "i", 0);
const KEY_H: Key = key("h", 4, "h", "h", 0);
const KEY_M: Key = key("m", 46, "m", "m", 0);

/// Клетка отчёта мыши (с нуля).
const MOUSE_CELL: (u32, u32) = (10, 4);
/// Строка и столбец (с единицы) слова для выделения.
const WORD_AT: (u16, u16) = (8, 5);

/// Итог одной проверки.
#[derive(Clone, Debug, Serialize)]
struct CheckRecord {
    name: &'static str,
    fence_to: &'static str,
    expected_a: String,
    got_a: String,
    expected_b: String,
    got_b: String,
    pass: bool,
    /// Расхождения состояния, записанные [`Input::verify`].
    failures: Vec<String>,
    notes: Vec<String>,
}

struct Pending {
    name: &'static str,
    fence_to: usize,
    marks: [usize; 2],
    expect: [Vec<u8>; 2],
    started: Instant,
    /// Ограда пришла: когда и сколько байтов было у другой фикстуры.
    fenced: Option<(Instant, usize)>,
    notes: Vec<String>,
    failures: Vec<String>,
}

pub(super) struct Input {
    /// Префикс выводов проверок: `h2` у `input`, `h3` у `ime`.
    prefix: &'static str,
    env: State,
    b: Extra,
    pub(super) scene: Rc<Scene>,
    board: String,
    pending: Option<Pending>,
    checks: Vec<CheckRecord>,
    /// Склейка ожидаемых отрезков каждой фикстуры, с оградами.
    totals: [Vec<u8>; 2],
    /// Ожидание квитанции `ack` команды фикстуре.
    ack: Option<(usize, u64)>,
    /// Ожидание маркера в видимом тексте поверхности.
    marker: Option<(usize, String)>,
    markers: u32,
    paste_text: String,
    word: String,
    second: Option<(Retained<NSWindow>, Retained<StandView>)>,
    placeholder: Option<Retained<NSView>>,
    responder_hidden: Option<bool>,
    observations: Vec<(String, serde_json::Value)>,
    /// Заметки, сделанные до начала проверки: их забирает следующая проверка.
    early_notes: Vec<String>,
    early_failures: Vec<String>,
    /// Номера отправленных событий, возврат которых из `sendEvent` ждёт сценарий.
    pub(super) posted: Vec<u64>,
}

type Shared = Rc<RefCell<Input>>;

impl Input {
    fn nonce(&self, which: usize) -> String {
        if which == A {
            self.env.fixture_nonce.clone()
        } else {
            self.b.fixture_nonce.clone()
        }
    }

    pub(super) fn view(&self, which: usize) -> Retained<SurfaceView> {
        if which == A {
            self.env.view.clone()
        } else {
            self.b.view.clone()
        }
    }

    pub(super) fn id(&self, which: usize) -> SurfaceId {
        if which == A { self.env.id } else { self.b.id }
    }

    /// Все байты, прочитанные фикстурой, по квитанциям `read`.
    fn bytes(&mut self, which: usize) -> Vec<u8> {
        self.env.hub.poll();
        let nonce = self.nonce(which);
        let Some(conn) = self.env.hub.by_nonce(&nonce) else {
            return Vec::new();
        };
        conn.receipts
            .iter()
            .filter_map(|receipt| match &receipt.event {
                Event::Read { hex, .. } => fixture::unhex(hex),
                _ => None,
            })
            .flatten()
            .collect()
    }

    /// Соединение фикстуры закрыто: она вышла.
    pub(super) fn fixture_closed(&mut self, which: usize) -> bool {
        self.env.hub.poll();
        let nonce = self.nonce(which);
        self.env
            .hub
            .by_nonce(&nonce)
            .is_some_and(|conn| conn.closed)
    }

    pub(super) fn observe(&mut self, key: &str, value: impl Serialize) {
        let value = serde_json::to_value(value).unwrap_or(serde_json::Value::Null);
        self.observations.push((key.to_owned(), value));
    }

    pub(super) fn send(&mut self, which: usize, command: Command) -> Result<(), String> {
        let nonce = self.nonce(which);
        let id = self.env.hub.send(&nonce, command)?;
        self.ack = Some((which, id));
        Ok(())
    }

    pub(super) fn acked(&mut self) -> Poll<()> {
        self.env.hub.poll();
        let Some((which, id)) = self.ack else {
            return Poll::Ready(());
        };
        let nonce = self.nonce(which);
        match self.env.hub.by_nonce(&nonce).and_then(|conn| conn.ack(id)) {
            Some((_, None)) => {
                self.ack = None;
                Poll::Ready(())
            }
            Some((_, Some(error))) => Poll::Pending(format!("ack #{id} с ошибкой: {error}")),
            None => Poll::Pending(format!("нет ack #{id} от {}", NAMES[which])),
        }
    }

    /// Маркер в PTY после команды режима: `ack` значит «байты записаны», а не «режим применён».
    fn print_marker(&mut self, which: usize) -> Result<(), String> {
        self.markers += 1;
        let marker = format!("mk{}x{}", self.markers, short(&self.env.fixture_nonce));
        self.send(
            which,
            Command::Print {
                text: format!("\r\n{marker}\r\n"),
            },
        )?;
        self.marker = Some((which, marker));
        Ok(())
    }

    fn marker_shown(&mut self) -> Poll<()> {
        let Some((which, marker)) = self.marker.clone() else {
            return Poll::Ready(());
        };
        let text = runtime::viewport_text(self.id(which)).unwrap_or_default();
        if text.contains(&marker) {
            self.marker = None;
            return Poll::Ready(());
        }
        Poll::Pending(format!("маркер {marker} не виден у {}", NAMES[which]))
    }

    fn begin(&mut self, name: &'static str, fence_to: usize, expect: [Vec<u8>; 2]) {
        let marks = [self.bytes(A).len(), self.bytes(B).len()];
        events::note(format!("проверка {name}"));
        self.pending = Some(Pending {
            name,
            fence_to,
            marks,
            expect,
            started: Instant::now(),
            fenced: None,
            notes: std::mem::take(&mut self.early_notes),
            failures: std::mem::take(&mut self.early_failures),
        });
    }

    pub(super) fn note(&mut self, note: String) {
        match self.pending.as_mut() {
            Some(pending) => pending.notes.push(note),
            None => self.early_notes.push(note),
        }
    }

    /// Сверка фактического состояния внутри проверки: расхождение роняет её итог.
    pub(super) fn verify(&mut self, what: String, ok: bool) {
        if !ok {
            match self.pending.as_mut() {
                Some(pending) => pending.failures.push(what.clone()),
                None => self.early_failures.push(what.clone()),
            }
        }
        self.note(format!("{what}: {}", if ok { "да" } else { "НЕТ" }));
    }

    /// Все события из [`Input::posted`] прошли `sendEvent` или убраны из таблицы без него.
    pub(super) fn dispatched(&self) -> Poll<()> {
        let waiting: Vec<u64> = self
            .posted
            .iter()
            .copied()
            .filter(|number| {
                !events::record(*number)
                    .is_some_and(|record| record.send_return.is_some() || record.undispatched)
            })
            .collect();
        poll(waiting.is_empty(), || {
            format!("события {waiting:?} не вернулись из sendEvent")
        })
    }

    fn since(&mut self, which: usize) -> Vec<u8> {
        let mark = self
            .pending
            .as_ref()
            .map_or(0, |pending| pending.marks[which]);
        let bytes = self.bytes(which);
        bytes.get(mark..).map(<[u8]>::to_vec).unwrap_or_default()
    }

    /// Ожидаемые байты получателя пришли или вышел внутренний предел.
    fn expected_arrived(&mut self) -> Poll<()> {
        let Some((fence_to, want, started)) = self.pending.as_ref().map(|pending| {
            (
                pending.fence_to,
                pending.expect[pending.fence_to].len(),
                pending.started,
            )
        }) else {
            return Poll::Ready(());
        };
        let got = self.since(fence_to).len();
        poll(got >= want || started.elapsed() >= BYTES_LIMIT, || {
            format!("у {} {got} из {want} байтов", NAMES[fence_to])
        })
    }

    /// Ограда пришла получателю, другая фикстура молчит [`QUIET`].
    fn fenced(&mut self) -> Poll<()> {
        let Some(fence_to) = self.pending.as_ref().map(|pending| pending.fence_to) else {
            return Poll::Ready(());
        };
        let receiver = self.since(fence_to);
        if receiver.last() != Some(&FENCE_BYTE) {
            return Poll::Pending(format!(
                "ограды нет у {}: {}",
                NAMES[fence_to],
                fixture::hex(&receiver)
            ));
        }
        let other = self.since(1 - fence_to).len();
        let Some(pending) = self.pending.as_mut() else {
            return Poll::Ready(());
        };
        match pending.fenced {
            Some((since, len)) if len == other => poll(since.elapsed() >= QUIET, || {
                format!(
                    "тишина {} {} мс",
                    NAMES[1 - fence_to],
                    since.elapsed().as_millis()
                )
            }),
            _ => {
                pending.fenced = Some((Instant::now(), other));
                Poll::Pending(format!("байты {} изменились", NAMES[1 - fence_to]))
            }
        }
    }

    fn evaluate(&mut self) -> Result<(), String> {
        let got = [self.since(A), self.since(B)];
        let pending = self
            .pending
            .take()
            .ok_or("итог проверки без начала".to_owned())?;
        let fence_to = pending.fence_to;
        let mut seen = got.clone();
        seen[fence_to].pop();
        let pass = seen[A] == pending.expect[A]
            && seen[B] == pending.expect[B]
            && pending.failures.is_empty();
        for which in [A, B] {
            self.totals[which].extend_from_slice(&pending.expect[which]);
        }
        self.totals[fence_to].push(FENCE_BYTE);
        self.checks.push(CheckRecord {
            name: pending.name,
            fence_to: NAMES[fence_to],
            expected_a: fixture::hex(&pending.expect[A]),
            got_a: fixture::hex(&seen[A]),
            expected_b: fixture::hex(&pending.expect[B]),
            got_b: fixture::hex(&seen[B]),
            pass,
            failures: pending.failures,
            notes: pending.notes,
        });
        Ok(())
    }
}

fn short(nonce: &str) -> String {
    nonce
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .rev()
        .take(6)
        .collect()
}

/// Сборщик шагов: замыкания получают стенд и состояние сценария.
pub(super) struct Plan {
    stand: Rc<Stand>,
    input: Shared,
    steps: Vec<Step>,
}

impl Plan {
    pub(super) fn step(
        &mut self,
        name: &'static str,
        timeout: Duration,
        mut ready: impl FnMut(&Stand, &mut Input) -> Poll<()> + 'static,
        act: impl FnOnce(&Stand, &mut Input) -> Result<(), String> + 'static,
    ) {
        let (ready_stand, ready_input) = (Rc::clone(&self.stand), Rc::clone(&self.input));
        let (act_stand, act_input) = (Rc::clone(&self.stand), Rc::clone(&self.input));
        self.steps.push(Step::new(
            name,
            timeout,
            move || ready(&ready_stand, &mut ready_input.borrow_mut()),
            move || act(&act_stand, &mut act_input.borrow_mut()),
        ));
    }

    pub(super) fn act(
        &mut self,
        name: &'static str,
        act: impl FnOnce(&Stand, &mut Input) -> Result<(), String> + 'static,
    ) {
        self.step(name, ACT_TIMEOUT, |_, _| Poll::Ready(()), act);
    }

    pub(super) fn wait(
        &mut self,
        name: &'static str,
        timeout: Duration,
        ready: impl FnMut(&Stand, &mut Input) -> Poll<()> + 'static,
    ) {
        self.step(name, timeout, ready, |_, _| Ok(()));
    }

    /// Команда фикстуре и маркер после неё.
    pub(super) fn mode(&mut self, name: &'static str, which: usize, command: Command) {
        self.act(name, move |_, input| input.send(which, command));
        self.step(
            "квитанция ack",
            STATE_TIMEOUT,
            |_, input| input.acked(),
            move |_, input| input.print_marker(which),
        );
        self.wait(
            "маркер в видимом тексте",
            STATE_TIMEOUT,
            |_, input| input.acked().and_marker(input),
        );
    }

    /// Начало проверки: отметки байтов и ожидаемые отрезки.
    pub(super) fn begin(
        &mut self,
        name: &'static str,
        fence_to: usize,
        expect: impl FnOnce(&mut Input) -> Result<[Vec<u8>; 2], String> + 'static,
    ) {
        self.act(name, move |_, input| {
            let expect = expect(input)?;
            input.begin(name, fence_to, expect);
            Ok(())
        });
    }

    /// Ожидание байтов, ограда, тишина и итог.
    pub(super) fn finish(&mut self) {
        self.expect_bytes();
        self.fence();
    }

    /// Ожидаемые байты получателя пришли или вышел внутренний предел.
    pub(super) fn expect_bytes(&mut self) {
        self.wait(
            "ожидаемые байты",
            BYTES_TIMEOUT,
            |_, input| input.expected_arrived(),
        );
    }

    /// Ограда получателю, тишина другой фикстуры и итог проверки.
    pub(super) fn fence(&mut self) {
        self.act("ограда", |_, _| events::press(FENCE).map(|_| ()));
        self.step(
            "ограда и тишина другой фикстуры",
            FENCE_TIMEOUT,
            |_, input| input.fenced(),
            |_, input| input.evaluate(),
        );
    }

    /// Проверка целиком: начало, действие, ожидание, ограда.
    pub(super) fn check(
        &mut self,
        name: &'static str,
        fence_to: usize,
        expect: impl FnOnce(&mut Input) -> Result<[Vec<u8>; 2], String> + 'static,
        act: impl FnOnce(&Stand, &mut Input) -> Result<(), String> + 'static,
    ) {
        self.begin(name, fence_to, expect);
        self.act("события проверки", act);
        self.finish();
    }
}

trait AndMarker {
    fn and_marker(self, input: &mut Input) -> Poll<()>;
}

impl AndMarker for Poll<()> {
    fn and_marker(self, input: &mut Input) -> Poll<()> {
        match self {
            Poll::Ready(()) => input.marker_shown(),
            pending => pending,
        }
    }
}

pub(super) fn only_a(bytes: &[u8]) -> Result<[Vec<u8>; 2], String> {
    Ok([bytes.to_vec(), Vec::new()])
}

pub(super) fn press_all(keys: &[Key]) -> Result<(), String> {
    for key in keys {
        events::press(*key)?;
    }
    Ok(())
}

pub fn run(stand: Rc<Stand>) {
    run_harness(
        stand,
        Harness {
            scenario: Scenario::Input,
            prefix: "h2",
            notes: &[
                (
                    "route_note",
                    "все клавиши и кнопки идут через postEvent и обычную доставку NSApplication; ghostty_surface_key сценарий напрямую не вызывает",
                ),
                (
                    "unicode_note",
                    "синтетическое событие клавиши с символом вне BMP (эмодзи) NSTextInputContext переводит заново по keyCode и отдаёт символ раскладки; эмодзи проверяется коммитом insertText в сценарии ime",
                ),
            ],
            build: build_input,
        },
    );
}

/// Сценарий на общей обвязке: две поверхности, проверки сценария между подготовкой и закрытием.
pub(super) struct Harness {
    pub(super) scenario: Scenario,
    pub(super) prefix: &'static str,
    pub(super) notes: &'static [(&'static str, &'static str)],
    pub(super) build: fn(&mut Plan),
}

pub(super) fn run_harness(stand: Rc<Stand>, harness: Harness) {
    let mut report = stand.report(harness.scenario);
    report.push("variant", &stand.prelude.variant);
    for (key, value) in harness.notes {
        report.push(key, value);
    }
    if let Err(reason) = stand.check_session(&mut report) {
        return stand.conclude(report, Err(reason));
    }
    let (bundle, problems) = window::verify_bundle();
    report.bundle = Some(bundle);
    if !problems.is_empty() {
        return stand.conclude(
            report,
            Err(format!("не бандл пробы: {}", problems.join("; "))),
        );
    }
    let board = format!("io.github.dsbasko.shellio.probe.{}", stand.prelude.nonce);
    clipboard::use_board(Board::Named(board.clone()));
    report.push("pasteboard", &board);
    let mtm = stand.view.mtm();
    let scene = Rc::new(Scene::new(mtm, false));
    scene.install(&stand.view, &stand.window);
    let env = surface_env::prepare_in(&stand, &mut report, ColorScheme::Light, |view| {
        scene.add_tab(Zone::Center, 0, "A", view)
    });
    let env = match env {
        Ok(env) => env,
        Err(reason) => return conclude(&stand, report, Err(reason), None),
    };
    let b_view = SurfaceView::new(mtm, NSRect::ZERO);
    let b = scene
        .add_tab(Zone::Bottom, 0, "B", &b_view)
        .and_then(|()| env.add_fixture(&stand, "s2", b_view));
    let b = match b {
        Ok(b) => b,
        Err(reason) => {
            let view = env.view.clone();
            return conclude(&stand, report, Err(reason), Some((view, None)));
        }
    };
    scene.sync_focus(&stand.window);
    let paste_text = format!("paste{}", short(&stand.prelude.nonce));
    let word = format!("sel{}", short(&stand.prelude.nonce));
    let input = Rc::new(RefCell::new(Input {
        prefix: harness.prefix,
        env,
        b,
        scene,
        board,
        pending: None,
        checks: Vec::new(),
        totals: [Vec::new(), Vec::new()],
        ack: None,
        marker: None,
        markers: 0,
        paste_text,
        word,
        second: None,
        placeholder: None,
        responder_hidden: None,
        observations: Vec::new(),
        early_notes: Vec::new(),
        early_failures: Vec::new(),
        posted: Vec::new(),
    }));
    let mut plan = Plan {
        stand: Rc::clone(&stand),
        input: Rc::clone(&input),
        steps: Vec::new(),
    };
    setup(&mut plan);
    (harness.build)(&mut plan);
    close(&mut plan);
    let steps = plan.steps;
    run_steps(steps, move |result| {
        let verdict = match result {
            Ok(()) => {
                judge(&mut input.borrow_mut(), &mut report);
                Ok(())
            }
            Err(failure) => Err(failure.describe()),
        };
        if let Some(router) = crate::router::uninstall() {
            report.push_json("router", &router);
        }
        let mut input = input.borrow_mut();
        report.push_json("checks", &input.checks);
        for (key, value) in std::mem::take(&mut input.observations) {
            report.push_json(&key, &value);
        }
        if let Some((window, _)) = input.second.take() {
            window.orderOut(None);
        }
        clipboard::release_named(&input.board);
        clipboard::use_board(Board::General);
        let views = (input.env.view.clone(), Some(input.b.view.clone()));
        drop(input);
        conclude(&stand, report, verdict, Some(views));
    });
}

fn build_input(plan: &mut Plan) {
    typing(plan);
    paste(plan);
    mouse(plan);
    routing(plan);
}

/// Поверхность, которую сценарий уже закрыл сам (`chord`), второй раз не закрывается.
fn close(plan: &mut Plan) {
    plan.act(
        "закрытие обеих поверхностей",
        |_, input| {
            for which in [A, B] {
                if runtime::phase(input.id(which)) != Some(Phase::Freed) {
                    runtime::request_close(input.id(which))?;
                }
            }
            Ok(())
        },
    );
    plan.wait(
        "освобождение и выход фикстур",
        CLOSE_TIMEOUT,
        |_, input| {
            let ready = [A, B].map(|which| {
                let phase = runtime::phase(input.id(which));
                let closed = input.fixture_closed(which);
                (phase == Some(Phase::Freed) && closed, phase)
            });
            poll(ready[A].0 && ready[B].0, || {
                format!("фазы A {:?}, B {:?}", ready[A].1, ready[B].1)
            })
        },
    );
}

fn setup(plan: &mut Plan) {
    plan.wait(
        "активность, key window и фокус A",
        FOCUS_TIMEOUT,
        |stand, input| focused(stand, &input.view(A)),
    );
    plan.act("создание A и B", |_, input| {
        let created = runtime::create(input.id(A))?;
        if !matches!(created, runtime::Created::Created { .. }) {
            return Err(format!("A не создана: {created:?}"));
        }
        surface_env::create_extra(&input.b)
    });
    plan.wait(
        "готовность обеих фикстур",
        READY_TIMEOUT,
        |_, input| {
            input.env.hub.poll();
            let ready = input.env.hub.ready_count();
            poll(ready >= 2, || input.env.hub.describe())
        },
    );
    plan.mode("raw-режим A", A, Command::Raw { on: true });
    plan.mode("raw-режим B", B, Command::Raw { on: true });
    plan.wait(
        "фокус A после создания",
        FOCUS_TIMEOUT,
        |stand, input| focused(stand, &input.view(A)),
    );
}

fn typing(plan: &mut Plan) {
    plan.check(
        "ascii",
        A,
        |_| only_a(b"aZ"),
        |_, _| press_all(&[KEY_A, KEY_SHIFT_Z]),
    );
    plan.check(
        "unicode",
        A,
        |_| only_a("жé".as_bytes()),
        |_, _| press_all(&[KEY_ZHE, KEY_E_ACUTE]),
    );
    plan.check(
        "autorepeat",
        A,
        |_| only_a(b"aaaa"),
        |_, _| {
            events::key_down(KEY_A, false)?;
            for _ in 0..3 {
                events::key_down(KEY_A, true)?;
            }
            events::key_up(KEY_A).map(|_| ())
        },
    );
    plan.check(
        "ctrl",
        A,
        |_| only_a(&[0x03, 0x01]),
        |_, _| press_all(&[KEY_CTRL_C, KEY_CTRL_A]),
    );
    plan.check(
        "alt",
        A,
        |input| {
            let handle = input.view(A).handle().ok_or("A без поверхности")?;
            let alt = shellio_sys_ghostty::ghostty_input_mods_e_GHOSTTY_MODS_ALT;
            // SAFETY: ручка живой поверхности, вызов на главном потоке; это чтение настройки, не
            // ввод.
            let translation = unsafe { handle.key_translation_mods(alt) };
            let option_as_text = translation & alt != 0;
            input.note(format!(
                "ghostty_surface_key_translation_mods(ALT) = {translation:#x}: Option {}",
                if option_as_text {
                    "даёт символ раскладки"
                } else {
                    "работает как Alt"
                }
            ));
            if option_as_text {
                only_a("å".as_bytes())
            } else {
                only_a(b"\x1ba")
            }
        },
        |_, _| press_all(&[KEY_ALT_A]),
    );
    plan.check(
        "cmd-binding",
        A,
        |_| only_a(&[0x01]),
        |_, input| {
            let before = input.view(A).input_log().bindings;
            input.note(format!("привязок до {before}"));
            press_all(&[KEY_CMD_LEFT])
        },
    );
    plan.act(
        "привязка через performKeyEquivalent",
        |_, input| {
            let log = input.view(A).input_log();
            input.observe("cmd_binding_count", log.bindings);
            if log.bindings == 0 {
                return Err("Cmd-Left не прошёл performKeyEquivalent как привязка".to_owned());
            }
            Ok(())
        },
    );
    plan.check(
        "cmd-unbound",
        A,
        |_| only_a(&[]),
        |_, input| {
            let arrivals = input.view(A).input_log().arrivals.len();
            input.note(format!("приходов до {arrivals}"));
            press_all(&[KEY_CMD_Y])
        },
    );
    plan.act("Cmd-y дошёл до keyDown", |_, input| {
        let log = input.view(A).input_log();
        let reached = log
            .arrivals
            .iter()
            .any(|arrival| arrival.kind == "keyDown" && arrival.key_code == KEY_CMD_Y.code);
        input.observe("cmd_unbound_reached_key_down", reached);
        Ok(())
    });
    plan.check(
        "arrows",
        A,
        |_| only_a(b"\x1b[D\x1b[A"),
        |_, _| press_all(&[KEY_LEFT, KEY_UP]),
    );
    plan.check(
        "esc-return",
        A,
        |_| only_a(&[0x1b, 0x0d]),
        |_, _| press_all(&[KEY_ESC, events::Key::RETURN]),
    );
}

fn paste(plan: &mut Plan) {
    plan.mode(
        "bracketed paste включён",
        A,
        Command::BracketedPaste { on: true },
    );
    plan.act("текст доски", |_, input| {
        if clipboard::write_text(&input.paste_text) {
            Ok(())
        } else {
            Err("именованная доска не приняла текст".to_owned())
        }
    });
    plan.check(
        "bracketed-paste",
        A,
        |input| {
            let mut bytes = b"x\x1b[200~".to_vec();
            bytes.extend_from_slice(input.paste_text.as_bytes());
            bytes.extend_from_slice(b"\x1b[201~");
            only_a(&bytes)
        },
        |_, _| press_all(&[KEY_X, KEY_CMD_V]),
    );
    plan.act(
        "запрос буфера обмена завершён",
        |_, input| {
            let state = runtime::clipboard(input.id(A));
            input.observe("clipboard_after_paste", &state);
            match state {
                Some((0, _)) => Ok(()),
                Some((pending, _)) => Err(format!("ожидающих запросов {pending} после вставки")),
                None => Err("A не живая".to_owned()),
            }
        },
    );
    plan.mode(
        "bracketed paste выключен",
        A,
        Command::BracketedPaste { on: false },
    );
}

fn mouse(plan: &mut Plan) {
    plan.mode(
        "отчёт мыши включён",
        A,
        Command::Mouse {
            mode: MouseMode::Click,
        },
    );
    plan.check(
        "mouse-report",
        A,
        |_| {
            let (column, row) = (MOUSE_CELL.0 + 1, MOUSE_CELL.1 + 1);
            only_a(format!("\x1b[<0;{column};{row}M\x1b[<0;{column};{row}m").as_bytes())
        },
        |stand, input| {
            let point = cell_point(input, A, MOUSE_CELL.0, MOUSE_CELL.1, 0.5)?;
            click(stand, point, "клетка отчёта")
        },
    );
    plan.mode(
        "отчёт мыши выключен",
        A,
        Command::Mouse {
            mode: MouseMode::Off,
        },
    );
    plan.act("слово для выделения", |_, input| {
        let text = format!("\x1b[{};{}H{}", WORD_AT.0, WORD_AT.1, input.word);
        input.send(A, Command::Print { text })
    });
    plan.wait(
        "слово в видимом тексте",
        STATE_TIMEOUT,
        |_, input| {
            let shown = runtime::viewport_text(input.id(A)).unwrap_or_default();
            let ok = input.acked();
            match ok {
                Poll::Ready(()) => poll(shown.contains(&input.word), || {
                    format!("слова {} нет на экране A", input.word)
                }),
                pending => pending,
            }
        },
    );
    plan.begin("select-copy-paste", A, |input| {
        let word = input.word.clone();
        only_a(word.as_bytes())
    });
    plan.act(
        "выделение перетаскиванием",
        |stand, input| {
            let (row, first) = (u32::from(WORD_AT.0) - 1, u32::from(WORD_AT.1) - 1);
            let last = first + input.word.len() as u32 - 1;
            let start = cell_point(input, A, first, row, 0.25)?;
            let end = cell_point(input, A, last, row, 0.75)?;
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
        },
    );
    plan.step(
        "выделение у поверхности",
        STATE_TIMEOUT,
        |_, input| {
            let handle = input.view(A).handle();
            // SAFETY: ручка живой поверхности, чтение на главном потоке.
            let selected = handle.is_some_and(|handle| unsafe { handle.has_selection() });
            poll(selected, || "у A нет выделения".to_owned())
        },
        |_, _| press_all(&[KEY_CMD_C]),
    );
    plan.step(
        "копия на доске",
        STATE_TIMEOUT,
        |_, input| {
            let text = clipboard::read_text();
            poll(text.as_deref() == Some(input.word.as_str()), || {
                format!("на доске {text:?}, ожидалось {}", input.word)
            })
        },
        |_, input| {
            input.note("Cmd-C записал выделение на именованную доску".to_owned());
            press_all(&[KEY_CMD_V])
        },
    );
    plan.finish();
}

fn routing(plan: &mut Plan) {
    plan.check(
        "focus-a-to-b",
        B,
        |_| Ok([Vec::new(), b"b".to_vec()]),
        |stand, input| {
            click(stand, view_center(&input.view(B))?, "B")?;
            press_all(&[KEY_B])
        },
    );
    plan.act("фокус у B", |stand, input| {
        let (a, b) = (input.view(A).feeds().focus, input.view(B).feeds().focus);
        input.observe("focus_feeds_after_click_b", [&a, &b]);
        if !is_responder(stand, &input.view(B)) {
            return Err("щелчок не сделал B first responder".to_owned());
        }
        Ok(())
    });
    plan.check(
        "keyup-owner",
        A,
        |_| Ok([Vec::new(), b"q".to_vec()]),
        |stand, input| {
            events::key_down(KEY_Q, false)?;
            click(stand, view_center(&input.view(A))?, "A")?;
            events::key_up(KEY_Q).map(|_| ())
        },
    );
    plan.act("отпускание ушло владельцу", |_, input| {
        let b_log = input.view(B).input_log();
        let a_log = input.view(A).input_log();
        let owned = b_log
            .arrivals
            .iter()
            .any(|arrival| arrival.kind == "keyUp" && arrival.key_code == KEY_Q.code && arrival.owned);
        let released = b_log.sent.iter().any(|sent| {
            sent.from == "keyUp" && sent.input.keycode == u32::from(KEY_Q.code)
        });
        let a_saw = a_log
            .arrivals
            .iter()
            .any(|arrival| arrival.kind == "keyUp" && arrival.key_code == KEY_Q.code);
        input.observe("keyup_owner", (owned, released, a_saw));
        if owned && released && !a_saw {
            Ok(())
        } else {
            Err(format!(
                "keyUp q: доставлен B как владельцу {owned}, отпускание в ghostty B {released}, дошёл до A {a_saw}"
            ))
        }
    });
    key_window(plan);
    inactive(plan);
    hidden(plan);
    moved(plan);
}

fn key_window(plan: &mut Plan) {
    plan.begin("key-window", A, |_| Ok([Vec::new(), Vec::new()]));
    plan.act("второе окно", |stand, input| {
        let mtm = stand.view.mtm();
        let frame = stand.window.frame();
        let rect = NSRect::new(
            NSPoint::new(frame.origin.x + 40.0, frame.origin.y + 40.0),
            NSSize::new(320.0, 200.0),
        );
        // SAFETY: обычная инициализация окна.
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                rect,
                NSWindowStyleMask::Titled,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        // SAFETY: окно держит Rust.
        unsafe { window.setReleasedWhenClosed(false) };
        let view = StandView::new(mtm, NSRect::new(NSPoint::ZERO, rect.size));
        window.setContentView(Some(&view));
        window.makeFirstResponder(Some(&view));
        window.makeKeyAndOrderFront(None);
        input.second = Some((window, view));
        Ok(())
    });
    plan.step(
        "второе окно key",
        STATE_TIMEOUT,
        |stand, input| {
            let key = stand.app.keyWindow();
            let second = input.second.as_ref().map(|(window, _)| window.clone());
            let is_second =
                matches!((key, second), (Some(key), Some(second)) if std::ptr::eq(&*key, &*second));
            poll(is_second, || events::focus_state(&stand.app))
        },
        |_, _| press_all(&[KEY_K]),
    );
    plan.step(
        "клавиша дошла до второго окна",
        STATE_TIMEOUT,
        |_, input| {
            let arrived = input.second.as_ref().is_some_and(|(_, view)| {
                view.arrivals()
                    .iter()
                    .any(|arrival| arrival.kind == "keyUp" && arrival.key_code == KEY_K.code)
            });
            poll(arrived, || "второе окно не получило k".to_owned())
        },
        |stand, input| {
            let focus = input.view(A).feeds().focus;
            input.note(format!("подачи фокуса A при втором окне: {focus:?}"));
            if let Some((window, _)) = &input.second {
                window.orderOut(None);
            }
            stand.window.makeKeyAndOrderFront(None);
            Ok(())
        },
    );
    plan.step(
        "главное окно снова key",
        STATE_TIMEOUT,
        |stand, input| focused(stand, &input.view(A)),
        |_, input| {
            let focus = input.view(A).feeds().focus;
            input.observe("focus_feeds_key_window", &focus);
            Ok(())
        },
    );
    plan.finish();
}

fn inactive(plan: &mut Plan) {
    plan.begin("inactive-app", A, |_| Ok([Vec::new(), Vec::new()]));
    // После `deactivate` приложение оставалось активным; скрытое приложение неактивно.
    plan.act("скрытие приложения", |stand, _| {
        stand.app.hide(None);
        Ok(())
    });
    plan.step(
        "приложение неактивно",
        STATE_TIMEOUT,
        |stand, _| poll(!stand.app.isActive(), || events::focus_state(&stand.app)),
        |stand, input| {
            input.note(format!(
                "клавиша i адресована главному окну при скрытом неактивном приложении: {}",
                events::focus_state(&stand.app)
            ));
            input.posted = events::press_in(KEY_I, &stand.window)?;
            Ok(())
        },
    );
    plan.wait(
        "клавиша при неактивном приложении прошла sendEvent",
        STATE_TIMEOUT,
        |_, input| input.dispatched(),
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
    plan.step(
        "приложение снова активно",
        STATE_TIMEOUT,
        |stand, input| focused(stand, &input.view(A)),
        |_, input| {
            let app_focus = runtime::stats().map(|stats| stats.app_focus);
            input.observe("app_focus_feeds", &app_focus);
            Ok(())
        },
    );
    plan.finish();
}

fn hidden(plan: &mut Plan) {
    plan.act("щелчок в B", |stand, input| {
        click(stand, view_center(&input.view(B))?, "B")
    });
    plan.step(
        "B first responder",
        STATE_TIMEOUT,
        |stand, input| poll(is_responder(stand, &input.view(B)), || events::focus_state(&stand.app)),
        |stand, input| {
            // Вкладка-заглушка убирает B из окна; попытка вернуть ей фокус записывается.
            let placeholder = NSView::new(stand.view.mtm());
            input.scene.add_tab(Zone::Bottom, 0, "заглушка", &placeholder)?;
            let b = input.view(B);
            let in_window = b.window().is_some();
            let accepted = stand.window.makeFirstResponder(Some(&b));
            input.responder_hidden = Some(accepted);
            input.note(format!(
                "B за вкладкой-заглушкой: в окне {in_window}, makeFirstResponder(B) {accepted}, first responder {}",
                events::responder_name(&stand.window)
            ));
            input.placeholder = Some(placeholder);
            Ok(())
        },
    );
    // Получатель клавиши — фактический first responder: если им стала A, буква её.
    plan.begin("hidden-surface", A, |input| {
        let a = input.view(A);
        let a_first = a
            .window()
            .and_then(|window| window.firstResponder())
            .is_some_and(|responder| std::ptr::addr_eq(&*responder, &*a));
        input.note(format!("A first responder при скрытой B: {a_first}"));
        if a_first {
            only_a(b"h")
        } else {
            Ok([Vec::new(), Vec::new()])
        }
    });
    plan.act("клавиша при скрытой B", |_, input| {
        input.posted = events::press(KEY_H)?;
        Ok(())
    });
    plan.step(
        "клавиша при скрытой B прошла sendEvent",
        STATE_TIMEOUT,
        |_, input| input.dispatched(),
        |stand, input| {
            let seen = |log: &crate::view::InputLog| {
                log.arrivals
                    .iter()
                    .any(|arrival| arrival.kind == "keyDown" && arrival.key_code == KEY_H.code)
            };
            let (a, b) = (
                seen(&input.view(A).input_log()),
                seen(&input.view(B).input_log()),
            );
            input.note(format!("keyDown h дошёл до вида A {a}, до вида B {b}"));
            if let Some(placeholder) = input.placeholder.take() {
                input.scene.take_tab(&placeholder)?;
                placeholder.removeFromSuperview();
            }
            let back = input.scene.locate(&input.view(B));
            input.note(format!("B снова в плитке: {back:?}"));
            click(stand, view_center(&input.view(A))?, "A")
        },
    );
    plan.wait(
        "фокус A после скрытия",
        FOCUS_TIMEOUT,
        |stand, input| focused(stand, &input.view(A)),
    );
    plan.finish();
}

fn moved(plan: &mut Plan) {
    plan.check(
        "moved-zone",
        A,
        |_| only_a(b"m"),
        |stand, input| {
            let view = input.view(A);
            let title = input.scene.take_tab(&view)?;
            input.scene.add_tab(Zone::Right, 0, &title, &view)?;
            let kept = is_responder(stand, &view);
            if !kept && !input.scene.focus(&stand.window, &view) {
                return Err("окно не приняло A first responder после переноса".to_owned());
            }
            input.scene.sync_focus(&stand.window);
            input.note(format!(
                "A перенесён в {:?}, first responder сохранён AppKit {kept}",
                input.scene.locate(&view)
            ));
            press_all(&[KEY_M])
        },
    );
}

/// Точка клетки поверхности в координатах экрана; `fraction` — доля ширины клетки слева.
fn cell_point(
    input: &mut Input,
    which: usize,
    column: u32,
    row: u32,
    fraction: f64,
) -> Result<NSPoint, String> {
    let view = input.view(which);
    let handle = view.handle().ok_or("поверхность не живая")?;
    // SAFETY: ручка живой поверхности, чтение на главном потоке.
    let cell = unsafe { handle.cell(column, row) }.ok_or("ghostty_surface_read_text клетки")?;
    let size = runtime::read(input.id(which))
        .ok_or("поверхность не живая")?
        .size;
    let scale = view.backing_scale();
    let width = f64::from(size.cell_width_px) / scale;
    let height = f64::from(size.cell_height_px) / scale;
    // `baseline_y` — базовая линия строки: точка чуть выше неё лежит внутри клетки.
    let x = cell.x + width * fraction;
    let y = cell.baseline_y - height * 0.4;
    input.note(format!(
        "клетка {column},{row}: x {:.1}, базовая линия {:.1}, клетка {width:.1}×{height:.1} точек",
        cell.x, cell.baseline_y
    ));
    screen_point(&view, x, y)
}

/// Точка вида от левого верхнего угла в координатах экрана.
fn screen_point(view: &SurfaceView, x: f64, y_top: f64) -> Result<NSPoint, String> {
    let window = view.window().ok_or("вид вне окна")?;
    let point = NSPoint::new(x, view.bounds().size.height - y_top);
    Ok(window.convertPointToScreen(view.convertPoint_toView(point, None)))
}

pub(super) fn view_center(view: &SurfaceView) -> Result<NSPoint, String> {
    let size = view.bounds().size;
    screen_point(view, size.width / 2.0, size.height / 2.0)
}

pub(super) fn click(stand: &Stand, point: NSPoint, label: &str) -> Result<(), String> {
    events::post_mouse(NSEventType::LeftMouseDown, &stand.window, point, label)?;
    events::post_mouse(NSEventType::LeftMouseUp, &stand.window, point, label)?;
    Ok(())
}

pub(super) fn is_responder(stand: &Stand, view: &SurfaceView) -> bool {
    stand
        .window
        .firstResponder()
        .as_deref()
        .is_some_and(|responder| std::ptr::addr_eq(responder, view))
}

pub(super) fn focused(stand: &Stand, view: &SurfaceView) -> Poll<()> {
    let key_is_main = stand
        .app
        .keyWindow()
        .as_deref()
        .is_some_and(|key| std::ptr::eq(key, &*stand.window));
    poll(
        stand.app.isActive() && key_is_main && is_responder(stand, view),
        || events::focus_state(&stand.app),
    )
}

/// Выводы H2 по проверкам и итоговым потокам байтов.
fn judge(input: &mut Input, report: &mut Report) {
    for check in &input.checks {
        report.finding(
            &format!("{}-{}", input.prefix, check.name),
            Requirement::Required,
            if check.pass {
                Verdict::Pass
            } else {
                Verdict::Fail
            },
            format!(
                "A: ожидалось {}, пришло {}; B: ожидалось {}, пришло {}; ограда у {}{}{}",
                check.expected_a,
                check.got_a,
                check.expected_b,
                check.got_b,
                check.fence_to,
                if check.failures.is_empty() {
                    String::new()
                } else {
                    format!("; расхождения: {}", check.failures.join("; "))
                },
                if check.notes.is_empty() {
                    String::new()
                } else {
                    format!("; {}", check.notes.join("; "))
                }
            ),
        );
    }
    let totals = [input.bytes(A), input.bytes(B)];
    let exact = totals[A] == input.totals[A] && totals[B] == input.totals[B];
    report.finding(
        &format!("{}-exact-bytes-one-receiver", input.prefix),
        Requirement::Required,
        if exact { Verdict::Pass } else { Verdict::Fail },
        format!(
            "весь поток A {} байт (ожидалось {}), B {} байт (ожидалось {}); точное совпадение {exact}",
            totals[A].len(),
            input.totals[A].len(),
            totals[B].len(),
            input.totals[B].len()
        ),
    );
    if !exact {
        report.push("bytes_a", fixture::hex(&totals[A]));
        report.push("bytes_a_expected", fixture::hex(&input.totals[A]));
        report.push("bytes_b", fixture::hex(&totals[B]));
        report.push("bytes_b_expected", fixture::hex(&input.totals[B]));
    }
    report.push_json("responder_hidden_accepted", &input.responder_hidden);
}

/// Итог: журналы ввода и подачи обоих видов, затем завершение runtime.
fn conclude(
    stand: &Stand,
    mut report: Report,
    verdict: Result<(), String>,
    views: Option<(Retained<SurfaceView>, Option<Retained<SurfaceView>>)>,
) {
    let mut list = Vec::new();
    if let Some((a, b)) = views {
        report.push_json("input_log_a", &a.input_log());
        list.push(a);
        if let Some(b) = b {
            report.push_json("input_log_b", &b.input_log());
            report.push_json("surface_view_feeds_b", &b.feeds());
            list.push(b);
        }
    }
    surface_env::finish_views(stand, report, verdict, list);
}
