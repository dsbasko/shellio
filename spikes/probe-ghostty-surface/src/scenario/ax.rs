//! Клиент доступности стенда: настоящий AXUIElement API (HIServices) в отдельном процессе —
//! режим `--ax-client` того же исполняемого файла. Сценарий запускает его из рабочего потока и ждёт
//! ответ JSON; главный поток процесса пробы в это время свободен и отвечает на межпроцессные
//! запросы через сервер доступности AppKit, как отвечал бы VoiceOver.
//!
//! Два пути, которые этого не проверяют: прямой `accessibilityAttributeValue:` у `NSView` в
//! процессе не переходит к методам протокола `NSAccessibility` (роль читается как `AXUnknown`), а
//! AXUIElement API к собственному процессу обходит межпроцессный путь и зовёт методы вида в
//! потоке клиента, а не на главном.
//!
//! Объекты Core Foundation держатся как `Retained<AnyObject>`: каждый из них — объект
//! Objective-C, строки, числа и массивы мостятся к `NSString`, `NSNumber` и `NSArray`.

use std::ffi::c_void;
use std::io::Write;
use std::process::{Command, ExitCode, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_foundation::{NSArray, NSAttributedString, NSNumber, NSString};
use serde::{Deserialize, Serialize};

use crate::utf16::Range16;

type AxError = i32;
type CfRef = *const c_void;

const AX_SUCCESS: AxError = 0;
/// `kAXErrorNoValue`: у атрибута нет значения.
const AX_NO_VALUE: AxError = -25212;
/// `kAXValueTypeCFRange`.
const AX_VALUE_CF_RANGE: u32 = 4;
/// Срок одного запроса к процессу, секунды.
const MESSAGING_TIMEOUT_S: f32 = 5.0;
/// Предел обхода дерева и всего запроса: процесс клиента заканчивает сам.
const TREE_LIMIT: usize = 5000;
const DEADLINE: Duration = Duration::from_secs(8);
/// Сколько ролей обхода записывается для диагностики.
const ROLES_LIMIT: usize = 64;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CfRange {
    location: isize,
    length: isize,
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> u8;
    fn AXUIElementCreateApplication(pid: libc::pid_t) -> CfRef;
    fn AXUIElementSetMessagingTimeout(element: CfRef, timeout: f32) -> AxError;
    fn AXUIElementCopyElementAtPosition(
        application: CfRef,
        x: f32,
        y: f32,
        element: *mut CfRef,
    ) -> AxError;
    fn AXUIElementCopyAttributeValue(
        element: CfRef,
        attribute: CfRef,
        value: *mut CfRef,
    ) -> AxError;
    fn AXUIElementCopyAttributeNames(element: CfRef, names: *mut CfRef) -> AxError;
    fn AXUIElementCopyParameterizedAttributeNames(element: CfRef, names: *mut CfRef) -> AxError;
    fn AXUIElementCopyParameterizedAttributeValue(
        element: CfRef,
        attribute: CfRef,
        parameter: CfRef,
        result: *mut CfRef,
    ) -> AxError;
    fn AXValueCreate(kind: u32, value: *const c_void) -> CfRef;
    fn AXValueGetValue(value: CfRef, kind: u32, out: *mut c_void) -> u8;
}

/// Процесс доверен Accessibility: без этого клиент получает отказ.
fn trusted() -> bool {
    // SAFETY: функция без аргументов.
    unsafe { AXIsProcessTrusted() != 0 }
}

/// Параметрический атрибут запроса.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Param {
    LineForIndex(usize),
    RangeForLine(usize),
    StringForRange(Range16),
    AttributedStringForRange(Range16),
}

/// Запрос клиента: элемент в точке экрана (от левого верхнего угла главного экрана, как в AX
/// API), его атрибуты и параметрические атрибуты, и обход дерева приложения.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request {
    pub pid: libc::pid_t,
    pub point: (f64, f64),
    pub params: Vec<Param>,
    pub tree: bool,
}

/// Значение атрибута, прочитанное клиентом.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Value {
    Text(String),
    Integer(isize),
    Range(Range16),
    /// Атрибут без значения (`kAXErrorNoValue`).
    None,
    /// Значение другого типа: имя класса.
    Other(String),
    /// Ошибка AX API.
    Error(AxError),
}

impl Value {
    pub fn text(&self) -> Option<&str> {
        match self {
            Value::Text(text) => Some(text),
            _ => None,
        }
    }

    pub fn integer(&self) -> Option<isize> {
        match self {
            Value::Integer(value) => Some(*value),
            _ => None,
        }
    }

    pub fn range(&self) -> Option<Range16> {
        match self {
            Value::Range(range) => Some(*range),
            _ => None,
        }
    }
}

/// Элемент в точке и его атрибуты текста.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Element {
    pub role: Value,
    pub role_description: Value,
    pub description: Value,
    pub value: Value,
    pub characters: Value,
    pub visible: Value,
    pub selected_text: Value,
    pub selected: Value,
    pub insertion_line: Value,
    pub names: Vec<String>,
    pub parameterized_names: Vec<String>,
    pub params: Vec<(Param, Value)>,
}

/// Ответ клиента.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Response {
    /// Процесс клиента доверен Accessibility.
    pub trusted: bool,
    /// PID процесса клиента: он не процесс пробы.
    pub client_pid: u32,
    /// Ошибка `AXUIElementCopyElementAtPosition`, если элемента нет.
    pub hit_error: Option<AxError>,
    pub element: Option<Element>,
    /// `AXDescription` текстовых областей в дереве окон приложения, число пройденных узлов и
    /// первые роли обхода.
    pub text_areas: Vec<String>,
    pub tree_nodes: usize,
    pub tree_roles: Vec<String>,
    /// Обход остановлен пределом узлов или срока.
    pub tree_cut: bool,
    pub ms: f64,
}

/// Запрос в рабочем потоке; ответ забирает [`Pending::poll`].
pub struct Pending {
    rx: Receiver<Result<Response, String>>,
}

pub enum Poll {
    Ready(Box<Result<Response, String>>),
    Waiting,
    Lost,
}

impl Pending {
    pub fn poll(&self) -> Poll {
        match self.rx.try_recv() {
            Ok(response) => Poll::Ready(Box::new(response)),
            Err(TryRecvError::Empty) => Poll::Waiting,
            Err(TryRecvError::Disconnected) => Poll::Lost,
        }
    }
}

/// Процесс клиента `--ax-client` из рабочего потока: он сам заканчивает за [`DEADLINE`].
pub fn start(request: &Request) -> Result<Pending, String> {
    let json = serde_json::to_string(request).map_err(|error| error.to_string())?;
    let exe =
        std::env::current_exe().map_err(|error| format!("путь исполняемого файла: {error}"))?;
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("probe-ax-client".to_owned())
        .spawn(move || {
            let output = Command::new(exe)
                .arg("--ax-client")
                .arg(json)
                .stdin(Stdio::null())
                .output()
                .map_err(|error| format!("запуск клиента доступности: {error}"));
            let response = output.and_then(|output| {
                if !output.status.success() {
                    return Err(format!(
                        "клиент доступности: {}, stderr: {}",
                        output.status,
                        String::from_utf8_lossy(&output.stderr).trim()
                    ));
                }
                serde_json::from_slice::<Response>(&output.stdout)
                    .map_err(|error| format!("ответ клиента доступности: {error}"))
            });
            let _ = tx.send(response);
        })
        .map_err(|error| format!("поток клиента доступности: {error}"))?;
    Ok(Pending { rx })
}

/// Режим `--ax-client`: запрос JSON из аргумента, ответ JSON в stdout.
pub fn client_main(request: &str) -> ExitCode {
    let request: Request = match serde_json::from_str(request) {
        Ok(request) => request,
        Err(error) => {
            let _ = writeln!(std::io::stderr(), "запрос клиента доступности: {error}");
            return ExitCode::from(2);
        }
    };
    let response = run(&request);
    match serde_json::to_string(&response) {
        Ok(json) => {
            let _ = writeln!(std::io::stdout(), "{json}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            let _ = writeln!(std::io::stderr(), "ответ клиента доступности: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Объект CF по правилу Copy: забирает владение.
///
/// # Safety
/// `raw` — объект CF с +1 или NULL.
unsafe fn owned(raw: CfRef) -> Option<Retained<AnyObject>> {
    // SAFETY: по контракту объект с +1; каждый объект CF — объект Objective-C.
    unsafe { Retained::from_raw(raw.cast_mut().cast::<AnyObject>()) }
}

fn as_ref(object: &AnyObject) -> CfRef {
    std::ptr::from_ref(object).cast()
}

fn copy_attribute(element: &AnyObject, attribute: &str) -> Result<Retained<AnyObject>, AxError> {
    let name = NSString::from_str(attribute);
    let mut value: CfRef = std::ptr::null();
    // SAFETY: элемент и имя — живые объекты CF, `value` — указатель на локальную переменную.
    let error =
        unsafe { AXUIElementCopyAttributeValue(as_ref(element), as_ref(&name), &mut value) };
    // SAFETY: AX API отдаёт значение по правилу Copy.
    match (error, unsafe { owned(value) }) {
        (AX_SUCCESS, Some(value)) => Ok(value),
        (AX_SUCCESS, None) => Err(AX_NO_VALUE),
        (error, _) => Err(error),
    }
}

fn names(element: &AnyObject, parameterized: bool) -> Vec<String> {
    let mut names: CfRef = std::ptr::null();
    // SAFETY: элемент — живой объект CF, `names` — указатель на локальную переменную.
    let error = unsafe {
        if parameterized {
            AXUIElementCopyParameterizedAttributeNames(as_ref(element), &mut names)
        } else {
            AXUIElementCopyAttributeNames(as_ref(element), &mut names)
        }
    };
    // SAFETY: AX API отдаёт массив по правилу Copy.
    let names = unsafe { owned(names) };
    match (error, names) {
        (AX_SUCCESS, Some(names)) => strings(&names),
        _ => Vec::new(),
    }
}

fn strings(array: &AnyObject) -> Vec<String> {
    array.downcast_ref::<NSArray>().map_or(Vec::new(), |array| {
        array
            .iter()
            .filter_map(|item| item.downcast_ref::<NSString>().map(ToString::to_string))
            .collect()
    })
}

fn convert(value: Result<Retained<AnyObject>, AxError>) -> Value {
    let value = match value {
        Ok(value) => value,
        Err(AX_NO_VALUE) => return Value::None,
        Err(error) => return Value::Error(error),
    };
    if let Some(text) = value.downcast_ref::<NSString>() {
        return Value::Text(text.to_string());
    }
    if let Some(text) = value.downcast_ref::<NSAttributedString>() {
        return Value::Text(text.string().to_string());
    }
    if let Some(number) = value.downcast_ref::<NSNumber>() {
        return Value::Integer(number.integerValue());
    }
    let mut range = CfRange::default();
    // SAFETY: объект CF жив, `range` — локальная структура `CFRange`; для значения другого типа
    // функция возвращает 0 и ничего не пишет.
    let is_range =
        unsafe { AXValueGetValue(as_ref(&value), AX_VALUE_CF_RANGE, (&raw mut range).cast()) } != 0;
    if is_range {
        return Value::Range(Range16::new(
            usize::try_from(range.location).unwrap_or(usize::MAX),
            usize::try_from(range.length).unwrap_or(0),
        ));
    }
    Value::Other(value.class().name().to_string_lossy().into_owned())
}

fn attribute(element: &AnyObject, attribute: &str) -> Value {
    convert(copy_attribute(element, attribute))
}

fn parameterized(element: &AnyObject, param: &Param) -> Value {
    let (name, parameter) = match param {
        Param::LineForIndex(index) => ("AXLineForIndex", number(*index)),
        Param::RangeForLine(line) => ("AXRangeForLine", number(*line)),
        Param::StringForRange(range) => ("AXStringForRange", range_value(*range)),
        Param::AttributedStringForRange(range) => {
            ("AXAttributedStringForRange", range_value(*range))
        }
    };
    let Some(parameter) = parameter else {
        return Value::Error(-1);
    };
    let name = NSString::from_str(name);
    let mut result: CfRef = std::ptr::null();
    // SAFETY: элемент, имя и параметр — живые объекты CF, `result` — локальная переменная.
    let error = unsafe {
        AXUIElementCopyParameterizedAttributeValue(
            as_ref(element),
            as_ref(&name),
            as_ref(&parameter),
            &mut result,
        )
    };
    // SAFETY: AX API отдаёт значение по правилу Copy.
    let result = unsafe { owned(result) };
    convert(match (error, result) {
        (AX_SUCCESS, Some(result)) => Ok(result),
        (AX_SUCCESS, None) => Err(AX_NO_VALUE),
        (error, _) => Err(error),
    })
}

fn number(value: usize) -> Option<Retained<AnyObject>> {
    let number = NSNumber::numberWithInteger(isize::try_from(value).ok()?);
    Some(Retained::into_super(Retained::into_super(
        Retained::into_super(number),
    )))
}

fn range_value(range: Range16) -> Option<Retained<AnyObject>> {
    let range = CfRange {
        location: isize::try_from(range.location).ok()?,
        length: isize::try_from(range.length).ok()?,
    };
    // SAFETY: `range` — `CFRange` того типа, что указан; значение по правилу Create.
    unsafe { owned(AXValueCreate(AX_VALUE_CF_RANGE, (&raw const range).cast())) }
}

fn element(element: &AnyObject, params: &[Param]) -> Element {
    Element {
        role: attribute(element, "AXRole"),
        role_description: attribute(element, "AXRoleDescription"),
        description: attribute(element, "AXDescription"),
        value: attribute(element, "AXValue"),
        characters: attribute(element, "AXNumberOfCharacters"),
        visible: attribute(element, "AXVisibleCharacterRange"),
        selected_text: attribute(element, "AXSelectedText"),
        selected: attribute(element, "AXSelectedTextRange"),
        insertion_line: attribute(element, "AXInsertionPointLineNumber"),
        names: names(element, false),
        parameterized_names: names(element, true),
        params: params
            .iter()
            .map(|param| (param.clone(), parameterized(element, param)))
            .collect(),
    }
}

/// Обход окон приложения по `AXChildren`: `AXDescription` текстовых областей.
fn text_areas(application: &AnyObject, deadline: Instant, response: &mut Response) {
    let mut stack: Vec<Retained<AnyObject>> = match copy_attribute(application, "AXWindows") {
        Ok(windows) => windows
            .downcast_ref::<NSArray>()
            .map_or(Vec::new(), |windows| windows.iter().collect()),
        Err(_) => Vec::new(),
    };
    while let Some(node) = stack.pop() {
        if response.tree_nodes >= TREE_LIMIT || Instant::now() >= deadline {
            response.tree_cut = true;
            break;
        }
        response.tree_nodes += 1;
        let role = attribute(&node, "AXRole");
        if response.tree_roles.len() < ROLES_LIMIT {
            response.tree_roles.push(
                role.text()
                    .map_or_else(|| format!("{role:?}"), str::to_owned),
            );
        }
        if role.text() == Some("AXTextArea")
            && let Value::Text(label) = attribute(&node, "AXDescription")
        {
            response.text_areas.push(label);
        }
        if let Ok(children) = copy_attribute(&node, "AXChildren")
            && let Some(children) = children.downcast_ref::<NSArray>()
        {
            stack.extend(children.iter());
        }
    }
}

fn run(request: &Request) -> Response {
    let started = crate::clock::uptime_ns();
    let deadline = Instant::now() + DEADLINE;
    let mut response = Response {
        trusted: trusted(),
        client_pid: std::process::id(),
        hit_error: None,
        element: None,
        text_areas: Vec::new(),
        tree_nodes: 0,
        tree_roles: Vec::new(),
        tree_cut: false,
        ms: 0.0,
    };
    // SAFETY: создание элемента приложения по PID, значение по правилу Create.
    let application = unsafe { owned(AXUIElementCreateApplication(request.pid)) };
    if let Some(application) = application.filter(|_| response.trusted) {
        // SAFETY: элемент приложения жив.
        unsafe { AXUIElementSetMessagingTimeout(as_ref(&application), MESSAGING_TIMEOUT_S) };
        let mut hit: CfRef = std::ptr::null();
        // SAFETY: элемент приложения жив, `hit` — локальная переменная; значение по правилу Copy.
        let error = unsafe {
            AXUIElementCopyElementAtPosition(
                as_ref(&application),
                request.point.0 as f32,
                request.point.1 as f32,
                &mut hit,
            )
        };
        // SAFETY: AX API отдаёт элемент по правилу Copy.
        match (error, unsafe { owned(hit) }) {
            (AX_SUCCESS, Some(hit)) => response.element = Some(element(&hit, &request.params)),
            (error, _) => response.hit_error = Some(error),
        }
        if request.tree {
            text_areas(&application, deadline, &mut response);
        }
    }
    response.ms = crate::clock::interval_ms(started, crate::clock::uptime_ns());
    response
}
