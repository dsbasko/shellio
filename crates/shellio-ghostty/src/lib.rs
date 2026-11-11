//! Безопасный слой над libghostty (ADR-0002, ADR-0003).
//!
//! Весь `unsafe` вокруг libghostty живёт здесь, типы Ghostty наружу не выходят. В P0.1 crate даёт
//! только [`info`]: сведения о слинкованной сборке, которым не нужна инициализация.
//!
//! `ghostty_init` в P0.1 не вызывается, и безопасного контракта запуска crate пока не обещает. Вызов
//! влияет на весь процесс: ставит обработчики сигналов, меняет locale и окружение, хранит указатели
//! `argv` без копирования. Контракт запуска определяет P0.3 по исходным данным из раздела
//! «Уточнено в P0.1» ADR-0003.

use std::error::Error;
use std::fmt;
use std::slice;
use std::str::{self, Utf8Error};

use shellio_sys_ghostty as sys;

/// Режим, в котором собран libghostty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildMode {
    Debug,
    ReleaseSafe,
    ReleaseFast,
    ReleaseSmall,
}

/// Сведения о слинкованной сборке libghostty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Info {
    pub build_mode: BuildMode,
    pub version: &'static str,
}

/// Причина, по которой сведения libghostty не удалось принять.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InfoError {
    UnknownBuildMode(u32),
    NullVersion,
    InvalidVersion(Utf8Error),
}

impl fmt::Display for InfoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownBuildMode(value) => {
                write!(f, "libghostty вернул неизвестный режим сборки {value}")
            }
            Self::NullVersion => f.write_str("libghostty вернул пустой указатель версии"),
            Self::InvalidVersion(err) => write!(f, "версия libghostty не в UTF-8: {err}"),
        }
    }
}

impl Error for InfoError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidVersion(err) => Some(err),
            Self::UnknownBuildMode(_) | Self::NullVersion => None,
        }
    }
}

/// Возвращает режим сборки и версию слинкованного libghostty.
///
/// Не требует `ghostty_init` и вызывается с любого потока.
pub fn info() -> Result<Info, InfoError> {
    // SAFETY: ghostty_info не принимает аргументов, не требует инициализации и только возвращает структуру.
    let raw = unsafe { sys::ghostty_info() };
    // SAFETY: в закреплённой ревизии `version` указывает на comptime-строку `build_config.version_string`
    // длиной `version_len` (src/main_c.zig), она лежит в образе библиотеки и живёт всё время процесса.
    unsafe { from_raw(raw) }
}

/// Переводит сырые сведения в [`Info`]. Версия читается по длине, а не до NUL.
///
/// # Safety
///
/// Ненулевой `raw.version` указывает на `raw.version_len` байт, доступных для чтения всё время процесса.
unsafe fn from_raw(raw: sys::ghostty_info_s) -> Result<Info, InfoError> {
    let build_mode = match raw.build_mode {
        sys::ghostty_build_mode_e_GHOSTTY_BUILD_MODE_DEBUG => BuildMode::Debug,
        sys::ghostty_build_mode_e_GHOSTTY_BUILD_MODE_RELEASE_SAFE => BuildMode::ReleaseSafe,
        sys::ghostty_build_mode_e_GHOSTTY_BUILD_MODE_RELEASE_FAST => BuildMode::ReleaseFast,
        sys::ghostty_build_mode_e_GHOSTTY_BUILD_MODE_RELEASE_SMALL => BuildMode::ReleaseSmall,
        other => return Err(InfoError::UnknownBuildMode(other)),
    };
    if raw.version.is_null() {
        return Err(InfoError::NullVersion);
    }
    // SAFETY: указатель ненулевой, длина и время жизни — обязанность вызывающего.
    let bytes: &'static [u8] =
        unsafe { slice::from_raw_parts(raw.version.cast::<u8>(), raw.version_len) };
    let version = str::from_utf8(bytes).map_err(InfoError::InvalidVersion)?;
    Ok(Info {
        build_mode,
        version,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(build_mode: u32, version: &'static [u8], version_len: usize) -> sys::ghostty_info_s {
        sys::ghostty_info_s {
            build_mode,
            version: version.as_ptr().cast(),
            version_len,
        }
    }

    fn convert(raw: sys::ghostty_info_s) -> Result<Info, InfoError> {
        // SAFETY: версии в тестах — статические байтовые строки не короче переданной длины.
        unsafe { from_raw(raw) }
    }

    #[test]
    fn info_matches_stamp() {
        let info = info().unwrap();
        assert_eq!(info.build_mode, BuildMode::ReleaseFast);
        assert_eq!(info.version, sys::GHOSTTY_VERSION);
    }

    #[test]
    fn maps_every_build_mode() {
        for (value, mode) in [
            (
                sys::ghostty_build_mode_e_GHOSTTY_BUILD_MODE_DEBUG,
                BuildMode::Debug,
            ),
            (
                sys::ghostty_build_mode_e_GHOSTTY_BUILD_MODE_RELEASE_SAFE,
                BuildMode::ReleaseSafe,
            ),
            (
                sys::ghostty_build_mode_e_GHOSTTY_BUILD_MODE_RELEASE_FAST,
                BuildMode::ReleaseFast,
            ),
            (
                sys::ghostty_build_mode_e_GHOSTTY_BUILD_MODE_RELEASE_SMALL,
                BuildMode::ReleaseSmall,
            ),
        ] {
            assert_eq!(convert(raw(value, b"1.4.0", 5)).unwrap().build_mode, mode);
        }
    }

    #[test]
    fn reads_version_by_length() {
        let info = convert(raw(2, b"1.4.0junk", 5)).unwrap();
        assert_eq!(info.version, "1.4.0");
    }

    #[test]
    fn rejects_unknown_build_mode() {
        assert_eq!(
            convert(raw(4, b"1.4.0", 5)),
            Err(InfoError::UnknownBuildMode(4))
        );
    }

    #[test]
    fn rejects_null_version() {
        let raw = sys::ghostty_info_s {
            build_mode: 2,
            version: std::ptr::null(),
            version_len: 0,
        };
        assert_eq!(convert(raw), Err(InfoError::NullVersion));
    }

    #[test]
    fn rejects_invalid_utf8() {
        let err = convert(raw(2, b"1.4.\xff", 5)).unwrap_err();
        assert!(matches!(err, InfoError::InvalidVersion(_)));
        assert!(err.source().is_some());
    }
}
