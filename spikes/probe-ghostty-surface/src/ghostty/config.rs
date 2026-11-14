//! Конфигурация libghostty из файла, который `run.sh` пишет в `--out` сценария. Пользовательский
//! `ghostty.conf` не читается: `ghostty_config_load_default_files` и `load_cli_args` не вызываются.

use std::path::Path;

use shellio_sys_ghostty as sys;

/// Имя файла конфигурации в `--out` сценария.
pub const FILE: &str = "ghostty.conf";

/// Конфигурация после `ghostty_config_finalize`. Освобождается после приложения, которое из неё
/// создано.
pub struct Config {
    raw: sys::ghostty_config_t,
}

impl Config {
    pub fn load(path: &Path) -> Result<Config, String> {
        let text = path.to_str().ok_or("путь конфигурации не в UTF-8")?;
        let path = super::c_string("путь конфигурации", text)?;
        // SAFETY: функция без аргументов, NULL проверяется.
        let raw = unsafe { sys::ghostty_config_new() };
        if raw.is_null() {
            return Err("ghostty_config_new вернул NULL".to_owned());
        }
        let config = Config { raw };
        // SAFETY: конфигурация жива, путь — строка с нулём на время вызова.
        unsafe {
            sys::ghostty_config_load_file(config.raw, path.as_ptr());
            sys::ghostty_config_finalize(config.raw);
        }
        Ok(config)
    }

    /// Сообщения `ghostty_config_get_diagnostic`.
    pub fn diagnostics(&self) -> Vec<String> {
        // SAFETY: конфигурация жива.
        let count = unsafe { sys::ghostty_config_diagnostics_count(self.raw) };
        (0..count)
            .map(|index| {
                // SAFETY: индекс меньше числа диагностик, сообщение живёт вместе с конфигурацией.
                let diagnostic = unsafe { sys::ghostty_config_get_diagnostic(self.raw, index) };
                // SAFETY: сообщение — строка с нулём или NULL.
                unsafe { super::copy_c_str(diagnostic.message) }
                    .unwrap_or_else(|| "<пустое сообщение>".to_owned())
            })
            .collect()
    }

    pub fn raw(&self) -> sys::ghostty_config_t {
        self.raw
    }
}

impl Drop for Config {
    fn drop(&mut self) {
        // SAFETY: конфигурация принадлежит этой структуре и освобождается один раз.
        unsafe { sys::ghostty_config_free(self.raw) };
    }
}

/// Схема цвета приложения, которая выставляется до первой поверхности.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorScheme {
    Light,
    Dark,
}

impl ColorScheme {
    /// Схема по имени варианта сценария.
    pub fn from_variant(variant: &str) -> Option<ColorScheme> {
        match variant {
            "light" => Some(ColorScheme::Light),
            "dark" => Some(ColorScheme::Dark),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            ColorScheme::Light => "light",
            ColorScheme::Dark => "dark",
        }
    }

    pub fn raw(self) -> sys::ghostty_color_scheme_e {
        match self {
            ColorScheme::Light => sys::ghostty_color_scheme_e_GHOSTTY_COLOR_SCHEME_LIGHT,
            ColorScheme::Dark => sys::ghostty_color_scheme_e_GHOSTTY_COLOR_SCHEME_DARK,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheme_follows_variant() {
        assert_eq!(ColorScheme::from_variant("light"), Some(ColorScheme::Light));
        assert_eq!(ColorScheme::from_variant("dark"), Some(ColorScheme::Dark));
        assert_eq!(ColorScheme::from_variant("base"), None);
        assert_ne!(ColorScheme::Light.raw(), ColorScheme::Dark.raw());
        assert_eq!(ColorScheme::Dark.name(), "dark");
    }
}
