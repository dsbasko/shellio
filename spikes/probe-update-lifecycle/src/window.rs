//! Окно роли GUI `--window` (задача 9, клетка H): непрозрачное окно AppKit с текстом сессии и
//! полем ввода, без libghostty.
//!
//! - Главный поток процесса держит AppKit: окно, текстовый вид и поле, свой цикл событий. Роль GUI
//!   (`owner::run`) идёт в отдельном потоке и завершает процесс сама, поэтому выход GUI N после
//!   `activate` не зависит от цикла событий.
//! - Окно подключается к сессии обычным `attach` с байта 0 и показывает нумерованный вывод
//!   нагрузки. Поток чтения передаёт куски главному потоку через главную очередь libdispatch, без
//!   опроса.
//! - Квитанция окна ([`Receipt`]) — текст, прочитанный обратно из `NSTextView` после `display`, а
//!   не то, что окно собиралось показать. Разбор номеров — [`read_text`], правила стенограммы
//!   `attach`.
//! - Ввод: нажатия клавиш, пришедшие в очередь процесса, собираются в поле, `Return` отправляет
//!   строку нагрузке с номером [`SEQ_BASE`] и выше. Нажатия перехватываются в цикле событий до
//!   `sendEvent`, поэтому доставка не зависит от того, ключевое ли окно. Принимаются только
//!   нажатия с меткой стенда [`MARK`] в `kCGEventSourceUserData`: окно пробы бывает активным, и
//!   физические нажатия владельца в нём не должны смешиваться с синтетическим вводом. Чужие
//!   нажатия считаются в квитанции.
//!
//! Чтение через доступность и синтетический ввод для сценария — [`ax_text`] и [`type_text`]:
//! они работают из другого процесса, как VoiceOver и средства автоматизации. Код AX упрощён из
//! `spikes/probe-ghostty-surface/src/scenario/ax.rs`. Кнопки окна чужого процесса и `AXPress` —
//! [`ax_dialog`] и [`ax_press`] (окно Gatekeeper клетки G, задача 15).

use std::cell::RefCell;
use std::ffi::c_void;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use objc2::MainThreadOnly;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSAutoresizingMaskOptions, NSBackingStoreType,
    NSColor, NSEvent, NSEventMask, NSEventType, NSFont, NSTextField, NSTextView, NSWindow,
    NSWindowStyleMask,
};
use objc2_core_graphics::{CGEvent, CGEventField, CGPreflightPostEventAccess};
use objc2_foundation::{
    MainThreadMarker, NSArray, NSDate, NSDefaultRunLoopMode, NSPoint, NSRect, NSSize, NSString,
};
use serde::{Deserialize, Serialize};

use crate::attach::{self, Connected, Transcript};
use crate::fixture;
use crate::host::SessionRecord;
use crate::proto::{self, ATTACH_VERSIONS, Message, Purpose};
use crate::pty::Winsize;

/// Начало номеров ввода окна: квитанция `ack <seq> <команда>` не совпадает с вводом стенда и
/// health. Номер — `SEQ_BASE + 1000 × поколение + n`.
pub const SEQ_BASE: u64 = 900_000;
/// Метка синтетических нажатий стенда в `kCGEventSourceUserData`.
pub const MARK: i64 = 0x5045_5531;
pub const KEY_RETURN: u16 = 36;
pub const KEY_ENTER: u16 = 76;
pub const KEY_DELETE: u16 = 51;
const WINDOW_SIZE: (f64, f64) = (720.0, 480.0);
const FIELD_HEIGHT: f64 = 28.0;
/// Срок ответа главного потока на чтение вида.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

// --- чистая часть ---

/// Нумерованный текст окна по правилам стенограммы `attach`: первая строка — 1, пропуск и повтор
/// — в `problems`, неполная последняя строка — в `partial`.
pub fn read_text(text: &str) -> Transcript {
    let mut transcript = Transcript::new(0, Some(1));
    transcript.push(0, text.as_bytes());
    transcript
}

/// Два прочтения совпадают: одни и те же строки с номерами по порядку и тот же хвост без
/// перевода строки.
pub fn agree(receipt: &Transcript, other: &Transcript) -> Result<(), String> {
    let pairs = |transcript: &Transcript| -> Vec<(u64, String)> {
        transcript
            .lines
            .iter()
            .map(|line| (line.number, line.text.clone()))
            .collect()
    };
    let (left, right) = (pairs(receipt), pairs(other));
    if left.len() != right.len() {
        return Err(format!("строк с номерами {} и {}", left.len(), right.len()));
    }
    if let Some((index, (a, b))) = left
        .iter()
        .zip(&right)
        .enumerate()
        .find(|(_, (a, b))| a != b)
    {
        return Err(format!("строка {index}: {a:?} и {b:?}"));
    }
    if receipt.partial != other.partial {
        return Err("неполная последняя строка различается".to_owned());
    }
    Ok(())
}

/// Что делает нажатие клавиши в окне.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Key {
    Submit,
    Erase,
    Type(String),
    Ignore,
}

/// Нажатие с меткой стенда: только такие окно принимает.
pub fn accepts(user_data: i64) -> bool {
    user_data == MARK
}

pub fn key_action(keycode: u16, characters: Option<&str>) -> Key {
    match keycode {
        KEY_RETURN | KEY_ENTER => Key::Submit,
        KEY_DELETE => Key::Erase,
        _ => match characters {
            Some(text) if !text.is_empty() && !text.chars().any(char::is_control) => {
                Key::Type(text.to_owned())
            }
            _ => Key::Ignore,
        },
    }
}

/// Отправленный окном ввод.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sent {
    pub seq: u64,
    pub command: String,
}

/// Чего ждёт запрос квитанции окна.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "wait", rename_all = "kebab-case")]
pub enum Wait {
    /// Квитанция сразу.
    Now,
    /// Строка вывода с этим текстом.
    Line { text: String },
    /// Окно отправило не меньше `count` вводов, и квитанция каждого есть в тексте.
    SentAcked { count: usize },
}

/// Выполнено ли условие ожидания на прочитанном тексте.
pub fn satisfied(wait: &Wait, text: &str, sent: &[Sent]) -> bool {
    let has = |wanted: &str| {
        text.lines()
            .filter_map(fixture::parse_line)
            .any(|(_, line)| line == wanted)
    };
    match wait {
        Wait::Now => true,
        Wait::Line { text } => has(text),
        Wait::SentAcked { count } => {
            sent.len() >= *count
                && sent
                    .iter()
                    .all(|sent| has(&fixture::receipt(sent.seq, &sent.command)))
        }
    }
}

/// Подключение окна к сессии.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub session: String,
    pub version: u16,
    /// Номер первого байта хвоста.
    pub start: u64,
    /// Соединение закрыто: `Done`, конец потока или ошибка.
    pub closed: Option<String>,
}

/// Квитанция окна: текст, прочитанный обратно из текстового вида после отрисовки.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub pid: i32,
    pub generation: u32,
    pub attachment: Option<Attachment>,
    pub attach_error: Option<String>,
    pub window_number: isize,
    pub visible: bool,
    pub key: bool,
    pub active: bool,
    pub text: String,
    pub field: String,
    pub sent: Vec<Sent>,
    /// Нажатия без метки стенда, отброшенные окном.
    pub foreign_keys: u32,
    pub wait: Wait,
    pub satisfied: bool,
    pub readbacks: u32,
}

// --- окно ---

struct Shared {
    generation: u32,
    inner: Mutex<Inner>,
    changed: Condvar,
}

#[derive(Default)]
struct Inner {
    attachment: Option<Attachment>,
    error: Option<String>,
    input: Option<Sender<Sent>>,
    sent: Vec<Sent>,
    foreign_keys: u32,
    /// Сколько кусков вывода главный поток уже положил в текстовый вид.
    applied: u64,
    queue: Vec<Ui>,
}

enum Ui {
    Append(String),
    Read(Sender<View>),
}

/// Состояние вида, прочитанное на главном потоке.
struct View {
    window_number: isize,
    visible: bool,
    key: bool,
    active: bool,
    text: String,
    field: String,
}

static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Окно есть в этом процессе.
pub fn active() -> bool {
    SHARED.get().is_some()
}

/// Окно уже показывает сессию.
pub fn showing() -> bool {
    SHARED
        .get()
        .is_some_and(|shared| shared.lock().attachment.is_some())
}

struct Widgets {
    app: Retained<NSApplication>,
    window: Retained<NSWindow>,
    text: Retained<NSTextView>,
    field: Retained<NSTextField>,
    content: String,
    input: String,
}

thread_local! {
    static WIDGETS: RefCell<Option<Widgets>> = const { RefCell::new(None) };
}

unsafe extern "C" {
    static _dispatch_main_q: u8;
    fn dispatch_async_f(
        queue: *const c_void,
        context: *mut c_void,
        work: extern "C" fn(*mut c_void),
    );
}

/// Задача главному потоку: в очередь и пробуждение через главную очередь libdispatch.
fn post(shared: &Shared, ui: Ui) {
    shared.lock().queue.push(ui);
    // SAFETY: главная очередь существует всё время процесса, контекст не используется.
    unsafe {
        dispatch_async_f(
            (&raw const _dispatch_main_q).cast(),
            std::ptr::null_mut(),
            drain,
        );
    }
}

extern "C" fn drain(_context: *mut c_void) {
    let Some(shared) = SHARED.get() else {
        return;
    };
    let queue = std::mem::take(&mut shared.lock().queue);
    WIDGETS.with(|cell| {
        let mut widgets = cell.borrow_mut();
        let Some(widgets) = widgets.as_mut() else {
            return;
        };
        for ui in queue {
            match ui {
                Ui::Append(chunk) => {
                    widgets.content.push_str(&chunk);
                    widgets
                        .text
                        .setString(&NSString::from_str(&widgets.content));
                    widgets.window.displayIfNeeded();
                    shared.lock().applied += 1;
                    shared.changed.notify_all();
                }
                Ui::Read(reply) => {
                    widgets.window.display();
                    let _ = reply.send(View {
                        window_number: widgets.window.windowNumber(),
                        visible: widgets.window.isVisible(),
                        key: widgets.window.isKeyWindow(),
                        active: widgets.app.isActive(),
                        text: widgets.text.string().to_string(),
                        field: widgets.field.stringValue().to_string(),
                    });
                }
            }
        }
    });
}

fn build(mtm: MainThreadMarker, generation: u32) -> Widgets {
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    app.finishLaunching();
    let (width, height) = WINDOW_SIZE;
    let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, height));
    // SAFETY: обычная инициализация окна на главном потоке.
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            rect,
            NSWindowStyleMask::Titled
                | NSWindowStyleMask::Closable
                | NSWindowStyleMask::Miniaturizable
                | NSWindowStyleMask::Resizable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    // SAFETY: окно держит Rust, поэтому AppKit не должен освобождать его при закрытии.
    unsafe { window.setReleasedWhenClosed(false) };
    window.setOpaque(true);
    window.setBackgroundColor(Some(&NSColor::windowBackgroundColor()));
    window.setTitle(&NSString::from_str(&format!(
        "probe-update-app — поколение {generation}"
    )));
    let scroll = NSTextView::scrollableTextView(mtm);
    scroll.setFrame(NSRect::new(
        NSPoint::new(0.0, FIELD_HEIGHT),
        NSSize::new(width, height - FIELD_HEIGHT),
    ));
    scroll.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    scroll.setDrawsBackground(true);
    let text = scroll
        .documentView()
        .and_then(|view| view.downcast::<NSTextView>().ok())
        .unwrap_or_else(|| NSTextView::new(mtm));
    text.setEditable(false);
    text.setSelectable(true);
    text.setRichText(false);
    if let Some(font) = NSFont::userFixedPitchFontOfSize(12.0) {
        text.setFont(Some(&font));
    }
    text.setDrawsBackground(true);
    text.setBackgroundColor(&NSColor::textBackgroundColor());
    let field = NSTextField::textFieldWithString(&NSString::from_str(""), mtm);
    field.setFrame(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(width, FIELD_HEIGHT),
    ));
    field.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
    if let Some(content) = window.contentView() {
        content.addSubview(&scroll);
        content.addSubview(&field);
    }
    window.center();
    window.makeKeyAndOrderFront(None);
    window.makeFirstResponder(Some(&field));
    app.activate();
    Widgets {
        app,
        window,
        text,
        field,
        content: String::new(),
        input: String::new(),
    }
}

/// Главный поток процесса с окном: окно, `owner` в отдельном потоке, цикл событий. `owner`
/// завершает процесс сам; цикл не возвращается.
pub fn run_main(generation: u32, owner: impl FnOnce() + Send + 'static) -> Result<(), String> {
    let mtm = MainThreadMarker::new().ok_or("окно: не главный поток")?;
    let shared = Arc::new(Shared {
        generation,
        inner: Mutex::new(Inner::default()),
        changed: Condvar::new(),
    });
    SHARED
        .set(Arc::clone(&shared))
        .map_err(|_| "окно уже создано".to_owned())?;
    let widgets = build(mtm, generation);
    let app = Retained::clone(&widgets.app);
    WIDGETS.with(|cell| *cell.borrow_mut() = Some(widgets));
    thread::Builder::new()
        .name("owner".to_owned())
        .spawn(owner)
        .map_err(|err| format!("поток роли GUI: {err}"))?;
    let until = NSDate::distantFuture();
    loop {
        // SAFETY: чтение константы режима Foundation.
        let mode = unsafe { NSDefaultRunLoopMode };
        let Some(event) = app.nextEventMatchingMask_untilDate_inMode_dequeue(
            NSEventMask::Any,
            Some(&until),
            mode,
            true,
        ) else {
            continue;
        };
        if event.r#type() == NSEventType::KeyDown {
            key_down(&shared, &event);
            continue;
        }
        app.sendEvent(&event);
        app.updateWindows();
    }
}

fn key_down(shared: &Shared, event: &NSEvent) {
    let user_data = event.CGEvent().map_or(0, |cg| {
        CGEvent::integer_value_field(Some(&cg), CGEventField::EventSourceUserData)
    });
    if !accepts(user_data) {
        shared.lock().foreign_keys += 1;
        return;
    }
    let characters = event.characters().map(|text| text.to_string());
    let action = key_action(event.keyCode(), characters.as_deref());
    WIDGETS.with(|cell| {
        let mut widgets = cell.borrow_mut();
        let Some(widgets) = widgets.as_mut() else {
            return;
        };
        match action {
            Key::Submit => {
                let command = std::mem::take(&mut widgets.input);
                if !command.trim().is_empty() {
                    submit(shared, command.trim().to_owned());
                }
            }
            Key::Erase => {
                widgets.input.pop();
            }
            Key::Type(text) => widgets.input.push_str(&text),
            Key::Ignore => {}
        }
        widgets
            .field
            .setStringValue(&NSString::from_str(&widgets.input));
    });
}

fn submit(shared: &Shared, command: String) {
    let mut inner = shared.lock();
    let seq = SEQ_BASE + 1000 * u64::from(shared.generation) + inner.sent.len() as u64 + 1;
    let sent = Sent { seq, command };
    inner.sent.push(sent.clone());
    if let Some(input) = &inner.input {
        let _ = input.send(sent);
    }
    drop(inner);
    shared.changed.notify_all();
}

/// Окно подключается к сессии с байта 0, если ещё ни к какой не подключено.
pub fn show(record: &SessionRecord) -> Result<(), String> {
    let shared = SHARED.get().ok_or("окна нет")?;
    if shared.lock().attachment.is_some() {
        return Ok(());
    }
    let hello = attach::hello(Purpose::Attach, ATTACH_VERSIONS, 0, Winsize::cells(0, 0));
    let client = match attach::connect(Path::new(&record.socket), &hello, attach::STEP_TIMEOUT) {
        Ok(Connected::Welcome(client)) => client,
        Ok(Connected::Refused(reason)) => {
            let reason = format!("attach {}: отказ: {reason}", record.session);
            shared.lock().error = Some(reason.clone());
            return Err(reason);
        }
        Err(reason) => {
            shared.lock().error = Some(reason.clone());
            return Err(reason);
        }
    };
    let (reader, welcome) = client.into_parts();
    let writer = reader
        .try_clone()
        .and_then(|writer| reader.set_read_timeout(None).map(|()| writer))
        .map_err(|err| format!("сокет окна: {err}"))?;
    let (sender, receiver) = mpsc::channel::<Sent>();
    {
        let mut inner = shared.lock();
        inner.attachment = Some(Attachment {
            session: record.session.clone(),
            version: welcome.version,
            start: welcome.start,
            closed: None,
        });
        inner.input = Some(sender);
    }
    let reading = Arc::clone(shared);
    thread::Builder::new()
        .name("window-read".to_owned())
        .spawn(move || read_loop(&reading, reader))
        .map_err(|err| format!("поток чтения окна: {err}"))?;
    thread::Builder::new()
        .name("window-write".to_owned())
        .spawn(move || write_loop(writer, &receiver))
        .map_err(|err| format!("поток ввода окна: {err}"))?;
    Ok(())
}

fn read_loop(shared: &Shared, mut stream: UnixStream) {
    let closed = loop {
        match proto::read_message(&mut stream) {
            Ok(Some(Message::Output { bytes, .. })) => {
                post(
                    shared,
                    Ui::Append(String::from_utf8_lossy(&bytes).into_owned()),
                );
            }
            Ok(Some(Message::InputAck { .. })) => {}
            Ok(Some(Message::Done)) => break "done".to_owned(),
            Ok(Some(other)) => break format!("неожиданное сообщение {other:?}"),
            Ok(None) => break "конец потока".to_owned(),
            Err(err) => break format!("чтение: {err}"),
        }
    };
    if let Some(attachment) = shared.lock().attachment.as_mut() {
        attachment.closed = Some(closed);
    }
    shared.changed.notify_all();
}

fn write_loop(mut stream: UnixStream, receiver: &mpsc::Receiver<Sent>) {
    for sent in receiver {
        let message = Message::Input {
            seq: sent.seq,
            bytes: fixture::input_line(sent.seq, &sent.command),
        };
        if proto::write_message(&mut stream, &message).is_err() {
            return;
        }
    }
}

fn read_view(shared: &Shared, deadline: Instant) -> Result<View, String> {
    let (sender, receiver) = mpsc::channel();
    post(shared, Ui::Read(sender));
    let left = deadline
        .saturating_duration_since(Instant::now())
        .max(READ_TIMEOUT);
    receiver
        .recv_timeout(left)
        .map_err(|_| "главный поток не ответил на чтение вида".to_owned())
}

/// Квитанция окна: текст вида после отрисовки, когда выполнено `wait`, или по сроку.
pub fn receipt(wait: &Wait, deadline: Instant) -> Result<Receipt, String> {
    let shared = SHARED.get().ok_or("окна нет")?;
    let mut readbacks = 0;
    loop {
        let applied = shared.lock().applied;
        let view = read_view(shared, deadline)?;
        readbacks += 1;
        let inner = shared.lock();
        let done = satisfied(wait, &view.text, &inner.sent);
        if done || Instant::now() >= deadline {
            return Ok(Receipt {
                pid: std::process::id() as i32,
                generation: shared.generation,
                attachment: inner.attachment.clone(),
                attach_error: inner.error.clone(),
                window_number: view.window_number,
                visible: view.visible,
                key: view.key,
                active: view.active,
                text: view.text,
                field: view.field,
                sent: inner.sent.clone(),
                foreign_keys: inner.foreign_keys,
                wait: wait.clone(),
                satisfied: done,
                readbacks,
            });
        }
        let left = deadline.saturating_duration_since(Instant::now());
        let sent = inner.sent.len();
        let _ = shared
            .changed
            .wait_timeout_while(inner, left, |inner| {
                inner.applied == applied && inner.sent.len() == sent
            })
            .unwrap_or_else(|poisoned| poisoned.into_inner());
    }
}

// --- сторона сценария: доступность и синтетический ввод ---

type AxError = i32;
type CfRef = *const c_void;
const AX_SUCCESS: AxError = 0;
const AX_NO_VALUE: AxError = -25212;
const AX_MESSAGING_TIMEOUT_S: f32 = 5.0;
const AX_TREE_LIMIT: usize = 2000;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> u8;
    fn AXUIElementCreateApplication(pid: libc::pid_t) -> CfRef;
    fn AXUIElementSetMessagingTimeout(element: CfRef, timeout: f32) -> AxError;
    fn AXUIElementCopyAttributeValue(
        element: CfRef,
        attribute: CfRef,
        value: *mut CfRef,
    ) -> AxError;
    fn AXUIElementPerformAction(element: CfRef, action: CfRef) -> AxError;
}

/// Процесс доверен Accessibility.
pub fn ax_trusted() -> bool {
    // SAFETY: функция без аргументов.
    unsafe { AXIsProcessTrusted() != 0 }
}

/// Процессу разрешено посылать события другим процессам.
pub fn post_event_access() -> bool {
    CGPreflightPostEventAccess()
}

/// # Safety
/// `raw` — объект Core Foundation с правом владения или NULL.
unsafe fn owned(raw: CfRef) -> Option<Retained<AnyObject>> {
    // SAFETY: по контракту функции.
    unsafe { Retained::from_raw(raw.cast_mut().cast::<AnyObject>()) }
}

fn as_ref(object: &AnyObject) -> CfRef {
    std::ptr::from_ref(object).cast()
}

fn copy_attribute(element: &AnyObject, attribute: &str) -> Result<Retained<AnyObject>, AxError> {
    let name = NSString::from_str(attribute);
    let mut value: CfRef = std::ptr::null();
    // SAFETY: элемент и имя — живые объекты CF, `value` получает объект с правом владения.
    let error =
        unsafe { AXUIElementCopyAttributeValue(as_ref(element), as_ref(&name), &mut value) };
    // SAFETY: Copy-функция отдаёт объект с правом владения.
    match (error, unsafe { owned(value) }) {
        (AX_SUCCESS, Some(value)) => Ok(value),
        (AX_SUCCESS, None) => Err(AX_NO_VALUE),
        (error, _) => Err(error),
    }
}

fn string_attribute(element: &AnyObject, attribute: &str) -> Option<String> {
    copy_attribute(element, attribute)
        .ok()?
        .downcast_ref::<NSString>()
        .map(ToString::to_string)
}

fn children(element: &AnyObject, attribute: &str) -> Vec<Retained<AnyObject>> {
    copy_attribute(element, attribute)
        .ok()
        .and_then(|value| {
            value
                .downcast_ref::<NSArray>()
                .map(|array| array.iter().collect())
        })
        .unwrap_or_default()
}

/// Текстовые области окон процесса, прочитанные через доступность.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AxRead {
    pub trusted: bool,
    pub windows: usize,
    pub nodes: usize,
    /// `AXValue` каждой `AXTextArea` в порядке обхода.
    pub text_areas: Vec<String>,
    /// Ошибка чтения `AXWindows`, если окон не получено.
    pub windows_error: Option<AxError>,
}

/// Обход дерева доступности окон процесса `pid` и значения его текстовых областей.
pub fn ax_text(pid: i32) -> AxRead {
    let mut read = AxRead {
        trusted: ax_trusted(),
        windows: 0,
        nodes: 0,
        text_areas: Vec::new(),
        windows_error: None,
    };
    // SAFETY: Create-функция отдаёт объект с правом владения.
    let Some(application) = (unsafe { owned(AXUIElementCreateApplication(pid)) }) else {
        read.windows_error = Some(-1);
        return read;
    };
    // SAFETY: элемент — живой объект AX.
    unsafe { AXUIElementSetMessagingTimeout(as_ref(&application), AX_MESSAGING_TIMEOUT_S) };
    let mut stack = match copy_attribute(&application, "AXWindows") {
        Ok(windows) => windows
            .downcast_ref::<NSArray>()
            .map_or(Vec::new(), |array| array.iter().collect()),
        Err(error) => {
            read.windows_error = Some(error);
            Vec::new()
        }
    };
    read.windows = stack.len();
    stack.reverse();
    while let Some(element) = stack.pop() {
        read.nodes += 1;
        if read.nodes > AX_TREE_LIMIT {
            break;
        }
        if string_attribute(&element, "AXRole").as_deref() == Some("AXTextArea") {
            read.text_areas
                .push(string_attribute(&element, "AXValue").unwrap_or_default());
        }
        let mut next = children(&element, "AXChildren");
        next.reverse();
        stack.append(&mut next);
    }
    read
}

/// Кнопка окна чужого процесса, прочитанная через доступность (окно Gatekeeper клетки G).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AxButton {
    pub identifier: Option<String>,
    pub title: Option<String>,
    /// Кнопка — `AXDefaultButton` своего окна.
    pub default: bool,
}

/// Окна процесса: подроли, статический текст и кнопки.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AxDialog {
    pub trusted: bool,
    pub windows: usize,
    pub subroles: Vec<String>,
    pub texts: Vec<String>,
    pub buttons: Vec<AxButton>,
    pub windows_error: Option<AxError>,
}

fn application(pid: i32) -> Option<Retained<AnyObject>> {
    // SAFETY: Create-функция отдаёт объект с правом владения.
    let application = unsafe { owned(AXUIElementCreateApplication(pid)) }?;
    // SAFETY: элемент — живой объект AX.
    unsafe { AXUIElementSetMessagingTimeout(as_ref(&application), AX_MESSAGING_TIMEOUT_S) };
    Some(application)
}

/// Обход окна в глубину не дальше [`AX_TREE_LIMIT`] узлов.
fn walk_tree(window: Retained<AnyObject>, mut visit: impl FnMut(&Retained<AnyObject>)) {
    let mut stack = vec![window];
    let mut nodes = 0;
    while let Some(element) = stack.pop() {
        nodes += 1;
        if nodes > AX_TREE_LIMIT {
            break;
        }
        visit(&element);
        let mut next = children(&element, "AXChildren");
        next.reverse();
        stack.append(&mut next);
    }
}

/// Кнопки и текст окон процесса `pid`. Ничего не нажимает.
pub fn ax_dialog(pid: i32) -> AxDialog {
    let mut dialog = AxDialog {
        trusted: ax_trusted(),
        ..AxDialog::default()
    };
    let Some(application) = application(pid) else {
        dialog.windows_error = Some(-1);
        return dialog;
    };
    let windows = match copy_attribute(&application, "AXWindows") {
        Ok(windows) => windows
            .downcast_ref::<NSArray>()
            .map_or(Vec::new(), |array| array.iter().collect()),
        Err(error) => {
            dialog.windows_error = Some(error);
            Vec::new()
        }
    };
    dialog.windows = windows.len();
    for window in windows {
        dialog
            .subroles
            .push(string_attribute(&window, "AXSubrole").unwrap_or_default());
        let default = copy_attribute(&window, "AXDefaultButton")
            .ok()
            .and_then(|button| string_attribute(&button, "AXIdentifier"));
        walk_tree(window, |element| {
            match string_attribute(element, "AXRole").as_deref() {
                Some("AXButton") => {
                    let identifier = string_attribute(element, "AXIdentifier");
                    dialog.buttons.push(AxButton {
                        default: identifier.is_some() && identifier == default,
                        identifier,
                        title: string_attribute(element, "AXTitle"),
                    });
                }
                Some("AXStaticText") => {
                    dialog
                        .texts
                        .push(string_attribute(element, "AXValue").unwrap_or_default());
                }
                _ => {}
            }
        });
    }
    dialog
}

/// `AXPress` кнопки с `AXIdentifier` = `identifier` в окнах процесса `pid`.
pub fn ax_press(pid: i32, identifier: &str) -> Result<(), String> {
    let application = application(pid).ok_or("AXUIElementCreateApplication вернул NULL")?;
    let windows = copy_attribute(&application, "AXWindows")
        .map_err(|error| format!("AXWindows: ошибка AX {error}"))?;
    let windows: Vec<Retained<AnyObject>> = windows
        .downcast_ref::<NSArray>()
        .map_or(Vec::new(), |array| array.iter().collect());
    let mut target = None;
    for window in windows {
        walk_tree(window, |element| {
            if target.is_none()
                && string_attribute(element, "AXRole").as_deref() == Some("AXButton")
                && string_attribute(element, "AXIdentifier").as_deref() == Some(identifier)
            {
                target = Some(element.clone());
            }
        });
    }
    let button = target.ok_or_else(|| format!("кнопки {identifier} в окнах нет"))?;
    let action = NSString::from_str("AXPress");
    // SAFETY: элемент и имя действия — живые объекты CF.
    let error = unsafe { AXUIElementPerformAction(as_ref(&button), as_ref(&action)) };
    if error == AX_SUCCESS {
        Ok(())
    } else {
        Err(format!("AXPress {identifier}: ошибка AX {error}"))
    }
}

/// Нажатия клавиш процессу `pid`: каждый символ — нажатие с этим символом Unicode, затем
/// `Return`. События идут в очередь процесса, а не в активное приложение.
pub fn type_text(pid: i32, text: &str) -> Result<(), String> {
    let post = |keycode: u16, unicode: Option<&[u16]>| -> Result<(), String> {
        for down in [true, false] {
            let event = CGEvent::new_keyboard_event(None, keycode, down)
                .ok_or("CGEventCreateKeyboardEvent вернул NULL")?;
            CGEvent::set_integer_value_field(Some(&event), CGEventField::EventSourceUserData, MARK);
            if let Some(units) = unicode {
                // SAFETY: указатель и длина — живой срез UTF-16.
                unsafe {
                    CGEvent::keyboard_set_unicode_string(
                        Some(&event),
                        units.len() as _,
                        units.as_ptr(),
                    );
                }
            }
            CGEvent::post_to_pid(pid, Some(&event));
        }
        Ok(())
    };
    for ch in text.chars() {
        let mut units = [0u16; 2];
        post(0, Some(ch.encode_utf16(&mut units)))?;
    }
    post(KEY_RETURN, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numbered(lines: &[(u64, &str)]) -> String {
        lines
            .iter()
            .map(|(number, text)| fixture::format_line(*number, text))
            .collect()
    }

    #[test]
    fn contiguous_text_reads_without_problems() {
        let text = numbered(&[(1, "ready pid=1 generation=1"), (2, "ack 1 ping"), (3, "x")]);
        let read = read_text(&text);
        assert!(read.problems.is_empty(), "{:?}", read.problems);
        assert_eq!(read.lines.len(), 3);
        assert_eq!(read.last_number(), Some(3));
        assert!(read.partial.is_empty());
    }

    #[test]
    fn skip_and_repeat_are_reported() {
        let skipped = read_text(&numbered(&[(1, "a"), (2, "b"), (4, "d")]));
        assert_eq!(skipped.problems.len(), 1);
        assert!(
            skipped.problems[0].starts_with("пропуск"),
            "{:?}",
            skipped.problems
        );
        let repeated = read_text(&numbered(&[(1, "a"), (2, "b"), (2, "b"), (3, "c")]));
        assert!(
            repeated.problems.iter().any(|p| p.starts_with("повтор")),
            "{:?}",
            repeated.problems
        );
        let late_start = read_text(&numbered(&[(2, "b"), (3, "c")]));
        assert!(
            late_start.problems.iter().any(|p| p.starts_with("пропуск")),
            "хвост не с первой строки: {:?}",
            late_start.problems
        );
    }

    #[test]
    fn unnumbered_lines_and_partial_tail_are_kept_apart() {
        let mut text = numbered(&[(1, "a")]);
        text.push_str("без номера\n");
        text.push_str("000002 part");
        let read = read_text(&text);
        assert_eq!(read.lines.len(), 1);
        assert!(read.problems.iter().any(|p| p.contains("без номера")));
        assert_eq!(read.partial, b"000002 part".to_vec());
    }

    #[test]
    fn readings_agree_only_on_identical_numbered_lines() {
        let base = read_text(&numbered(&[(1, "a"), (2, "b")]));
        assert_eq!(
            agree(&base, &read_text(&numbered(&[(1, "a"), (2, "b")]))),
            Ok(())
        );
        assert!(agree(&base, &read_text(&numbered(&[(1, "a")]))).is_err());
        assert!(agree(&base, &read_text(&numbered(&[(1, "a"), (2, "c")]))).is_err());
        let mut tail = numbered(&[(1, "a"), (2, "b")]);
        tail.push_str("000003");
        assert!(agree(&base, &read_text(&tail)).is_err());
    }

    #[test]
    fn only_marked_keys_are_accepted() {
        assert!(accepts(MARK));
        assert!(!accepts(0));
        assert!(!accepts(MARK + 1));
    }

    #[test]
    fn keys_submit_erase_and_type() {
        assert_eq!(key_action(KEY_RETURN, Some("\r")), Key::Submit);
        assert_eq!(key_action(KEY_ENTER, None), Key::Submit);
        assert_eq!(key_action(KEY_DELETE, Some("\u{7f}")), Key::Erase);
        assert_eq!(key_action(0, Some("e")), Key::Type("e".to_owned()));
        assert_eq!(key_action(0, Some("\u{1b}")), Key::Ignore);
        assert_eq!(key_action(0, None), Key::Ignore);
    }

    #[test]
    fn waits_are_met_by_lines_and_receipts() {
        let sent = vec![Sent {
            seq: 902_001,
            command: "emit 2".to_owned(),
        }];
        let before = numbered(&[(1, "a"), (2, "emit 902001 1/2")]);
        let after = numbered(&[
            (1, "a"),
            (2, "emit 902001 1/2"),
            (3, "emit 902001 2/2"),
            (4, "ack 902001 emit 2"),
        ]);
        assert!(satisfied(&Wait::Now, "", &[]));
        let line = Wait::Line {
            text: "a".to_owned(),
        };
        assert!(satisfied(&line, &before, &[]));
        assert!(!satisfied(&line, "", &[]));
        let acked = Wait::SentAcked { count: 1 };
        assert!(!satisfied(&acked, &after, &[]));
        assert!(!satisfied(&acked, &before, &sent));
        assert!(satisfied(&acked, &after, &sent));
    }
}
