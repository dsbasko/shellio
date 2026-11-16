//! Безопасная обёртка пробы над `libghostty-vt` набора `vt` (функция `vt`, ADR-0034, ADR-0035).
//!
//! Весь `unsafe` над C API живёт здесь. Терминал, его формат, снимок и continuation, чтение клеток,
//! курсора, режимов и экрана через типизированный `get`, колбэки эффектов и кодировщики ввода.
//! Данные, которые C API одалживает на время вызова или колбэка, копируются сразу. Память,
//! выделенная библиотекой, освобождается через `ghostty_free`.
//!
//! Колбэки эффектов вызываются синхронно внутри [`Terminal::write`]: их состояние лежит в отдельной
//! куче по указателю userdata и не пересекается с `&mut Terminal`, через который идёт запись.

#[rustfmt::skip]
#[allow(
    dead_code,
    non_camel_case_types,
    non_upper_case_globals,
    non_snake_case,
    unsafe_op_in_unsafe_fn,
    clippy::all
)]
mod bindings;

use std::ffi::c_void;
use std::fmt;
use std::mem::{self, MaybeUninit};
use std::ptr::{self, NonNull};

use bindings as sys;

/// Отказ функции C API: имя вызова и код `GhosttyResult`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    pub call: &'static str,
    pub code: i32,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self.code {
            sys::GhosttyResult_GHOSTTY_OUT_OF_MEMORY => "OUT_OF_MEMORY",
            sys::GhosttyResult_GHOSTTY_INVALID_VALUE => "INVALID_VALUE",
            sys::GhosttyResult_GHOSTTY_OUT_OF_SPACE => "OUT_OF_SPACE",
            sys::GhosttyResult_GHOSTTY_NO_VALUE => "NO_VALUE",
            sys::GhosttyResult_GHOSTTY_IO_ERROR => "IO_ERROR",
            sys::GhosttyResult_GHOSTTY_LIMIT_EXCEEDED => "LIMIT_EXCEEDED",
            sys::GhosttyResult_GHOSTTY_REJECTED => "REJECTED",
            _ => "неизвестный код",
        };
        write!(f, "{}: {} ({name})", self.call, self.code)
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

fn check(call: &'static str, code: sys::GhosttyResult) -> Result<()> {
    if code == sys::GhosttyResult_GHOSTTY_SUCCESS {
        Ok(())
    } else {
        Err(Error { call, code })
    }
}

/// Структура C с полем `size` в начале, заполненная нулями: аналог `GHOSTTY_INIT_SIZED`.
///
/// # Safety
/// `T` — структура привязок из целых, логических значений, указателей и объединений, для которой
/// нулевые байты — допустимое значение, а первое поле — `size: usize`.
unsafe fn sized<T>() -> T {
    // SAFETY: условие вызывающего: нулевые байты допустимы для T.
    let mut value: T = unsafe { MaybeUninit::zeroed().assume_init() };
    // SAFETY: условие вызывающего: первое поле T — size: usize.
    unsafe {
        ptr::write(
            ptr::addr_of_mut!(value).cast::<usize>(),
            mem::size_of::<T>(),
        )
    };
    value
}

fn zeroed<T>() -> T {
    // SAFETY: используется только для структур привязок без ссылок и ненулевых типов, где нули —
    // допустимое значение; каждое место вызова передаёт такую структуру.
    unsafe { MaybeUninit::zeroed().assume_init() }
}

/// Копия байтов, выделенных библиотекой, с освобождением через `ghostty_free`.
///
/// # Safety
/// `ptr` и `len` получены от функции `_alloc` с аллокатором по умолчанию, `ptr` может быть NULL
/// при `len == 0`.
unsafe fn take_alloc(ptr: *mut u8, len: usize) -> Vec<u8> {
    if ptr.is_null() {
        return Vec::new();
    }
    // SAFETY: условие вызывающего: ptr указывает на len байтов, выделенных библиотекой.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec();
    // SAFETY: тот же аллокатор по умолчанию, тот же указатель и длина.
    unsafe { sys::ghostty_free(ptr::null(), ptr, len) };
    bytes
}

/// Копия заимствованной строки C API.
///
/// # Safety
/// `value.ptr` указывает на `value.len` байтов или равен NULL при нулевой длине.
unsafe fn string_bytes(value: sys::GhosttyString) -> Vec<u8> {
    if value.ptr.is_null() || value.len == 0 {
        return Vec::new();
    }
    // SAFETY: условие вызывающего.
    unsafe { std::slice::from_raw_parts(value.ptr, value.len) }.to_vec()
}

// --- режимы, экран, стиль ---

/// Режим терминала: номер и признак ANSI, как `ghostty_mode_new` (макросы `GHOSTTY_MODE_*` bindgen
/// не выдаёт).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Mode(u16);

impl Mode {
    pub const fn dec(value: u16) -> Self {
        Mode(value & 0x7FFF)
    }
    pub const fn ansi(value: u16) -> Self {
        Mode((value & 0x7FFF) | 0x8000)
    }
    pub const fn value(self) -> u16 {
        self.0 & 0x7FFF
    }
    pub const fn is_ansi(self) -> bool {
        self.0 & 0x8000 != 0
    }

    pub const INSERT: Mode = Mode::ansi(4);
    pub const LINEFEED: Mode = Mode::ansi(20);
    pub const CURSOR_KEYS: Mode = Mode::dec(1);
    pub const ORIGIN: Mode = Mode::dec(6);
    pub const WRAPAROUND: Mode = Mode::dec(7);
    pub const CURSOR_VISIBLE: Mode = Mode::dec(25);
    pub const KEYPAD_KEYS: Mode = Mode::dec(66);
    pub const LEFT_RIGHT_MARGIN: Mode = Mode::dec(69);
    pub const NORMAL_MOUSE: Mode = Mode::dec(1000);
    pub const BUTTON_MOUSE: Mode = Mode::dec(1002);
    pub const ANY_MOUSE: Mode = Mode::dec(1003);
    pub const FOCUS_EVENT: Mode = Mode::dec(1004);
    pub const SGR_MOUSE: Mode = Mode::dec(1006);
    pub const ALT_SCREEN: Mode = Mode::dec(1047);
    pub const ALT_SCREEN_SAVE: Mode = Mode::dec(1049);
    pub const BRACKETED_PASTE: Mode = Mode::dec(2004);
    pub const SYNC_OUTPUT: Mode = Mode::dec(2026);
    pub const GRAPHEME_CLUSTER: Mode = Mode::dec(2027);
    pub const IN_BAND_RESIZE: Mode = Mode::dec(2048);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    Primary,
    Alternate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Color {
    None,
    Palette(u8),
    Rgb(u8, u8, u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    fn from_sys(rgb: sys::GhosttyColorRgb) -> Rgb {
        Rgb {
            r: rgb.r,
            g: rgb.g,
            b: rgb.b,
        }
    }

    fn to_sys(self) -> sys::GhosttyColorRgb {
        sys::GhosttyColorRgb {
            r: self.r,
            g: self.g,
            b: self.b,
        }
    }
}

/// Цвета терминала: текущие динамические (переопределение OSC 10/11/12 или значение по умолчанию,
/// `None` — не задано ни то, ни другое) и палитра из 256 цветов.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Colors {
    pub foreground: Option<Rgb>,
    pub background: Option<Rgb>,
    pub cursor: Option<Rgb>,
    pub palette: Vec<Rgb>,
}

impl Color {
    fn from_sys(color: &sys::GhosttyStyleColor) -> Color {
        match color.tag {
            // SAFETY: тег говорит, какое поле объединения записано.
            sys::GhosttyStyleColorTag_GHOSTTY_STYLE_COLOR_PALETTE => {
                Color::Palette(unsafe { color.value.palette })
            }
            sys::GhosttyStyleColorTag_GHOSTTY_STYLE_COLOR_RGB => {
                // SAFETY: тег RGB.
                let rgb = unsafe { color.value.rgb };
                Color::Rgb(rgb.r, rgb.g, rgb.b)
            }
            _ => Color::None,
        }
    }
}

/// SGR-стиль клетки или курсора. `underline` — значение `GHOSTTY_SGR_UNDERLINE_*`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub underline_color: Color,
    pub bold: bool,
    pub italic: bool,
    pub faint: bool,
    pub blink: bool,
    pub inverse: bool,
    pub invisible: bool,
    pub strikethrough: bool,
    pub overline: bool,
    pub underline: i32,
}

impl Style {
    fn from_sys(style: &sys::GhosttyStyle) -> Style {
        Style {
            fg: Color::from_sys(&style.fg_color),
            bg: Color::from_sys(&style.bg_color),
            underline_color: Color::from_sys(&style.underline_color),
            bold: style.bold,
            italic: style.italic,
            faint: style.faint,
            blink: style.blink,
            inverse: style.inverse,
            invisible: style.invisible,
            strikethrough: style.strikethrough,
            overline: style.overline,
            underline: style.underline,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cursor {
    pub x: u16,
    pub y: u16,
    pub pending_wrap: bool,
    pub visible: bool,
    pub style: Style,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wide {
    Narrow,
    Wide,
    SpacerTail,
    SpacerHead,
}

/// Система координат точки сетки.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Area {
    /// Активная область, где ходит курсор.
    Active,
    /// Видимая область.
    Viewport,
    /// Весь экран вместе с историей.
    Screen,
    /// Только история до активной области.
    History,
}

impl Area {
    fn tag(self) -> sys::GhosttyPointTag {
        match self {
            Area::Active => sys::GhosttyPointTag_GHOSTTY_POINT_TAG_ACTIVE,
            Area::Viewport => sys::GhosttyPointTag_GHOSTTY_POINT_TAG_VIEWPORT,
            Area::Screen => sys::GhosttyPointTag_GHOSTTY_POINT_TAG_SCREEN,
            Area::History => sys::GhosttyPointTag_GHOSTTY_POINT_TAG_HISTORY,
        }
    }
}

/// Клетка сетки: кластер графем (пусто для пустой клетки), ширина, стиль, гиперссылка и строка.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    pub text: Vec<u32>,
    pub wide: Wide,
    pub style: Style,
    pub hyperlink: Option<String>,
    pub protected: bool,
    /// Строка мягко перенесена на следующую.
    pub row_wrap: bool,
    /// Строка продолжает мягко перенесённую предыдущую.
    pub row_wrap_continuation: bool,
}

impl Cell {
    pub fn text_string(&self) -> String {
        self.text
            .iter()
            .filter_map(|&c| char::from_u32(c))
            .collect()
    }
}

// --- эффекты ---

/// Ответы, которые терминал отдаёт на запросы через колбэки. `None` — колбэк не отвечает: терминал
/// молчит на запросы размера и цветовой схемы, а на XTVERSION и DA отвечает своим значением по
/// умолчанию (см. [`Terminal::enable_effects`]).
#[derive(Clone, Debug, Default)]
pub struct Responder {
    pub device_attributes: Option<DeviceAttributes>,
    pub xtversion: Option<String>,
    pub size: Option<SizeReport>,
    pub color_scheme: Option<ColorScheme>,
    /// Содержимое буфера обмена для ответа на чтение OSC 52; `None` — чтение без ответа.
    pub clipboard: Option<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceAttributes {
    pub conformance_level: u16,
    pub features: Vec<u16>,
    pub device_type: u16,
    pub firmware_version: u16,
    pub unit_id: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SizeReport {
    pub rows: u16,
    pub columns: u16,
    pub cell_width: u32,
    pub cell_height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorScheme {
    Light,
    Dark,
}

/// Эффект, о котором терминал сообщил колбэком. Байты ответов в PTY идут отдельно,
/// см. [`Terminal::take_pty_output`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    Bell,
    /// Заголовок после смены. Читается в колбэке: несколько смен в одной записи иначе слились бы в
    /// последнюю.
    TitleChanged {
        title: Vec<u8>,
    },
    ClipboardWrite {
        location: i32,
        /// Пары «MIME, данные»; пусто — очистка буфера.
        contents: Vec<(String, Vec<u8>)>,
    },
    ClipboardRead {
        location: i32,
        mimes: Vec<String>,
        answered: bool,
    },
}

/// Запрос чтения буфера обмена, переданный [`ClipboardSource`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardRequest {
    pub location: i32,
    pub mimes: Vec<String>,
}

/// Источник ответа на чтение буфера обмена вместо [`Responder::clipboard`]: вызывается синхронно
/// внутри записи и может ждать ответа, например GUI; `None` — вернуться без ответа.
pub type ClipboardSource = Box<dyn FnMut(&ClipboardRequest) -> Option<Vec<u8>> + Send>;

#[derive(Default)]
struct EffectState {
    responder: Responder,
    clipboard_source: Option<ClipboardSource>,
    pty: Vec<u8>,
    effects: Vec<Effect>,
    /// Ответ XTVERSION должен жить до возврата из колбэка.
    xtversion: Vec<u8>,
}

/// Состояние эффектов по userdata. Только внутри колбэка, вызванного синхронно из записи.
///
/// # Safety
/// `userdata` — указатель `EffectState` терминала, который сейчас пишет; другой ссылки на состояние
/// во время колбэка нет.
unsafe fn state<'a>(userdata: *mut c_void) -> Option<&'a mut EffectState> {
    // SAFETY: условие вызывающего.
    unsafe { userdata.cast::<EffectState>().as_mut() }
}

unsafe extern "C" fn on_write_pty(
    _terminal: sys::GhosttyTerminal,
    userdata: *mut c_void,
    data: *const u8,
    len: usize,
) {
    // SAFETY: колбэк вызван синхронно из записи терминала, userdata — его состояние; данные
    // одолжены на время вызова и копируются.
    unsafe {
        if let Some(state) = state(userdata)
            && !data.is_null()
        {
            state
                .pty
                .extend_from_slice(std::slice::from_raw_parts(data, len));
        }
    }
}

unsafe extern "C" fn on_bell(_terminal: sys::GhosttyTerminal, userdata: *mut c_void) {
    // SAFETY: см. on_write_pty.
    if let Some(state) = unsafe { state(userdata) } {
        state.effects.push(Effect::Bell);
    }
}

unsafe extern "C" fn on_title_changed(terminal: sys::GhosttyTerminal, userdata: *mut c_void) {
    let mut value = sys::GhosttyString {
        ptr: ptr::null(),
        len: 0,
    };
    // SAFETY: см. on_write_pty. `get` не пишет в терминал, поэтому не нарушает запрет повторного
    // входа в запись; строка одолжена до следующего изменения терминала и копируется сразу.
    unsafe {
        let status = sys::ghostty_terminal_get(
            terminal,
            sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_TITLE,
            ptr::from_mut(&mut value).cast(),
        );
        let title = if status == sys::GhosttyResult_GHOSTTY_SUCCESS {
            string_bytes(value)
        } else {
            Vec::new()
        };
        if let Some(state) = state(userdata) {
            state.effects.push(Effect::TitleChanged { title });
        }
    }
}

unsafe extern "C" fn on_xtversion(
    _terminal: sys::GhosttyTerminal,
    userdata: *mut c_void,
) -> sys::GhosttyString {
    let empty = sys::GhosttyString {
        ptr: ptr::null(),
        len: 0,
    };
    // SAFETY: см. on_write_pty. Буфер ответа живёт в состоянии до следующего колбэка.
    let Some(state) = (unsafe { state(userdata) }) else {
        return empty;
    };
    match &state.responder.xtversion {
        Some(version) => {
            state.xtversion = version.as_bytes().to_vec();
            sys::GhosttyString {
                ptr: state.xtversion.as_ptr(),
                len: state.xtversion.len(),
            }
        }
        None => empty,
    }
}

unsafe extern "C" fn on_size(
    _terminal: sys::GhosttyTerminal,
    userdata: *mut c_void,
    out: *mut sys::GhosttySizeReportSize,
) -> bool {
    // SAFETY: см. on_write_pty; out — выходной параметр на время вызова.
    let (Some(state), Some(out)) = (unsafe { state(userdata) }, unsafe { out.as_mut() }) else {
        return false;
    };
    let Some(size) = state.responder.size else {
        return false;
    };
    *out = sys::GhosttySizeReportSize {
        rows: size.rows,
        columns: size.columns,
        cell_width: size.cell_width,
        cell_height: size.cell_height,
    };
    true
}

unsafe extern "C" fn on_color_scheme(
    _terminal: sys::GhosttyTerminal,
    userdata: *mut c_void,
    out: *mut sys::GhosttyColorScheme,
) -> bool {
    // SAFETY: см. on_size.
    let (Some(state), Some(out)) = (unsafe { state(userdata) }, unsafe { out.as_mut() }) else {
        return false;
    };
    match state.responder.color_scheme {
        Some(ColorScheme::Light) => *out = sys::GhosttyColorScheme_GHOSTTY_COLOR_SCHEME_LIGHT,
        Some(ColorScheme::Dark) => *out = sys::GhosttyColorScheme_GHOSTTY_COLOR_SCHEME_DARK,
        None => return false,
    }
    true
}

unsafe extern "C" fn on_device_attributes(
    _terminal: sys::GhosttyTerminal,
    userdata: *mut c_void,
    out: *mut sys::GhosttyDeviceAttributes,
) -> bool {
    // SAFETY: см. on_size.
    let (Some(state), Some(out)) = (unsafe { state(userdata) }, unsafe { out.as_mut() }) else {
        return false;
    };
    let Some(attrs) = &state.responder.device_attributes else {
        return false;
    };
    let mut filled: sys::GhosttyDeviceAttributes = zeroed();
    filled.primary.conformance_level = attrs.conformance_level;
    let count = attrs.features.len().min(filled.primary.features.len());
    filled.primary.features[..count].copy_from_slice(&attrs.features[..count]);
    filled.primary.num_features = count;
    filled.secondary.device_type = attrs.device_type;
    filled.secondary.firmware_version = attrs.firmware_version;
    filled.tertiary.unit_id = attrs.unit_id;
    *out = filled;
    true
}

/// Поле структуры с размером помещается в объявленный ею `size`.
fn has_field(size: usize, offset: usize, len: usize) -> bool {
    offset.checked_add(len).is_some_and(|end| end <= size)
}

/// # Safety
/// `list` указывает на `len` строк `GhosttyString`, каждая на свои байты, или равен NULL при `len == 0`.
unsafe fn strings(list: *const sys::GhosttyString, len: usize) -> Vec<String> {
    if list.is_null() {
        return Vec::new();
    }
    // SAFETY: условие вызывающего.
    unsafe { std::slice::from_raw_parts(list, len) }
        .iter()
        // SAFETY: условие вызывающего для каждой строки.
        .map(|value| String::from_utf8_lossy(&unsafe { string_bytes(*value) }).into_owned())
        .collect()
}

// Ответ `reply` уходит через `WRITE_PTY` синхронно, и `on_write_pty` берёт своё `&mut` состояния.
// Поэтому колбэки буфера обмена копируют всё нужное, отпускают состояние до `reply` и берут его
// заново после.

unsafe extern "C" fn on_clipboard_write(
    _terminal: sys::GhosttyTerminal,
    userdata: *mut c_void,
    write: *const sys::GhosttyClipboardWrite,
) {
    // SAFETY: см. on_write_pty; запрос одолжен на время вызова, поля читаются только в пределах его
    // size, содержимое копируется. Ссылка на состояние не живёт во время reply.
    unsafe {
        let Some(request) = write.as_ref() else {
            return;
        };
        let reply_offset = mem::offset_of!(sys::GhosttyClipboardWrite, reply);
        if !has_field(
            request.size,
            reply_offset,
            mem::size_of::<sys::GhosttyClipboardWriteReplyFn>(),
        ) {
            return;
        }
        let contents = if request.contents.is_null() {
            Vec::new()
        } else {
            std::slice::from_raw_parts(request.contents, request.contents_len)
                .iter()
                .map(|item| {
                    (
                        String::from_utf8_lossy(&string_bytes(item.mime)).into_owned(),
                        string_bytes(item.data),
                    )
                })
                .collect()
        };
        match state(userdata) {
            Some(state) => state.effects.push(Effect::ClipboardWrite {
                location: request.location,
                contents,
            }),
            None => return,
        }
        if let Some(reply) = request.reply {
            let mut answer: sys::GhosttyClipboardWriteReply = sized();
            answer.result = sys::GhosttyClipboardWriteResult_GHOSTTY_CLIPBOARD_WRITE_RESULT_SUCCESS;
            reply(write, &answer);
        }
    }
}

unsafe extern "C" fn on_clipboard_read(
    _terminal: sys::GhosttyTerminal,
    userdata: *mut c_void,
    read: *const sys::GhosttyClipboardRead,
) {
    // SAFETY: см. on_clipboard_write. Ответ отдаётся до возврата из колбэка, как требует C API:
    // буферы ответа — локальная копия, они живут до конца вызова reply.
    unsafe {
        let Some(request) = read.as_ref() else {
            return;
        };
        let reply_offset = mem::offset_of!(sys::GhosttyClipboardRead, reply);
        if !has_field(
            request.size,
            reply_offset,
            mem::size_of::<sys::GhosttyClipboardReadReplyFn>(),
        ) {
            return;
        }
        let mimes = strings(request.mimes, request.mimes_len);
        let Some(state_now) = state(userdata) else {
            return;
        };
        let clipboard = match state_now.clipboard_source.as_mut() {
            Some(source) => source(&ClipboardRequest {
                location: request.location,
                mimes: mimes.clone(),
            }),
            None => state_now.responder.clipboard.clone(),
        };
        let answered = match (clipboard, request.reply) {
            (Some(data), Some(reply)) => {
                let mime = b"text/plain";
                let content = sys::GhosttyClipboardContent {
                    mime: sys::GhosttyString {
                        ptr: mime.as_ptr(),
                        len: mime.len(),
                    },
                    data: sys::GhosttyString {
                        ptr: data.as_ptr(),
                        len: data.len(),
                    },
                };
                let mut answer: sys::GhosttyClipboardReadReply = sized();
                answer.result =
                    sys::GhosttyClipboardReadResult_GHOSTTY_CLIPBOARD_READ_RESULT_SUCCESS;
                answer.contents = &content;
                answer.contents_len = 1;
                reply(read, &answer);
                true
            }
            _ => false,
        };
        if let Some(state) = state(userdata) {
            state.effects.push(Effect::ClipboardRead {
                location: request.location,
                mimes,
                answered,
            });
        }
    }
}

// --- терминал ---

/// Формат вывода formatter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Plain,
    Vt,
    Html,
}

/// Параметры formatter терминала. `extras` включает в VT-вывод всё состояние, которое formatter
/// умеет выдать: палитру, режимы, поля прокрутки, табуляторы, pwd, режимы клавиатуры, курсор,
/// стиль, гиперссылку, защиту, Kitty и наборы символов.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FormatOptions {
    pub format: Format,
    pub unwrap: bool,
    pub trim: bool,
    pub extras: bool,
}

impl FormatOptions {
    pub const PLAIN: FormatOptions = FormatOptions {
        format: Format::Plain,
        unwrap: false,
        trim: true,
        extras: false,
    };
    pub const VT_FULL: FormatOptions = FormatOptions {
        format: Format::Vt,
        unwrap: false,
        trim: false,
        extras: true,
    };
}

/// Терминал `libghostty-vt` со своим состоянием эффектов.
pub struct Terminal {
    raw: NonNull<sys::GhosttyTerminalImpl>,
    effects: NonNull<EffectState>,
}

// SAFETY: у терминала C API нет привязки к потоку; доступ сериализуется через &mut и владение,
// Terminal не Sync.
unsafe impl Send for Terminal {}

impl Terminal {
    pub fn new(cols: u16, rows: u16) -> Result<Terminal> {
        let mut raw: sys::GhosttyTerminal = ptr::null_mut();
        // SAFETY: аллокатор по умолчанию, выходной указатель на стеке.
        check("ghostty_terminal_new", unsafe {
            sys::ghostty_terminal_new(ptr::null(), &mut raw, cols, rows)
        })?;
        Terminal::adopt(raw, "ghostty_terminal_new")
    }

    /// Терминал из полного снимка `ghostty_snapshot_encode`. При `max_continuation > 0`
    /// незавершённая последовательность снимка остаётся доступной через [`Terminal::continuation`]
    /// и отслеживается дальше с этим пределом. Эффекты нового терминала выключены.
    pub fn from_snapshot(bytes: &[u8], max_continuation: usize) -> Result<Terminal> {
        let mut decoder: sys::GhosttySnapshotDecoder = ptr::null_mut();
        // SAFETY: байты одолжены на время жизни декодера, он освобождается ниже.
        check("ghostty_snapshot_decoder_new_buf", unsafe {
            sys::ghostty_snapshot_decoder_new_buf(
                ptr::null(),
                &mut decoder,
                bytes.as_ptr(),
                bytes.len(),
            )
        })?;
        let mut raw: sys::GhosttyTerminal = ptr::null_mut();
        let retain = max_continuation > 0;
        // SAFETY: декодер жив, значения опций — указатели на значения документированных типов.
        let decoded = unsafe {
            check(
                "ghostty_snapshot_decoder_set",
                sys::ghostty_snapshot_decoder_set(
                    decoder,
                    sys::GhosttySnapshotDecoderOption_GHOSTTY_SNAPSHOT_DECODER_OPT_MAX_CONTINUATION_BYTES,
                    ptr::addr_of!(max_continuation).cast(),
                ),
            )
            .and_then(|()| {
                check(
                    "ghostty_snapshot_decoder_set",
                    sys::ghostty_snapshot_decoder_set(
                        decoder,
                        sys::GhosttySnapshotDecoderOption_GHOSTTY_SNAPSHOT_DECODER_OPT_RETAIN_CONTINUATION,
                        ptr::addr_of!(retain).cast(),
                    ),
                )
            })
            .and_then(|()| {
                check(
                    "ghostty_snapshot_decoder_decode",
                    sys::ghostty_snapshot_decoder_decode(decoder, &mut raw),
                )
            })
        };
        // SAFETY: декодер больше не используется; терминал после decode принадлежит вызывающему.
        unsafe { sys::ghostty_snapshot_decoder_free(decoder) };
        decoded?;
        Terminal::adopt(raw, "ghostty_snapshot_decoder_decode")
    }

    fn adopt(raw: sys::GhosttyTerminal, call: &'static str) -> Result<Terminal> {
        let Some(raw) = NonNull::new(raw) else {
            return Err(Error {
                call,
                code: sys::GhosttyResult_GHOSTTY_INVALID_VALUE,
            });
        };
        let effects = NonNull::from(Box::leak(Box::<EffectState>::default()));
        let terminal = Terminal { raw, effects };
        // SAFETY: userdata — указатель состояния, которое живёт столько же, сколько терминал.
        check("ghostty_terminal_set(USERDATA)", unsafe {
            sys::ghostty_terminal_set(
                terminal.raw(),
                sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_USERDATA,
                terminal.effects.as_ptr().cast_const().cast(),
            )
        })?;
        Ok(terminal)
    }

    fn raw(&self) -> sys::GhosttyTerminal {
        self.raw.as_ptr()
    }

    fn effect_state(&mut self) -> &mut EffectState {
        // SAFETY: состояние живёт столько же, сколько терминал; колбэки обращаются к нему только во
        // время записи, а она требует &mut self, поэтому здесь других ссылок нет.
        unsafe { self.effects.as_mut() }
    }

    /// Подаёт байты в разборщик. Колбэки эффектов вызываются синхронно внутри.
    pub fn write(&mut self, bytes: &[u8]) {
        // SAFETY: терминал жив, байты одолжены на время вызова.
        unsafe { sys::ghostty_terminal_vt_write(self.raw(), bytes.as_ptr(), bytes.len()) };
    }

    pub fn resize(
        &mut self,
        cols: u16,
        rows: u16,
        cell_width: u32,
        cell_height: u32,
    ) -> Result<()> {
        // SAFETY: терминал жив.
        check("ghostty_terminal_resize", unsafe {
            sys::ghostty_terminal_resize(self.raw(), cols, rows, cell_width, cell_height)
        })
    }

    pub fn reset(&mut self) {
        // SAFETY: терминал жив.
        unsafe { sys::ghostty_terminal_reset(self.raw()) };
    }

    fn set_value<T>(
        &mut self,
        option: sys::GhosttyTerminalOption,
        name: &'static str,
        value: &T,
    ) -> Result<()> {
        // SAFETY: значение — указатель на значение типа, который документирует опция; вызывающий
        // выбирает T по документации опции.
        check(name, unsafe {
            sys::ghostty_terminal_set(self.raw(), option, ptr::from_ref(value).cast())
        })
    }

    /// Предел continuation; задаётся до первого байта, иначе начатая последовательность недоступна.
    pub fn set_continuation_max_bytes(&mut self, max: usize) -> Result<()> {
        self.set_value(
            sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_CONTINUATION_MAX_BYTES,
            "ghostty_terminal_set(CONTINUATION_MAX_BYTES)",
            &max,
        )
    }

    pub fn set_scrollback_max_lines(&mut self, lines: usize) -> Result<()> {
        self.set_value(
            sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_LINES,
            "ghostty_terminal_set(SCROLLBACK_MAX_LINES)",
            &lines,
        )
    }

    /// Регистрирует колбэки эффектов: ответы в PTY, звонок, заголовок, XTVERSION, размер, цветовая
    /// схема, DA и буфер обмена.
    ///
    /// Вопреки `terminal.h` запрос DA при установленном `WRITE_PTY` не глушится ни колбэком DA,
    /// вернувшим `false`, ни его отсутствием: C API всегда ставит обработчику свой трамплин, а тот
    /// без ответа колбэка отдаёт атрибуты по умолчанию (`deviceAttributesTrampoline` в
    /// `src/terminal/c/terminal.zig`). Поэтому `device_attributes: None` означает ответ по умолчанию.
    pub fn enable_effects(&mut self, responder: Responder) -> Result<()> {
        self.effect_state().responder = responder;
        let write_pty: sys::GhosttyTerminalWritePtyFn = Some(on_write_pty);
        let bell: sys::GhosttyTerminalBellFn = Some(on_bell);
        let title: sys::GhosttyTerminalTitleChangedFn = Some(on_title_changed);
        let xtversion: sys::GhosttyTerminalXtversionFn = Some(on_xtversion);
        let size: sys::GhosttyTerminalSizeFn = Some(on_size);
        let scheme: sys::GhosttyTerminalColorSchemeFn = Some(on_color_scheme);
        let attrs: sys::GhosttyTerminalDeviceAttributesFn = Some(on_device_attributes);
        let clip_write: sys::GhosttyTerminalClipboardWriteFn = Some(on_clipboard_write);
        let clip_read: sys::GhosttyTerminalClipboardReadFn = Some(on_clipboard_read);
        let callbacks: [(sys::GhosttyTerminalOption, &'static str, *const c_void); 9] = [
            (
                sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_WRITE_PTY,
                "ghostty_terminal_set(WRITE_PTY)",
                write_pty.map_or(ptr::null(), |f| f as *const c_void),
            ),
            (
                sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_BELL,
                "ghostty_terminal_set(BELL)",
                bell.map_or(ptr::null(), |f| f as *const c_void),
            ),
            (
                sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_TITLE_CHANGED,
                "ghostty_terminal_set(TITLE_CHANGED)",
                title.map_or(ptr::null(), |f| f as *const c_void),
            ),
            (
                sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_XTVERSION,
                "ghostty_terminal_set(XTVERSION)",
                xtversion.map_or(ptr::null(), |f| f as *const c_void),
            ),
            (
                sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_SIZE,
                "ghostty_terminal_set(SIZE)",
                size.map_or(ptr::null(), |f| f as *const c_void),
            ),
            (
                sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_COLOR_SCHEME,
                "ghostty_terminal_set(COLOR_SCHEME)",
                scheme.map_or(ptr::null(), |f| f as *const c_void),
            ),
            (
                sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_DEVICE_ATTRIBUTES,
                "ghostty_terminal_set(DEVICE_ATTRIBUTES)",
                attrs.map_or(ptr::null(), |f| f as *const c_void),
            ),
            (
                sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_CLIPBOARD_WRITE,
                "ghostty_terminal_set(CLIPBOARD_WRITE)",
                clip_write.map_or(ptr::null(), |f| f as *const c_void),
            ),
            (
                sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_CLIPBOARD_READ,
                "ghostty_terminal_set(CLIPBOARD_READ)",
                clip_read.map_or(ptr::null(), |f| f as *const c_void),
            ),
        ];
        for (option, name, value) in callbacks {
            self.set_callback(option, name, value)?;
        }
        Ok(())
    }

    /// Меняет ответы колбэков, например размер после ресайза; колбэки остаются прежними.
    pub fn set_responder(&mut self, responder: Responder) {
        self.effect_state().responder = responder;
    }

    /// Источник ответа на чтение буфера обмена; `None` возвращает ответ из [`Responder`].
    pub fn set_clipboard_source(&mut self, source: Option<ClipboardSource>) {
        self.effect_state().clipboard_source = source;
    }

    /// Снимает колбэк чтения буфера обмена: так видно, что терминал делает с OSC 52 чтения без него.
    pub fn unset_clipboard_read(&mut self) -> Result<()> {
        self.set_callback(
            sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_CLIPBOARD_READ,
            "ghostty_terminal_set(CLIPBOARD_READ)",
            ptr::null(),
        )
    }

    /// Цвета по умолчанию: основной, фон и курсор. `None` снимает значение. От них зависят ответы на
    /// OSC 10, 11 и 12 и текущие цвета, которые читает [`Terminal::colors`].
    pub fn set_default_colors(
        &mut self,
        foreground: Option<Rgb>,
        background: Option<Rgb>,
        cursor: Option<Rgb>,
    ) -> Result<()> {
        let options = [
            (
                sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_COLOR_FOREGROUND,
                "ghostty_terminal_set(COLOR_FOREGROUND)",
                foreground,
            ),
            (
                sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_COLOR_BACKGROUND,
                "ghostty_terminal_set(COLOR_BACKGROUND)",
                background,
            ),
            (
                sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_COLOR_CURSOR,
                "ghostty_terminal_set(COLOR_CURSOR)",
                cursor,
            ),
        ];
        for (option, name, color) in options {
            let value = color.map(Rgb::to_sys);
            let ptr: *const c_void = value
                .as_ref()
                .map_or(ptr::null(), |value| ptr::from_ref(value).cast());
            // SAFETY: значение опции цвета — указатель на GhosttyColorRgb или NULL; терминал копирует
            // цвет до возврата.
            check(name, unsafe {
                sys::ghostty_terminal_set(self.raw(), option, ptr)
            })?;
        }
        Ok(())
    }

    /// Колбэк передаётся самим значением; NULL снимает его.
    fn set_callback(
        &mut self,
        option: sys::GhosttyTerminalOption,
        name: &'static str,
        value: *const c_void,
    ) -> Result<()> {
        // SAFETY: значение — NULL или функция с сигнатурой типа привязок для этой опции, которую
        // проверяют места вызова.
        check(name, unsafe {
            sys::ghostty_terminal_set(self.raw(), option, value)
        })
    }

    /// Байты, которые терминал записал бы в PTY (ответы на запросы), с момента прошлого вызова.
    pub fn take_pty_output(&mut self) -> Vec<u8> {
        mem::take(&mut self.effect_state().pty)
    }

    /// Эффекты с момента прошлого вызова.
    pub fn take_effects(&mut self) -> Vec<Effect> {
        mem::take(&mut self.effect_state().effects)
    }

    /// Незавершённая последовательность на конце потока; пусто на ground.
    pub fn continuation(&self) -> Result<Vec<u8>> {
        let mut out: *mut u8 = ptr::null_mut();
        let mut len = 0usize;
        // SAFETY: терминал жив, выходные параметры на стеке; результат освобождает take_alloc.
        unsafe {
            check(
                "ghostty_terminal_continuation_alloc",
                sys::ghostty_terminal_continuation_alloc(
                    self.raw(),
                    ptr::null(),
                    &mut out,
                    &mut len,
                ),
            )?;
            Ok(take_alloc(out, len))
        }
    }

    /// Полный снимок состояния в формате `GHOSTSNP`.
    pub fn snapshot(&self) -> Result<Vec<u8>> {
        let mut out: *mut u8 = ptr::null_mut();
        let mut len = 0usize;
        // SAFETY: см. continuation.
        unsafe {
            check(
                "ghostty_snapshot_encode_alloc",
                sys::ghostty_snapshot_encode_alloc(self.raw(), ptr::null(), &mut out, &mut len),
            )?;
            Ok(take_alloc(out, len))
        }
    }

    /// Активный экран через formatter.
    pub fn format(&self, options: FormatOptions) -> Result<Vec<u8>> {
        // SAFETY: структуры параметров с size заполнены нулями, как GHOSTTY_INIT_SIZED.
        let mut opts: sys::GhosttyFormatterTerminalOptions = unsafe { sized() };
        // SAFETY: то же для вложенных структур.
        opts.extra = unsafe { sized() };
        // SAFETY: то же.
        opts.extra.screen = unsafe { sized() };
        opts.emit = match options.format {
            Format::Plain => sys::GhosttyFormatterFormat_GHOSTTY_FORMATTER_FORMAT_PLAIN,
            Format::Vt => sys::GhosttyFormatterFormat_GHOSTTY_FORMATTER_FORMAT_VT,
            Format::Html => sys::GhosttyFormatterFormat_GHOSTTY_FORMATTER_FORMAT_HTML,
        };
        opts.unwrap = options.unwrap;
        opts.trim = options.trim;
        if options.extras {
            let extra = &mut opts.extra;
            extra.palette = true;
            extra.modes = true;
            extra.scrolling_region = true;
            extra.tabstops = true;
            extra.pwd = true;
            extra.keyboard = true;
            let screen = &mut extra.screen;
            screen.cursor = true;
            screen.style = true;
            screen.hyperlink = true;
            screen.protection = true;
            screen.kitty_keyboard = true;
            screen.charsets = true;
        }
        let mut formatter: sys::GhosttyFormatter = ptr::null_mut();
        // SAFETY: терминал переживает formatter, который освобождается в этом же вызове.
        unsafe {
            check(
                "ghostty_formatter_terminal_new",
                sys::ghostty_formatter_terminal_new(ptr::null(), &mut formatter, self.raw(), opts),
            )?;
            let mut out: *mut u8 = ptr::null_mut();
            let mut len = 0usize;
            let result = check(
                "ghostty_formatter_format_alloc",
                sys::ghostty_formatter_format_alloc(formatter, ptr::null(), &mut out, &mut len),
            )
            .map(|()| take_alloc(out, len));
            sys::ghostty_formatter_free(formatter);
            result
        }
    }

    /// Типизированный `get`.
    ///
    /// # Safety
    /// `T` — выходной тип, который документирует `data`.
    unsafe fn get<T>(
        &self,
        data: sys::GhosttyTerminalData,
        name: &'static str,
        out: &mut T,
    ) -> Result<()> {
        // SAFETY: условие вызывающего.
        check(name, unsafe {
            sys::ghostty_terminal_get(self.raw(), data, ptr::from_mut(out).cast())
        })
    }

    fn get_u16(&self, data: sys::GhosttyTerminalData, name: &'static str) -> Result<u16> {
        let mut value = 0u16;
        // SAFETY: вызывающие передают данные с выходным типом uint16_t.
        unsafe { self.get(data, name, &mut value) }?;
        Ok(value)
    }

    fn get_bool(&self, data: sys::GhosttyTerminalData, name: &'static str) -> Result<bool> {
        let mut value = false;
        // SAFETY: вызывающие передают данные с выходным типом bool.
        unsafe { self.get(data, name, &mut value) }?;
        Ok(value)
    }

    fn get_usize(&self, data: sys::GhosttyTerminalData, name: &'static str) -> Result<usize> {
        let mut value = 0usize;
        // SAFETY: вызывающие передают данные с выходным типом size_t.
        unsafe { self.get(data, name, &mut value) }?;
        Ok(value)
    }

    fn get_string(&self, data: sys::GhosttyTerminalData, name: &'static str) -> Result<Vec<u8>> {
        let mut value = sys::GhosttyString {
            ptr: ptr::null(),
            len: 0,
        };
        // SAFETY: вызывающие передают данные с выходным типом GhosttyString; строка одолжена до
        // следующего изменяющего вызова и копируется сразу.
        unsafe {
            self.get(data, name, &mut value)?;
            Ok(string_bytes(value))
        }
    }

    pub fn cols(&self) -> Result<u16> {
        self.get_u16(
            sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_COLS,
            "get(COLS)",
        )
    }

    pub fn rows(&self) -> Result<u16> {
        self.get_u16(
            sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_ROWS,
            "get(ROWS)",
        )
    }

    pub fn cursor(&self) -> Result<Cursor> {
        // SAFETY: GhosttyStyle — структура с size, нули допустимы.
        let mut style: sys::GhosttyStyle = unsafe { sized() };
        // SAFETY: выходной тип CURSOR_STYLE — GhosttyStyle.
        unsafe {
            self.get(
                sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_CURSOR_STYLE,
                "get(CURSOR_STYLE)",
                &mut style,
            )
        }?;
        Ok(Cursor {
            x: self.get_u16(
                sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_CURSOR_X,
                "get(CURSOR_X)",
            )?,
            y: self.get_u16(
                sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_CURSOR_Y,
                "get(CURSOR_Y)",
            )?,
            pending_wrap: self.get_bool(
                sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_CURSOR_PENDING_WRAP,
                "get(CURSOR_PENDING_WRAP)",
            )?,
            visible: self.get_bool(
                sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_CURSOR_VISIBLE,
                "get(CURSOR_VISIBLE)",
            )?,
            style: Style::from_sys(&style),
        })
    }

    pub fn active_screen(&self) -> Result<Screen> {
        let mut screen: sys::GhosttyTerminalScreen = 0;
        // SAFETY: выходной тип ACTIVE_SCREEN — GhosttyTerminalScreen.
        unsafe {
            self.get(
                sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN,
                "get(ACTIVE_SCREEN)",
                &mut screen,
            )
        }?;
        Ok(
            if screen == sys::GhosttyTerminalScreen_GHOSTTY_TERMINAL_SCREEN_ALTERNATE {
                Screen::Alternate
            } else {
                Screen::Primary
            },
        )
    }

    pub fn mode(&self, mode: Mode) -> Result<bool> {
        let mut config = sys::GhosttyTerminalModeConfig {
            mode: mode.0,
            value: false,
        };
        // SAFETY: выходной тип MODE — GhosttyTerminalModeConfig с заполненным mode.
        unsafe {
            self.get(
                sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_MODE,
                "get(MODE)",
                &mut config,
            )
        }?;
        Ok(config.value)
    }

    /// Текущие флаги клавиатуры Kitty (верх стека).
    pub fn kitty_keyboard_flags(&self) -> Result<u8> {
        let mut flags: sys::GhosttyKittyKeyFlags = 0;
        // SAFETY: выходной тип KITTY_KEYBOARD_FLAGS — GhosttyKittyKeyFlags.
        unsafe {
            self.get(
                sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_KITTY_KEYBOARD_FLAGS,
                "get(KITTY_KEYBOARD_FLAGS)",
                &mut flags,
            )
        }?;
        Ok(flags)
    }

    /// Текущие цвета и палитра.
    pub fn colors(&self) -> Result<Colors> {
        let dynamic = |data, name| -> Result<Option<Rgb>> {
            let mut value = sys::GhosttyColorRgb { r: 0, g: 0, b: 0 };
            // SAFETY: выходной тип COLOR_FOREGROUND, _BACKGROUND и _CURSOR — GhosttyColorRgb.
            match unsafe { self.get(data, name, &mut value) } {
                Ok(()) => Ok(Some(Rgb::from_sys(value))),
                Err(err) if err.code == sys::GhosttyResult_GHOSTTY_NO_VALUE => Ok(None),
                Err(err) => Err(err),
            }
        };
        let mut palette = [sys::GhosttyColorRgb { r: 0, g: 0, b: 0 }; 256];
        // SAFETY: выходной тип COLOR_PALETTE — GhosttyColorRgb[256].
        unsafe {
            self.get(
                sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_COLOR_PALETTE,
                "get(COLOR_PALETTE)",
                &mut palette,
            )
        }?;
        Ok(Colors {
            foreground: dynamic(
                sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_COLOR_FOREGROUND,
                "get(COLOR_FOREGROUND)",
            )?,
            background: dynamic(
                sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_COLOR_BACKGROUND,
                "get(COLOR_BACKGROUND)",
            )?,
            cursor: dynamic(
                sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_COLOR_CURSOR,
                "get(COLOR_CURSOR)",
            )?,
            palette: palette.into_iter().map(Rgb::from_sys).collect(),
        })
    }

    pub fn title(&self) -> Result<Vec<u8>> {
        self.get_string(
            sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_TITLE,
            "get(TITLE)",
        )
    }

    pub fn pwd(&self) -> Result<Vec<u8>> {
        self.get_string(
            sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_PWD,
            "get(PWD)",
        )
    }

    pub fn total_rows(&self) -> Result<usize> {
        self.get_usize(
            sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_TOTAL_ROWS,
            "get(TOTAL_ROWS)",
        )
    }

    pub fn scrollback_rows(&self) -> Result<usize> {
        self.get_usize(
            sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_SCROLLBACK_ROWS,
            "get(SCROLLBACK_ROWS)",
        )
    }

    pub fn continuation_max_bytes(&self) -> Result<usize> {
        self.get_usize(
            sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_CONTINUATION_MAX_BYTES,
            "get(CONTINUATION_MAX_BYTES)",
        )
    }

    /// Разборщик на ground: ни одной начатой последовательности.
    pub fn vt_ground(&self) -> Result<bool> {
        self.get_bool(
            sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_VT_GROUND,
            "get(VT_GROUND)",
        )
    }

    pub fn mouse_tracking(&self) -> Result<bool> {
        self.get_bool(
            sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_MOUSE_TRACKING,
            "get(MOUSE_TRACKING)",
        )
    }

    /// Ошибка разбора, которая могла помешать изменению состояния; не сбрасывается.
    pub fn processing_error(&self) -> Result<bool> {
        self.get_bool(
            sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_VT_PROCESSING_ERROR,
            "get(VT_PROCESSING_ERROR)",
        )
    }

    /// Клетка в точке `(x, y)` системы координат `area`.
    pub fn cell(&self, area: Area, x: u16, y: u32) -> Result<Cell> {
        let point = sys::GhosttyPoint {
            tag: area.tag(),
            value: sys::GhosttyPointValue {
                coordinate: sys::GhosttyPointCoordinate { x, y },
            },
        };
        // SAFETY: GhosttyGridRef — структура с size, нули допустимы.
        let mut grid: sys::GhosttyGridRef = unsafe { sized() };
        // SAFETY: терминал жив; ссылка годна до следующего изменяющего вызова, а &self его не допускает.
        unsafe {
            check(
                "ghostty_terminal_grid_ref",
                sys::ghostty_terminal_grid_ref(self.raw(), point, &mut grid),
            )?;
            let mut cell: sys::GhosttyCell = 0;
            check(
                "ghostty_grid_ref_cell",
                sys::ghostty_grid_ref_cell(&grid, &mut cell),
            )?;
            let mut row: sys::GhosttyRow = 0;
            check(
                "ghostty_grid_ref_row",
                sys::ghostty_grid_ref_row(&grid, &mut row),
            )?;
            let mut style: sys::GhosttyStyle = sized();
            check(
                "ghostty_grid_ref_style",
                sys::ghostty_grid_ref_style(&grid, &mut style),
            )?;

            let mut wide: sys::GhosttyCellWide = 0;
            check(
                "ghostty_cell_get(WIDE)",
                sys::ghostty_cell_get(
                    cell,
                    sys::GhosttyCellData_GHOSTTY_CELL_DATA_WIDE,
                    ptr::from_mut(&mut wide).cast(),
                ),
            )?;
            let mut protected = false;
            check(
                "ghostty_cell_get(PROTECTED)",
                sys::ghostty_cell_get(
                    cell,
                    sys::GhosttyCellData_GHOSTTY_CELL_DATA_PROTECTED,
                    ptr::from_mut(&mut protected).cast(),
                ),
            )?;
            let mut has_link = false;
            check(
                "ghostty_cell_get(HAS_HYPERLINK)",
                sys::ghostty_cell_get(
                    cell,
                    sys::GhosttyCellData_GHOSTTY_CELL_DATA_HAS_HYPERLINK,
                    ptr::from_mut(&mut has_link).cast(),
                ),
            )?;
            let mut row_wrap = false;
            check(
                "ghostty_row_get(WRAP)",
                sys::ghostty_row_get(
                    row,
                    sys::GhosttyRowData_GHOSTTY_ROW_DATA_WRAP,
                    ptr::from_mut(&mut row_wrap).cast(),
                ),
            )?;
            let mut row_wrap_continuation = false;
            check(
                "ghostty_row_get(WRAP_CONTINUATION)",
                sys::ghostty_row_get(
                    row,
                    sys::GhosttyRowData_GHOSTTY_ROW_DATA_WRAP_CONTINUATION,
                    ptr::from_mut(&mut row_wrap_continuation).cast(),
                ),
            )?;

            let text = sized_read("ghostty_grid_ref_graphemes", |buf: *mut u32, cap, len| {
                sys::ghostty_grid_ref_graphemes(&grid, buf, cap, len)
            })?;
            let hyperlink = if has_link {
                let uri = sized_read(
                    "ghostty_grid_ref_hyperlink_uri",
                    |buf: *mut u8, cap, len| {
                        sys::ghostty_grid_ref_hyperlink_uri(&grid, buf, cap, len)
                    },
                )?;
                Some(String::from_utf8_lossy(&uri).into_owned())
            } else {
                None
            };
            Ok(Cell {
                text,
                wide: match wide {
                    sys::GhosttyCellWide_GHOSTTY_CELL_WIDE_WIDE => Wide::Wide,
                    sys::GhosttyCellWide_GHOSTTY_CELL_WIDE_SPACER_TAIL => Wide::SpacerTail,
                    sys::GhosttyCellWide_GHOSTTY_CELL_WIDE_SPACER_HEAD => Wide::SpacerHead,
                    _ => Wide::Narrow,
                },
                style: Style::from_sys(&style),
                hyperlink,
                protected,
                row_wrap,
                row_wrap_continuation,
            })
        }
    }

    /// Текст строки активной области по клеткам, без хвостовых пустых клеток.
    pub fn row_text(&self, y: u16) -> Result<String> {
        let cols = self.cols()?;
        let mut text = String::new();
        let mut pending_blanks = 0usize;
        for x in 0..cols {
            let cell = self.cell(Area::Active, x, u32::from(y))?;
            if matches!(cell.wide, Wide::SpacerTail | Wide::SpacerHead) {
                continue;
            }
            if cell.text.is_empty() {
                pending_blanks += 1;
                continue;
            }
            text.extend(std::iter::repeat_n(' ', pending_blanks));
            pending_blanks = 0;
            text.push_str(&cell.text_string());
        }
        Ok(text)
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        // SAFETY: терминал освобождается один раз; после этого колбэки не вызываются, и состояние
        // эффектов освобождается следом.
        unsafe {
            sys::ghostty_terminal_free(self.raw());
            drop(Box::from_raw(self.effects.as_ptr()));
        }
    }
}

/// Чтение в буфер с запросом размера: `GHOSTTY_OUT_OF_SPACE` сообщает нужную длину.
///
/// # Safety
/// `read` пишет не больше `cap` элементов в `buf` и длину в `len`.
unsafe fn sized_read<T: Copy + Default>(
    call: &'static str,
    mut read: impl FnMut(*mut T, usize, *mut usize) -> sys::GhosttyResult,
) -> Result<Vec<T>> {
    let mut buf: Vec<T> = vec![T::default(); 16];
    loop {
        let mut len = 0usize;
        let code = read(buf.as_mut_ptr(), buf.len(), &mut len);
        if code == sys::GhosttyResult_GHOSTTY_OUT_OF_SPACE && len > buf.len() {
            buf.resize(len, T::default());
            continue;
        }
        check(call, code)?;
        buf.truncate(len);
        return Ok(buf);
    }
}

// --- ввод ---

/// Клавиша `GhosttyKey`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key(pub i32);

impl Key {
    pub const A: Key = Key(sys::GhosttyKey_GHOSTTY_KEY_A);
    pub const ENTER: Key = Key(sys::GhosttyKey_GHOSTTY_KEY_ENTER);
    pub const ARROW_UP: Key = Key(sys::GhosttyKey_GHOSTTY_KEY_ARROW_UP);
    pub const ARROW_DOWN: Key = Key(sys::GhosttyKey_GHOSTTY_KEY_ARROW_DOWN);
    pub const ESCAPE: Key = Key(sys::GhosttyKey_GHOSTTY_KEY_ESCAPE);
    pub const TAB: Key = Key(sys::GhosttyKey_GHOSTTY_KEY_TAB);
    pub const BACKSPACE: Key = Key(sys::GhosttyKey_GHOSTTY_KEY_BACKSPACE);
    pub const NUMPAD_1: Key = Key(sys::GhosttyKey_GHOSTTY_KEY_NUMPAD_1);
}

pub const MODS_SHIFT: u16 = sys::GHOSTTY_MODS_SHIFT as u16;
pub const MODS_CTRL: u16 = sys::GHOSTTY_MODS_CTRL as u16;
pub const MODS_ALT: u16 = sys::GHOSTTY_MODS_ALT as u16;
pub const KITTY_DISAMBIGUATE: u8 = sys::GHOSTTY_KITTY_KEY_DISAMBIGUATE as u8;
pub const KITTY_REPORT_EVENTS: u8 = sys::GHOSTTY_KITTY_KEY_REPORT_EVENTS as u8;
pub const KITTY_ALL: u8 = sys::GHOSTTY_KITTY_KEY_ALL as u8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyAction {
    Press,
    Release,
    Repeat,
}

/// Событие клавиши для кодировщика: клавиша, модификаторы, текст и кодовая точка без Shift.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyInput {
    pub key: Key,
    pub action: KeyAction,
    pub mods: u16,
    pub text: String,
    pub unshifted: Option<char>,
}

impl KeyInput {
    pub fn press(key: Key, text: &str) -> KeyInput {
        KeyInput {
            key,
            action: KeyAction::Press,
            mods: 0,
            text: text.to_owned(),
            unshifted: text.chars().next(),
        }
    }
}

/// Кодировщик клавиш с режимами, взятыми у терминала.
pub struct KeyEncoder {
    raw: NonNull<sys::GhosttyKeyEncoderImpl>,
}

impl KeyEncoder {
    pub fn new() -> Result<KeyEncoder> {
        let mut raw: sys::GhosttyKeyEncoder = ptr::null_mut();
        // SAFETY: аллокатор по умолчанию, выходной указатель на стеке.
        check("ghostty_key_encoder_new", unsafe {
            sys::ghostty_key_encoder_new(ptr::null(), &mut raw)
        })?;
        NonNull::new(raw)
            .map(|raw| KeyEncoder { raw })
            .ok_or(Error {
                call: "ghostty_key_encoder_new",
                code: sys::GhosttyResult_GHOSTTY_INVALID_VALUE,
            })
    }

    /// Режимы клавиатуры (DECCKM, keypad, Kitty и другие) из терминала.
    pub fn sync_from(&mut self, terminal: &Terminal) {
        // SAFETY: оба объекта живы.
        unsafe { sys::ghostty_key_encoder_setopt_from_terminal(self.raw.as_ptr(), terminal.raw()) };
    }

    pub fn encode(&mut self, input: &KeyInput) -> Result<Vec<u8>> {
        let mut event: sys::GhosttyKeyEvent = ptr::null_mut();
        // SAFETY: событие создаётся и освобождается в этом вызове; текст одолжен на время encode.
        unsafe {
            check(
                "ghostty_key_event_new",
                sys::ghostty_key_event_new(ptr::null(), &mut event),
            )?;
            let action = match input.action {
                KeyAction::Press => sys::GhosttyKeyAction_GHOSTTY_KEY_ACTION_PRESS,
                KeyAction::Release => sys::GhosttyKeyAction_GHOSTTY_KEY_ACTION_RELEASE,
                KeyAction::Repeat => sys::GhosttyKeyAction_GHOSTTY_KEY_ACTION_REPEAT,
            };
            sys::ghostty_key_event_set_action(event, action);
            sys::ghostty_key_event_set_key(event, input.key.0);
            sys::ghostty_key_event_set_mods(event, input.mods);
            sys::ghostty_key_event_set_consumed_mods(event, 0);
            sys::ghostty_key_event_set_utf8(event, input.text.as_ptr().cast(), input.text.len());
            sys::ghostty_key_event_set_unshifted_codepoint(
                event,
                input.unshifted.map_or(0, u32::from),
            );
            let encoder = self.raw.as_ptr();
            let result = sized_read("ghostty_key_encoder_encode", |buf: *mut u8, cap, len| {
                sys::ghostty_key_encoder_encode(encoder, event, buf.cast(), cap, len)
            });
            sys::ghostty_key_event_free(event);
            result
        }
    }
}

impl Drop for KeyEncoder {
    fn drop(&mut self) {
        // SAFETY: освобождается один раз.
        unsafe { sys::ghostty_key_encoder_free(self.raw.as_ptr()) };
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseAction {
    Press,
    Release,
    Motion,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

/// Событие мыши в пикселях поверхности.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MouseInput {
    pub action: MouseAction,
    pub button: Option<MouseButton>,
    pub mods: u16,
    pub x: f32,
    pub y: f32,
}

/// Геометрия для кодировщика мыши: размер экрана и клетки в пикселях.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MouseGeometry {
    pub screen_width: u32,
    pub screen_height: u32,
    pub cell_width: u32,
    pub cell_height: u32,
}

/// Кодировщик мыши с режимом и форматом, взятыми у терминала.
pub struct MouseEncoder {
    raw: NonNull<sys::GhosttyMouseEncoderImpl>,
}

impl MouseEncoder {
    pub fn new(geometry: MouseGeometry) -> Result<MouseEncoder> {
        let mut raw: sys::GhosttyMouseEncoder = ptr::null_mut();
        // SAFETY: аллокатор по умолчанию, выходной указатель на стеке.
        check("ghostty_mouse_encoder_new", unsafe {
            sys::ghostty_mouse_encoder_new(ptr::null(), &mut raw)
        })?;
        let raw = NonNull::new(raw).ok_or(Error {
            call: "ghostty_mouse_encoder_new",
            code: sys::GhosttyResult_GHOSTTY_INVALID_VALUE,
        })?;
        // SAFETY: GhosttyMouseEncoderSize — структура с size, нули допустимы.
        let mut size: sys::GhosttyMouseEncoderSize = unsafe { sized() };
        size.screen_width = geometry.screen_width;
        size.screen_height = geometry.screen_height;
        size.cell_width = geometry.cell_width;
        size.cell_height = geometry.cell_height;
        // SAFETY: значение SIZE — указатель на GhosttyMouseEncoderSize, кодировщик его копирует.
        unsafe {
            sys::ghostty_mouse_encoder_setopt(
                raw.as_ptr(),
                sys::GhosttyMouseEncoderOption_GHOSTTY_MOUSE_ENCODER_OPT_SIZE,
                ptr::from_ref(&size).cast(),
            )
        };
        Ok(MouseEncoder { raw })
    }

    /// Режим отслеживания и формат мыши из терминала.
    pub fn sync_from(&mut self, terminal: &Terminal) {
        // SAFETY: оба объекта живы.
        unsafe {
            sys::ghostty_mouse_encoder_setopt_from_terminal(self.raw.as_ptr(), terminal.raw())
        };
    }

    pub fn encode(&mut self, input: &MouseInput) -> Result<Vec<u8>> {
        let mut event: sys::GhosttyMouseEvent = ptr::null_mut();
        // SAFETY: событие создаётся и освобождается в этом вызове.
        unsafe {
            check(
                "ghostty_mouse_event_new",
                sys::ghostty_mouse_event_new(ptr::null(), &mut event),
            )?;
            let action = match input.action {
                MouseAction::Press => sys::GhosttyMouseAction_GHOSTTY_MOUSE_ACTION_PRESS,
                MouseAction::Release => sys::GhosttyMouseAction_GHOSTTY_MOUSE_ACTION_RELEASE,
                MouseAction::Motion => sys::GhosttyMouseAction_GHOSTTY_MOUSE_ACTION_MOTION,
            };
            sys::ghostty_mouse_event_set_action(event, action);
            if let Some(button) = input.button {
                let button = match button {
                    MouseButton::Left => sys::GhosttyMouseButton_GHOSTTY_MOUSE_BUTTON_LEFT,
                    MouseButton::Middle => sys::GhosttyMouseButton_GHOSTTY_MOUSE_BUTTON_MIDDLE,
                    MouseButton::Right => sys::GhosttyMouseButton_GHOSTTY_MOUSE_BUTTON_RIGHT,
                };
                sys::ghostty_mouse_event_set_button(event, button);
            }
            sys::ghostty_mouse_event_set_mods(event, input.mods);
            sys::ghostty_mouse_event_set_position(
                event,
                sys::GhosttyMousePosition {
                    x: input.x,
                    y: input.y,
                },
            );
            let encoder = self.raw.as_ptr();
            let result = sized_read("ghostty_mouse_encoder_encode", |buf: *mut u8, cap, len| {
                sys::ghostty_mouse_encoder_encode(encoder, event, buf.cast(), cap, len)
            });
            sys::ghostty_mouse_event_free(event);
            result
        }
    }
}

impl Drop for MouseEncoder {
    fn drop(&mut self) {
        // SAFETY: освобождается один раз.
        unsafe { sys::ghostty_mouse_encoder_free(self.raw.as_ptr()) };
    }
}

/// Отчёт фокуса режима 1004: `CSI I` при получении фокуса, `CSI O` при потере. Включён ли режим,
/// проверяет вызывающий.
pub fn focus_report(gained: bool) -> Result<Vec<u8>> {
    let event = if gained {
        sys::GhosttyFocusEvent_GHOSTTY_FOCUS_GAINED
    } else {
        sys::GhosttyFocusEvent_GHOSTTY_FOCUS_LOST
    };
    // SAFETY: функция пишет не больше cap байтов в буфер и длину в len.
    unsafe {
        sized_read("ghostty_focus_encode", |buf: *mut u8, cap, len| {
            sys::ghostty_focus_encode(event, buf.cast(), cap, len)
        })
    }
}

// --- сведения о сборке ---

/// Версия `libghostty-vt`, с которой слинкована проба, из `ghostty_build_info`.
pub fn library_version() -> Result<String> {
    let mut value = sys::GhosttyString {
        ptr: ptr::null(),
        len: 0,
    };
    // SAFETY: выходной тип VERSION_STRING — GhosttyString со статическими байтами библиотеки.
    unsafe {
        check(
            "ghostty_build_info(VERSION_STRING)",
            sys::ghostty_build_info(
                sys::GhosttyBuildInfo_GHOSTTY_BUILD_INFO_VERSION_STRING,
                ptr::from_mut(&mut value).cast(),
            ),
        )?;
        Ok(String::from_utf8_lossy(&string_bytes(value)).into_owned())
    }
}

#[cfg(test)]
mod tests;
