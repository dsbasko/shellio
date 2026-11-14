//! Режимы `Input` и `Navigation` и аккорд выхода `Ctrl-\` (ADR-0010) без продуктового keymap.
//!
//! Маршрутизатор стоит в `sendEvent` приложения пробы после composition IME и перед поверхностью:
//! нажатие при marked text у first responder он не видит. В `Input` он забирает только первый
//! аккорд, остальное идёт обычной доставкой. В `Navigation` он забирает каждое новое нажатие.
//! Съеденное нажатие забирает свои `keyUp` и автоповтор (владелец «маршрутизатор» в таблице
//! владельцев вида). Нажатие, начатое до аккорда, отпускается у своего прежнего владельца.
//! `flagsChanged` маршрутизатор пропускает в обоих режимах: модификатор не меняет режим и не
//! снимает ожидание, а без `report_all` kitty keyboard protocol ghostty байтов на него не даёт.
//!
//! «Сразу после» — следующее нажатие клавиши, которое не модификатор и не автоповтор первого
//! аккорда; миллисекундного окна нет. Ожидание второго аккорда:
//!
//! | Событие при ожидании | Ожидание | Режим после | Что происходит |
//! | --- | --- | --- | --- |
//! | второй `Ctrl-\` | снято | `Input` | сохранённое событие уходит прежнему получателю парой нажатие и отпускание |
//! | автоповтор первого аккорда | остаётся | `Navigation` | съеден |
//! | модификатор | остаётся | `Navigation` | проходит обычной доставкой |
//! | другая клавиша | снято | `Navigation` | обрабатывается как клавиша `Navigation` |
//! | автоповтор другой клавиши | уже снято | `Navigation` | у своего владельца |
//! | смена фокуса | снято | `Navigation` | — |
//! | щелчок по поверхности | снято | `Input` | щелчок доставляется, поверхность получает фокус |
//! | щелчок вне поверхности | снято | `Navigation` | щелчок доставляется |
//! | прокрутка | снято | `Navigation` | прокрутка доставляется |
//! | открытие наложения | снято | прежний режим возвращается при закрытии | ввод у наложения |
//! | переключение приложения | снято | `Navigation` | — |
//! | закрытие или смена поколения получателя | снято | `Navigation` | — |
//!
//! Клавиши `Navigation` сопоставляются по физическому `keyCode`, поэтому работают на любой
//! раскладке: `h/j/k/l` — фокус в соседнюю плитку, `Ctrl-o` — на один шаг к плитке, с которой
//! пришёл последний переход `h/j/k/l` (полной истории нет), `i` и `Enter` — `Input`, сочетание с
//! `Cmd` — только меню приложения. Остальные нажатия съедаются, `Ctrl-\` без ожидания тоже.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use objc2::rc::{Retained, Weak};
use objc2::{MainThreadMarker, MainThreadOnly, Message, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationWillResignActiveNotification, NSEvent, NSEventType, NSResponder,
    NSView, NSWindow,
};
use objc2_foundation::{NSNotification, NSNotificationCenter, NSObject, NSRect};
use serde::Serialize;

use crate::ghostty::surface::SurfaceKey;
use crate::keys::{KeyInput, flags};
use crate::scenario::counters::{Live, counter};
use crate::scene::{Scene, Zone};
use crate::view::{self, SurfaceView};

counter!(ROUTER_OBSERVER, "ProbeRouterObserver");

/// Предел журнала переходов.
const LOG_LIMIT: usize = 2000;
/// Допуск сравнения краёв плиток, точки.
const EDGE: f64 = 0.5;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    Input,
    Navigation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Direction {
    Left,
    Down,
    Up,
    Right,
}

/// Коды клавиш macOS, которые различает маршрутизатор.
pub mod code {
    pub const BACKSLASH: u16 = 42;
    pub const H: u16 = 4;
    pub const J: u16 = 38;
    pub const K: u16 = 40;
    pub const L: u16 = 37;
    pub const O: u16 = 31;
    pub const I: u16 = 34;
    pub const RETURN: u16 = 36;
    pub const KEYPAD_ENTER: u16 = 76;
}

/// Модификаторы без сторон, `Caps Lock` и `Fn`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Mods(u8);

impl Mods {
    pub const NONE: Mods = Mods(0);
    pub const SHIFT: Mods = Mods(1);
    pub const CONTROL: Mods = Mods(2);
    pub const OPTION: Mods = Mods(4);
    pub const COMMAND: Mods = Mods(8);

    /// По флагам события AppKit.
    pub fn of(raw: u64) -> Mods {
        [
            (flags::SHIFT, Mods::SHIFT),
            (flags::CONTROL, Mods::CONTROL),
            (flags::OPTION, Mods::OPTION),
            (flags::COMMAND, Mods::COMMAND),
        ]
        .into_iter()
        .filter(|(flag, _)| raw & flag != 0)
        .fold(Mods::NONE, |mods, (_, bit)| Mods(mods.0 | bit.0))
    }

    pub fn contains(self, other: Mods) -> bool {
        self.0 & other.0 == other.0
    }
}

/// Нажатие клавиши, как его видит маршрутизатор.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Press {
    pub code: u16,
    pub mods: Mods,
    pub repeat: bool,
}

impl Press {
    /// Аккорд выхода: физическая клавиша `\` ровно с `Control`.
    pub fn is_chord(&self) -> bool {
        self.code == code::BACKSLASH && self.mods == Mods::CONTROL
    }
}

/// Что сделать с нажатием.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action<T> {
    /// Не маршрутизатору: обычная доставка.
    Pass,
    /// Съедено без действия.
    Swallow,
    /// Первый аккорд: `Navigation` и ожидание второго.
    FirstChord,
    /// Второй аккорд: сохранённое событие прежнему получателю, `Input`. `None` — получатель
    /// первого аккорда был не поверхностью.
    Replay(Option<T>),
    Focus(Direction),
    ReturnToSource,
    /// `i` или `Enter`.
    EnterInput,
    /// Сочетание с `Cmd` в `Navigation`.
    Menu,
}

impl<T> Action<T> {
    pub fn name(&self) -> &'static str {
        match self {
            Action::Pass => "pass",
            Action::Swallow => "swallow",
            Action::FirstChord => "first-chord",
            Action::Replay(_) => "replay",
            Action::Focus(Direction::Left) => "focus-left",
            Action::Focus(Direction::Down) => "focus-down",
            Action::Focus(Direction::Up) => "focus-up",
            Action::Focus(Direction::Right) => "focus-right",
            Action::ReturnToSource => "return-to-source",
            Action::EnterInput => "enter-input",
            Action::Menu => "menu",
        }
    }
}

/// Почему снято ожидание второго аккорда.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Cancel {
    OtherKey,
    FocusChanged,
    Click,
    Scroll,
    Overlay,
    AppSwitch,
    ReceiverGone,
}

/// Запись журнала переходов.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Transition {
    pub signal: &'static str,
    pub from: Mode,
    pub to: Mode,
    pub waiting_before: bool,
    pub waiting_after: bool,
    pub action: &'static str,
    pub cancel: Option<Cancel>,
}

struct Waiting<T> {
    saved: Option<T>,
}

/// Автомат режимов. `T` — сохранённое событие первого аккорда вместе с его получателем.
pub struct Router<T> {
    mode: Mode,
    waiting: Option<Waiting<T>>,
    /// Режим до открытия наложения.
    before_overlay: Option<Mode>,
    log: Vec<Transition>,
}

impl<T> Default for Router<T> {
    fn default() -> Self {
        Router {
            mode: Mode::Input,
            waiting: None,
            before_overlay: None,
            log: Vec::new(),
        }
    }
}

impl<T> Router<T> {
    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn is_waiting(&self) -> bool {
        self.waiting.is_some()
    }

    pub fn saved(&self) -> Option<&T> {
        self.waiting
            .as_ref()
            .and_then(|waiting| waiting.saved.as_ref())
    }

    pub fn log(&self) -> &[Transition] {
        &self.log
    }

    fn record(
        &mut self,
        signal: &'static str,
        from: Mode,
        waiting_before: bool,
        action: &'static str,
        cancel: Option<Cancel>,
    ) {
        if self.log.len() < LOG_LIMIT {
            self.log.push(Transition {
                signal,
                from,
                to: self.mode,
                waiting_before,
                waiting_after: self.waiting.is_some(),
                action,
                cancel,
            });
        }
    }

    /// Новое нажатие или автоповтор нажатия, которое съел маршрутизатор. `saved` — копия
    /// события и получатель на случай первого аккорда; иначе отбрасывается.
    pub fn key(&mut self, press: Press, saved: Option<T>) -> Action<T> {
        let (from, waiting_before) = (self.mode, self.waiting.is_some());
        let mut cancel = None;
        let action = if press.repeat {
            Action::Swallow
        } else {
            match self.mode {
                Mode::Input if press.is_chord() => {
                    self.mode = Mode::Navigation;
                    self.waiting = Some(Waiting { saved });
                    Action::FirstChord
                }
                Mode::Input => Action::Pass,
                Mode::Navigation => match self.waiting.take() {
                    Some(waiting) if press.is_chord() => {
                        self.mode = Mode::Input;
                        Action::Replay(waiting.saved)
                    }
                    Some(_) => {
                        cancel = Some(Cancel::OtherKey);
                        self.navigation_key(press)
                    }
                    None => self.navigation_key(press),
                },
            }
        };
        let signal = if press.repeat { "repeat" } else { "key" };
        self.record(signal, from, waiting_before, action.name(), cancel);
        action
    }

    fn navigation_key(&mut self, press: Press) -> Action<T> {
        if press.mods.contains(Mods::COMMAND) {
            return Action::Menu;
        }
        match (press.code, press.mods) {
            (code::H, Mods::NONE) => Action::Focus(Direction::Left),
            (code::J, Mods::NONE) => Action::Focus(Direction::Down),
            (code::K, Mods::NONE) => Action::Focus(Direction::Up),
            (code::L, Mods::NONE) => Action::Focus(Direction::Right),
            (code::O, Mods::CONTROL) => Action::ReturnToSource,
            (code::I | code::RETURN | code::KEYPAD_ENTER, Mods::NONE) => {
                self.mode = Mode::Input;
                Action::EnterInput
            }
            _ => Action::Swallow,
        }
    }

    /// Модификатор: режим и ожидание не меняются.
    pub fn modifier(&mut self) {
        let (from, waiting) = (self.mode, self.waiting.is_some());
        self.record("modifier", from, waiting, "pass", None);
    }

    fn cancel(&mut self, signal: &'static str, reason: Cancel) {
        let (from, waiting) = (self.mode, self.waiting.is_some());
        self.waiting = None;
        self.record(signal, from, waiting, "none", waiting.then_some(reason));
    }

    pub fn focus_lost(&mut self) {
        self.cancel("focus-lost", Cancel::FocusChanged);
    }

    /// Щелчок по поверхности включает `Input` (ADR-0010).
    pub fn click(&mut self, on_surface: bool) {
        let (from, waiting) = (self.mode, self.waiting.is_some());
        self.waiting = None;
        if on_surface {
            self.mode = Mode::Input;
        }
        let signal = if on_surface {
            "click-surface"
        } else {
            "click-elsewhere"
        };
        self.record(
            signal,
            from,
            waiting,
            "pass",
            waiting.then_some(Cancel::Click),
        );
    }

    pub fn scroll(&mut self) {
        self.cancel("scroll", Cancel::Scroll);
    }

    pub fn app_switched(&mut self) {
        self.cancel("app-switch", Cancel::AppSwitch);
    }

    /// Получатель первого аккорда закрыт или сменил поколение.
    pub fn receiver_gone(&mut self, matches: impl Fn(&T) -> bool) {
        if self.saved().is_some_and(matches) {
            self.cancel("receiver-gone", Cancel::ReceiverGone);
        }
    }

    pub fn overlay_opened(&mut self) {
        if self.before_overlay.is_none() {
            self.before_overlay = Some(self.mode);
        }
        self.cancel("overlay-opened", Cancel::Overlay);
    }

    pub fn overlay_closed(&mut self) {
        let (from, waiting) = (self.mode, self.waiting.is_some());
        if let Some(mode) = self.before_overlay.take() {
            self.mode = mode;
        }
        self.record("overlay-closed", from, waiting, "none", None);
    }
}

/// Плитка, в которую переходит фокус из плитки `from` в направлении `direction`. Кандидаты
/// лежат по эту сторону края `from`; выигрывает ближайший край, затем наибольшее перекрытие по
/// другой оси, затем ближайший центр. Координаты окна: `y` растёт вверх.
pub fn neighbor(frames: &[NSRect], from: usize, direction: Direction) -> Option<usize> {
    let origin = *frames.get(from)?;
    let (min_x, max_x) = (origin.origin.x, origin.origin.x + origin.size.width);
    let (min_y, max_y) = (origin.origin.y, origin.origin.y + origin.size.height);
    let overlap = |a0: f64, a1: f64, b0: f64, b1: f64| (a1.min(b1) - a0.max(b0)).max(0.0);
    let center = |frame: &NSRect| {
        (
            frame.origin.x + frame.size.width / 2.0,
            frame.origin.y + frame.size.height / 2.0,
        )
    };
    let here = center(&origin);
    frames
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != from)
        .filter_map(|(index, frame)| {
            let (c_min_x, c_max_x) = (frame.origin.x, frame.origin.x + frame.size.width);
            let (c_min_y, c_max_y) = (frame.origin.y, frame.origin.y + frame.size.height);
            let there = center(frame);
            let (gap, cross, offset) = match direction {
                Direction::Right if c_min_x >= max_x - EDGE => (
                    c_min_x - max_x,
                    overlap(min_y, max_y, c_min_y, c_max_y),
                    (there.1 - here.1).abs(),
                ),
                Direction::Left if c_max_x <= min_x + EDGE => (
                    min_x - c_max_x,
                    overlap(min_y, max_y, c_min_y, c_max_y),
                    (there.1 - here.1).abs(),
                ),
                Direction::Up if c_min_y >= max_y - EDGE => (
                    c_min_y - max_y,
                    overlap(min_x, max_x, c_min_x, c_max_x),
                    (there.0 - here.0).abs(),
                ),
                Direction::Down if c_max_y <= min_y + EDGE => (
                    min_y - c_max_y,
                    overlap(min_x, max_x, c_min_x, c_max_x),
                    (there.0 - here.0).abs(),
                ),
                _ => return None,
            };
            Some((index, cross > 0.0, gap, cross, offset))
        })
        .min_by(|a, b| {
            b.1.cmp(&a.1)
                .then(a.2.total_cmp(&b.2))
                .then(b.3.total_cmp(&a.3))
                .then(a.4.total_cmp(&b.4))
        })
        .map(|(index, ..)| index)
}

// ---- AppKit ----

/// Сохранённое при первом аккорде: first responder окна и, если это поверхность, её поколение
/// и собственная копия нажатия и отпускания.
pub struct Saved {
    responder: Option<Weak<NSResponder>>,
    surface: Option<Receiver>,
}

struct Receiver {
    view: Weak<SurfaceView>,
    key: SurfaceKey,
    press: KeyInput,
    release: KeyInput,
}

/// Повтор аккорда выхода.
#[derive(Clone, Debug, Serialize)]
pub struct ReplayRecord {
    pub receiver: Option<String>,
    pub delivered: bool,
    pub reason: Option<String>,
    pub inputs: Vec<KeyInput>,
}

/// Переход фокуса клавишей `Navigation`.
#[derive(Clone, Debug, Serialize)]
pub struct MoveRecord {
    pub action: &'static str,
    pub from: Option<(Zone, usize)>,
    pub to: Option<(Zone, usize)>,
    pub moved: bool,
}

/// Что маршрутизатор сделал с событиями, кроме переходов автомата.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Record {
    pub replays: Vec<ReplayRecord>,
    pub moves: Vec<MoveRecord>,
    /// Нажатия, пропущенные к composition IME.
    pub composing_passed: u64,
    pub menu: u64,
    pub repeats_swallowed: u64,
    pub key_ups_swallowed: u64,
}

/// Состояние маршрутизатора для сценария.
#[derive(Clone, Debug, Serialize)]
pub struct Snapshot {
    pub mode: Mode,
    pub waiting: bool,
    pub saved_press: Option<KeyInput>,
    pub has_source: bool,
    pub transitions: Vec<Transition>,
    pub record: Record,
    /// Уведомления, пришедшие, пока состояние маршрутизатора было занято.
    pub reentrant: u64,
}

struct Glue {
    router: Router<Saved>,
    scene: Rc<Scene>,
    window: Weak<NSWindow>,
    /// Плитка до последнего перехода `h/j/k/l`.
    source: Option<Weak<NSView>>,
    record: Record,
    observer: Retained<RouterObserver>,
}

thread_local! {
    static GLUE: RefCell<Option<Glue>> = const { RefCell::new(None) };
    static REENTRANT: Cell<u64> = const { Cell::new(0) };
}

/// Действие над состоянием маршрутизатора. AppKit и libghostty вызываются только вне его
/// заимствования: `makeFirstResponder` и `ghostty_surface_key` возвращаются в вид синхронно.
fn with<R>(f: impl FnOnce(&mut Glue) -> R) -> Option<R> {
    GLUE.with(|cell| match cell.try_borrow_mut() {
        Ok(mut glue) => glue.as_mut().map(f),
        Err(_) => {
            REENTRANT.with(|count| count.set(count.get() + 1));
            None
        }
    })
}

pub struct ObserverIvars {
    _live: Live,
}

define_class!(
    /// Наблюдатель `NSApplicationWillResignActive`: переключение приложения снимает ожидание.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeRouterObserver"]
    #[ivars = ObserverIvars]
    struct RouterObserver;

    impl RouterObserver {
        #[unsafe(method(appWillResignActive:))]
        fn app_will_resign_active(&self, _notification: &NSNotification) {
            with(|glue| glue.router.app_switched());
        }
    }
);

impl RouterObserver {
    fn new(mtm: MainThreadMarker) -> Retained<RouterObserver> {
        let this = RouterObserver::alloc(mtm).set_ivars(ObserverIvars {
            _live: ROUTER_OBSERVER.live(),
        });
        // SAFETY: `init` — назначенный инициализатор NSObject.
        let observer: Retained<RouterObserver> = unsafe { msg_send![super(this), init] };
        // SAFETY: имя уведомления — неизменяемая константа AppKit, селектор реализован
        // наблюдателем; подписка снимается в `uninstall`.
        unsafe {
            NSNotificationCenter::defaultCenter().addObserver_selector_name_object(
                &observer,
                sel!(appWillResignActive:),
                Some(NSApplicationWillResignActiveNotification),
                None,
            );
        }
        observer
    }
}

/// Ставит маршрутизатор на окно сцены в режиме `Input`.
pub fn install(scene: Rc<Scene>, window: &NSWindow) {
    scene.set_mode(Mode::Input);
    let glue = Glue {
        router: Router::default(),
        scene,
        window: Weak::new(window),
        source: None,
        record: Record::default(),
        observer: RouterObserver::new(window.mtm()),
    };
    let previous = GLUE.with(|cell| cell.borrow_mut().replace(glue));
    drop(previous);
    REENTRANT.with(|count| count.set(0));
}

/// Снимает маршрутизатор и отдаёт его последнее состояние.
pub fn uninstall() -> Option<Snapshot> {
    let last = snapshot();
    let glue = GLUE.with(|cell| cell.borrow_mut().take());
    if let Some(glue) = glue {
        // SAFETY: снимается подписка, поставленная в `RouterObserver::new`.
        unsafe { NSNotificationCenter::defaultCenter().removeObserver(&glue.observer) };
    }
    last
}

pub fn snapshot() -> Option<Snapshot> {
    with(|glue| Snapshot {
        mode: glue.router.mode(),
        waiting: glue.router.is_waiting(),
        saved_press: glue
            .router
            .saved()
            .and_then(|saved| saved.surface.as_ref())
            .map(|receiver| receiver.press.clone()),
        has_source: glue
            .source
            .as_ref()
            .is_some_and(|source| source.load().is_some()),
        transitions: glue.router.log().to_vec(),
        record: glue.record.clone(),
        reentrant: REENTRANT.with(Cell::get),
    })
}

fn installed_window() -> Option<Retained<NSWindow>> {
    with(|glue| glue.window.load()).flatten()
}

/// Тон плитки с фокусом по режиму автомата.
fn sync_tone() {
    if let Some((scene, mode)) = with(|glue| (Rc::clone(&glue.scene), glue.router.mode())) {
        scene.set_mode(mode);
    }
}

/// Вызывается `sendEvent` пробы до обычной доставки. `true` — событие съедено.
pub fn intercept(event: &NSEvent) -> bool {
    let Some(window) = installed_window() else {
        return false;
    };
    // `keyUp` и автоповтор съеденного нажатия остаются у маршрутизатора в любом окне.
    if let Some(owned) = owned_by_router(event) {
        return owned;
    }
    if !addressed_to(event, &window) {
        return false;
    }
    match event.r#type() {
        NSEventType::KeyDown => key_down(event, &window),
        NSEventType::FlagsChanged => {
            with(|glue| glue.router.modifier());
            false
        }
        NSEventType::LeftMouseDown | NSEventType::RightMouseDown | NSEventType::OtherMouseDown => {
            let on_surface = hits_surface(event, &window);
            with(|glue| glue.router.click(on_surface));
            sync_tone();
            false
        }
        NSEventType::ScrollWheel => {
            with(|glue| glue.router.scroll());
            false
        }
        _ => false,
    }
}

/// `Some(true)` — `keyUp` или автоповтор нажатия, которое съел маршрутизатор: событие съедено.
/// `Some(false)` — такое же событие чужого нажатия: оно идёт своему владельцу. `None` — не они.
fn owned_by_router(event: &NSEvent) -> Option<bool> {
    // `keyCode` и `isARepeat` у событий не клавиатуры бросают исключение Objective-C.
    let repeat = match event.r#type() {
        NSEventType::KeyUp => false,
        NSEventType::KeyDown if event.isARepeat() => true,
        _ => return None,
    };
    let code = event.keyCode();
    let press = repeat.then(|| Press {
        code,
        mods: Mods::of(event.modifierFlags().0 as u64),
        repeat: true,
    });
    if !view::router_owns(code) {
        return Some(false);
    }
    match press {
        Some(press) => with(|glue| {
            glue.router.key(press, None);
            glue.record.repeats_swallowed += 1;
        }),
        None => {
            view::release_router(code);
            with(|glue| glue.record.key_ups_swallowed += 1)
        }
    };
    Some(true)
}

/// Событие окна сцены. Событие без окна (номер 0, например прокрутка из `CGEvent`) AppKit
/// адресует key-окну, поэтому оно относится к сцене, пока её окно key.
fn addressed_to(event: &NSEvent, window: &NSWindow) -> bool {
    match event.windowNumber() {
        0 => window.isKeyWindow(),
        number => number == window.windowNumber(),
    }
}

/// Вызывается `sendEvent` пробы после обычной доставки: щелчок мог сменить плитку с фокусом.
pub fn after_dispatch(event: &NSEvent) {
    let kind = event.r#type();
    if ![
        NSEventType::LeftMouseDown,
        NSEventType::RightMouseDown,
        NSEventType::OtherMouseDown,
    ]
    .contains(&kind)
    {
        return;
    }
    if let (Some(window), Some(scene)) = (installed_window(), with(|glue| Rc::clone(&glue.scene))) {
        scene.sync_focus(&window);
    }
}

fn same(responder: Option<&NSResponder>, saved: &Saved) -> bool {
    match (responder, saved.responder.as_ref().and_then(Weak::load)) {
        (Some(current), Some(saved)) => std::ptr::eq(current, &*saved),
        (None, None) => true,
        _ => false,
    }
}

/// Новое нажатие окна сцены.
fn key_down(event: &NSEvent, window: &NSWindow) -> bool {
    let code = event.keyCode();
    let press = Press {
        code,
        mods: Mods::of(event.modifierFlags().0 as u64),
        repeat: false,
    };
    let responder = window.firstResponder();
    let surface = responder
        .as_deref()
        .and_then(|responder| responder.downcast_ref::<SurfaceView>())
        .map(|surface| surface.retain());
    if surface
        .as_ref()
        .is_some_and(|surface| surface.is_composing())
    {
        with(|glue| glue.record.composing_passed += 1);
        return false;
    }
    with(|glue| {
        let moved = glue
            .router
            .saved()
            .is_some_and(|saved| !same(responder.as_deref(), saved));
        if moved {
            glue.router.focus_lost();
        }
    });
    let saved = press.is_chord().then(|| Saved {
        responder: responder.as_deref().map(Weak::new),
        surface: surface.and_then(|surface| receiver(&surface, event)),
    });
    let Some(action) = with(|glue| glue.router.key(press, saved)) else {
        return false;
    };
    if matches!(action, Action::Pass) {
        return false;
    }
    view::claim_for_router(code);
    match action {
        Action::Pass | Action::Swallow | Action::FirstChord | Action::EnterInput => {}
        Action::Replay(saved) => replay(saved),
        Action::Focus(direction) => move_focus(window, Some(direction)),
        Action::ReturnToSource => move_focus(window, None),
        Action::Menu => {
            let app = NSApplication::sharedApplication(window.mtm());
            if let Some(menu) = app.mainMenu() {
                menu.performKeyEquivalent(event);
            }
            with(|glue| glue.record.menu += 1);
        }
    }
    sync_tone();
    true
}

fn receiver(surface: &SurfaceView, event: &NSEvent) -> Option<Receiver> {
    let key = surface.surface_key()?;
    let (press, release) = surface.chord_inputs(event)?;
    Some(Receiver {
        view: Weak::new(surface),
        key,
        press,
        release,
    })
}

fn replay(saved: Option<Saved>) {
    let record = match saved.and_then(|saved| saved.surface) {
        None => ReplayRecord {
            receiver: None,
            delivered: false,
            reason: Some("получатель первого аккорда — не поверхность".to_owned()),
            inputs: Vec::new(),
        },
        Some(receiver) => {
            let inputs = vec![receiver.press, receiver.release];
            let result = match receiver.view.load() {
                Some(view) => view.replay(receiver.key, &inputs),
                None => Err("вид освобождён"),
            };
            ReplayRecord {
                receiver: Some(receiver.key.to_string()),
                delivered: result.is_ok(),
                reason: result.err().map(str::to_owned),
                inputs,
            }
        }
    };
    with(|glue| glue.record.replays.push(record));
}

/// `Some(direction)` — переход `h/j/k/l` с запоминанием источника, `None` — возврат к нему.
fn move_focus(window: &NSWindow, direction: Option<Direction>) {
    let Some(scene) = with(|glue| Rc::clone(&glue.scene)) else {
        return;
    };
    let current: Option<Retained<NSView>> = window
        .firstResponder()
        .as_deref()
        .and_then(|responder| responder.downcast_ref::<NSView>())
        .map(|view| view.retain());
    let from = current.as_deref().and_then(|view| scene.locate(view));
    let target = match direction {
        Some(direction) => {
            let targets = scene.targets();
            let frames: Vec<NSRect> = targets.iter().map(|target| target.frame).collect();
            from.and_then(|(zone, part)| {
                targets
                    .iter()
                    .position(|target| target.zone == zone && target.part == part)
            })
            .and_then(|index| neighbor(&frames, index, direction))
            .map(|index| targets[index].view.clone())
        }
        None => with(|glue| glue.source.take())
            .flatten()
            .and_then(|source| source.load())
            .filter(|view| {
                view.window()
                    .is_some_and(|owner| std::ptr::eq(&*owner, window))
            }),
    };
    let moved = target
        .as_ref()
        .is_some_and(|view| scene.focus(window, view));
    if moved && direction.is_some() {
        let source = current.as_deref().map(Weak::new);
        with(|glue| glue.source = source);
    }
    let action = match direction {
        Some(direction) => Action::<()>::Focus(direction).name(),
        None => Action::<()>::ReturnToSource.name(),
    };
    let to = target.as_deref().and_then(|view| scene.locate(view));
    with(|glue| {
        glue.record.moves.push(MoveRecord {
            action,
            from,
            to: if moved { to } else { None },
            moved,
        })
    });
}

/// Нажатие кнопки мыши попадает в вид поверхности или в его потомка.
fn hits_surface(event: &NSEvent, window: &NSWindow) -> bool {
    let Some(content) = window.contentView() else {
        return false;
    };
    // SAFETY: `superview` возвращает удержанного родителя или nil; вызов на главном потоке.
    let point = match unsafe { content.superview() } {
        Some(frame) => frame.convertPoint_fromView(event.locationInWindow(), None),
        None => event.locationInWindow(),
    };
    let mut hit = content.hitTest(point);
    while let Some(view) = hit {
        if view.downcast_ref::<SurfaceView>().is_some() {
            return true;
        }
        // SAFETY: см. выше.
        hit = unsafe { view.superview() };
    }
    false
}

/// Вид поверхности потерял фокус: если он получатель первого аккорда, ожидание снимается.
pub fn focus_lost(view: &SurfaceView) {
    with(|glue| {
        let receiver = glue
            .router
            .saved()
            .and_then(|saved| saved.responder.as_ref())
            .and_then(Weak::load);
        if receiver.is_some_and(|receiver| std::ptr::addr_eq(Retained::as_ptr(&receiver), view)) {
            glue.router.focus_lost();
        }
    });
}

/// Наложение открывается: ожидание снимается, режим запоминается до закрытия. Вызывается до
/// того, как наложение станет key, иначе потеря фокуса вида снимет ожидание раньше.
pub fn overlay_opened() {
    with(|glue| glue.router.overlay_opened());
    sync_tone();
}

/// Наложение закрыто: возвращается режим до открытия.
pub fn overlay_closed() {
    with(|glue| glue.router.overlay_closed());
    sync_tone();
}

/// Поверхность освобождается: ожидание её второго аккорда снимается.
pub fn receiver_gone(key: SurfaceKey) {
    with(|glue| {
        glue.router.receiver_gone(|saved| {
            saved
                .surface
                .as_ref()
                .is_some_and(|receiver| receiver.key == key)
        })
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_foundation::{NSPoint, NSSize};

    fn press(code: u16, mods: Mods) -> Press {
        Press {
            code,
            mods,
            repeat: false,
        }
    }

    fn repeat(code: u16, mods: Mods) -> Press {
        Press {
            code,
            mods,
            repeat: true,
        }
    }

    const CHORD: Press = Press {
        code: code::BACKSLASH,
        mods: Mods::CONTROL,
        repeat: false,
    };

    /// Маршрутизатор в `Navigation` с ожиданием второго аккорда от получателя `"A"`.
    fn waiting() -> Router<&'static str> {
        let mut router = Router::default();
        assert_eq!(router.key(CHORD, Some("A")), Action::FirstChord);
        assert_eq!(router.mode(), Mode::Navigation);
        assert!(router.is_waiting());
        assert_eq!(router.saved(), Some(&"A"));
        router
    }

    fn last_cancel(router: &Router<&str>) -> Option<Cancel> {
        router.log().last().and_then(|transition| transition.cancel)
    }

    #[test]
    fn modifiers_ignore_sides_caps_and_fn() {
        assert_eq!(Mods::of(0), Mods::NONE);
        assert_eq!(
            Mods::of(flags::CONTROL | flags::RIGHT_CONTROL | flags::CAPS_LOCK | (1 << 23)),
            Mods::CONTROL
        );
        assert_eq!(
            Mods::of(flags::SHIFT | flags::COMMAND),
            Mods(Mods::SHIFT.0 | Mods::COMMAND.0)
        );
        assert!(CHORD.is_chord());
        assert!(!press(code::BACKSLASH, Mods::NONE).is_chord());
        assert!(!press(code::BACKSLASH, Mods(Mods::CONTROL.0 | Mods::SHIFT.0)).is_chord());
        assert!(!press(code::L, Mods::CONTROL).is_chord());
    }

    #[test]
    fn input_passes_everything_but_the_chord() {
        let mut router: Router<&str> = Router::default();
        assert_eq!(router.mode(), Mode::Input);
        for key in [
            press(code::H, Mods::NONE),
            press(code::I, Mods::NONE),
            press(8, Mods::CONTROL),
            press(code::O, Mods::CONTROL),
            press(9, Mods::COMMAND),
            press(code::BACKSLASH, Mods::NONE),
            repeat(0, Mods::NONE),
        ] {
            let action = router.key(key, Some("A"));
            assert!(
                matches!(action, Action::Pass | Action::Swallow),
                "{key:?}: {action:?}"
            );
        }
        assert_eq!(router.mode(), Mode::Input);
        assert!(!router.is_waiting());
    }

    #[test]
    fn first_chord_enters_navigation_and_second_replays() {
        let mut router = waiting();
        assert_eq!(router.key(CHORD, None), Action::Replay(Some("A")));
        assert_eq!(router.mode(), Mode::Input);
        assert!(!router.is_waiting());
        // Получатель первого аккорда не поверхность: повтор без события.
        let mut other: Router<&str> = Router::default();
        other.key(CHORD, None);
        assert_eq!(other.key(CHORD, Some("B")), Action::Replay(None));
    }

    #[test]
    fn autorepeat_of_the_first_chord_is_not_a_second_chord() {
        let mut router = waiting();
        for _ in 0..3 {
            assert_eq!(
                router.key(repeat(code::BACKSLASH, Mods::CONTROL), None),
                Action::Swallow
            );
        }
        assert!(router.is_waiting(), "автоповтор ожидание не снимает");
        assert_eq!(router.mode(), Mode::Navigation);
        assert_eq!(router.key(CHORD, None), Action::Replay(Some("A")));
    }

    #[test]
    fn modifier_between_chords_keeps_waiting() {
        let mut router = waiting();
        router.modifier();
        router.modifier();
        assert!(router.is_waiting());
        assert_eq!(router.log().last().map(|t| t.signal), Some("modifier"));
        assert_eq!(router.key(CHORD, None), Action::Replay(Some("A")));
    }

    #[test]
    fn other_key_cancels_and_is_a_navigation_key() {
        let mut router = waiting();
        assert_eq!(
            router.key(press(code::J, Mods::NONE), None),
            Action::Focus(Direction::Down)
        );
        assert_eq!(last_cancel(&router), Some(Cancel::OtherKey));
        assert!(!router.is_waiting());
        assert_eq!(router.mode(), Mode::Navigation);
        // Автоповтор другой клавиши второго аккорда не возвращает.
        assert_eq!(
            router.key(repeat(code::J, Mods::NONE), None),
            Action::Swallow
        );
        assert_eq!(router.key(CHORD, None), Action::Swallow, "ожидания нет");
        assert_eq!(router.mode(), Mode::Navigation);
        // Неназначенная клавиша тоже снимает ожидание и съедается.
        let mut router = waiting();
        assert_eq!(router.key(press(7, Mods::NONE), None), Action::Swallow);
        assert_eq!(last_cancel(&router), Some(Cancel::OtherKey));
        assert_eq!(router.key(CHORD, None), Action::Swallow);
    }

    #[test]
    fn navigation_keys_by_physical_code() {
        let mut router = waiting();
        let table = [
            (press(code::H, Mods::NONE), Action::Focus(Direction::Left)),
            (press(code::J, Mods::NONE), Action::Focus(Direction::Down)),
            (press(code::K, Mods::NONE), Action::Focus(Direction::Up)),
            (press(code::L, Mods::NONE), Action::Focus(Direction::Right)),
            (press(code::O, Mods::CONTROL), Action::ReturnToSource),
            (press(code::O, Mods::NONE), Action::Swallow),
            (press(code::H, Mods::SHIFT), Action::Swallow),
            (press(9, Mods::COMMAND), Action::Menu),
            (press(code::H, Mods::COMMAND), Action::Menu),
        ];
        for (key, expected) in table {
            assert_eq!(router.key(key, None), expected, "{key:?}");
            assert_eq!(router.mode(), Mode::Navigation);
        }
        for enter in [code::I, code::RETURN, code::KEYPAD_ENTER] {
            if router.mode() == Mode::Input {
                router.key(CHORD, None);
            }
            assert_eq!(router.mode(), Mode::Navigation);
            assert_eq!(
                router.key(press(enter, Mods::NONE), None),
                Action::EnterInput
            );
            assert_eq!(router.mode(), Mode::Input);
        }
    }

    #[test]
    fn focus_click_scroll_and_app_switch_cancel_waiting() {
        type Signal = fn(&mut Router<&'static str>);
        let cases: [(Signal, Cancel, Mode); 5] = [
            (
                |router| router.focus_lost(),
                Cancel::FocusChanged,
                Mode::Navigation,
            ),
            (|router| router.click(true), Cancel::Click, Mode::Input),
            (
                |router| router.click(false),
                Cancel::Click,
                Mode::Navigation,
            ),
            (|router| router.scroll(), Cancel::Scroll, Mode::Navigation),
            (
                |router| router.app_switched(),
                Cancel::AppSwitch,
                Mode::Navigation,
            ),
        ];
        for (signal, reason, mode) in cases {
            let mut router = waiting();
            signal(&mut router);
            assert!(!router.is_waiting(), "{reason:?}");
            assert_eq!(last_cancel(&router), Some(reason));
            assert_eq!(router.mode(), mode, "{reason:?}");
            let after = router.key(CHORD, Some("B"));
            match mode {
                Mode::Input => assert_eq!(after, Action::FirstChord, "{reason:?}"),
                Mode::Navigation => assert_eq!(after, Action::Swallow, "{reason:?}"),
            }
        }
        // Без ожидания сигнал ничего не снимает и причину не пишет.
        let mut router: Router<&str> = Router::default();
        router.scroll();
        assert_eq!(last_cancel(&router), None);
    }

    #[test]
    fn receiver_gone_cancels_only_for_its_receiver() {
        let mut router = waiting();
        router.receiver_gone(|saved| *saved == "B");
        assert!(router.is_waiting(), "закрылась другая поверхность");
        router.receiver_gone(|saved| *saved == "A");
        assert!(!router.is_waiting());
        assert_eq!(last_cancel(&router), Some(Cancel::ReceiverGone));
        assert_eq!(router.mode(), Mode::Navigation);
        assert_eq!(router.key(CHORD, None), Action::Swallow);
        // Получатель — не поверхность: закрытие поверхности ожидание не снимает.
        let mut other: Router<&str> = Router::default();
        other.key(CHORD, None);
        other.receiver_gone(|_| true);
        assert!(other.is_waiting());
    }

    #[test]
    fn overlay_cancels_and_restores_mode() {
        let mut router = waiting();
        router.overlay_opened();
        assert!(!router.is_waiting());
        assert_eq!(last_cancel(&router), Some(Cancel::Overlay));
        router.overlay_closed();
        assert_eq!(router.mode(), Mode::Navigation);
        assert_eq!(
            router.key(CHORD, None),
            Action::Swallow,
            "ожидание не вернулось"
        );
        let mut input: Router<&str> = Router::default();
        input.overlay_opened();
        input.overlay_closed();
        assert_eq!(input.mode(), Mode::Input);
    }

    #[test]
    fn log_records_every_transition() {
        let mut router = waiting();
        router.modifier();
        router.key(press(code::J, Mods::NONE), None);
        router.key(press(code::I, Mods::NONE), None);
        let signals: Vec<(&str, &str, Mode, Mode)> = router
            .log()
            .iter()
            .map(|t| (t.signal, t.action, t.from, t.to))
            .collect();
        assert_eq!(
            signals,
            [
                ("key", "first-chord", Mode::Input, Mode::Navigation),
                ("modifier", "pass", Mode::Navigation, Mode::Navigation),
                ("key", "focus-down", Mode::Navigation, Mode::Navigation),
                ("key", "enter-input", Mode::Navigation, Mode::Input),
            ]
        );
        assert!(router.log()[1].waiting_before && router.log()[1].waiting_after);
        assert!(router.log()[2].waiting_before && !router.log()[2].waiting_after);
    }

    fn rect(x: f64, y: f64, width: f64, height: f64) -> NSRect {
        NSRect::new(NSPoint::new(x, y), NSSize::new(width, height))
    }

    /// Плитки сцены 1180×720: левая, центр, правая, нижняя.
    fn scene() -> [NSRect; 4] {
        [
            rect(6.0, 6.0, 236.0, 708.0),
            rect(248.0, 232.0, 560.0, 482.0),
            rect(814.0, 6.0, 360.0, 708.0),
            rect(248.0, 6.0, 560.0, 220.0),
        ]
    }

    #[test]
    fn neighbors_follow_the_four_zone_layout() {
        let frames = scene();
        let (left, center, right, bottom) = (0, 1, 2, 3);
        let go = |from, direction| neighbor(&frames, from, direction);
        assert_eq!(go(center, Direction::Down), Some(bottom));
        assert_eq!(go(center, Direction::Left), Some(left));
        assert_eq!(go(center, Direction::Right), Some(right));
        assert_eq!(go(center, Direction::Up), None);
        assert_eq!(go(bottom, Direction::Up), Some(center));
        assert_eq!(go(bottom, Direction::Left), Some(left));
        assert_eq!(go(bottom, Direction::Right), Some(right));
        assert_eq!(go(bottom, Direction::Down), None);
        assert_eq!(
            go(left, Direction::Right),
            Some(center),
            "большее перекрытие"
        );
        assert_eq!(go(right, Direction::Left), Some(center), "ближний край");
        assert_eq!(go(left, Direction::Left), None);
        assert_eq!(go(left, Direction::Up), None);
        assert_eq!(neighbor(&frames, 9, Direction::Up), None);
    }

    #[test]
    fn neighbors_without_overlap_take_the_nearest() {
        // Две части левой зоны одна над другой и центр справа от нижней части.
        let frames = [
            rect(0.0, 360.0, 200.0, 340.0),
            rect(0.0, 0.0, 200.0, 340.0),
            rect(220.0, 0.0, 400.0, 100.0),
        ];
        assert_eq!(neighbor(&frames, 0, Direction::Down), Some(1));
        assert_eq!(neighbor(&frames, 1, Direction::Up), Some(0));
        assert_eq!(
            neighbor(&frames, 0, Direction::Right),
            Some(2),
            "без перекрытия"
        );
        assert_eq!(
            neighbor(&frames, 2, Direction::Left),
            Some(1),
            "с перекрытием"
        );
    }
}
