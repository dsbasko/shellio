//! Доставка синтетических событий: подкласс `NSApplication` с отметками `sendEvent`, таблица
//! отправленных событий, фабрика клавиш, конец прохода отображения и защита от внешнего ввода.
//!
//! Отметки — наносекунды `CLOCK_UPTIME_RAW` (`clock::uptime_ns`): `t_post` берётся
//! непосредственно перед `postEvent` и сравнивается с квитанциями фикстуры из другого процесса.
//! `NSEvent.timestamp` из этой шкалы не вычитается: он служит только сигнатурой.
//!
//! `postEvent` кладёт в очередь событие, а `sendEvent` получает другой объект с тем же
//! содержимым: тип, окно и `keyCode` сохраняются, `timestamp` округляется до наносекунды. Поэтому
//! событие узнаётся по сигнатуре, а `timestamp` соседних событий стенд разводит не меньше чем на
//! `TIMESTAMP_SPACING`.
//!
//! Конец прохода отображения — наблюдатель `kCFRunLoopBeforeWaiting` главного цикла в общих
//! режимах с порядком после фиксации транзакции Core Animation. Это не момент появления кадра на
//! экране.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{ClassType, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSApplication, NSEvent, NSEventModifierFlags, NSEventType, NSResponder, NSText, NSWindow,
};
use objc2_core_foundation::{
    CFIndex, CFRunLoop, CFRunLoopActivity, CFRunLoopObserver, kCFRunLoopCommonModes,
};
use objc2_core_graphics::{
    CGDisplayBounds, CGEvent, CGEventField, CGMainDisplayID, CGScrollEventUnit,
};
use objc2_foundation::{NSObjectProtocol, NSPoint, NSProcessInfo, NSString};

use super::{Finisher, Outcome, Report};
use crate::clock;
use crate::results::{EventRecord, StepRecord};

/// Минимальный шаг `timestamp` между отправленными событиями.
pub const TIMESTAMP_SPACING: f64 = 10e-6;
/// Допуск сравнения `timestamp`: округление до наносекунды и запас.
const TIMESTAMP_TOLERANCE: f64 = 2e-6;
const TRACE_CAPACITY: usize = 64;
/// Сколько служебных событий записать в след вместе с их источником.
const SERVICE_EVENTS_TRACED: usize = 12;
/// Порядок наблюдателя прохода: после фиксации транзакции Core Animation (2 000 000) и до
/// снятия пула autorelease (2 147 483 647).
const PASS_ORDER: CFIndex = 2_100_000;
/// Сколько последних проходов хранится: около 80 с при опросе раз в 5 мс.
const PASS_CAPACITY: usize = 16_384;

/// Событие ввода, которое стенд обязан узнать: клавиатура, кнопки и движение мыши, прокрутка.
/// Служебные события AppKit и системы, включая пробуждение `Finisher`, к вводу не относятся.
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
    pub key_code: Option<u16>,
}

impl Signature {
    pub fn of(event: &NSEvent) -> Signature {
        let kind = event.r#type();
        Signature {
            kind,
            window: event.windowNumber(),
            timestamp: event.timestamp(),
            key_code: is_key(kind).then(|| event.keyCode()),
        }
    }

    pub fn matches(&self, received: &Signature) -> bool {
        self.kind == received.kind
            && self.window == received.window
            && (self.timestamp - received.timestamp).abs() <= TIMESTAMP_TOLERANCE
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

/// Отметки одного отправленного события, наносекунды `CLOCK_UPTIME_RAW`.
#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    pub number: u64,
    pub label: String,
    pub signature: Signature,
    /// Состояние AppKit, прочитанное перед отправкой.
    pub before: String,
    pub t_post: u64,
    pub dispatch_start: Option<u64>,
    pub send_return: Option<u64>,
    /// Событие убрано из таблицы, не пройдя `sendEvent`.
    pub undispatched: bool,
}

impl Record {
    /// Запись для `result.json`; `display_pass` — конец первого прохода отображения после
    /// возврата `sendEvent`.
    pub fn export(&self, display_pass: Option<u64>) -> EventRecord {
        EventRecord {
            number: self.number,
            label: self.label.clone(),
            window: self.signature.window,
            before: self.before.clone(),
            t_post_ns: self.t_post,
            dispatch_start_ns: self.dispatch_start,
            send_return_ns: self.send_return,
            display_pass_ns: display_pass,
            undispatched: self.undispatched,
        }
    }
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
    pub fn peek_number(&self) -> u64 {
        self.next
    }

    pub fn register(
        &mut self,
        label: String,
        signature: Signature,
        before: String,
        t_post: u64,
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
            t_post,
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

    pub fn mark_start(&mut self, number: u64, at: u64) {
        if let Some(record) = self.record_mut(number) {
            record.dispatch_start = Some(at);
        }
    }

    /// Отмечает возврат `sendEvent` и отпускает объект события.
    pub fn mark_return(&mut self, number: u64, at: u64) {
        if let Some(record) = self.record_mut(number) {
            record.send_return = Some(at);
        }
        self.held.retain(|(held, _, _)| *held != number);
    }

    /// Отпускает все удерживаемые события; не начавшие dispatch помечаются.
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

/// Отметки концов проходов отображения по возрастанию.
pub struct PassLog {
    times: VecDeque<u64>,
    capacity: usize,
    /// Последняя вытесненная отметка: проход раньше неё уже не найти.
    evicted: Option<u64>,
    total: u64,
}

impl Default for PassLog {
    fn default() -> Self {
        PassLog::with_capacity(PASS_CAPACITY)
    }
}

impl PassLog {
    pub fn with_capacity(capacity: usize) -> PassLog {
        PassLog {
            times: VecDeque::with_capacity(capacity.min(PASS_CAPACITY)),
            capacity: capacity.max(1),
            evicted: None,
            total: 0,
        }
    }

    pub fn record(&mut self, at: u64) {
        if self.times.len() == self.capacity {
            self.evicted = self.times.pop_front();
        }
        self.times.push_back(at);
        self.total += 1;
    }

    /// Первый проход не раньше `after`. `None`, если его ещё не было или он вытеснен.
    pub fn first_after(&self, after: u64) -> Option<u64> {
        if self.evicted.is_some_and(|evicted| evicted >= after) {
            return None;
        }
        let index = self.times.partition_point(|time| *time < after);
        self.times.get(index).copied()
    }

    pub fn total(&self) -> u64 {
        self.total
    }
}

/// Кольцо последних строк следа. Его читает сторож из своего потока, поэтому внутри только
/// данные без объектов Objective-C.
#[derive(Default)]
pub struct TraceRing {
    lines: VecDeque<String>,
    dropped: usize,
}

impl TraceRing {
    pub fn push(&mut self, line: String) {
        if self.lines.len() == TRACE_CAPACITY {
            self.lines.pop_front();
            self.dropped += 1;
        }
        self.lines.push_back(line);
    }

    pub fn lines(&self) -> Vec<String> {
        let mut lines = Vec::with_capacity(self.lines.len() + 1);
        if self.dropped > 0 {
            lines.push(format!("… {} ранних строк вытеснено", self.dropped));
        }
        lines.extend(self.lines.iter().cloned());
        lines
    }
}

pub type SharedTrace = Arc<Mutex<TraceRing>>;

pub fn push_trace(trace: &SharedTrace, line: String) {
    trace
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(line);
}

/// Клавиша с согласованными `keyCode`, `characters` и `charactersIgnoringModifiers`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key {
    pub label: &'static str,
    pub code: u16,
    pub characters: &'static str,
    pub ignoring: &'static str,
    pub flags: NSEventModifierFlags,
}

impl Key {
    pub const RETURN: Key = Key {
        label: "Return",
        code: 36,
        characters: "\r",
        ignoring: "\r",
        flags: NSEventModifierFlags::empty(),
    };

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

// --- AppKit ---------------------------------------------------------------------------------

define_class!(
    /// `NSApp` сценариев и демонстрации. Без установленного стенда пропускает события без отметок.
    #[unsafe(super(NSApplication, NSResponder, objc2_foundation::NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeGhosttySurfaceApplication"]
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
                Admit::External(description) => external_input(description),
            }
        }
    }
);

impl ProbeApplication {
    /// Сначала маршрутизатор режимов (`router::intercept`), затем `keyUp` и автоповтор нажатия,
    /// съеденного видом поверхности, — его владельцу мимо обычной цепочки
    /// (`view::before_dispatch`), затем обычная доставка.
    fn dispatch(&self, event: &NSEvent) {
        if crate::router::intercept(event) || crate::view::before_dispatch(event) {
            return;
        }
        // SAFETY: вызов реализации `sendEvent:` суперкласса с тем же событием.
        let () = unsafe { msg_send![super(self), sendEvent: event] };
        crate::view::after_dispatch(event);
        crate::router::after_dispatch(event);
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
            "NSApp создан классом {}, а не ProbeGhosttySurfaceApplication",
            object.class().name().to_string_lossy()
        ))
    }
}

/// Стенд доставки событий процесса сценария.
pub struct Bench {
    app: Retained<NSApplication>,
    finisher: Finisher,
    trace: SharedTrace,
    epoch: u64,
    table: Table<Retained<NSEvent>>,
    last_timestamp: Option<f64>,
    external: bool,
    service_traced: usize,
    internal: Vec<String>,
    passes: PassLog,
    steps: Vec<StepRecord>,
}

thread_local! {
    static BENCH: RefCell<Option<Bench>> = const { RefCell::new(None) };
    static HOOKS: RefCell<Hooks> = RefCell::new(Hooks::default());
}

type PassHook = Box<dyn FnMut(u64) -> bool>;
type ReturnHook = Box<dyn FnMut(u64, u64) -> bool>;

/// Обработчики, которые вызываются сразу в конце прохода отображения и после возврата
/// `sendEvent` события из таблицы, без ожидания таймера. Они живут отдельно от стенда и
/// вызываются вне его заимствования, поэтому могут отправлять события и читать таблицу.
#[derive(Default)]
struct Hooks {
    pass: Vec<PassHook>,
    returned: Vec<ReturnHook>,
}

/// Вызывается в конце каждого прохода отображения с его отметкой, пока возвращает `true`.
pub fn on_display_pass(hook: impl FnMut(u64) -> bool + 'static) {
    HOOKS.with(|hooks| hooks.borrow_mut().pass.push(Box::new(hook)));
}

/// Вызывается после возврата `sendEvent` каждого события таблицы с его номером и отметкой
/// возврата, пока возвращает `true`.
pub fn on_send_return(hook: impl FnMut(u64, u64) -> bool + 'static) {
    HOOKS.with(|hooks| hooks.borrow_mut().returned.push(Box::new(hook)));
}

fn run_pass_hooks(at: u64) {
    let mut running = HOOKS.with(|hooks| std::mem::take(&mut hooks.borrow_mut().pass));
    running.retain_mut(|hook| hook(at));
    HOOKS.with(|hooks| {
        let mut hooks = hooks.borrow_mut();
        running.append(&mut hooks.pass);
        hooks.pass = running;
    });
}

fn run_return_hooks(number: u64, at: u64) {
    let mut running = HOOKS.with(|hooks| std::mem::take(&mut hooks.borrow_mut().returned));
    running.retain_mut(|hook| hook(number, at));
    HOOKS.with(|hooks| {
        let mut hooks = hooks.borrow_mut();
        running.append(&mut hooks.returned);
        hooks.returned = running;
    });
}

/// Ставит стенд и наблюдатель конца прохода отображения: с этого момента событие ввода вне
/// таблицы — ошибка исполнения.
pub fn install(
    app: Retained<NSApplication>,
    finisher: Finisher,
    trace: SharedTrace,
) -> Result<(), String> {
    let bench = Bench {
        app,
        finisher,
        trace,
        epoch: clock::uptime_ns(),
        table: Table::default(),
        last_timestamp: None,
        external: false,
        service_traced: 0,
        internal: Vec::new(),
        passes: PassLog::default(),
        steps: Vec::new(),
    };
    BENCH.with(|cell| *cell.borrow_mut() = Some(bench));
    let block = RcBlock::new(
        move |_observer: *mut CFRunLoopObserver, _activity: CFRunLoopActivity| {
            let at = clock::uptime_ns();
            with_bench(|bench| bench.passes.record(at));
            run_pass_hooks(at);
        },
    );
    // SAFETY: распределитель по умолчанию, блок жив на время вызова и копируется наблюдателем.
    let observer = unsafe {
        CFRunLoopObserver::with_handler(
            None,
            CFRunLoopActivity::BeforeWaiting.0,
            true,
            PASS_ORDER,
            Some(&block),
        )
    }
    .ok_or("CFRunLoopObserverCreateWithHandler вернул NULL")?;
    let run_loop = CFRunLoop::main().ok_or("нет главного цикла CFRunLoop")?;
    // SAFETY: `kCFRunLoopCommonModes` — неизменяемая константа Core Foundation.
    run_loop.add_observer(Some(&observer), unsafe { kCFRunLoopCommonModes });
    Ok(())
}

/// Время от установки стенда, мс.
pub fn elapsed_ms() -> Option<f64> {
    with_bench(|bench| clock::interval_ms(bench.epoch, clock::uptime_ns()))
}

/// Конец первого прохода отображения не раньше `after`.
pub fn display_pass_after(after: u64) -> Option<u64> {
    with_bench(|bench| bench.passes.first_after(after)).flatten()
}

/// Отметка шага сценария.
pub fn step(record: StepRecord) {
    with_bench(|bench| bench.steps.push(record));
}

/// Состояние фокуса для отметок шагов.
pub fn current_state() -> String {
    with_bench(|bench| focus_state(&bench.app)).unwrap_or_else(no_bench)
}

fn with_bench<R>(f: impl FnOnce(&mut Bench) -> R) -> Option<R> {
    BENCH.with(|cell| cell.try_borrow_mut().ok()?.as_mut().map(f))
}

fn since(bench: &Bench, at: u64) -> String {
    format!("{:.3}ms", clock::interval_ms(bench.epoch, at))
}

enum Admit {
    Pass,
    Ours(u64),
    External(String),
}

fn admit(event: &NSEvent) -> Admit {
    let received = Signature::of(event);
    with_bench(|bench| {
        if let Some(number) = bench.table.find(&received) {
            let at = clock::uptime_ns();
            bench.table.mark_start(number, at);
            push_trace(
                &bench.trace,
                format!("{} dispatch #{number}", since(bench, at)),
            );
            return Admit::Ours(number);
        }
        let source = Source::of(event);
        let description = format!(
            "type={} window={} timestamp={:.6} keyCode={:?} {}",
            received.kind.0,
            received.window,
            received.timestamp,
            received.key_code,
            source.describe()
        );
        let at = clock::uptime_ns();
        if !is_input(received.kind) {
            if bench.service_traced < SERVICE_EVENTS_TRACED {
                bench.service_traced += 1;
                push_trace(
                    &bench.trace,
                    format!("{} служебное событие {description}", since(bench, at)),
                );
            }
            return Admit::Pass;
        }
        if is_internal(received.kind, &source, std::process::id()) {
            let line = format!(
                "{} внутреннее событие AppKit {description}",
                since(bench, at)
            );
            push_trace(&bench.trace, line.clone());
            bench.internal.push(line);
            return Admit::Pass;
        }
        if bench.external {
            return Admit::Pass;
        }
        bench.external = true;
        Admit::External(description)
    })
    .unwrap_or(Admit::Pass)
}

fn returned(number: u64) {
    let at = clock::uptime_ns();
    with_bench(|bench| {
        bench.table.mark_return(number, at);
        push_trace(
            &bench.trace,
            format!("{} return #{number}", since(bench, at)),
        );
    });
    run_return_hooks(number, at);
}

fn external_input(description: String) {
    let Some((app, finisher)) = with_bench(|bench| {
        push_trace(&bench.trace, format!("внешний ввод: {description}"));
        (bench.app.clone(), bench.finisher.clone())
    }) else {
        return;
    };
    // Событие не доставляется: прогон всё равно повторяется.
    let mut report = Report::default();
    clear();
    add_to(&mut report);
    finisher.finish(
        &app,
        Outcome::error(
            format!("внешний ввод: событие вне таблицы стенда ({description})"),
            report,
        ),
    );
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

/// Движение мыши, созданное самим процессом, — служебное событие AppKit. Клавиши и кнопки
/// этого исключения не получают: стенд создаёт их тем же процессом, и промах сопоставления не
/// должен пройти молча.
pub fn is_internal(kind: NSEventType, source: &Source, own_pid: u32) -> bool {
    kind == NSEventType::MouseMoved && source.has_cg && source.pid == i64::from(own_pid)
}

/// Строка следа в кольцо сторожа с отметкой времени стенда.
pub fn note(line: impl AsRef<str>) {
    with_bench(|bench| {
        let at = clock::uptime_ns();
        push_trace(
            &bench.trace,
            format!("{} {}", since(bench, at), line.as_ref()),
        );
    });
}

/// Отметки события по номеру.
pub fn record(number: u64) -> Option<Record> {
    with_bench(|bench| bench.table.record(number).cloned()).flatten()
}

/// Отпускает удерживаемые события перед подсчётом живых объектов.
pub fn clear() {
    with_bench(|bench| bench.table.clear());
}

/// Отметки всех событий и число удерживаемых — в итог.
pub fn add_to(report: &mut Report) {
    with_bench(|bench| bench.add_to(report));
}

impl Bench {
    fn add_to(&self, report: &mut Report) {
        report.push("events_posted", self.table.records().len());
        report.push("events_held", self.table.held_count());
        report.push("display_passes", self.passes.total());
        report.events = self
            .table
            .records()
            .iter()
            .map(|record| {
                let pass = record
                    .send_return
                    .and_then(|back| self.passes.first_after(back));
                record.export(pass)
            })
            .collect();
        report.steps = self.steps.clone();
        report.diagnostics.appkit_internal_events = self.internal.clone();
    }

    fn post(&mut self, label: String, event: Retained<NSEvent>, before: String) -> u64 {
        let signature = Signature::of(&event);
        let number = self.table.peek_number();
        let t_post = clock::uptime_ns();
        push_trace(
            &self.trace,
            format!("{} post #{number} {label}", since(self, t_post)),
        );
        let number = self
            .table
            .register(label, signature, before, t_post, event.clone());
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
    let cursor = NSEvent::mouseLocation();
    format!(
        "isActive={} keyWindow={} mainWindow={} firstResponder={} cursor={},{}",
        app.isActive(),
        window(key),
        window(app.mainWindow()),
        responder,
        cursor.x,
        cursor.y
    )
}

/// Класс first responder окна; field editor помечается.
pub fn responder_name(window: &NSWindow) -> String {
    let Some(responder) = window.firstResponder() else {
        return "none".to_owned();
    };
    let object: &AnyObject = &responder;
    let class = object.class().name().to_string_lossy().into_owned();
    match object.downcast_ref::<NSText>() {
        Some(text) if text.isFieldEditor() => format!("field-editor({class})"),
        _ => class,
    }
}

fn no_bench() -> String {
    "стенд не установлен".to_owned()
}

/// Пара `keyDown` и `keyUp`, для модификаторов — `flagsChanged` до и после. Адресат —
/// фактическое key window, прочитанное перед каждым событием.
pub fn press(key: Key) -> Result<Vec<u64>, String> {
    press_to(key, None)
}

/// То же, но адресат — окно `window`, даже если оно не key: без key window AppKit подставляет
/// событию с номером окна 0 прежнее окно, и подпись разошлась бы с таблицей стенда.
pub fn press_in(key: Key, window: &NSWindow) -> Result<Vec<u64>, String> {
    press_to(key, Some(window.windowNumber()))
}

fn press_to(key: Key, window: Option<isize>) -> Result<Vec<u64>, String> {
    let post_key = |kind, label: &str, code, characters, ignoring, flags| {
        post_key_event(
            kind, label, code, characters, ignoring, flags, false, window,
        )
    };
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
    numbers.push(post_key(
        NSEventType::KeyDown,
        &format!("keyDown {}", key.label),
        key.code,
        key.characters,
        key.ignoring,
        key.flags,
    )?);
    numbers.push(post_key(
        NSEventType::KeyUp,
        &format!("keyUp {}", key.label),
        key.code,
        key.characters,
        key.ignoring,
        key.flags,
    )?);
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

/// Кнопка или перетаскивание левой кнопкой мыши в точке экрана `point` (координаты AppKit).
/// Точка переводится в координаты окна в момент отправки: окно могло сдвинуться или вырасти.
/// Курсор системы не двигается.
pub fn post_mouse(
    kind: NSEventType,
    window: &NSWindow,
    point: NSPoint,
    label: &str,
) -> Result<u64, String> {
    with_bench(|bench| {
        let before = focus_state(&bench.app);
        let location = window.convertPointFromScreen(point);
        let timestamp = bench.timestamp();
        let number = bench.table.peek_number();
        let event_number =
            isize::try_from(number).map_err(|_| format!("номер события {number} вне isize"))?;
        let pressure = if kind == NSEventType::LeftMouseUp {
            0.0
        } else {
            1.0
        };
        let event = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
            kind,
            location,
            NSEventModifierFlags::empty(),
            timestamp,
            window.windowNumber(),
            None,
            event_number,
            1,
            pressure,
        )
        .ok_or_else(|| format!("NSEvent не создал событие {label}"))?;
        Ok(bench.post(
            format!("{label} at {:.0},{:.0}", point.x, point.y),
            event,
            before,
        ))
    })
    .unwrap_or_else(|| Err(no_bench()))
}

/// Одно нажатие клавиши без отпускания; `repeat` — автоповтор. Модификаторы — флаги события,
/// без `flagsChanged`.
pub fn key_down(key: Key, repeat: bool) -> Result<u64, String> {
    post_key_event(
        NSEventType::KeyDown,
        &format!(
            "keyDown {}{}",
            key.label,
            if repeat { " repeat" } else { "" }
        ),
        key.code,
        key.characters,
        key.ignoring,
        key.flags,
        repeat,
        None,
    )
}

/// `flagsChanged` клавиши модификатора `code`; `flags` — модификаторы после изменения.
pub fn modifier(code: u16, flags: NSEventModifierFlags) -> Result<u64, String> {
    let sign = if flags.is_empty() { '-' } else { '+' };
    post_key(
        NSEventType::FlagsChanged,
        &format!("flagsChanged{sign}{code}"),
        code,
        "",
        "",
        flags,
    )
}

/// Прокрутка на одну строку вниз в точке экрана `point` (координаты AppKit). У `NSEvent` нет
/// конструктора прокрутки, поэтому событие строится из `CGEvent`; в таблицу идёт сигнатура
/// готового `NSEvent`. Такое событие приходит без окна (номер 0), и AppKit до вида его не
/// доводит. Курсор системы не двигается.
pub fn post_scroll(point: NSPoint, label: &str) -> Result<u64, String> {
    with_bench(|bench| {
        let before = focus_state(&bench.app);
        let cg = CGEvent::new_scroll_wheel_event2(None, CGScrollEventUnit::Line, 1, -1, 0, 0)
            .ok_or_else(|| format!("CGEvent не создал прокрутку {label}"))?;
        let primary = CGDisplayBounds(CGMainDisplayID());
        CGEvent::set_location(
            Some(&cg),
            NSPoint::new(point.x, primary.size.height - point.y),
        );
        let event = NSEvent::eventWithCGEvent(&cg)
            .ok_or_else(|| format!("NSEvent не создал прокрутку {label}"))?;
        let timestamp = event.timestamp();
        if let Some(last) = bench.last_timestamp
            && (timestamp - last).abs() < TIMESTAMP_SPACING
        {
            return Err(format!(
                "timestamp прокрутки {timestamp:.6} ближе {TIMESTAMP_SPACING} к соседнему {last:.6}"
            ));
        }
        bench.last_timestamp = Some(
            bench
                .last_timestamp
                .map_or(timestamp, |last| last.max(timestamp)),
        );
        Ok(bench.post(
            format!("{label} at {:.0},{:.0}", point.x, point.y),
            event,
            before,
        ))
    })
    .unwrap_or_else(|| Err(no_bench()))
}

/// Одно отпускание клавиши.
pub fn key_up(key: Key) -> Result<u64, String> {
    post_key(
        NSEventType::KeyUp,
        &format!("keyUp {}", key.label),
        key.code,
        key.characters,
        key.ignoring,
        key.flags,
    )
}

fn post_key(
    kind: NSEventType,
    label: &str,
    code: u16,
    characters: &str,
    ignoring: &str,
    flags: NSEventModifierFlags,
) -> Result<u64, String> {
    post_key_event(kind, label, code, characters, ignoring, flags, false, None)
}

/// `window` — номер окна адресата; `None` — фактическое key window или 0 без него.
#[allow(clippy::too_many_arguments)]
fn post_key_event(
    kind: NSEventType,
    label: &str,
    code: u16,
    characters: &str,
    ignoring: &str,
    flags: NSEventModifierFlags,
    repeat: bool,
    window: Option<isize>,
) -> Result<u64, String> {
    with_bench(|bench| {
        let before = focus_state(&bench.app);
        let window = window.unwrap_or_else(|| {
            bench
                .app
                .keyWindow()
                .map_or(0, |window| window.windowNumber())
        });
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
            repeat,
            code,
        )
        .ok_or_else(|| format!("NSEvent не создал событие {label}"))?;
        Ok(bench.post(label.to_owned(), event, before))
    })
    .unwrap_or_else(|| Err(no_bench()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hooks_run_outside_borrow_and_keep_new_ones() {
        let calls = std::rc::Rc::new(RefCell::new(Vec::new()));
        let outer = std::rc::Rc::clone(&calls);
        on_display_pass(move |at| {
            outer.borrow_mut().push(("first", at));
            let inner = std::rc::Rc::clone(&outer);
            // Регистрация изнутри обработчика не теряется и не встречает заимствование.
            on_display_pass(move |at| {
                inner.borrow_mut().push(("second", at));
                true
            });
            false
        });
        run_pass_hooks(1);
        run_pass_hooks(2);
        run_pass_hooks(3);
        assert_eq!(
            *calls.borrow(),
            [("first", 1), ("second", 2), ("second", 3)]
        );
        let returns = std::rc::Rc::new(RefCell::new(Vec::new()));
        let seen = std::rc::Rc::clone(&returns);
        on_send_return(move |number, at| {
            seen.borrow_mut().push((number, at));
            number != 7
        });
        run_return_hooks(5, 10);
        run_return_hooks(7, 20);
        run_return_hooks(8, 30);
        assert_eq!(*returns.borrow(), [(5, 10), (7, 20)]);
        HOOKS.with(|hooks| *hooks.borrow_mut() = Hooks::default());
    }

    fn signature(kind: NSEventType, timestamp: f64) -> Signature {
        Signature {
            kind,
            window: 7,
            timestamp,
            key_code: is_key(kind).then_some(36),
        }
    }

    #[test]
    fn input_classification() {
        for kind in [
            NSEventType::KeyDown,
            NSEventType::KeyUp,
            NSEventType::FlagsChanged,
            NSEventType::LeftMouseDown,
            NSEventType::MouseMoved,
            NSEventType::ScrollWheel,
        ] {
            assert!(is_input(kind), "{kind:?}");
        }
        for kind in [
            NSEventType::ApplicationDefined,
            NSEventType::AppKitDefined,
            NSEventType::SystemDefined,
            NSEventType::Periodic,
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
        assert!(!is_internal(NSEventType::KeyDown, &own, 42));
        assert!(!is_internal(NSEventType::MouseMoved, &own, 43));
        let without_cg = Source {
            pid: 42,
            state: 0,
            has_cg: false,
        };
        assert!(!is_internal(NSEventType::MouseMoved, &without_cg, 42));
    }

    #[test]
    fn signature_tolerates_nanosecond_rounding() {
        let posted = signature(NSEventType::KeyDown, 100.000_000_4);
        assert!(posted.matches(&signature(NSEventType::KeyDown, 100.0)));
        assert!(!posted.matches(&signature(NSEventType::KeyDown, 100.000_01)));
        assert!(!posted.matches(&signature(NSEventType::KeyUp, 100.000_000_4)));
        let other_window = Signature {
            window: 8,
            ..posted
        };
        assert!(!posted.matches(&other_window));
        let other_key = Signature {
            key_code: Some(0),
            ..posted
        };
        assert!(!posted.matches(&other_key));
    }

    #[test]
    fn timestamps_are_spaced() {
        assert_eq!(next_timestamp(5.0, None), 5.0);
        assert_eq!(next_timestamp(5.0, Some(4.0)), 5.0);
        assert_eq!(next_timestamp(5.0, Some(5.0)), 5.0 + TIMESTAMP_SPACING);
    }

    #[test]
    fn table_tracks_dispatch_and_release() {
        let mut table: Table<&str> = Table::default();
        let down = signature(NSEventType::KeyDown, 1.0);
        let up = signature(NSEventType::KeyUp, 1.0 + TIMESTAMP_SPACING);
        assert_eq!(table.peek_number(), 1);
        let first = table.register("keyDown".into(), down, "before".into(), 1_000, "down");
        let second = table.register("keyUp".into(), up, "before".into(), 2_000, "up");
        assert_eq!((first, second, table.held_count()), (1, 2, 2));
        assert_eq!(table.find(&down), Some(1));
        table.mark_start(1, 1_500);
        assert_eq!(table.find(&down), None, "dispatch уже начат");
        table.mark_return(1, 1_700);
        assert_eq!(table.held_count(), 1);
        let record = table.record(1).unwrap();
        assert_eq!(
            (record.t_post, record.dispatch_start, record.send_return),
            (1_000, Some(1_500), Some(1_700))
        );
        table.clear();
        assert_eq!(table.held_count(), 0);
        assert!(table.record(2).unwrap().undispatched);
        assert!(!table.record(1).unwrap().undispatched);
        let exported = table.record(1).unwrap().export(Some(1_900));
        assert_eq!(exported.t_post_ns, 1_000);
        assert_eq!(exported.display_pass_ns, Some(1_900));
    }

    #[test]
    fn first_pass_not_before_change() {
        let mut log = PassLog::default();
        for at in [5, 10, 15] {
            log.record(at);
        }
        assert_eq!(log.first_after(0), Some(5));
        assert_eq!(log.first_after(10), Some(10));
        assert_eq!(log.first_after(11), Some(15));
        assert_eq!(log.first_after(16), None);
        assert_eq!(log.total(), 3);
        let mut short = PassLog::with_capacity(2);
        for at in [5, 10, 15] {
            short.record(at);
        }
        assert_eq!(short.first_after(1), None, "проход 5 вытеснен");
        assert_eq!(short.first_after(6), Some(10));
    }

    #[test]
    fn trace_ring_keeps_last_lines() {
        let mut ring = TraceRing::default();
        for index in 0..TRACE_CAPACITY + 3 {
            ring.push(format!("line {index}"));
        }
        let lines = ring.lines();
        assert_eq!(lines.len(), TRACE_CAPACITY + 1);
        assert_eq!(lines[0], "… 3 ранних строк вытеснено");
        assert_eq!(lines[1], "line 3");
    }

    #[test]
    fn key_modifiers_follow_flags() {
        assert!(Key::RETURN.modifiers().is_empty());
        let chord = Key {
            flags: NSEventModifierFlags::Control | NSEventModifierFlags::Shift,
            ..Key::RETURN
        };
        assert_eq!(
            chord.modifiers(),
            [
                (NSEventModifierFlags::Shift, 56),
                (NSEventModifierFlags::Control, 59)
            ]
        );
    }
}
