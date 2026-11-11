//! Привязки к libghostty из опубликованного набора `vendor/ghostty/` (ADR-0003).
//!
//! `bindings.rs` генерирует `make bindings` из `include/ghostty.h` набора. `build.rs` линкует только набор,
//! прошедший сверку штампа и манифеста. Безопасный слой и весь `unsafe` живут в `shellio-ghostty`.

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
    fn version_matches_pin() {
        let pin = include_str!("../../../scripts/ghostty/pin");
        let pinned = pin
            .lines()
            .find_map(|line| line.strip_prefix("GHOSTTY_VERSION_STRING="))
            .expect("в pin нет GHOSTTY_VERSION_STRING");
        assert_eq!(GHOSTTY_VERSION, pinned);
    }
}
