//! Источники ввода системы для приёмочного прохода: «Русская», ABC Extended и японский метод
//! ввода через Text Input Sources. Системные C API пробы (ADR-0034), упрощённая копия
//! `spikes/probe-session-survival/src/surface/input.rs`.
//!
//! Источник ввода общий для системы. Прежний и включённые стендом источники записываются в
//! файл [`STATE_FILE`] в `--out` процесса до смены: прежний возвращает сам процесс сценария, а
//! если он убит раньше — `--launch` по файлу после процесса.

use std::ffi::{CStr, c_char, c_void};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use objc2_core_foundation::{CFRunLoop, kCFRunLoopDefaultMode};
use serde::{Deserialize, Serialize};

pub const ABC: &str = "com.apple.keylayout.ABC";
pub const RUSSIAN: &str = "com.apple.keylayout.Russian";
pub const KOTOERI: &str = "com.apple.inputmethod.Kotoeri.RomajiTyping";
pub const JAPANESE: &str = "com.apple.inputmethod.Kotoeri.RomajiTyping.Japanese";

/// Прежний источник и включённые стендом, в `--out` процесса.
pub const STATE_FILE: &str = "input-source.json";
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

/// Источники ввода процесса сценария: смена с записью прежнего и возврат.
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

    /// Запись временного файла и `rename`: убитый посреди записи процесс оставляет прежний
    /// полный файл.
    fn save(&self) -> Result<(), String> {
        let text = serde_json::to_string(&self.state).map_err(|err| format!("{err}"))?;
        let tmp = self.file.with_extension("json.tmp");
        fs::write(&tmp, text).map_err(|err| format!("{}: {err}", tmp.display()))?;
        fs::rename(&tmp, &self.file).map_err(|err| format!("{}: {err}", self.file.display()))
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
    /// процессу. `Ok(false)` — источник не выбран за срок.
    pub fn select(&mut self, id: &str) -> Result<bool, String> {
        if self.state.is_none() {
            let original = current().ok_or("текущий источник ввода не прочитан")?;
            self.state = Some(SourceState {
                original,
                enabled: Vec::new(),
            });
            self.save()?;
        }
        if id == JAPANESE {
            self.enable(KOTOERI)?;
        }
        self.enable(id)?;
        Ok(select_now(id))
    }

    /// Прежний источник и выключение включённых стендом. Файл состояния удаляется только после
    /// подтверждённого возврата, иначе он остаётся для `--launch`.
    pub fn restore(&mut self) -> Option<Result<String, String>> {
        let state = self.state.take()?;
        let result = restore_state(&state);
        if result.is_ok() {
            let _ = fs::remove_file(&self.file);
        } else {
            self.state = Some(state);
        }
        Some(result)
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

/// Выбор прежнего источника с проверкой `current() == original` и выключение включённых
/// стендом. `Ok` — фактический текущий источник.
fn restore_state(state: &SourceState) -> Result<String, String> {
    let mut problems = Vec::new();
    if !select_now(&state.original) {
        problems.push(format!(
            "прежний источник {} не выбран, текущий {:?}",
            state.original,
            current()
        ));
    }
    for id in state.enabled.iter().rev() {
        match find(id, true) {
            Some(source) => {
                // SAFETY: источник жив, пока жив `source`.
                let status = unsafe { TISDisableInputSource(source.0) };
                if status != 0 {
                    problems.push(format!("TISDisableInputSource {id}: {status}"));
                }
            }
            None => problems.push(format!("источник {id} не найден для выключения")),
        }
    }
    let now = current().unwrap_or_default();
    if problems.is_empty() && now == state.original {
        Ok(now)
    } else {
        Err(problems.join("; "))
    }
}

/// Возврат по файлу состояния в `dir`, если процесс сценария не вернул источник сам. Файл
/// удаляется только после подтверждённого возврата.
pub fn restore_file(dir: &Path) -> Option<Result<String, String>> {
    let file = dir.join(STATE_FILE);
    let text = fs::read_to_string(&file).ok()?;
    let state: Option<SourceState> = match serde_json::from_str(&text) {
        Ok(state) => state,
        Err(err) => return Some(Err(format!("{}: {err}", file.display()))),
    };
    let state = state?;
    let result = restore_state(&state);
    if result.is_ok() {
        let _ = fs::remove_file(&file);
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_file_round_trip() {
        let dir = std::env::temp_dir().join(format!("probe-sources-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("каталог теста");
        let state = Some(SourceState {
            original: ABC.to_owned(),
            enabled: vec![RUSSIAN.to_owned()],
        });
        fs::write(
            dir.join(STATE_FILE),
            serde_json::to_string(&state).expect("json"),
        )
        .expect("запись");
        let text = fs::read_to_string(dir.join(STATE_FILE)).expect("чтение");
        let back: Option<SourceState> = serde_json::from_str(&text).expect("разбор");
        assert_eq!(back, state);
        let _ = fs::remove_dir_all(&dir);
    }
}
