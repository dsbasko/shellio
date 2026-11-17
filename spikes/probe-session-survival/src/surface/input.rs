//! Синтетический ввод прохода F4 (задача 19): источники ввода через Text Input Sources и события
//! клавиш по пути клавиатуры P0.3. Системные C API пробы (ADR-0034).
//!
//! - Событие задаётся только кодом клавиши и модификаторами: `CGEventCreateKeyboardEvent` из
//!   собственного источника состояния (`Private`), поэтому удерживаемые владельцем модификаторы
//!   в событие не попадают, а символы даёт текущий источник ввода системы. Событие идёт через
//!   `NSApp.postEvent` и обычную доставку `sendEvent` процесса, а не в системный поток событий.
//! - Источник ввода общий для системы. Прежний и включённые стендом источники записываются в
//!   файл [`STATE_FILE`] в каталоге GUI до смены: прежний возвращает GUI, водитель после части и
//!   `--launch` после процесса сценария, если GUI убит раньше.

use std::ffi::{CStr, c_char, c_void};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use objc2_app_kit::{NSApplication, NSEvent, NSEventMask};
use objc2_core_foundation::{CFRunLoop, kCFRunLoopDefaultMode};
use objc2_core_graphics::{CGEvent, CGEventFlags, CGEventSource, CGEventSourceStateID};
use objc2_foundation::{NSDate, NSDefaultRunLoopMode};
use serde::{Deserialize, Serialize};

use crate::matrix::apps_surface::{Mods, SOURCE_JAPANESE, SOURCE_KOTOERI, Stroke};

/// Прежний источник и включённые стендом, в каталоге GUI.
pub const STATE_FILE: &str = "input-source.json";
/// Срок, за который смена источника становится видна процессу.
const SELECT_WAIT: Duration = Duration::from_secs(3);

type CFTypeRef = *const c_void;
type OSStatus = i32;
const UTF8: u32 = 0x0800_0100;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
    fn CFRelease(cf: CFTypeRef);
    fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
    fn CFStringCreateWithBytes(
        alloc: CFTypeRef,
        bytes: *const u8,
        len: isize,
        encoding: u32,
        external: bool,
    ) -> CFTypeRef;
    fn CFStringGetCString(
        string: CFTypeRef,
        buffer: *mut c_char,
        size: isize,
        encoding: u32,
    ) -> bool;
    fn CFDictionaryCreate(
        alloc: CFTypeRef,
        keys: *const CFTypeRef,
        values: *const CFTypeRef,
        count: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CFTypeRef;
    fn CFArrayGetCount(array: CFTypeRef) -> isize;
    fn CFArrayGetValueAtIndex(array: CFTypeRef, index: isize) -> CFTypeRef;
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    static kTISPropertyInputSourceID: CFTypeRef;
    fn TISCopyCurrentKeyboardInputSource() -> CFTypeRef;
    fn TISCreateInputSourceList(properties: CFTypeRef, include_all: bool) -> CFTypeRef;
    fn TISSelectInputSource(source: CFTypeRef) -> OSStatus;
    fn TISEnableInputSource(source: CFTypeRef) -> OSStatus;
    fn TISDisableInputSource(source: CFTypeRef) -> OSStatus;
    fn TISGetInputSourceProperty(source: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
}

/// Владеемая ссылка CoreFoundation.
struct Owned(CFTypeRef);

impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: ссылка получена функцией Create/Copy или CFRetain и освобождается один раз.
            unsafe { CFRelease(self.0) };
        }
    }
}

fn cf_string(text: &str) -> Owned {
    // SAFETY: байты живут на время вызова, длина явная.
    Owned(unsafe {
        CFStringCreateWithBytes(
            std::ptr::null(),
            text.as_ptr(),
            text.len() as isize,
            UTF8,
            false,
        )
    })
}

/// ID источника ввода.
///
/// # Safety
/// `source` — живой `TISInputSourceRef`.
unsafe fn source_id(source: CFTypeRef) -> Option<String> {
    // SAFETY: по контракту источник жив; ключ — константа Carbon.
    let value = unsafe { TISGetInputSourceProperty(source, kTISPropertyInputSourceID) };
    if value.is_null() {
        return None;
    }
    let mut buffer = [0 as c_char; 256];
    // SAFETY: свойство — CFString, буфер локальный с явной длиной.
    let ok = unsafe { CFStringGetCString(value, buffer.as_mut_ptr(), buffer.len() as isize, UTF8) };
    // SAFETY: успешный вызов записал строку с нулём в буфер.
    ok.then(|| {
        unsafe { CStr::from_ptr(buffer.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    })
}

/// Текущий источник ввода клавиатуры.
pub fn current() -> Option<String> {
    // SAFETY: функция без аргументов, результат освобождает `Owned`.
    let source = Owned(unsafe { TISCopyCurrentKeyboardInputSource() });
    if source.0.is_null() {
        return None;
    }
    // SAFETY: источник жив, пока жив `source`.
    unsafe { source_id(source.0) }
}

/// Источник по ID среди включённых (`all = false`) или всех установленных.
fn find(id: &str, all: bool) -> Option<Owned> {
    let key = cf_string(id);
    // SAFETY: ключ — константа Carbon, значение живо до конца функции.
    let properties = Owned(unsafe {
        CFDictionaryCreate(
            std::ptr::null(),
            &raw const kTISPropertyInputSourceID,
            &key.0,
            1,
            &raw const kCFTypeDictionaryKeyCallBacks,
            &raw const kCFTypeDictionaryValueCallBacks,
        )
    });
    // SAFETY: словарь жив, результат освобождает `Owned`.
    let list = Owned(unsafe { TISCreateInputSourceList(properties.0, all) });
    // SAFETY: список жив; пустой список даёт счёт 0.
    if list.0.is_null() || unsafe { CFArrayGetCount(list.0) } == 0 {
        return None;
    }
    // SAFETY: индекс 0 в пределах; элемент удерживается до освобождения списка.
    Some(Owned(unsafe {
        CFRetain(CFArrayGetValueAtIndex(list.0, 0))
    }))
}

/// Прежний источник и включённые стендом.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceState {
    pub original: String,
    pub enabled: Vec<String>,
}

/// Источники ввода GUI-процесса: смена с записью прежнего и возврат.
pub struct Sources {
    file: PathBuf,
    state: Option<SourceState>,
}

impl Sources {
    pub fn new(dir: &Path) -> Sources {
        Sources {
            file: dir.join(STATE_FILE),
            state: None,
        }
    }

    fn save(&self) -> Result<(), String> {
        let text = serde_json::to_string(&self.state).map_err(|err| format!("{err}"))?;
        fs::write(&self.file, text).map_err(|err| format!("{}: {err}", self.file.display()))
    }

    fn enable(&mut self, id: &str) -> Result<(), String> {
        if find(id, false).is_some() {
            return Ok(());
        }
        let source = find(id, true).ok_or_else(|| format!("источника {id} нет в системе"))?;
        if let Some(state) = self.state.as_mut() {
            state.enabled.push(id.to_owned());
        }
        self.save()?;
        // SAFETY: источник жив, пока жив `source`.
        let status = unsafe { TISEnableInputSource(source.0) };
        if status != 0 {
            return Err(format!("TISEnableInputSource {id}: {status}"));
        }
        Ok(())
    }

    /// Включает при необходимости и выбирает источник `id`, ждёт, пока смена станет видна
    /// процессу. `Ok(false)` — стенд не смог выбрать источник.
    pub fn select(&mut self, id: &str) -> Result<bool, String> {
        if self.state.is_none() {
            let original = current().ok_or("текущий источник ввода не прочитан")?;
            self.state = Some(SourceState {
                original,
                enabled: Vec::new(),
            });
            self.save()?;
        }
        if id == SOURCE_JAPANESE {
            self.enable(SOURCE_KOTOERI)?;
        }
        self.enable(id)?;
        Ok(select_now(id))
    }

    /// Прежний источник и выключение включённых стендом; файл состояния удаляется.
    pub fn restore(&mut self) -> Option<String> {
        let state = self.state.take()?;
        restore_state(&state);
        let _ = fs::remove_file(&self.file);
        current()
    }
}

fn select_now(id: &str) -> bool {
    let Some(source) = find(id, false) else {
        return false;
    };
    // SAFETY: источник жив, пока жив `source`.
    if unsafe { TISSelectInputSource(source.0) } != 0 {
        return false;
    }
    let deadline = Instant::now() + SELECT_WAIT;
    while current().as_deref() != Some(id) {
        if Instant::now() >= deadline {
            return false;
        }
        // SAFETY: чтение константы режима CoreFoundation.
        let mode = unsafe { kCFRunLoopDefaultMode };
        CFRunLoop::run_in_mode(mode, 0.01, true);
    }
    true
}

fn restore_state(state: &SourceState) {
    select_now(&state.original);
    for id in state.enabled.iter().rev() {
        if let Some(source) = find(id, true) {
            // SAFETY: источник жив, пока жив `source`.
            unsafe { TISDisableInputSource(source.0) };
        }
    }
}

/// Возврат по файлу состояния в `dir`, если GUI не вернула источник сама.
pub fn restore_file(dir: &Path) -> Option<SourceState> {
    let file = dir.join(STATE_FILE);
    let text = fs::read_to_string(&file).ok()?;
    let state: Option<SourceState> = serde_json::from_str(&text).ok()?;
    let state = state?;
    restore_state(&state);
    let _ = fs::remove_file(&file);
    Some(state)
}

/// Возврат по файлам состояния всех каталогов GUI в `out`.
pub fn restore_all(out: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(out) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| {
            restore_file(&entry.path()).map(|state| {
                format!(
                    "источник ввода возвращён по {}: {}",
                    entry.path().display(),
                    state.original
                )
            })
        })
        .collect()
}

fn flags_of(mods: Mods) -> CGEventFlags {
    let mut flags = CGEventFlags::empty();
    for (on, flag) in [
        (mods.shift, CGEventFlags::MaskShift),
        (mods.control, CGEventFlags::MaskControl),
        (mods.option, CGEventFlags::MaskAlternate),
        (mods.command, CGEventFlags::MaskCommand),
    ] {
        if on {
            flags |= flag;
        }
    }
    flags
}

/// Нажатие и отпускание клавиши через `postEvent` и обычную доставку процесса.
pub fn post(app: &NSApplication, stroke: Stroke) -> Result<(), String> {
    let source = CGEventSource::new(CGEventSourceStateID::Private)
        .ok_or("CGEventSourceCreate вернул NULL")?;
    for down in [true, false] {
        let event = CGEvent::new_keyboard_event(Some(&source), stroke.keycode, down)
            .ok_or("CGEventCreateKeyboardEvent вернул NULL")?;
        CGEvent::set_flags(Some(&event), flags_of(stroke.mods));
        let event = NSEvent::eventWithCGEvent(&event).ok_or("NSEvent из CGEvent не построен")?;
        app.postEvent_atStart(&event, false);
        drain(app);
    }
    Ok(())
}

/// Доставляет события, уже стоящие в очереди процесса, без ожидания.
pub fn drain_now(app: &NSApplication) {
    drain_for(app, 0.0);
}

/// Доставляет события очереди процесса, пока она не опустеет.
pub fn drain(app: &NSApplication) {
    drain_for(app, 0.02);
}

fn drain_for(app: &NSApplication, wait: f64) {
    loop {
        let until = NSDate::dateWithTimeIntervalSinceNow(wait);
        // SAFETY: чтение константы режима Foundation.
        let mode = unsafe { NSDefaultRunLoopMode };
        let Some(event) = app.nextEventMatchingMask_untilDate_inMode_dequeue(
            NSEventMask::Any,
            Some(&until),
            mode,
            true,
        ) else {
            return;
        };
        app.sendEvent(&event);
    }
}
