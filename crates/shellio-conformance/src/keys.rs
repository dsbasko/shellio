//! Таблица шаблонов ключей сверки (ADR-0037, «Ключ сверки»).
//!
//! Ключ — путь по смыслу из сегментов через точку, без названий, процессов и порядковых id.
//! Номер части, вкладки, пункта — порядок на экране, с единицы. Псевдоэлемент — ключ элемента с
//! суффиксом `::before` или `::after`. Таблица одна: ей следуют `data-ck` прототипа и поле `ck`
//! узлов `ui.inspect` продукта.
//!
//! Подстановки шаблона: `{zone}` — `left|right|center|bottom`, `{n}` — номер с единицы без
//! ведущего нуля, `{button}` — кнопка окна `close|minimize|zoom`, `{tool}` — контрол строки
//! вкладок `split|zoom|restore|menu`.
//!
//! Шаблон ключа в исключении реестра ([`glob_matches`]) — другое: `*` в нём заменяет один или
//! несколько сегментов, например `*.menu.item.*`.

use std::collections::BTreeSet;

/// Таблица шаблонов ключей сверки.
pub const TEMPLATES: &[&str] = &[
    // Заголовок окна.
    "chrome",
    "chrome.window.{button}",
    "chrome.workspace",
    "chrome.workspace.label",
    "chrome.workspace.count",
    "chrome.workspace.chevron",
    "chrome.search",
    "chrome.search.icon",
    "chrome.search.label",
    "chrome.search.kbd",
    "chrome.zones",
    "chrome.zones.{zone}",
    "chrome.zones.{zone}.icon",
    "chrome.appearance",
    "chrome.appearance.icon",
    // Строка статуса.
    "status",
    "status.mode",
    "status.message",
    "status.hint.{n}",
    "status.hint.{n}.key",
    "status.hint.{n}.key.{n}",
    "status.hint.{n}.label",
    "status.load",
    "status.load.label",
    "status.load.value",
    // Рабочая область и зоны.
    "workbench",
    "workbench.empty",
    "workbench.empty.title",
    "workbench.empty.hint",
    "workbench.empty.action.{n}",
    "workbench.empty.action.{n}.icon",
    "workbench.empty.action.{n}.label",
    "zone.{zone}",
    "zone.{zone}.divider.{n}",
    "zone.{zone}.part.{n}",
    "zone.{zone}.part.{n}.tabs",
    "zone.{zone}.part.{n}.tabs.tab.{n}",
    "zone.{zone}.part.{n}.tabs.tab.{n}.icon",
    "zone.{zone}.part.{n}.tabs.tab.{n}.label",
    "zone.{zone}.part.{n}.tabs.tab.{n}.close",
    "zone.{zone}.part.{n}.tabs.tab.{n}.close.icon",
    "zone.{zone}.part.{n}.tabs.{tool}",
    "zone.{zone}.part.{n}.tabs.{tool}.icon",
    "zone.{zone}.part.{n}.body",
    "zone.{zone}.part.{n}.body.stub",
    "zone.{zone}.part.{n}.body.stub.label",
    "zone.{zone}.part.{n}.body.stub.expand",
    "zone.{zone}.part.{n}.body.stub.expand.label",
    "zone.{zone}.part.{n}.body.empty",
    "zone.{zone}.part.{n}.body.empty.icon",
    "zone.{zone}.part.{n}.body.empty.title",
    "zone.{zone}.part.{n}.body.empty.hint",
    "zone.{zone}.part.{n}.body.empty.action.{n}",
    "zone.{zone}.part.{n}.body.empty.action.{n}.label",
    // Строка фильтра хоста над телом панели.
    "zone.{zone}.part.{n}.filter",
    "zone.{zone}.part.{n}.filter.icon",
    "zone.{zone}.part.{n}.filter.input",
    // Меню «…» части.
    "zone.{zone}.part.{n}.menu",
    "zone.{zone}.part.{n}.menu.item.{n}",
    "zone.{zone}.part.{n}.menu.item.{n}.label",
    "zone.{zone}.part.{n}.menu.separator.{n}",
    // Выбор «Открыть в новой части».
    "picker",
    "picker.title",
    "picker.item.{n}",
    "picker.item.{n}.label",
    // Палитра.
    "palette",
    "palette.header",
    "palette.header.icon",
    "palette.query",
    "palette.category",
    "palette.escape",
    "palette.list",
    "palette.group.{n}",
    "palette.group.{n}.title",
    "palette.group.{n}.row.{n}",
    "palette.group.{n}.row.{n}.icon",
    "palette.group.{n}.row.{n}.title",
    "palette.group.{n}.row.{n}.detail",
    "palette.group.{n}.row.{n}.key",
    "palette.group.{n}.row.{n}.key.{n}",
    "palette.empty",
    "palette.footer",
    "palette.footer.hint.{n}",
    "palette.footer.hint.{n}.key",
    "palette.footer.hint.{n}.key.{n}",
    "palette.footer.hint.{n}.label",
    // Подсказка which-key.
    "whichkey",
    "whichkey.prefix",
    "whichkey.prefix.{n}",
    "whichkey.title",
    "whichkey.item.{n}",
    "whichkey.item.{n}.key",
    "whichkey.item.{n}.key.{n}",
    "whichkey.item.{n}.label",
];

const PSEUDO: [&str; 2] = ["::before", "::after"];

fn placeholder_matches(placeholder: &str, segment: &str) -> bool {
    match placeholder {
        "{zone}" => matches!(segment, "left" | "right" | "center" | "bottom"),
        "{n}" => {
            !segment.is_empty()
                && !segment.starts_with('0')
                && segment.bytes().all(|b| b.is_ascii_digit())
        }
        "{button}" => matches!(segment, "close" | "minimize" | "zoom"),
        "{tool}" => matches!(segment, "split" | "zoom" | "restore" | "menu"),
        literal => literal == segment,
    }
}

fn template_matches(template: &str, key: &str) -> bool {
    let mut template = template.split('.');
    let mut key = key.split('.');
    loop {
        match (template.next(), key.next()) {
            (None, None) => return true,
            (Some(t), Some(k)) if placeholder_matches(t, k) => {}
            _ => return false,
        }
    }
}

/// Шаблон таблицы, которому соответствует ключ, с учётом суффикса псевдоэлемента.
pub fn template_of(key: &str) -> Option<&'static str> {
    let base = PSEUDO
        .iter()
        .find_map(|suffix| key.strip_suffix(suffix))
        .unwrap_or(key);
    TEMPLATES
        .iter()
        .copied()
        .find(|template| template_matches(template, base))
}

/// Ключ соответствует таблице шаблонов.
pub fn check_key(key: &str) -> Result<(), String> {
    template_of(key)
        .map(|_| ())
        .ok_or_else(|| format!("ключ «{key}» не соответствует таблице шаблонов"))
}

/// Ключи уникальны.
pub fn check_unique<'a>(keys: impl IntoIterator<Item = &'a str>) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    for key in keys {
        if !seen.insert(key) {
            return Err(format!("ключ «{key}» повторяется"));
        }
    }
    Ok(())
}

/// Шаблон ключа исключения: сегменты через точку, `*` — один или несколько сегментов.
pub fn glob_matches(pattern: &str, key: &str) -> bool {
    let pattern: Vec<&str> = pattern.split('.').collect();
    let key: Vec<&str> = key.split('.').collect();
    glob_from(&pattern, &key)
}

fn glob_from(pattern: &[&str], key: &[&str]) -> bool {
    match pattern.split_first() {
        None => key.is_empty(),
        Some((&"*", rest)) => (1..=key.len()).any(|taken| glob_from(rest, &key[taken..])),
        Some((segment, rest)) => key
            .split_first()
            .is_some_and(|(k, key_rest)| k == segment && glob_from(rest, key_rest)),
    }
}

/// Шаблон ключа исключения правильный: непустые сегменты, `*` — только целым сегментом.
pub fn check_glob(pattern: &str) -> Result<(), String> {
    if pattern
        .split('.')
        .all(|segment| !segment.is_empty() && (segment == "*" || !segment.contains('*')))
    {
        Ok(())
    } else {
        Err(format!("шаблон ключа «{pattern}» неправильный"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_row_keys_belong_to_a_part_and_not_to_its_body() {
        for key in [
            "zone.center.part.1.filter",
            "zone.right.part.2.filter.icon",
            "zone.bottom.part.1.filter.input",
        ] {
            assert_eq!(check_key(key), Ok(()), "{key}");
            assert!(!glob_matches("zone.*.body.*", key), "{key}");
        }
        for key in [
            "zone.center.part.1.body.filter",
            "zone.center.filter",
            "zone.center.part.1.filter.label",
        ] {
            assert!(check_key(key).is_err(), "{key}");
        }
    }

    #[test]
    fn which_key_keys_are_a_floating_plate_of_the_layout_surface() {
        use crate::cdp::kind_of;
        use crate::fields::ElementKind;
        use crate::registry::Surface;
        for key in [
            "whichkey",
            "whichkey.prefix",
            "whichkey.prefix.2",
            "whichkey.title",
            "whichkey.item.3",
            "whichkey.item.1.key",
            "whichkey.item.1.key.2",
            "whichkey.item.14.label",
        ] {
            assert_eq!(check_key(key), Ok(()), "{key}");
            assert!(Surface::Layout.owns(key), "{key}");
        }
        assert!(check_key("whichkey.item.1.title").is_err());
        assert_eq!(kind_of("div", "whichkey"), ElementKind::Plate);
        assert_eq!(kind_of("div", "whichkey.item.1"), ElementKind::Container);
        assert_eq!(kind_of("kbd", "whichkey.item.1.key"), ElementKind::Badge);
        assert_eq!(kind_of("b", "whichkey.item.1.label"), ElementKind::Text);
        assert_eq!(kind_of("span", "whichkey.title"), ElementKind::Text);
    }

    #[test]
    fn palette_keys_belong_to_the_palette_surface() {
        use crate::cdp::kind_of;
        use crate::fields::ElementKind;
        use crate::registry::Surface;
        for key in [
            "palette",
            "palette.query",
            "palette.group.2.title",
            "palette.group.1.row.3.key.2",
            "palette.footer.hint.1.key.2",
            "palette.empty",
        ] {
            assert_eq!(check_key(key), Ok(()), "{key}");
            assert!(Surface::Palette.owns(key), "{key}");
        }
        assert!(check_key("palette.row.1").is_err());
        assert!(!Surface::Palette.owns("status.hint.1"));
        assert!(!Surface::Palette.owns("palettes"));
        assert_eq!(kind_of("div", "palette"), ElementKind::Plate);
        assert_eq!(kind_of("span", "palette.category"), ElementKind::Badge);
        assert_eq!(kind_of("div", "palette.empty"), ElementKind::Text);
        assert_eq!(
            kind_of("span", "palette.footer.hint.2"),
            ElementKind::Container
        );
        assert_eq!(kind_of("div", "palette.group.1.title"), ElementKind::Text);
        assert_eq!(
            kind_of("button", "palette.group.1.row.1"),
            ElementKind::Control
        );
    }
}
