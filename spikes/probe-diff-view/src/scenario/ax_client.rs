//! Внешний клиент доступности: процесс пробы `--ax-client <pid>`. Он читает дерево AX окна
//! процесса `pid` через `AXUIElement` (ApplicationServices): контейнер строк, его видимые и
//! выбранные строки, элемент с фокусом. Наблюдатель `AXObserver` на элементе приложения
//! записывает уведомления. Клиент держит элементы строк предыдущего снимка и перечитывает их по
//! удержанной ссылке: так видно, не стал ли элемент указывать на другую строку.
//!
//! Протокол — строки: на stdin `snapshot <метка>`, на stdout одна строка JSON [`AxSnapshot`] на
//! каждую команду. Первая строка stdout — [`READY_LINE`] или [`ERROR_LINE`] с причиной. Закрытие
//! stdin завершает клиент.
//!
//! Стенд главный поток на клиенте не блокирует: запросы AX обслуживает основной цикл процесса
//! пробы, поэтому ответы читает рабочий поток ([`Client`]), а сценарий забирает их опросом.

use std::ffi::c_void;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::ptr::NonNull;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::Instant;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_core_foundation::{CGPoint, CGSize};
use objc2_foundation::{NSArray, NSNumber, NSString};

use probe_diff_view::access::{
    AxElement, AxFrame, AxHeld, AxNote, AxRowRead, AxSnapshot, ROW_ROLE, TABLE_ROLE,
};

pub const READY_LINE: &str = "ax_client_ready";
pub const ERROR_LINE: &str = "ax_client_error=";
/// Срок ответа одного запроса AX к процессу пробы, секунды.
pub const MESSAGING_TIMEOUT_S: f32 = 5.0;
/// Глубина поиска контейнера строк от окна.
const SEARCH_DEPTH: usize = 8;

/// Уведомления, на которые подписан наблюдатель клиента.
pub const NOTIFICATIONS: [&str; 6] = [
    "AXFocusedUIElementChanged",
    "AXSelectedRowsChanged",
    "AXSelectedChildrenChanged",
    "AXLayoutChanged",
    "AXRowCountChanged",
    "AXValueChanged",
];

const AX_ERROR_SUCCESS: i32 = 0;
const AX_ERROR_ATTRIBUTE_UNSUPPORTED: i32 = -25205;
const AX_ERROR_NO_VALUE: i32 = -25212;
/// Процесс пробы не ответил за срок: снимок недействителен.
pub const AX_ERROR_CANNOT_COMPLETE: i32 = -25204;
const AX_VALUE_CG_POINT: u32 = 1;
const AX_VALUE_CG_SIZE: u32 = 2;

type ObserverCallback = unsafe extern "C" fn(
    observer: *mut c_void,
    element: *const c_void,
    notification: *const c_void,
    refcon: *mut c_void,
);

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> u8;
    fn AXUIElementCreateApplication(pid: i32) -> *mut c_void;
    fn AXUIElementCopyAttributeValue(
        element: *const c_void,
        attribute: *const c_void,
        value: *mut *const c_void,
    ) -> i32;
    fn AXUIElementCopyAttributeNames(element: *const c_void, names: *mut *const c_void) -> i32;
    fn AXUIElementSetMessagingTimeout(element: *const c_void, timeout: f32) -> i32;
    fn AXValueGetValue(value: *const c_void, kind: u32, out: *mut c_void) -> u8;
    fn AXObserverCreate(pid: i32, callback: ObserverCallback, observer: *mut *mut c_void) -> i32;
    fn AXObserverAddNotification(
        observer: *mut c_void,
        element: *const c_void,
        notification: *const c_void,
        refcon: *mut c_void,
    ) -> i32;
    fn AXObserverGetRunLoopSource(observer: *mut c_void) -> *mut c_void;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRunLoopGetMain() -> *mut c_void;
    fn CFRunLoopAddSource(run_loop: *mut c_void, source: *mut c_void, mode: *const c_void);
    fn CFRunLoopRun();
    fn CFRunLoopStop(run_loop: *mut c_void);
    fn CFEqual(a: *const c_void, b: *const c_void) -> u8;
    static kCFRunLoopDefaultMode: *const c_void;
}

/// Объект Core Foundation во владении: отпускается в `Drop`. Все объекты CF — объекты
/// Objective-C, поэтому владение держит `Retained<AnyObject>`.
#[derive(Clone)]
struct Cf(Retained<AnyObject>);

// SAFETY: `AXUIElementRef` и значения атрибутов неизменяемы, API доступности потокобезопасен.
unsafe impl Send for Cf {}

impl Cf {
    /// Объект с правом владения (+1) из функции `Create` или `Copy`.
    ///
    /// # Safety
    ///
    /// `ptr` — объект CF с переданным владением.
    unsafe fn owned(ptr: *const c_void) -> Option<Cf> {
        let ptr = NonNull::new(ptr.cast_mut())?;
        // SAFETY: объект CF — объект Objective-C, владение передано вызывающим.
        unsafe { Retained::from_raw(ptr.cast::<AnyObject>().as_ptr()) }.map(Cf)
    }

    fn ptr(&self) -> *const c_void {
        Retained::as_ptr(&self.0).cast()
    }

    fn string(&self) -> Option<String> {
        self.0.downcast_ref::<NSString>().map(NSString::to_string)
    }

    fn number(&self) -> Option<&NSNumber> {
        self.0.downcast_ref::<NSNumber>()
    }

    fn array(&self) -> Option<Vec<Cf>> {
        let array = self.0.downcast_ref::<NSArray>()?;
        Some(array.iter().map(Cf).collect())
    }

    fn same(&self, other: &Cf) -> bool {
        // SAFETY: оба объекта живы.
        unsafe { CFEqual(self.ptr(), other.ptr()) != 0 }
    }
}

/// Значение атрибута: `Ok(None)` — атрибута нет или у него нет значения, `Err` — ошибка AX.
fn attribute(element: &Cf, name: &str) -> Result<Option<Cf>, i32> {
    let name = NSString::from_str(name);
    let mut value: *const c_void = std::ptr::null();
    // SAFETY: элемент жив, имя — NSString (мост к CFString), значение возвращается с владением.
    let code = unsafe {
        AXUIElementCopyAttributeValue(element.ptr(), Retained::as_ptr(&name).cast(), &mut value)
    };
    match code {
        // SAFETY: успешный `Copy` передаёт владение.
        AX_ERROR_SUCCESS => Ok(unsafe { Cf::owned(value) }),
        AX_ERROR_ATTRIBUTE_UNSUPPORTED | AX_ERROR_NO_VALUE => Ok(None),
        code => Err(code),
    }
}

fn names(element: &Cf) -> Vec<String> {
    let mut value: *const c_void = std::ptr::null();
    // SAFETY: элемент жив, массив имён возвращается с владением.
    let code = unsafe { AXUIElementCopyAttributeNames(element.ptr(), &mut value) };
    if code != AX_ERROR_SUCCESS {
        return Vec::new();
    }
    // SAFETY: успешный `Copy` передаёт владение.
    unsafe { Cf::owned(value) }
        .and_then(|array| array.array())
        .map(|items| items.iter().filter_map(Cf::string).collect())
        .unwrap_or_default()
}

fn point(value: &Cf) -> Option<CGPoint> {
    let mut point = CGPoint::new(0.0, 0.0);
    // SAFETY: значение AX типа CGPoint пишется в точку того же типа.
    let ok = unsafe {
        AXValueGetValue(
            value.ptr(),
            AX_VALUE_CG_POINT,
            (&mut point as *mut CGPoint).cast(),
        )
    };
    (ok != 0).then_some(point)
}

fn size(value: &Cf) -> Option<CGSize> {
    let mut size = CGSize::new(0.0, 0.0);
    // SAFETY: значение AX типа CGSize пишется в размер того же типа.
    let ok = unsafe {
        AXValueGetValue(
            value.ptr(),
            AX_VALUE_CG_SIZE,
            (&mut size as *mut CGSize).cast(),
        )
    };
    (ok != 0).then_some(size)
}

/// Ошибка чтения, при которой снимок недействителен.
struct Fatal(String);

fn fatal(what: &str, code: i32) -> Option<Fatal> {
    (code == AX_ERROR_CANNOT_COMPLETE).then(|| {
        Fatal(format!(
            "процесс пробы не ответил за {MESSAGING_TIMEOUT_S} с (kAXErrorCannotComplete) при чтении {what}"
        ))
    })
}

/// Атрибуты элемента. Ошибка роли означает, что элемент не прочитан (например, освобождён).
fn read(element: &Cf, with_names: bool) -> Result<AxElement, Fatal> {
    let mut out = AxElement::default();
    match attribute(element, "AXRole") {
        Ok(role) => out.role = role.and_then(|role| role.string()),
        Err(code) => {
            if let Some(fatal) = fatal("роли", code) {
                return Err(fatal);
            }
            out.error = Some(code);
            return Ok(out);
        }
    }
    let mut get = |name: &str| -> Result<Option<Cf>, Fatal> {
        match attribute(element, name) {
            Ok(value) => Ok(value),
            Err(code) => match fatal(name, code) {
                Some(fatal) => Err(fatal),
                None => {
                    out.error.get_or_insert(code);
                    Ok(None)
                }
            },
        }
    };
    let role_description = get("AXRoleDescription")?.and_then(|value| value.string());
    let description = get("AXDescription")?.and_then(|value| value.string());
    let value = get("AXValue")?.and_then(|value| value.string());
    let selected = get("AXSelected")?.and_then(|value| value.number().map(NSNumber::as_bool));
    let index = get("AXIndex")?.and_then(|value| value.number().map(NSNumber::integerValue));
    let position = get("AXPosition")?.and_then(|value| point(&value));
    let extent = get("AXSize")?.and_then(|value| size(&value));
    out.role_description = role_description;
    out.description = description;
    out.value = value;
    out.selected = selected;
    out.index = index.map(|index| index as i64);
    out.frame = position.zip(extent).map(|(position, extent)| AxFrame {
        x: position.x,
        y: position.y,
        width: extent.width,
        height: extent.height,
    });
    if with_names {
        out.attributes = names(element);
    }
    Ok(out)
}

fn read_array(element: &Cf, name: &str) -> Result<Option<Vec<Cf>>, Fatal> {
    match attribute(element, name) {
        Ok(value) => Ok(value.and_then(|value| value.array())),
        Err(code) => match fatal(name, code) {
            Some(fatal) => Err(fatal),
            None => Ok(None),
        },
    }
}

fn role(element: &Cf) -> Result<Option<String>, Fatal> {
    match attribute(element, "AXRole") {
        Ok(value) => Ok(value.and_then(|value| value.string())),
        Err(code) => match fatal("роли", code) {
            Some(fatal) => Err(fatal),
            None => Ok(None),
        },
    }
}

/// Первый контейнер строк в глубину от `element` и роли по пути к нему.
fn find_table(element: &Cf, depth: usize, path: &mut Vec<String>) -> Result<Option<Cf>, Fatal> {
    let own = role(element)?.unwrap_or_default();
    if own == TABLE_ROLE {
        return Ok(Some(element.clone()));
    }
    if depth == 0 {
        return Ok(None);
    }
    path.push(own);
    for child in read_array(element, "AXChildren")?.unwrap_or_default() {
        if let Some(found) = find_table(&child, depth - 1, path)? {
            return Ok(Some(found));
        }
    }
    path.pop();
    Ok(None)
}

static NOTES: Mutex<Vec<AxNote>> = Mutex::new(Vec::new());
static STARTED: OnceLock<Instant> = OnceLock::new();

fn elapsed_ms() -> f64 {
    STARTED.get_or_init(Instant::now).elapsed().as_secs_f64() * 1e3
}

unsafe extern "C" fn on_notification(
    _observer: *mut c_void,
    element: *const c_void,
    notification: *const c_void,
    _refcon: *mut c_void,
) {
    let at_ms = elapsed_ms();
    // SAFETY: имя уведомления — CFString на время вызова, элемент жив на время вызова.
    let name = unsafe { notification.cast::<NSString>().as_ref() }
        .map(NSString::to_string)
        .unwrap_or_default();
    let element = NonNull::new(element.cast_mut()).and_then(|ptr| {
        // SAFETY: элемент — объект CF на время вызова; удержание продлевает его жизнь.
        unsafe { Retained::retain(ptr.cast::<AnyObject>().as_ptr()) }.map(Cf)
    });
    let (role, index) = match &element {
        Some(element) => (
            attribute(element, "AXRole")
                .ok()
                .flatten()
                .and_then(|value| value.string()),
            attribute(element, "AXIndex")
                .ok()
                .flatten()
                .and_then(|value| value.number().map(|number| number.integerValue() as i64)),
        ),
        None => (None, None),
    };
    if let Ok(mut notes) = NOTES.lock() {
        notes.push(AxNote {
            name,
            role,
            index,
            at_ms,
        });
    }
}

/// Строка предыдущего снимка: подпись, элемент и прочитанное тогда.
struct Previous {
    description: Option<String>,
    element: Cf,
    read: AxElement,
}

fn snapshot(app: &Cf, label: &str, previous: &mut Vec<Previous>) -> AxSnapshot {
    let started = Instant::now();
    let mut out = AxSnapshot {
        label: label.to_owned(),
        ..AxSnapshot::default()
    };
    if let Err(Fatal(reason)) = fill(app, &mut out, previous) {
        out.error = Some(reason);
    }
    if let Ok(mut notes) = NOTES.lock() {
        out.notes = std::mem::take(&mut *notes);
    }
    out.read_ms = started.elapsed().as_secs_f64() * 1e3;
    out
}

fn fill(app: &Cf, out: &mut AxSnapshot, previous: &mut Vec<Previous>) -> Result<(), Fatal> {
    let window = match attribute(app, "AXFocusedWindow") {
        Ok(Some(window)) => Some(window),
        Ok(None) => read_array(app, "AXWindows")?.and_then(|windows| windows.into_iter().next()),
        Err(code) => {
            return Err(fatal("окна", code)
                .unwrap_or_else(|| Fatal(format!("окно не прочитано: ошибка AX {code}"))));
        }
    };
    let Some(window) = window else {
        return Err(Fatal("у процесса пробы нет окна".to_owned()));
    };
    let Some(table) = find_table(&window, SEARCH_DEPTH, &mut out.path)? else {
        return Ok(());
    };
    out.table = Some(read(&table, true)?);
    let rows = match read_array(&table, "AXVisibleRows")? {
        Some(rows) => {
            out.rows_source = "AXVisibleRows".to_owned();
            rows
        }
        None => {
            out.rows_source = "AXChildren".to_owned();
            let mut rows = Vec::new();
            for child in read_array(&table, "AXChildren")?.unwrap_or_default() {
                if role(&child)?.as_deref() == Some(ROW_ROLE) {
                    rows.push(child);
                }
            }
            rows
        }
    };
    let mut current = Vec::with_capacity(rows.len());
    for (number, row) in rows.into_iter().enumerate() {
        let element = read(&row, number == 0)?;
        let same_as_before = previous
            .iter()
            .find(|before| {
                before.description.is_some() && before.description == element.description
            })
            .map(|before| before.element.same(&row));
        out.rows.push(AxRowRead {
            element: element.clone(),
            same_as_before,
        });
        current.push(Previous {
            description: element.description.clone(),
            element: row,
            read: element,
        });
    }
    for before in previous.iter() {
        out.held.push(AxHeld {
            before: before.read.clone(),
            now: read(&before.element, false)?,
        });
    }
    *previous = current;
    out.selected_rows = read_array(&table, "AXSelectedRows")?.map(|rows| {
        rows.iter()
            .filter_map(|row| {
                attribute(row, "AXIndex")
                    .ok()
                    .flatten()
                    .and_then(|value| value.number().map(|number| number.integerValue() as i64))
            })
            .collect()
    });
    match attribute(app, "AXFocusedUIElement") {
        Ok(Some(focused)) => {
            out.focused_is_table = focused.same(&table);
            out.focused = Some(read(&focused, false)?);
        }
        Ok(None) => {}
        Err(code) => {
            if let Some(fatal) = fatal("фокуса", code) {
                return Err(fatal);
            }
        }
    }
    Ok(())
}

/// Процесс клиента. Возвращается после закрытия stdin или с отказом до готовности.
pub fn run(pid: i32) -> Result<(), String> {
    let _ = STARTED.get_or_init(Instant::now);
    // SAFETY: функция без аргументов.
    if unsafe { AXIsProcessTrusted() } == 0 {
        return Err(
            "предусловие: у терминала нет разрешения Accessibility (AXIsProcessTrusted ложно)"
                .to_owned(),
        );
    }
    // SAFETY: `Create` передаёт владение элементом приложения.
    let app = unsafe { Cf::owned(AXUIElementCreateApplication(pid)) }
        .ok_or("AXUIElementCreateApplication вернул NULL")?;
    // SAFETY: элемент жив.
    let code = unsafe { AXUIElementSetMessagingTimeout(app.ptr(), MESSAGING_TIMEOUT_S) };
    if code != AX_ERROR_SUCCESS {
        return Err(format!("AXUIElementSetMessagingTimeout: ошибка AX {code}"));
    }
    let mut observer: *mut c_void = std::ptr::null_mut();
    // SAFETY: обратный вызов — функция с сигнатурой `AXObserverCallback`.
    let code = unsafe { AXObserverCreate(pid, on_notification, &mut observer) };
    if code != AX_ERROR_SUCCESS || observer.is_null() {
        return Err(format!("AXObserverCreate: ошибка AX {code}"));
    }
    // SAFETY: `Create` передаёт владение наблюдателем; он живёт до выхода из `run`.
    let observer = unsafe { Cf::owned(observer) }.ok_or("наблюдатель NULL")?;
    let mut added = Vec::new();
    for name in NOTIFICATIONS {
        let ns = NSString::from_str(name);
        // SAFETY: наблюдатель и элемент живы, имя — CFString.
        let code = unsafe {
            AXObserverAddNotification(
                observer.ptr().cast_mut(),
                app.ptr(),
                Retained::as_ptr(&ns).cast(),
                std::ptr::null_mut(),
            )
        };
        added.push(format!("{name}={code}"));
    }
    // SAFETY: источник принадлежит наблюдателю, главный цикл живёт весь процесс.
    unsafe {
        let source = AXObserverGetRunLoopSource(observer.ptr().cast_mut());
        CFRunLoopAddSource(CFRunLoopGetMain(), source, kCFRunLoopDefaultMode);
    }
    {
        let mut stdout = std::io::stdout().lock();
        let _ = writeln!(stdout, "{READY_LINE} {}", added.join(" "));
        let _ = stdout.flush();
    }
    let reader = thread::Builder::new()
        .name("ax-client-stdin".to_owned())
        .spawn(move || {
            let mut previous = Vec::new();
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else {
                    break;
                };
                let Some(label) = line.strip_prefix("snapshot ") else {
                    continue;
                };
                let snapshot = snapshot(&app, label, &mut previous);
                let json = serde_json::to_string(&snapshot).unwrap_or_else(|err| {
                    format!("{{\"label\":\"{label}\",\"error\":\"JSON: {err}\"}}")
                });
                let mut stdout = std::io::stdout().lock();
                if writeln!(stdout, "{json}").is_err() || stdout.flush().is_err() {
                    break;
                }
            }
            // SAFETY: остановка главного цикла из другого потока допустима.
            unsafe { CFRunLoopStop(CFRunLoopGetMain()) };
        })
        .map_err(|err| format!("поток stdin не запущен: {err}"))?;
    // SAFETY: главный цикл потока, источник наблюдателя добавлен.
    unsafe { CFRunLoopRun() };
    let _ = reader.join();
    drop(observer);
    Ok(())
}

/// Ответ клиента сценарию.
#[derive(Debug)]
pub enum Reply {
    Ready(String),
    Snapshot(Box<AxSnapshot>),
    /// Клиент отказал, вышел или прислал непонятную строку.
    Failed(String),
}

/// Клиент глазами сценария: процесс, его stdin и ответы рабочего потока.
pub struct Client {
    stdin: Option<ChildStdin>,
    replies: Receiver<Reply>,
    pid: u32,
}

impl Client {
    /// Запускает клиент к процессу `pid`, stderr клиента — в `dir/ax-client-stderr.txt`.
    pub fn spawn(pid: u32, dir: &Path) -> Result<Client, String> {
        let exe = std::env::current_exe().map_err(|err| format!("путь пробы: {err}"))?;
        let stderr = std::fs::File::create(dir.join("ax-client-stderr.txt"))
            .map_err(|err| format!("ax-client-stderr.txt: {err}"))?;
        let mut child: Child = Command::new(exe)
            .arg("--ax-client")
            .arg(pid.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()
            .map_err(|err| format!("клиент доступности не запущен: {err}"))?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().ok_or("нет stdout клиента")?;
        let client_pid = child.id();
        let (tx, replies) = mpsc::channel();
        thread::Builder::new()
            .name("ax-client-reader".to_owned())
            .spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else {
                        break;
                    };
                    let reply = if let Some(rest) = line.strip_prefix(READY_LINE) {
                        Reply::Ready(rest.trim().to_owned())
                    } else if let Some(reason) = line.strip_prefix(ERROR_LINE) {
                        Reply::Failed(reason.to_owned())
                    } else {
                        match serde_json::from_str::<AxSnapshot>(&line) {
                            Ok(snapshot) => Reply::Snapshot(Box::new(snapshot)),
                            Err(err) => {
                                Reply::Failed(format!("строка клиента не разобрана: {err}"))
                            }
                        }
                    };
                    if tx.send(reply).is_err() {
                        break;
                    }
                }
                let status = child.wait();
                let _ = tx.send(Reply::Failed(format!("клиент вышел: {status:?}")));
            })
            .map_err(|err| format!("поток ответов клиента не запущен: {err}"))?;
        Ok(Client {
            stdin,
            replies,
            pid: client_pid,
        })
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Просит снимок с меткой `label`. Строка короче буфера канала, запись не ждёт клиента.
    pub fn request(&mut self, label: &str) -> Result<(), String> {
        let stdin = self.stdin.as_mut().ok_or("stdin клиента закрыт")?;
        writeln!(stdin, "snapshot {label}")
            .and_then(|()| stdin.flush())
            .map_err(|err| format!("запрос клиенту не записан: {err}"))
    }

    /// Ответ клиента, если он пришёл.
    pub fn poll(&self) -> Option<Reply> {
        match self.replies.try_recv() {
            Ok(reply) => Some(reply),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                Some(Reply::Failed("поток ответов клиента завершился".to_owned()))
            }
        }
    }
}

impl Drop for Client {
    /// Закрытие stdin завершает клиент, его ждёт рабочий поток.
    fn drop(&mut self) {
        drop(self.stdin.take());
    }
}
