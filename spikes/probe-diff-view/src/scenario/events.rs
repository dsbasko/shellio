//! Доставка синтетических событий: подкласс `NSApplication` с отметками `sendEvent`, таблица
//! отправленных событий, фабрики клавиш и мыши и защита от внешнего ввода.
//!
//! Источник — `spikes/probe-appkit-shell/src/scenario/events.rs` (P0.2), упрощён: нет
//! наблюдателя конца прохода отображения (его место занимает отметка `drawmark`), нет
//! перехватчиков возврата и завершения через `Finisher`. Внешний ввод не доставляется и
//! записывается, сценарий читает его через `external_input` и сам даёт ошибку исполнения. Кольцо
//! следа — `watchdog::SharedTrace`. Клавиши — набор команд diff из ADR-0010 и `docs/ux.md`,
//! у мыши есть модификаторы, число щелчков и протяжка.
//!
//! `postEvent` кладёт в очередь событие, а `sendEvent` получает другой объект с тем же
//! содержимым: тип, окно, `eventNumber` у мыши и `keyCode` сохраняются, `timestamp` округляется
//! до наносекунды. Поэтому событие узнаётся по сигнатуре, а `timestamp` соседних событий стенд
//! разводит не меньше чем на `TIMESTAMP_SPACING`.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{ClassType, MainThreadMarker, MainThreadOnly, Message, define_class, msg_send};
use objc2_app_kit::{
    NSApplication, NSEvent, NSEventModifierFlags, NSEventType, NSResponder, NSScreen, NSWindow,
};
use objc2_core_foundation::CGPoint;
use objc2_core_graphics::{CGEvent, CGEventField, CGWarpMouseCursorPosition};
use objc2_foundation::{NSObjectProtocol, NSPoint, NSProcessInfo, NSRect, NSString};
use serde::Serialize;

use super::watchdog::{SharedTrace, push_trace};

/// Минимальный шаг `timestamp` между отправленными событиями.
pub const TIMESTAMP_SPACING: f64 = 10e-6;
/// Допуск сравнения `timestamp`: округление до наносекунды и запас.
const TIMESTAMP_TOLERANCE: f64 = 2e-6;
/// Сколько служебных событий записать в след вместе с их источником.
const SERVICE_EVENTS_TRACED: usize = 12;

/// Событие ввода, которое стенд обязан узнать: клавиатура, кнопки и движение мыши, прокрутка.
/// Служебные события AppKit и системы к вводу не относятся.
pub fn is_input(kind: NSEventType) -> bool {
    [
        NSEventType::KeyDown,
        NSEventType::KeyUp,
        NSEventType::FlagsChanged,
        NSEventType::LeftMouseDown,
        NSEventType::LeftMouseUp,
        NSEventType::RightMouseDown,
        NSEventType::RightMouseUp,
        NSEventType::OtherMouseDown,
        NSEventType::OtherMouseUp,
        NSEventType::MouseMoved,
        NSEventType::LeftMouseDragged,
        NSEventType::RightMouseDragged,
        NSEventType::OtherMouseDragged,
        NSEventType::ScrollWheel,
    ]
    .contains(&kind)
}

fn is_mouse(kind: NSEventType) -> bool {
    [
        NSEventType::LeftMouseDown,
        NSEventType::LeftMouseUp,
        NSEventType::RightMouseDown,
        NSEventType::RightMouseUp,
        NSEventType::OtherMouseDown,
        NSEventType::OtherMouseUp,
        NSEventType::MouseMoved,
        NSEventType::LeftMouseDragged,
        NSEventType::RightMouseDragged,
        NSEventType::OtherMouseDragged,
    ]
    .contains(&kind)
}

fn is_key(kind: NSEventType) -> bool {
    [
        NSEventType::KeyDown,
        NSEventType::KeyUp,
        NSEventType::FlagsChanged,
    ]
    .contains(&kind)
}

/// Содержимое события, которое переживает путь `postEvent` → `sendEvent`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Signature {
    pub kind: NSEventType,
    pub window: isize,
    pub timestamp: f64,
    /// `eventNumber` у событий мыши, у остальных он не определён.
    pub event_number: Option<isize>,
    pub key_code: Option<u16>,
}

impl Signature {
    pub fn of(event: &NSEvent) -> Signature {
        let kind = event.r#type();
        Signature {
            kind,
            window: event.windowNumber(),
            timestamp: event.timestamp(),
            event_number: is_mouse(kind).then(|| event.eventNumber()),
            key_code: is_key(kind).then(|| event.keyCode()),
        }
    }

    pub fn matches(&self, received: &Signature) -> bool {
        self.kind == received.kind
            && self.window == received.window
            && (self.timestamp - received.timestamp).abs() <= TIMESTAMP_TOLERANCE
            && self.event_number == received.event_number
            && self.key_code == received.key_code
    }
}

/// Следующий `timestamp`: текущее время от старта системы, но не ближе `TIMESTAMP_SPACING` к
/// предыдущему.
pub fn next_timestamp(now: f64, last: Option<f64>) -> f64 {
    match last {
        Some(last) if now < last + TIMESTAMP_SPACING => last + TIMESTAMP_SPACING,
        _ => now,
    }
}

/// Отметки одного отправленного события.
#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    pub number: u64,
    pub label: String,
    pub signature: Signature,
    /// Состояние AppKit, прочитанное перед отправкой.
    pub before: String,
    pub posted: Duration,
    pub dispatch_start: Option<Duration>,
    pub send_return: Option<Duration>,
    /// Событие убрано из таблицы, не пройдя `sendEvent`: его забрал вложенный цикл.
    pub undispatched: bool,
}

/// Запись события для `result.json`, время — миллисекунды от установки стенда.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct EventRecord {
    pub number: u64,
    pub label: String,
    pub window: isize,
    pub before: String,
    pub posted_ms: f64,
    pub dispatch_start_ms: Option<f64>,
    pub send_return_ms: Option<f64>,
    pub undispatched: bool,
}

impl Record {
    pub fn export(&self) -> EventRecord {
        EventRecord {
            number: self.number,
            label: self.label.clone(),
            window: self.signature.window,
            before: self.before.clone(),
            posted_ms: to_ms(self.posted),
            dispatch_start_ms: self.dispatch_start.map(to_ms),
            send_return_ms: self.send_return.map(to_ms),
            undispatched: self.undispatched,
        }
    }
}

pub fn to_ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1e3
}

fn ms(duration: Duration) -> String {
    format!("{:.3}ms", duration.as_secs_f64() * 1e3)
}

/// Таблица «номер → удерживаемый объект события» и отметки всех отправленных событий.
pub struct Table<T> {
    next: u64,
    held: Vec<(u64, Signature, T)>,
    records: Vec<Record>,
}

impl<T> Default for Table<T> {
    fn default() -> Self {
        Table {
            next: 1,
            held: Vec::new(),
            records: Vec::new(),
        }
    }
}

impl<T> Table<T> {
    /// Номер для следующего события; его же получает `eventNumber` событий мыши.
    pub fn peek_number(&self) -> u64 {
        self.next
    }

    pub fn register(
        &mut self,
        label: String,
        signature: Signature,
        before: String,
        posted: Duration,
        held: T,
    ) -> u64 {
        let number = self.next;
        self.next += 1;
        self.held.push((number, signature, held));
        self.records.push(Record {
            number,
            label,
            signature,
            before,
            posted,
            dispatch_start: None,
            send_return: None,
            undispatched: false,
        });
        number
    }

    /// Номер удерживаемого события с такой сигнатурой, ещё не начавшего dispatch.
    pub fn find(&self, received: &Signature) -> Option<u64> {
        self.held
            .iter()
            .find(|(number, signature, _)| {
                signature.matches(received)
                    && self
                        .record(*number)
                        .is_some_and(|record| record.dispatch_start.is_none())
            })
            .map(|(number, _, _)| *number)
    }

    pub fn mark_start(&mut self, number: u64, at: Duration) {
        if let Some(record) = self.record_mut(number) {
            record.dispatch_start = Some(at);
        }
    }

    /// Отмечает возврат `sendEvent` и отпускает объект события.
    pub fn mark_return(&mut self, number: u64, at: Duration) {
        if let Some(record) = self.record_mut(number) {
            record.send_return = Some(at);
        }
        self.held.retain(|(held, _, _)| *held != number);
    }

    /// Отпускает все удерживаемые события. Не начавшие dispatch помечаются как забранные
    /// вложенным циклом.
    pub fn clear(&mut self) {
        let numbers: Vec<u64> = self.held.drain(..).map(|(number, _, _)| number).collect();
        for number in numbers {
            if let Some(record) = self.record_mut(number)
                && record.dispatch_start.is_none()
            {
                record.undispatched = true;
            }
        }
    }

    pub fn held_count(&self) -> usize {
        self.held.len()
    }

    /// События, чей `sendEvent` начался и ещё не вернулся.
    pub fn open(&self) -> Vec<u64> {
        self.records
            .iter()
            .filter(|record| record.dispatch_start.is_some() && record.send_return.is_none())
            .map(|record| record.number)
            .collect()
    }

    pub fn record(&self, number: u64) -> Option<&Record> {
        self.records.iter().find(|record| record.number == number)
    }

    fn record_mut(&mut self, number: u64) -> Option<&mut Record> {
        self.records
            .iter_mut()
            .find(|record| record.number == number)
    }

    pub fn records(&self) -> &[Record] {
        &self.records
    }
}

/// Клавиша с согласованными `keyCode`, `characters` и `charactersIgnoringModifiers`. Коды —
/// раскладка ANSI, источник ввода латинский (предусловие плана).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key {
    pub label: &'static str,
    pub code: u16,
    pub characters: &'static str,
    pub ignoring: &'static str,
    pub flags: NSEventModifierFlags,
}

impl Key {
    pub const J: Key = Key::plain("j", 38);
    pub const K: Key = Key::plain("k", 40);
    pub const V: Key = Key::plain("v", 9);
    pub const C: Key = Key::plain("c", 8);
    pub const G: Key = Key::plain("g", 5);
    pub const RIGHT_BRACKET: Key = Key::plain("]", 30);
    pub const LEFT_BRACKET: Key = Key::plain("[", 33);
    /// `charactersIgnoringModifiers` сохраняет Shift, поэтому у `G` обе строки заглавные.
    pub const SHIFT_G: Key = Key {
        label: "G",
        code: 5,
        characters: "G",
        ignoring: "G",
        flags: NSEventModifierFlags::Shift,
    };
    pub const CTRL_D: Key = Key {
        label: "Ctrl-d",
        code: 2,
        characters: "\u{4}",
        ignoring: "d",
        flags: NSEventModifierFlags::Control,
    };
    pub const CTRL_U: Key = Key {
        label: "Ctrl-u",
        code: 32,
        characters: "\u{15}",
        ignoring: "u",
        flags: NSEventModifierFlags::Control,
    };
    pub const CMD_RETURN: Key = Key {
        label: "Cmd-Enter",
        code: 36,
        characters: "\r",
        ignoring: "\r",
        flags: NSEventModifierFlags::Command,
    };
    pub const ESCAPE: Key = Key::plain_with("Escape", 53, "\u{1b}");
    /// Клавиша открытия фикстуры, только для стенда пробы: в keymap продукта её нет (⚠️ решение
    /// исполнителя задачи 7).
    pub const OPEN: Key = Key::plain("o", 31);

    const fn plain(characters: &'static str, code: u16) -> Key {
        Key::plain_with(characters, code, characters)
    }

    const fn plain_with(label: &'static str, code: u16, characters: &'static str) -> Key {
        Key {
            label,
            code,
            characters,
            ignoring: characters,
            flags: NSEventModifierFlags::empty(),
        }
    }

    /// Клавиши модификаторов, нажатые вместе с этой клавишей: (флаг, `keyCode`).
    pub fn modifiers(&self) -> Vec<(NSEventModifierFlags, u16)> {
        [
            (NSEventModifierFlags::Command, 55),
            (NSEventModifierFlags::Shift, 56),
            (NSEventModifierFlags::Option, 58),
            (NSEventModifierFlags::Control, 59),
        ]
        .into_iter()
        .filter(|(flag, _)| self.flags.contains(*flag))
        .collect()
    }
}

/// Последовательность клавиш команды diff по её имени в keymap: `gg` и `]c` — две клавиши.
pub fn sequence(name: &str) -> Option<&'static [Key]> {
    const GG: [Key; 2] = [Key::G, Key::G];
    const NEXT_CHANGE: [Key; 2] = [Key::RIGHT_BRACKET, Key::C];
    const PREVIOUS_CHANGE: [Key; 2] = [Key::LEFT_BRACKET, Key::C];
    Some(match name {
        "j" => &[Key::J],
        "k" => &[Key::K],
        "v" => &[Key::V],
        "c" => &[Key::C],
        "G" => &[Key::SHIFT_G],
        "gg" => &GG,
        "]c" => &NEXT_CHANGE,
        "[c" => &PREVIOUS_CHANGE,
        "Ctrl-d" => &[Key::CTRL_D],
        "Ctrl-u" => &[Key::CTRL_U],
        "Cmd-Enter" => &[Key::CMD_RETURN],
        "Escape" => &[Key::ESCAPE],
        _ => return None,
    })
}

/// Переход точки из координат AppKit (начало внизу слева главного экрана) в глобальные
/// координаты Core Graphics (начало вверху слева главного экрана).
pub fn ns_to_cg(point: NSPoint, primary_height: f64) -> CGPoint {
    CGPoint::new(point.x, primary_height - point.y)
}

pub fn contains(rect: NSRect, point: NSPoint) -> bool {
    point.x >= rect.origin.x
        && point.x < rect.origin.x + rect.size.width
        && point.y >= rect.origin.y
        && point.y < rect.origin.y + rect.size.height
}

pub fn inset(rect: NSRect, by: f64) -> NSRect {
    NSRect::new(
        NSPoint::new(rect.origin.x + by, rect.origin.y + by),
        objc2_foundation::NSSize::new(
            (rect.size.width - 2.0 * by).max(0.0),
            (rect.size.height - 2.0 * by).max(0.0),
        ),
    )
}

pub fn center(rect: NSRect) -> NSPoint {
    NSPoint::new(
        rect.origin.x + rect.size.width / 2.0,
        rect.origin.y + rect.size.height / 2.0,
    )
}

pub fn describe_rect(rect: NSRect) -> String {
    format!(
        "{},{} {}x{}",
        rect.origin.x, rect.origin.y, rect.size.width, rect.size.height
    )
}

pub fn describe_point(point: NSPoint) -> String {
    format!("{},{}", point.x, point.y)
}

/// Источник события по полям его `CGEvent`.
pub struct Source {
    pub pid: i64,
    pub state: i64,
    /// У события есть `CGEvent`; без него поля читаются нулями и ничего не доказывают.
    pub has_cg: bool,
}

impl Source {
    pub fn of(event: &NSEvent) -> Source {
        let cg = event.CGEvent();
        let field = |field| CGEvent::integer_value_field(cg.as_deref(), field);
        Source {
            pid: field(CGEventField::EventSourceUnixProcessID),
            state: field(CGEventField::EventSourceStateID),
            has_cg: cg.is_some(),
        }
    }

    fn describe(&self) -> String {
        if self.has_cg {
            format!("source_pid={} source_state={}", self.pid, self.state)
        } else {
            "cg_event=nil".to_owned()
        }
    }
}

/// Движение мыши, созданное самим процессом, — служебное событие AppKit, например после
/// закрытия меню. Клавиши и кнопки этого исключения не получают: стенд создаёт их тем же
/// процессом, и промах сопоставления не должен пройти молча.
pub fn is_internal(kind: NSEventType, source: &Source, own_pid: u32) -> bool {
    kind == NSEventType::MouseMoved && source.has_cg && source.pid == i64::from(own_pid)
}

// --- AppKit ---------------------------------------------------------------------------------

define_class!(
    /// `NSApp` сценариев. Без установленного стенда пропускает события без отметок.
    #[unsafe(super(NSApplication, NSResponder, objc2_foundation::NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeDiffViewApplication"]
    pub struct ProbeApplication;

    impl ProbeApplication {
        #[unsafe(method(sendEvent:))]
        fn send_event(&self, event: &NSEvent) {
            match admit(event) {
                Admit::Pass => self.dispatch(event),
                Admit::Ours(number) => {
                    self.dispatch(event);
                    returned(number);
                }
                Admit::External => {}
            }
        }
    }
);

impl ProbeApplication {
    fn dispatch(&self, event: &NSEvent) {
        // SAFETY: вызов реализации `sendEvent:` суперкласса с тем же событием.
        let () = unsafe { msg_send![super(self), sendEvent: event] };
    }
}

/// Создаёт `NSApp` класса `ProbeApplication`. Вызывается до первого `sharedApplication`.
pub fn shared_application(_mtm: MainThreadMarker) -> Result<Retained<NSApplication>, String> {
    // SAFETY: `+sharedApplication`, вызванный у подкласса до создания `NSApp`, создаёт `NSApp`
    // этого класса и возвращает его.
    let app: Retained<NSApplication> =
        unsafe { msg_send![ProbeApplication::class(), sharedApplication] };
    if app.isKindOfClass(ProbeApplication::class()) {
        Ok(app)
    } else {
        let object: &AnyObject = &app;
        Err(format!(
            "NSApp создан классом {}, а не ProbeDiffViewApplication",
            object.class().name().to_string_lossy()
        ))
    }
}

/// Открытый жест мыши: окно и последняя точка для отпускания при закрытии.
struct OpenGesture {
    window: Retained<NSWindow>,
    point: Rc<Cell<NSPoint>>,
    released: Rc<Cell<bool>>,
    label: String,
}

/// Стенд доставки событий процесса сценария.
pub struct Bench {
    app: Retained<NSApplication>,
    trace: SharedTrace,
    epoch: Instant,
    table: Table<Retained<NSEvent>>,
    last_timestamp: Option<f64>,
    external: Option<String>,
    service_traced: usize,
    internal: Vec<String>,
    gestures: Vec<OpenGesture>,
}

thread_local! {
    static BENCH: RefCell<Option<Bench>> = const { RefCell::new(None) };
}

/// Ставит стенд: с этого момента событие ввода вне таблицы не доставляется и записывается как
/// внешний ввод. Время отметок — от этого вызова.
pub fn install(app: Retained<NSApplication>, trace: SharedTrace) {
    let bench = Bench {
        app,
        trace,
        epoch: Instant::now(),
        table: Table::default(),
        last_timestamp: None,
        external: None,
        service_traced: 0,
        internal: Vec::new(),
        gestures: Vec::new(),
    };
    BENCH.with(|cell| *cell.borrow_mut() = Some(bench));
}

/// Снимает стенд и отпускает удерживаемые события.
pub fn uninstall() {
    BENCH.with(|cell| cell.borrow_mut().take());
}

fn with_bench<R>(f: impl FnOnce(&mut Bench) -> R) -> Option<R> {
    BENCH.with(|cell| cell.borrow_mut().as_mut().map(f))
}

enum Admit {
    Pass,
    Ours(u64),
    External,
}

fn admit(event: &NSEvent) -> Admit {
    let received = Signature::of(event);
    with_bench(|bench| {
        if let Some(number) = bench.table.find(&received) {
            let at = bench.epoch.elapsed();
            bench.table.mark_start(number, at);
            push_trace(&bench.trace, format!("{} dispatch #{number}", ms(at)));
            return Admit::Ours(number);
        }
        let source = Source::of(event);
        let description = format!(
            "type={} window={} timestamp={:.6} keyCode={:?} eventNumber={:?} {}",
            received.kind.0,
            received.window,
            received.timestamp,
            received.key_code,
            received.event_number,
            source.describe()
        );
        let at = bench.epoch.elapsed();
        if !is_input(received.kind) {
            if bench.service_traced < SERVICE_EVENTS_TRACED {
                bench.service_traced += 1;
                push_trace(
                    &bench.trace,
                    format!("{} служебное событие {description}", ms(at)),
                );
            }
            return Admit::Pass;
        }
        if is_internal(received.kind, &source, std::process::id()) {
            push_trace(
                &bench.trace,
                format!("{} внутреннее событие AppKit {description}", ms(at)),
            );
            bench.internal.push(format!("{} {description}", ms(at)));
            return Admit::Pass;
        }
        push_trace(
            &bench.trace,
            format!("{} внешний ввод: {description}", ms(at)),
        );
        bench.external.get_or_insert(description);
        Admit::External
    })
    .unwrap_or(Admit::Pass)
}

fn returned(number: u64) {
    with_bench(|bench| {
        let at = bench.epoch.elapsed();
        bench.table.mark_return(number, at);
        push_trace(&bench.trace, format!("{} return #{number}", ms(at)));
    });
}

/// Первое событие ввода вне таблицы стенда. Сценарий превращает его в ошибку исполнения:
/// прогон с чужим вводом повторяется.
pub fn external_input() -> Option<String> {
    with_bench(|bench| bench.external.clone()).flatten()
}

/// Служебные события AppKit, созданные самим процессом и пропущенные без отметок.
pub fn internal_events() -> Vec<String> {
    with_bench(|bench| bench.internal.clone()).unwrap_or_default()
}

/// Строка следа в кольцо сторожа с отметкой времени стенда.
pub fn note(line: impl AsRef<str>) {
    with_bench(|bench| {
        let at = bench.epoch.elapsed();
        push_trace(&bench.trace, format!("{} {}", ms(at), line.as_ref()));
    });
}

/// Отметки события по номеру.
pub fn record(number: u64) -> Option<Record> {
    with_bench(|bench| bench.table.record(number).cloned()).flatten()
}

/// Отметки всех событий для `result.json`.
pub fn export() -> Vec<EventRecord> {
    with_bench(|bench| bench.table.records().iter().map(Record::export).collect())
        .unwrap_or_default()
}

/// События, чей `sendEvent` начался и ещё не вернулся: открытый цикл отслеживания.
pub fn open_dispatch() -> Vec<u64> {
    with_bench(|bench| bench.table.open()).unwrap_or_default()
}

/// Отпускает удерживаемые события перед подсчётом живых объектов.
pub fn clear() {
    with_bench(|bench| bench.table.clear());
}

/// Время от установки стенда.
pub fn elapsed() -> Option<Duration> {
    with_bench(|bench| bench.epoch.elapsed())
}

impl Bench {
    fn post(&mut self, label: String, event: Retained<NSEvent>, before: String) -> u64 {
        let signature = Signature::of(&event);
        let posted = self.epoch.elapsed();
        push_trace(
            &self.trace,
            format!("{} post #{} {label}", ms(posted), self.table.peek_number()),
        );
        let number = self
            .table
            .register(label, signature, before, posted, event.clone());
        self.app.postEvent_atStart(&event, false);
        number
    }

    fn timestamp(&mut self) -> f64 {
        let timestamp = next_timestamp(
            NSProcessInfo::processInfo().systemUptime(),
            self.last_timestamp,
        );
        self.last_timestamp = Some(timestamp);
        timestamp
    }
}

/// Фактическое состояние фокуса, прочитанное перед событием.
pub fn focus_state(app: &NSApplication) -> String {
    let window = |window: Option<Retained<NSWindow>>| match window {
        Some(window) => format!("#{}", window.windowNumber()),
        None => "none".to_owned(),
    };
    let key = app.keyWindow();
    let responder = key
        .as_deref()
        .map_or_else(|| "none".to_owned(), responder_name);
    format!(
        "isActive={} keyWindow={} mainWindow={} firstResponder={} cursor={}",
        app.isActive(),
        window(key),
        window(app.mainWindow()),
        responder,
        describe_point(NSEvent::mouseLocation())
    )
}

/// Класс first responder окна.
pub fn responder_name(window: &NSWindow) -> String {
    let Some(responder) = window.firstResponder() else {
        return "none".to_owned();
    };
    let object: &AnyObject = &responder;
    object.class().name().to_string_lossy().into_owned()
}

fn no_bench() -> String {
    "стенд не установлен".to_owned()
}

/// Пара `keyDown` и `keyUp`, для модификаторов — `flagsChanged` до и после. Адресат —
/// фактическое key window, прочитанное перед каждым событием.
pub fn press(key: Key) -> Result<Vec<u64>, String> {
    let modifiers = key.modifiers();
    let mut numbers = Vec::new();
    let mut held = NSEventModifierFlags::empty();
    for (flag, code) in &modifiers {
        held |= *flag;
        numbers.push(post_key(
            NSEventType::FlagsChanged,
            &format!("flagsChanged+{code}"),
            *code,
            "",
            "",
            held,
        )?);
    }
    for kind in [NSEventType::KeyDown, NSEventType::KeyUp] {
        let name = if kind == NSEventType::KeyDown {
            "keyDown"
        } else {
            "keyUp"
        };
        numbers.push(post_key(
            kind,
            &format!("{name} {}", key.label),
            key.code,
            key.characters,
            key.ignoring,
            key.flags,
        )?);
    }
    for (flag, code) in modifiers.iter().rev() {
        held.remove(*flag);
        numbers.push(post_key(
            NSEventType::FlagsChanged,
            &format!("flagsChanged-{code}"),
            *code,
            "",
            "",
            held,
        )?);
    }
    Ok(numbers)
}

/// Все клавиши команды diff подряд, номера событий по порядку.
pub fn press_sequence(name: &str) -> Result<Vec<u64>, String> {
    let keys = sequence(name).ok_or_else(|| format!("нет клавиш для команды «{name}»"))?;
    let mut numbers = Vec::new();
    for key in keys {
        numbers.extend(press(*key)?);
    }
    Ok(numbers)
}

fn post_key(
    kind: NSEventType,
    label: &str,
    code: u16,
    characters: &str,
    ignoring: &str,
    flags: NSEventModifierFlags,
) -> Result<u64, String> {
    with_bench(|bench| {
        let before = focus_state(&bench.app);
        let window = bench.app.keyWindow().map_or(0, |window| window.windowNumber());
        let timestamp = bench.timestamp();
        let event = NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
            kind,
            NSPoint::ZERO,
            flags,
            timestamp,
            window,
            None,
            &NSString::from_str(characters),
            &NSString::from_str(ignoring),
            false,
            code,
        )
        .ok_or_else(|| format!("NSEvent не создал событие {label}"))?;
        Ok(bench.post(label.to_owned(), event, before))
    })
    .unwrap_or_else(|| Err(no_bench()))
}

/// Начатый жест левой кнопки. Продолжение адресуется окну, получившему нажатие, как при
/// захвате мыши WindowServer.
pub struct Gesture {
    window: Retained<NSWindow>,
    point: Rc<Cell<NSPoint>>,
    frame: NSRect,
    flags: NSEventModifierFlags,
    clicks: isize,
    released: Rc<Cell<bool>>,
    pub down: u64,
}

/// Нажатие левой кнопки в точке экрана с модификаторами и числом щелчков. Адресат — видимое
/// окно под точкой; если это не `expected`, событие не отправляется и возвращается ошибка.
pub fn mouse_down(
    mtm: MainThreadMarker,
    point: NSPoint,
    expected: &NSWindow,
    flags: NSEventModifierFlags,
    clicks: isize,
    label: &str,
) -> Result<Gesture, String> {
    with_bench(|bench| {
        let before = focus_state(&bench.app);
        let number = NSWindow::windowNumberAtPoint_belowWindowWithWindowNumber(point, 0, mtm);
        let window = bench.app.windowWithWindowNumber(number).ok_or_else(|| {
            format!(
                "под точкой {} окно #{number} не принадлежит пробе",
                describe_point(point)
            )
        })?;
        if !std::ptr::eq(&*window, expected) {
            return Err(format!(
                "под точкой {} окно #{number}, а ожидалось #{}",
                describe_point(point),
                expected.windowNumber()
            ));
        }
        let frame = window.frame();
        let down = post_mouse(
            bench,
            NSEventType::LeftMouseDown,
            &window,
            MouseFields {
                point,
                flags,
                clicks,
                pressure: 1.0,
            },
            format!("mouseDown {label} at {}", describe_point(point)),
            before,
        )?;
        let point = Rc::new(Cell::new(point));
        let released = Rc::new(Cell::new(false));
        bench.gestures.push(OpenGesture {
            window: window.retain(),
            point: Rc::clone(&point),
            released: Rc::clone(&released),
            label: label.to_owned(),
        });
        Ok(Gesture {
            window,
            point,
            frame,
            flags,
            clicks,
            released,
            down,
        })
    })
    .unwrap_or_else(|| Err(no_bench()))
}

impl Gesture {
    /// Протяжка с нажатой кнопкой в точку экрана. Сдвиг окна посреди жеста — ошибка.
    pub fn drag_to(&self, point: NSPoint, label: &str) -> Result<u64, String> {
        self.continue_with(
            NSEventType::LeftMouseDragged,
            point,
            1.0,
            "mouseDragged",
            label,
        )
    }

    /// Отпускание кнопки в последней точке жеста. Сдвиг окна посреди жеста — ошибка.
    pub fn up(&self, label: &str) -> Result<u64, String> {
        let number = self.continue_with(
            NSEventType::LeftMouseUp,
            self.point.get(),
            0.0,
            "mouseUp",
            label,
        )?;
        self.released.set(true);
        Ok(number)
    }

    fn continue_with(
        &self,
        kind: NSEventType,
        point: NSPoint,
        pressure: f32,
        name: &str,
        label: &str,
    ) -> Result<u64, String> {
        if self.released.get() {
            return Err(format!("жест {label} уже закрыт"));
        }
        with_bench(|bench| {
            let before = focus_state(&bench.app);
            let frame = self.window.frame();
            if frame != self.frame {
                return Err(format!(
                    "окно #{} сдвинулось посреди жеста: {} → {}",
                    self.window.windowNumber(),
                    describe_rect(self.frame),
                    describe_rect(frame)
                ));
            }
            self.point.set(point);
            post_mouse(
                bench,
                kind,
                &self.window,
                MouseFields {
                    point,
                    flags: self.flags,
                    clicks: self.clicks,
                    pressure,
                },
                format!("{name} {label} at {}", describe_point(point)),
                before,
            )
        })
        .unwrap_or_else(|| Err(no_bench()))
    }
}

/// Закрывает незавершённые жесты отпусканием кнопки в их последней точке, например перед
/// итогом сценария: открытый цикл отслеживания не даст `stop:` завершить основной цикл.
/// Возвращает число отправленных отпусканий.
pub fn release_open_gestures() -> usize {
    let gestures = with_bench(|bench| std::mem::take(&mut bench.gestures)).unwrap_or_default();
    let mut released = 0;
    for gesture in gestures.into_iter().rev() {
        if gesture.released.replace(true) {
            continue;
        }
        let point = gesture.point.get();
        let posted = with_bench(|bench| {
            let before = focus_state(&bench.app);
            post_mouse(
                bench,
                NSEventType::LeftMouseUp,
                &gesture.window,
                MouseFields {
                    point,
                    flags: NSEventModifierFlags::empty(),
                    clicks: 1,
                    pressure: 0.0,
                },
                format!("mouseUp {} (закрытие жеста)", gesture.label),
                before,
            )
        });
        match posted {
            Some(Ok(_)) => released += 1,
            Some(Err(reason)) => note(format!("закрытие жеста не отправило mouseUp: {reason}")),
            None => {}
        }
    }
    released
}

struct MouseFields {
    point: NSPoint,
    flags: NSEventModifierFlags,
    clicks: isize,
    pressure: f32,
}

fn post_mouse(
    bench: &mut Bench,
    kind: NSEventType,
    window: &NSWindow,
    fields: MouseFields,
    label: String,
    before: String,
) -> Result<u64, String> {
    let location = window.convertPointFromScreen(fields.point);
    let timestamp = bench.timestamp();
    let number = bench.table.peek_number();
    let event_number =
        isize::try_from(number).map_err(|_| format!("номер события {number} вне isize"))?;
    let event = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
        kind,
        location,
        fields.flags,
        timestamp,
        window.windowNumber(),
        None,
        event_number,
        fields.clicks,
        fields.pressure,
    )
    .ok_or_else(|| format!("NSEvent не создал событие {label}"))?;
    Ok(bench.post(label, event, before))
}

/// Уводит курсор в угол видимой области экрана вне окон пробы, чтобы движение настоящей мыши
/// над окном не приходило вводом. Возвращает точку в координатах AppKit.
pub fn warp_cursor_outside(mtm: MainThreadMarker, app: &NSApplication) -> Result<NSPoint, String> {
    let screens = NSScreen::screens(mtm);
    let primary = screens
        .firstObject()
        .ok_or("нет экранов для увода курсора")?;
    let windows: Vec<NSRect> = app
        .windows()
        .iter()
        .filter(|window| window.isVisible())
        .map(|window| inset(window.frame(), -8.0))
        .collect();
    let preferred = app.keyWindow().and_then(|window| window.screen());
    let candidates = preferred
        .into_iter()
        .chain(screens.iter())
        .flat_map(|screen| corners(inset(screen.visibleFrame(), 4.0)));
    for point in candidates {
        if windows.iter().any(|frame| contains(*frame, point)) {
            continue;
        }
        let error = CGWarpMouseCursorPosition(ns_to_cg(point, primary.frame().size.height));
        if error.0 != 0 {
            return Err(format!("CGWarpMouseCursorPosition вернул {}", error.0));
        }
        note(format!("курсор уведён в {}", describe_point(point)));
        return Ok(point);
    }
    Err("нет угла экрана вне окон пробы для курсора".to_owned())
}

/// Углы прямоугольника в порядке предпочтения: нижний правый, нижний левый, верхние.
fn corners(area: NSRect) -> [NSPoint; 4] {
    let (x0, y0) = (area.origin.x, area.origin.y);
    let (x1, y1) = (x0 + area.size.width, y0 + area.size.height);
    [
        NSPoint::new(x1, y0),
        NSPoint::new(x0, y0),
        NSPoint::new(x1, y1),
        NSPoint::new(x0, y1),
    ]
}

/// Курсор вне всех видимых окон пробы.
pub fn cursor_outside(app: &NSApplication) -> bool {
    let cursor = NSEvent::mouseLocation();
    !app.windows()
        .iter()
        .filter(|window| window.isVisible())
        .any(|window| contains(window.frame(), cursor))
}

/// Объект Objective-C, совпадающий с `expected`.
pub fn same<T: ?Sized, U: ?Sized>(object: &T, expected: &U) -> bool {
    std::ptr::addr_eq(object, expected)
}

pub fn is_first_responder(window: &NSWindow, expected: &NSResponder) -> bool {
    window
        .firstResponder()
        .is_some_and(|responder| same(&*responder, expected))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signature(kind: NSEventType, timestamp: f64) -> Signature {
        Signature {
            kind,
            window: 7,
            timestamp,
            event_number: is_mouse(kind).then_some(3),
            key_code: is_key(kind).then_some(125),
        }
    }

    #[test]
    fn input_classification() {
        for kind in [
            NSEventType::KeyDown,
            NSEventType::KeyUp,
            NSEventType::FlagsChanged,
            NSEventType::LeftMouseDown,
            NSEventType::RightMouseUp,
            NSEventType::OtherMouseDown,
            NSEventType::MouseMoved,
            NSEventType::LeftMouseDragged,
            NSEventType::ScrollWheel,
        ] {
            assert!(is_input(kind), "{kind:?}");
        }
        for kind in [
            NSEventType::AppKitDefined,
            NSEventType::SystemDefined,
            NSEventType::ApplicationDefined,
            NSEventType::Periodic,
            NSEventType::MouseEntered,
            NSEventType::MouseExited,
            NSEventType::CursorUpdate,
        ] {
            assert!(!is_input(kind), "{kind:?}");
        }
    }

    #[test]
    fn internal_event_only_for_own_mouse_moved() {
        let own = Source {
            pid: 42,
            state: 0,
            has_cg: true,
        };
        assert!(is_internal(NSEventType::MouseMoved, &own, 42));
        assert!(!is_internal(NSEventType::MouseMoved, &own, 43));
        for kind in [
            NSEventType::KeyDown,
            NSEventType::LeftMouseDown,
            NSEventType::LeftMouseUp,
            NSEventType::LeftMouseDragged,
            NSEventType::ScrollWheel,
        ] {
            assert!(!is_internal(kind, &own, 42), "{kind:?}");
        }
        let hid = Source {
            pid: 0,
            state: 1,
            has_cg: true,
        };
        assert!(!is_internal(NSEventType::MouseMoved, &hid, 42));
        let unknown = Source {
            pid: 42,
            state: 0,
            has_cg: false,
        };
        assert!(!is_internal(NSEventType::MouseMoved, &unknown, 42));
    }

    #[test]
    fn signature_tolerates_nanosecond_rounding() {
        let posted = signature(NSEventType::KeyDown, 235_797.198_090_833_3);
        let received = signature(NSEventType::KeyDown, 235_797.198_090_833);
        assert!(posted.matches(&received));
        let later = signature(
            NSEventType::KeyDown,
            235_797.198_090_833 + TIMESTAMP_SPACING,
        );
        assert!(!posted.matches(&later));
    }

    #[test]
    fn signature_compares_kind_window_and_numbers() {
        let posted = signature(NSEventType::LeftMouseDown, 10.0);
        let mut other = posted;
        other.kind = NSEventType::LeftMouseUp;
        assert!(!posted.matches(&other));
        let mut other = posted;
        other.window = 8;
        assert!(!posted.matches(&other));
        let mut other = posted;
        other.event_number = Some(4);
        assert!(!posted.matches(&other));
        let key = signature(NSEventType::KeyDown, 10.0);
        let mut other = key;
        other.key_code = Some(126);
        assert!(!key.matches(&other));
    }

    #[test]
    fn timestamps_are_spaced() {
        assert_eq!(next_timestamp(5.0, None), 5.0);
        assert_eq!(next_timestamp(5.0, Some(4.0)), 5.0);
        let spaced = next_timestamp(5.0, Some(5.0));
        assert!((spaced - (5.0 + TIMESTAMP_SPACING)).abs() < 1e-12);
        assert!(next_timestamp(5.0, Some(5.000_001)) > 5.000_001);
    }

    #[test]
    fn table_tracks_dispatch_and_release() {
        let mut table: Table<&str> = Table::default();
        assert_eq!(table.peek_number(), 1);
        let down = signature(NSEventType::KeyDown, 1.0);
        let up = signature(NSEventType::KeyUp, 1.0 + TIMESTAMP_SPACING);
        let a = table.register("a".into(), down, "s".into(), Duration::from_millis(1), "A");
        let b = table.register("b".into(), up, "s".into(), Duration::from_millis(2), "B");
        assert_eq!((a, b), (1, 2));
        assert_eq!(table.find(&up), Some(b));
        assert_eq!(table.find(&down), Some(a));

        table.mark_start(a, Duration::from_millis(3));
        assert_eq!(
            table.find(&down),
            None,
            "начавшее dispatch не узнаётся повторно"
        );
        assert_eq!(table.open(), vec![a]);
        table.mark_return(a, Duration::from_millis(4));
        assert!(table.open().is_empty());
        assert_eq!(table.held_count(), 1);

        table.clear();
        assert_eq!(table.held_count(), 0);
        assert_eq!(table.record(b).map(|r| r.undispatched), Some(true));
        assert_eq!(table.records().len(), 2);
        let exported = table.record(a).map(Record::export);
        assert_eq!(
            exported.map(|e| (e.posted_ms, e.dispatch_start_ms, e.send_return_ms)),
            Some((1.0, Some(3.0), Some(4.0)))
        );
        assert_eq!(table.record(b).map(|r| r.export().undispatched), Some(true));
    }

    #[test]
    fn unknown_signature_is_not_found() {
        let mut table: Table<()> = Table::default();
        table.register(
            "a".into(),
            signature(NSEventType::KeyDown, 1.0),
            String::new(),
            Duration::ZERO,
            (),
        );
        assert_eq!(table.find(&signature(NSEventType::KeyDown, 2.0)), None);
    }

    #[test]
    fn key_modifiers_follow_flags() {
        assert!(Key::J.modifiers().is_empty());
        assert_eq!(
            Key::CMD_RETURN.modifiers(),
            vec![(NSEventModifierFlags::Command, 55)]
        );
        assert_eq!(
            Key::SHIFT_G.modifiers(),
            vec![(NSEventModifierFlags::Shift, 56)]
        );
        assert_eq!(
            Key::CTRL_D.modifiers(),
            vec![(NSEventModifierFlags::Control, 59)]
        );
        assert_eq!(
            (Key::CTRL_D.characters, Key::CTRL_D.ignoring),
            ("\u{4}", "d")
        );
        assert_eq!(Key::ESCAPE.characters, "\u{1b}");
        assert_eq!(Key::ESCAPE.label, "Escape");
    }

    #[test]
    fn diff_commands_map_to_keys() {
        let codes =
            |name| sequence(name).map(|keys| keys.iter().map(|k| k.code).collect::<Vec<_>>());
        assert_eq!(codes("j"), Some(vec![38]));
        assert_eq!(codes("k"), Some(vec![40]));
        assert_eq!(codes("gg"), Some(vec![5, 5]));
        assert_eq!(codes("]c"), Some(vec![30, 8]));
        assert_eq!(codes("[c"), Some(vec![33, 8]));
        assert_eq!(sequence("G"), Some(&[Key::SHIFT_G][..]));
        assert_eq!(sequence("Ctrl-d"), Some(&[Key::CTRL_D][..]));
        assert_eq!(sequence("Cmd-Enter"), Some(&[Key::CMD_RETURN][..]));
        assert_eq!(sequence("x"), None);
        for name in [
            "j", "k", "v", "c", "G", "gg", "]c", "[c", "Ctrl-d", "Ctrl-u", "Escape",
        ] {
            let keys = sequence(name).unwrap_or_default();
            assert!(!keys.is_empty(), "{name}");
            assert!(
                keys.iter()
                    .all(|key| !key.flags.contains(NSEventModifierFlags::Command)),
                "{name}: команда diff без Cmd"
            );
        }
    }

    #[test]
    fn geometry_helpers() {
        let point = ns_to_cg(NSPoint::new(10.0, 900.0), 982.0);
        assert_eq!((point.x, point.y), (10.0, 82.0));
        // Внешний экран над встроенным: y больше высоты главного, CG уходит в минус.
        let above = ns_to_cg(NSPoint::new(10.0, 1200.0), 982.0);
        assert_eq!(above.y, -218.0);

        let rect = NSRect::new(
            NSPoint::new(0.0, 0.0),
            objc2_foundation::NSSize::new(10.0, 4.0),
        );
        assert!(contains(rect, NSPoint::new(0.0, 0.0)));
        assert!(!contains(rect, NSPoint::new(10.0, 2.0)));
        assert_eq!(center(rect), NSPoint::new(5.0, 2.0));
        let grown = inset(rect, -2.0);
        assert_eq!(describe_rect(grown), "-2,-2 14x8");
        assert_eq!(describe_rect(inset(rect, 3.0)), "3,3 4x0");
        assert_eq!(
            corners(rect),
            [
                NSPoint::new(10.0, 0.0),
                NSPoint::new(0.0, 0.0),
                NSPoint::new(10.0, 4.0),
                NSPoint::new(0.0, 4.0),
            ]
        );
    }
}
