//! Привязки к libghostty из опубликованного набора `vendor/ghostty/` (ADR-0003).
//!
//! `bindings.rs` генерирует `make bindings` из `include/ghostty.h` набора. `build.rs` линкует только набор,
//! прошедший сверку штампа и манифеста. Безопасный слой и весь продуктовый `unsafe` живут в
//! `shellio-ghostty`. Пробы в `spikes/` по ADR-0034 могут вызывать эти привязки напрямую из своего `unsafe`,
//! продукт от них не зависит.

#[rustfmt::skip]
#[allow(non_camel_case_types, non_upper_case_globals, non_snake_case)]
mod bindings;

pub use bindings::*;

/// Версия ghostty из штампа набора, с которым слинкован crate.
pub const GHOSTTY_VERSION: &str = env!("SHELLIO_GHOSTTY_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    // Ссылка на функцию заставляет линковщик разрешить её объект в архиве без вызова и без `unsafe`.
    #[test]
    fn links_ghostty_info() {
        let info: unsafe extern "C" fn() -> ghostty_info_s = ghostty_info;
        std::hint::black_box(info);
    }

    #[test]
    fn links_surface_embedding() {
        let init: unsafe extern "C" fn(
            usize,
            *mut *mut std::os::raw::c_char,
        ) -> std::os::raw::c_int = ghostty_init;
        let app_new: unsafe extern "C" fn(
            *const ghostty_runtime_config_s,
            ghostty_config_t,
        ) -> ghostty_app_t = ghostty_app_new;
        let surface_new: unsafe extern "C" fn(
            ghostty_app_t,
            *const ghostty_surface_config_s,
        ) -> ghostty_surface_t = ghostty_surface_new;
        let surface_free: unsafe extern "C" fn(ghostty_surface_t) = ghostty_surface_free;
        let read_text: unsafe extern "C" fn(
            ghostty_surface_t,
            ghostty_selection_s,
            *mut ghostty_text_s,
        ) -> bool = ghostty_surface_read_text;
        std::hint::black_box((init, app_new, surface_new, surface_free, read_text));
    }

    #[test]
    fn version_matches_pin() {
        let pin = include_str!("../../../scripts/ghostty/pin");
        let pinned = pin
            .lines()
            .find_map(|line| line.strip_prefix("GHOSTTY_VERSION_STRING="))
            .expect("в pin нет GHOSTTY_VERSION_STRING");
        assert_eq!(GHOSTTY_VERSION, pinned);
    }
}
