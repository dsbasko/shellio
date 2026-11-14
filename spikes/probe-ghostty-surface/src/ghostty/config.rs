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

    pub fn is_dark(self) -> bool {
        self == ColorScheme::Dark
    }
}

/// Встроенные темы пробы для светлой и тёмной схемы.
pub const LIGHT_THEME: &str = "Builtin Light";
pub const DARK_THEME: &str = "Builtin Dark";

/// Значение ключа `theme` условной темы: `light:<светлая>,dark:<тёмная>`. Имена с запятой или
/// двоеточием ghostty не разберёт, поэтому они отвергаются.
pub fn conditional_theme(light: &str, dark: &str) -> Result<String, String> {
    for name in [light, dark] {
        if name.trim().is_empty() || name.contains([',', ':', '\n']) {
            return Err(format!("имя темы {name:?} не годится для условной темы"));
        }
    }
    Ok(format!("light:{},dark:{}", light.trim(), dark.trim()))
}

/// Тема, которую условная тема выбирает для схемы.
pub fn theme_for(scheme: ColorScheme, light: &str, dark: &str) -> String {
    match scheme {
        ColorScheme::Light => light.to_owned(),
        ColorScheme::Dark => dark.to_owned(),
    }
}

/// Значение ключа `theme` в тексте конфигурации: последняя строка `theme = …`.
pub fn theme_value(text: &str) -> Option<String> {
    text.lines()
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            (key.trim() == "theme").then(|| value.trim().to_owned())
        })
        .next_back()
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

    #[test]
    fn scheme_darkness() {
        assert!(ColorScheme::Dark.is_dark());
        assert!(!ColorScheme::Light.is_dark());
    }

    #[test]
    fn theme_is_chosen_by_scheme() {
        assert_eq!(
            theme_for(ColorScheme::Light, LIGHT_THEME, DARK_THEME),
            "Builtin Light"
        );
        assert_eq!(
            theme_for(ColorScheme::Dark, LIGHT_THEME, DARK_THEME),
            "Builtin Dark"
        );
    }

    #[test]
    fn conditional_theme_line_is_built_and_validated() {
        assert_eq!(
            conditional_theme(LIGHT_THEME, DARK_THEME).as_deref(),
            Ok("light:Builtin Light,dark:Builtin Dark")
        );
        assert_eq!(
            conditional_theme(" A ", "B").as_deref(),
            Ok("light:A,dark:B")
        );
        assert!(conditional_theme("", "B").is_err());
        assert!(conditional_theme("A,B", "C").is_err());
        assert!(conditional_theme("A", "dark:C").is_err());
    }

    #[test]
    fn theme_value_reads_the_last_theme_line() {
        let text = "font-size = 13\ntheme = light:X,dark:Y\ncursor-style-blink = true\n";
        assert_eq!(theme_value(text).as_deref(), Some("light:X,dark:Y"));
        assert_eq!(
            theme_value("theme = a\ntheme = light:B,dark:C").as_deref(),
            Some("light:B,dark:C")
        );
        assert_eq!(theme_value("font-size = 13"), None);
        assert_eq!(theme_value("themes = x"), None);
    }
}
