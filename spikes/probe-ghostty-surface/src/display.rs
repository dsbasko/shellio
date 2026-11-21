//! Режим дисплея для приёмочного прохода: частота и масштаб встроенного дисплея вместо второго
//! дисплея. Системные C API пробы (ADR-0034).
//!
//! `CGDisplaySetDisplayMode` меняет режим на время жизни процесса: при выходе процесса система
//! возвращает постоянный режим из настроек «Дисплеи» (не обязательно режим, бывший при входе),
//! поэтому прерванный сценарий режим за собой не оставляет.

use std::ffi::c_void;

use serde::Serialize;

type CFTypeRef = *const c_void;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
    static kCFBooleanTrue: CFTypeRef;
    fn CFRelease(cf: CFTypeRef);
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

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    static kCGDisplayShowDuplicateLowResolutionModes: CFTypeRef;
    fn CGDisplayCopyAllDisplayModes(display: u32, options: CFTypeRef) -> CFTypeRef;
    fn CGDisplayCopyDisplayMode(display: u32) -> CFTypeRef;
    fn CGDisplaySetDisplayMode(display: u32, mode: CFTypeRef, options: CFTypeRef) -> i32;
    fn CGDisplayModeGetWidth(mode: CFTypeRef) -> usize;
    fn CGDisplayModeGetHeight(mode: CFTypeRef) -> usize;
    fn CGDisplayModeGetPixelWidth(mode: CFTypeRef) -> usize;
    fn CGDisplayModeGetPixelHeight(mode: CFTypeRef) -> usize;
    fn CGDisplayModeGetRefreshRate(mode: CFTypeRef) -> f64;
    fn CGDisplayModeIsUsableForDesktopGUI(mode: CFTypeRef) -> bool;
}

struct Owned(CFTypeRef);

impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: ссылка получена функцией Create/Copy и освобождается один раз.
            unsafe { CFRelease(self.0) };
        }
    }
}

/// Режим дисплея: размер в точках, в пикселях и частота.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Mode {
    pub width: usize,
    pub height: usize,
    pub pixel_width: usize,
    pub pixel_height: usize,
    pub refresh_hz: f64,
}

impl Mode {
    /// Масштаб подложки режима: пиксели на точку.
    pub fn scale(&self) -> f64 {
        if self.width == 0 {
            return 0.0;
        }
        self.pixel_width as f64 / self.width as f64
    }

    /// # Safety
    /// `mode` — живой `CGDisplayModeRef`.
    unsafe fn of(mode: CFTypeRef) -> Mode {
        // SAFETY: по контракту режим жив.
        unsafe {
            Mode {
                width: CGDisplayModeGetWidth(mode),
                height: CGDisplayModeGetHeight(mode),
                pixel_width: CGDisplayModeGetPixelWidth(mode),
                pixel_height: CGDisplayModeGetPixelHeight(mode),
                refresh_hz: CGDisplayModeGetRefreshRate(mode),
            }
        }
    }
}

/// Текущий режим дисплея.
pub fn current(display: u32) -> Option<Mode> {
    // SAFETY: идентификатор дисплея — число, результат освобождает `Owned`.
    let mode = Owned(unsafe { CGDisplayCopyDisplayMode(display) });
    if mode.0.is_null() {
        return None;
    }
    // SAFETY: режим жив, пока жив `mode`.
    Some(unsafe { Mode::of(mode.0) })
}

/// Есть ли пригодный режим, для которого `want` верно.
pub fn has(display: u32, want: impl Fn(&Mode) -> bool) -> bool {
    with_modes(display, |modes| {
        modes.iter().any(|(_, usable, mode)| *usable && want(mode))
    })
}

/// Ставит первый пригодный режим, для которого `want` верно, на время жизни процесса.
pub fn set(display: u32, want: impl Fn(&Mode) -> bool) -> Result<Mode, String> {
    with_modes(display, |modes| {
        let (raw, _, mode) = modes
            .iter()
            .find(|(_, usable, mode)| *usable && want(mode))
            .ok_or("подходящего режима дисплея нет")?;
        // SAFETY: режим жив, пока жив список режимов.
        let status = unsafe { CGDisplaySetDisplayMode(display, *raw, std::ptr::null()) };
        if status != 0 {
            return Err(format!("CGDisplaySetDisplayMode: {status}"));
        }
        Ok(*mode)
    })
}

fn with_modes<R>(display: u32, f: impl FnOnce(&[(CFTypeRef, bool, Mode)]) -> R) -> R {
    // SAFETY: ключ и значение — константы CoreFoundation и CoreGraphics.
    let options = Owned(unsafe {
        CFDictionaryCreate(
            std::ptr::null(),
            &raw const kCGDisplayShowDuplicateLowResolutionModes,
            &raw const kCFBooleanTrue,
            1,
            &raw const kCFTypeDictionaryKeyCallBacks,
            &raw const kCFTypeDictionaryValueCallBacks,
        )
    });
    // SAFETY: словарь жив, результат освобождает `Owned`.
    let list = Owned(unsafe { CGDisplayCopyAllDisplayModes(display, options.0) });
    let mut modes = Vec::new();
    if !list.0.is_null() {
        // SAFETY: список жив до конца функции.
        let count = unsafe { CFArrayGetCount(list.0) };
        for index in 0..count {
            // SAFETY: индекс в пределах, элемент удерживается списком.
            let raw = unsafe { CFArrayGetValueAtIndex(list.0, index) };
            // SAFETY: элемент списка режимов — живой `CGDisplayModeRef`.
            let (usable, mode) =
                unsafe { (CGDisplayModeIsUsableForDesktopGUI(raw), Mode::of(raw)) };
            modes.push((raw, usable, mode));
        }
    }
    f(&modes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_is_pixels_per_point() {
        let mode = Mode {
            width: 1512,
            height: 982,
            pixel_width: 3024,
            pixel_height: 1964,
            refresh_hz: 120.0,
        };
        assert_eq!(mode.scale(), 2.0);
        let flat = Mode { width: 0, ..mode };
        assert_eq!(flat.scale(), 0.0);
    }
}
