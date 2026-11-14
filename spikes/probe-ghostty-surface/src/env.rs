//! Окружение процесса пробы: построение с нуля для `--launch` и проверка фактического окружения в
//! самом процессе. Модуль не зависит от AppKit.
//!
//! Родительский терминал задаёт `GHOSTTY_RESOURCES_DIR`, а она в `ReleaseFast` важнее поиска
//! ресурсов в бандле, поэтому окружение не наследуется, а собирается по белому списку.

use crate::args::Route;

/// Переменные, которые берутся из окружения запуска как есть.
pub const INHERITED: [&str; 6] = ["HOME", "USER", "LOGNAME", "PATH", "TMPDIR", "SHELL"];
/// Без них процесс пробы не запускается.
const REQUIRED: [&str; 2] = ["HOME", "PATH"];
pub const STATE_DIR: &str = "SHELLIO_STATE_DIR";
pub const LANG: &str = "LANG";
/// Явный допустимый `LANG` прямого маршрута.
pub const LANG_VALUE: &str = "en_US.UTF-8";
/// Управляемые переменные варианта, которые `--launch` принимает через `--set`.
pub const VARIANT_VARS: [&str; 1] = ["MallocScribble"];
/// Префиксы, которых не бывает в окружении процесса пробы ни на одном маршруте.
pub const FORBIDDEN_PREFIXES: [&str; 5] = ["GHOSTTY_", "TERM", "LC_", "AGTERM_", "NVIM"];
/// Переменные языка, которых нет на маршруте LaunchServices: путь Cocoa выбирает язык сам.
const LANGUAGE_VARS: [&str; 2] = ["LANG", "LANGUAGE"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnvError {
    Missing(&'static str),
    /// Переменная варианта вне `VARIANT_VARS`.
    NotVariant(String),
}

impl std::fmt::Display for EnvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EnvError::Missing(name) => write!(f, "в окружении запуска нет {name}"),
            EnvError::NotVariant(name) => write!(f, "{name} не переменная варианта"),
        }
    }
}

/// Окружение процесса пробы. `inherited` читает окружение запуска.
pub fn build(
    inherited: impl Fn(&str) -> Option<String>,
    route: Route,
    state_dir: &str,
    variant: &[(String, String)],
) -> Result<Vec<(String, String)>, EnvError> {
    let mut env = Vec::new();
    for name in INHERITED {
        match inherited(name).filter(|value| !value.is_empty()) {
            Some(value) => env.push((name.to_owned(), value)),
            None if REQUIRED.contains(&name) => return Err(EnvError::Missing(name)),
            None => {}
        }
    }
    env.push((STATE_DIR.to_owned(), state_dir.to_owned()));
    match route {
        Route::Direct => env.push((LANG.to_owned(), LANG_VALUE.to_owned())),
        Route::LaunchServices => {}
    }
    for (name, value) in variant {
        if !VARIANT_VARS.contains(&name.as_str()) {
            return Err(EnvError::NotVariant(name.clone()));
        }
        env.push((name.clone(), value.clone()));
    }
    Ok(env)
}

/// Итог проверки фактического окружения процесса пробы.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Check {
    /// Нарушения предусловия: запрещённая переменная или неверный `LANG`.
    pub violations: Vec<String>,
    /// Переменные вне белого списка, которые не запрещены, например добавленные LaunchServices.
    pub extra: Vec<String>,
}

/// Сверяет фактическое окружение с маршрутом.
pub fn check(env: &[(String, String)], route: Route) -> Check {
    let mut result = Check::default();
    let lookup = |name: &str| {
        env.iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };
    for (name, _) in env {
        if let Some(prefix) = FORBIDDEN_PREFIXES
            .iter()
            .find(|prefix| name.starts_with(*prefix))
        {
            result
                .violations
                .push(format!("{name}: запрещённый префикс {prefix}"));
        } else if !INHERITED.contains(&name.as_str())
            && !VARIANT_VARS.contains(&name.as_str())
            && !LANGUAGE_VARS.contains(&name.as_str())
            && name != STATE_DIR
        {
            result.extra.push(name.clone());
        }
    }
    match route {
        Route::Direct => {
            if lookup(LANG) != Some(LANG_VALUE) {
                result.violations.push(format!(
                    "LANG={}: прямой маршрут требует {LANG_VALUE}",
                    lookup(LANG).unwrap_or("<нет>")
                ));
            }
            if lookup("LANGUAGE").is_some() {
                result
                    .violations
                    .push("LANGUAGE: вне белого списка прямого маршрута".to_owned());
            }
        }
        Route::LaunchServices => {
            for name in LANGUAGE_VARS {
                if let Some(value) = lookup(name) {
                    result.violations.push(format!(
                        "{name}={value}: маршрут LaunchServices идёт без переменных языка"
                    ));
                }
            }
        }
    }
    if lookup(STATE_DIR).is_none_or(str::is_empty) {
        result
            .violations
            .push(format!("{STATE_DIR}: нет временного каталога состояния"));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parent(name: &str) -> Option<String> {
        let value = match name {
            "HOME" => "/Users/test",
            "USER" | "LOGNAME" => "test",
            "PATH" => "/usr/bin:/bin",
            "TMPDIR" => "/var/folders/xx/T/",
            "SHELL" => "/bin/zsh",
            "GHOSTTY_RESOURCES_DIR" => "/Applications/Ghostty.app/Contents/Resources/ghostty",
            "TERM" => "xterm-ghostty",
            "TERMINFO" => "/Applications/Ghostty.app/Contents/Resources/terminfo",
            "LANG" => "ru_RU.UTF-8",
            "LC_ALL" => "ru_RU.UTF-8",
            "AGTERM_ENABLED" => "1",
            "NVIM" => "/tmp/nvim.sock",
            "MallocScribble" => "1",
            _ => return None,
        };
        Some(value.to_owned())
    }

    fn names(env: &[(String, String)]) -> Vec<&str> {
        env.iter().map(|(name, _)| name.as_str()).collect()
    }

    fn value<'a>(env: &'a [(String, String)], name: &str) -> Option<&'a str> {
        env.iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    #[test]
    fn direct_route_takes_whitelist_and_explicit_lang() {
        let env = build(parent, Route::Direct, "/tmp/state.x", &[]).unwrap();
        assert_eq!(
            names(&env),
            [
                "HOME", "USER", "LOGNAME", "PATH", "TMPDIR", "SHELL", STATE_DIR, LANG
            ]
        );
        assert_eq!(value(&env, LANG), Some(LANG_VALUE));
        assert_eq!(value(&env, STATE_DIR), Some("/tmp/state.x"));
        assert_eq!(value(&env, "GHOSTTY_RESOURCES_DIR"), None);
        assert_eq!(value(&env, "MallocScribble"), None, "только через --set");
        assert_eq!(check(&env, Route::Direct), Check::default());
    }

    #[test]
    fn launch_services_route_has_no_language() {
        let env = build(parent, Route::LaunchServices, "/tmp/state.y", &[]).unwrap();
        assert_eq!(value(&env, LANG), None);
        assert_eq!(value(&env, "LANGUAGE"), None);
        assert!(!names(&env).iter().any(|name| name.starts_with("LC_")));
        assert_eq!(check(&env, Route::LaunchServices), Check::default());
    }

    #[test]
    fn variant_variable_passes_through() {
        let variant = [("MallocScribble".to_owned(), "1".to_owned())];
        let env = build(parent, Route::Direct, "/tmp/s", &variant).unwrap();
        assert_eq!(value(&env, "MallocScribble"), Some("1"));
        assert_eq!(check(&env, Route::Direct), Check::default());
        assert_eq!(
            build(
                parent,
                Route::Direct,
                "/tmp/s",
                &[("LANG".into(), "C".into())]
            ),
            Err(EnvError::NotVariant("LANG".to_owned()))
        );
    }

    #[test]
    fn missing_required_variable_is_an_error() {
        let without_home = |name: &str| (name != "HOME").then(|| parent(name)).flatten();
        assert_eq!(
            build(without_home, Route::Direct, "/tmp/s", &[]),
            Err(EnvError::Missing("HOME"))
        );
        let minimal = |name: &str| match name {
            "HOME" | "PATH" => parent(name),
            _ => None,
        };
        let env = build(minimal, Route::Direct, "/tmp/s", &[]).unwrap();
        assert_eq!(names(&env), ["HOME", "PATH", STATE_DIR, LANG]);
    }

    #[test]
    fn check_flags_inherited_terminal_variables() {
        let env: Vec<(String, String)> = [
            "HOME",
            "PATH",
            "GHOSTTY_RESOURCES_DIR",
            "TERM",
            "TERMINFO",
            "LC_ALL",
            "AGTERM_ENABLED",
            "NVIM",
            "LANG",
        ]
        .into_iter()
        .map(|name| (name.to_owned(), parent(name).unwrap_or_default()))
        .chain([(STATE_DIR.to_owned(), "/tmp/s".to_owned())])
        .collect();
        let result = check(&env, Route::Direct);
        let flagged: Vec<&str> = result
            .violations
            .iter()
            .map(|line| line.split(':').next().unwrap_or_default())
            .collect();
        assert_eq!(
            flagged,
            [
                "GHOSTTY_RESOURCES_DIR",
                "TERM",
                "TERMINFO",
                "LC_ALL",
                "AGTERM_ENABLED",
                "NVIM",
                "LANG=ru_RU.UTF-8"
            ]
        );
        assert!(result.extra.is_empty());
    }

    #[test]
    fn check_on_launch_services_route() {
        let base = |extra: &[(&str, &str)]| -> Vec<(String, String)> {
            [("HOME", "/h"), ("PATH", "/p"), (STATE_DIR, "/tmp/s")]
                .iter()
                .chain(extra)
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect()
        };
        let added = base(&[("__CFBundleIdentifier", "x"), ("XPC_FLAGS", "0x0")]);
        let result = check(&added, Route::LaunchServices);
        assert!(result.violations.is_empty(), "{result:?}");
        assert_eq!(result.extra, ["__CFBundleIdentifier", "XPC_FLAGS"]);
        let with_lang = check(&base(&[("LANG", "en_US.UTF-8")]), Route::LaunchServices);
        assert_eq!(with_lang.violations.len(), 1);
        assert!(with_lang.violations[0].starts_with("LANG="));
        let without_state = check(&[("LANG".into(), LANG_VALUE.into())], Route::Direct);
        assert!(
            without_state
                .violations
                .iter()
                .any(|line| line.starts_with(STATE_DIR))
        );
    }
}
