//! Командная строка `:` (ADR-0010, «Уточнено в P1.6 (планирование): наложения»): грамматика
//! прототипа (`overlays.js:488`–`:500`) с `theme system` и нечёткий откат на строки команд
//! палитры. `go` — шаг P1.7.

use crate::fuzzy::score;
use crate::palette::{Axis, Group, PaletteRow};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Theme {
    Light,
    Dark,
    System,
}

/// Команда грамматики: приложение вызывает обработчик её действия.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmdlineCommand {
    /// `q`, `close`: закрыть панель с фокусом.
    Close,
    /// `zoom`.
    Zoom,
    /// `vs`, `vsplit` — части рядом, `sp`, `split` — одна над другой.
    Split(Axis),
    /// `theme light|dark|system`.
    Theme(Theme),
}

/// Что выполнить по набранному тексту.
#[derive(Clone, Debug, PartialEq)]
pub enum Cmdline<'a> {
    /// Пустая строка: ничего.
    Empty,
    Command(CmdlineCommand),
    /// Лучшая по нечёткому совпадению строка команд палитры.
    Fallback(&'a PaletteRow),
    /// Ни грамматика, ни откат: текст для сообщения в строке статуса.
    Unknown(String),
}

/// Команда грамматики. Лишние слова и неизвестная тема грамматикой не разбираются.
pub fn parse(text: &str) -> Option<CmdlineCommand> {
    let words: Vec<&str> = text.split_whitespace().collect();
    match words.as_slice() {
        ["q" | "close"] => Some(CmdlineCommand::Close),
        ["zoom"] => Some(CmdlineCommand::Zoom),
        ["vs" | "vsplit"] => Some(CmdlineCommand::Split(Axis::Row)),
        ["sp" | "split"] => Some(CmdlineCommand::Split(Axis::Stack)),
        ["theme", "light"] => Some(CmdlineCommand::Theme(Theme::Light)),
        ["theme", "dark"] => Some(CmdlineCommand::Theme(Theme::Dark)),
        ["theme", "system"] => Some(CmdlineCommand::Theme(Theme::System)),
        _ => None,
    }
}

/// Разбор строки `text` с откатом на строки группы «Команды» из `rows` по названию и имени
/// действия; при равной оценке побеждает строка раньше по порядку.
pub fn resolve<'a>(text: &str, rows: &'a [PaletteRow]) -> Cmdline<'a> {
    let text = text.trim();
    if text.is_empty() {
        return Cmdline::Empty;
    }
    if let Some(command) = parse(text) {
        return Cmdline::Command(command);
    }
    let mut best: Option<(u32, &PaletteRow)> = None;
    for row in rows.iter().filter(|row| row.group == Group::Commands) {
        let found = score(&format!("{} {}", row.title, row.action), text);
        if found > 0 && best.is_none_or(|(top, _)| found > top) {
            best = Some((found, row));
        }
    }
    match best {
        Some((_, row)) => Cmdline::Fallback(row),
        None => Cmdline::Unknown(text.to_owned()),
    }
}

/// Сообщение строки статуса о неизвестной команде. `key` — запись действующей привязки
/// `palette.open` для показа; без привязки называется команда без клавиши.
pub fn unknown_message(text: &str, key: Option<&str>) -> String {
    match key {
        Some(key) => format!("Неизвестная команда: {text}. Откройте поиск {key}"),
        None => format!("Неизвестная команда: {text}. Откройте поиск"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, group: Group, title: &str, action: &str) -> PaletteRow {
        PaletteRow {
            id: id.to_owned(),
            group,
            title: title.to_owned(),
            detail: String::new(),
            keys: None,
            action: action.to_owned(),
            args: None,
        }
    }

    fn rows() -> Vec<PaletteRow> {
        vec![
            row(
                "mode.toggle",
                Group::Commands,
                "Режим «Ввод»",
                "mode.toggle",
            ),
            row(
                "ui.appearance.cycle",
                Group::Commands,
                "Тёмная тема",
                "ui.appearance.cycle",
            ),
            row(
                "layout.toggle.bottom",
                Group::Commands,
                "Скрыть нижнюю зону",
                "layout.toggle.bottom",
            ),
            row(
                "layout.equal",
                Group::Commands,
                "Выровнять размеры",
                "layout.equal",
            ),
            row(
                "open:terminal",
                Group::Panels,
                "Открыть: Терминал",
                "panel.open.terminal",
            ),
        ]
    }

    #[test]
    fn grammar_names_the_commands() {
        assert_eq!(parse("q"), Some(CmdlineCommand::Close));
        assert_eq!(parse("close"), Some(CmdlineCommand::Close));
        assert_eq!(parse("zoom"), Some(CmdlineCommand::Zoom));
        assert_eq!(parse("vs"), Some(CmdlineCommand::Split(Axis::Row)));
        assert_eq!(parse("vsplit"), Some(CmdlineCommand::Split(Axis::Row)));
        assert_eq!(parse("sp"), Some(CmdlineCommand::Split(Axis::Stack)));
        assert_eq!(parse("split"), Some(CmdlineCommand::Split(Axis::Stack)));
        assert_eq!(
            parse("theme light"),
            Some(CmdlineCommand::Theme(Theme::Light))
        );
        assert_eq!(
            parse("theme  dark "),
            Some(CmdlineCommand::Theme(Theme::Dark))
        );
        assert_eq!(
            parse("theme system"),
            Some(CmdlineCommand::Theme(Theme::System))
        );
        for text in [
            "",
            "theme",
            "theme blue",
            "q now",
            "zoom 2",
            "go main",
            "Q",
            "vs x",
        ] {
            assert_eq!(parse(text), None, "{text}");
        }
    }

    #[test]
    fn resolve_runs_grammar_then_the_fuzzy_fallback() {
        let rows = rows();
        assert_eq!(resolve("", &rows), Cmdline::Empty);
        assert_eq!(resolve("   ", &rows), Cmdline::Empty);
        assert_eq!(
            resolve(" zoom ", &rows),
            Cmdline::Command(CmdlineCommand::Zoom)
        );
        // Откат идёт по всему тексту, а не по первому слову.
        assert_eq!(resolve("нижнюю зону", &rows), Cmdline::Fallback(&rows[2]));
        assert_eq!(resolve("тёмная", &rows), Cmdline::Fallback(&rows[1]));
        // Имя действия тоже участвует.
        assert_eq!(resolve("layout.equal", &rows), Cmdline::Fallback(&rows[3]));
        // Подстрока ближе к началу выше рассеянного совпадения.
        assert_eq!(resolve("режим", &rows), Cmdline::Fallback(&rows[0]));
        // Строки панелей в откат не входят.
        assert_eq!(
            resolve("терминал", &rows),
            Cmdline::Unknown("терминал".to_owned())
        );
        assert_eq!(
            resolve(" нет такой ", &rows),
            Cmdline::Unknown("нет такой".to_owned())
        );
        assert_eq!(
            resolve("go main", &[]),
            Cmdline::Unknown("go main".to_owned())
        );
        assert_eq!(
            resolve("theme blue", &[]),
            Cmdline::Unknown("theme blue".to_owned())
        );
    }

    #[test]
    fn equal_scores_keep_the_earlier_row() {
        let rows = vec![
            row("a", Group::Commands, "Развернуть", "a"),
            row("b", Group::Commands, "Развернуть", "b"),
        ];
        assert_eq!(resolve("развернуть", &rows), Cmdline::Fallback(&rows[0]));
    }

    #[test]
    fn unknown_message_names_the_palette_key() {
        assert_eq!(
            unknown_message("нет такой", Some("⌘K")),
            "Неизвестная команда: нет такой. Откройте поиск ⌘K"
        );
        assert_eq!(
            unknown_message("нет такой", None),
            "Неизвестная команда: нет такой. Откройте поиск"
        );
    }
}
