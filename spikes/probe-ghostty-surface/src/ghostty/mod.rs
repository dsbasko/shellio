//! Весь `unsafe` вокруг libghostty в пробе (ADR-0034): контракт запуска процесса (`init`),
//! конфигурация (`config`), колбэки runtime и доставка на главный поток (`runtime`), реестр
//! поверхностей с единственным местом освобождения (`surface`), реестр запросов буфера обмена
//! (`clipboard`).
//!
//! Правила владения:
//! - колбэки C только копируют полезную нагрузку в сообщения Rust и будят главный поток;
//! - главное состояние живёт в `runtime` на главном потоке, и заимствование `RefCell` никогда не
//!   держится во время вызова в libghostty: библиотека синхронно зовёт колбэки из `new`, `free`,
//!   `set_color_scheme` и `update_config`;
//! - контекст `userdata` поверхности живёт до возврата `ghostty_surface_free`, контекст
//!   приложения — до возврата `ghostty_app_free`, который идёт после всех поверхностей.

pub mod clipboard;
pub mod config;
pub mod init;
pub mod runtime;
pub mod surface;

use std::ffi::{CStr, CString, c_char};

use shellio_sys_ghostty as sys;

/// Версия и режим сборки слинкованной libghostty.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Info {
    pub version: String,
    pub build_mode: &'static str,
}

pub fn info() -> Info {
    // SAFETY: функция без аргументов возвращает статические данные библиотеки.
    let raw = unsafe { sys::ghostty_info() };
    // SAFETY: `version` указывает на `version_len` байт статической строки библиотеки.
    let version = unsafe { copy_bytes(raw.version, raw.version_len) }.unwrap_or_default();
    let build_mode = match raw.build_mode {
        sys::ghostty_build_mode_e_GHOSTTY_BUILD_MODE_DEBUG => "debug",
        sys::ghostty_build_mode_e_GHOSTTY_BUILD_MODE_RELEASE_SAFE => "release-safe",
        sys::ghostty_build_mode_e_GHOSTTY_BUILD_MODE_RELEASE_FAST => "release-fast",
        sys::ghostty_build_mode_e_GHOSTTY_BUILD_MODE_RELEASE_SMALL => "release-small",
        _ => "unknown",
    };
    Info {
        version,
        build_mode,
    }
}

/// Строка C без внутренних нулей.
pub fn c_string(what: &str, text: &str) -> Result<CString, String> {
    CString::new(text).map_err(|_| format!("{what}: внутренний нулевой байт"))
}

/// Копия строки C, оканчивающейся нулём.
///
/// # Safety
/// `ptr` — NULL или указатель на строку с нулём, живую на время вызова.
pub unsafe fn copy_c_str(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: по контракту функции строка завершена нулём и жива.
    Some(
        unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned(),
    )
}

/// Копия `len` байт строки без требования нуля в конце.
///
/// # Safety
/// `ptr` — NULL или указатель на `len` байт, живых на время вызова.
pub unsafe fn copy_bytes(ptr: *const c_char, len: usize) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: по контракту функции байты живы на время вызова.
    let bytes = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), len) };
    Some(String::from_utf8_lossy(bytes).into_owned())
}

/// Копирует строку, выделенную libghostty, и освобождает её через `ghostty_string_free`.
pub fn take_string(raw: sys::ghostty_string_s) -> Option<String> {
    // SAFETY: строка от libghostty: `ptr` указывает на `len` байт или равен NULL.
    let text = unsafe { copy_bytes(raw.ptr, raw.len) };
    // SAFETY: строку выделила libghostty, освобождается ровно один раз здесь.
    unsafe { sys::ghostty_string_free(raw) };
    text.filter(|text| !text.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn c_strings_reject_interior_nul() {
        assert_eq!(c_string("x", "abc").unwrap().as_bytes(), b"abc");
        assert!(c_string("команда", "a\0b").is_err_and(|e| e.starts_with("команда")));
    }

    #[test]
    fn copies_tolerate_null_and_lengths() {
        let text = c"xterm-ghostty";
        // SAFETY: строка живёт до конца теста.
        assert_eq!(
            unsafe { copy_c_str(text.as_ptr()) }.as_deref(),
            Some("xterm-ghostty")
        );
        // SAFETY: NULL допустим по контракту.
        assert_eq!(unsafe { copy_c_str(std::ptr::null()) }, None);
        // SAFETY: первые пять байт строки живы до конца теста.
        assert_eq!(
            unsafe { copy_bytes(text.as_ptr(), 5) }.as_deref(),
            Some("xterm")
        );
        // SAFETY: NULL допустим по контракту.
        assert_eq!(unsafe { copy_bytes(std::ptr::null(), 3) }, None);
    }
}
