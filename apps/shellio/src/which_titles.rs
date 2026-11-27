//! Названия продолжений which-key по командам прототипа (`commands.js:40`–`:141`). Привязка без
//! названия скрыта, как `hidden` прототипа: размеры, вкладки, режим и движения списка.

use serde_json::Value;
use shellio_keymap::Binding;
use shellio_plugin_api::Registry;

/// Действия хоста с названием.
const HOST: [(&str, &str); 20] = [
    ("palette.open", "Поиск и команды"),
    ("panel.focus.left", "Фокус влево"),
    ("panel.focus.down", "Фокус вниз"),
    ("panel.focus.up", "Фокус вверх"),
    ("panel.focus.right", "Фокус вправо"),
    // Перенос `Ctrl-w H|J|K|L` прототипа — в зону слева, снизу, центр, справа (`commands.js:115`).
    ("panel.move.left", "В зону: слева"),
    ("panel.move.down", "В зону: снизу"),
    ("panel.move.up", "В зону: центр"),
    ("panel.move.right", "В зону: справа"),
    ("panel.move_part.next", "В соседнюю часть"),
    ("panel.close", "Закрыть панель"),
    ("layout.only", "Только эта панель"),
    ("layout.equal", "Выровнять размеры"),
    ("layout.zoom", "Развернуть панель"),
    ("layout.toggle.navigator", "Показать или скрыть навигацию"),
    ("layout.toggle.bottom", "Показать или скрыть нижнюю зону"),
    ("layout.toggle.side", "Показать или скрыть правую зону"),
    ("ui.appearance.cycle", "Сменить тему"),
    ("app.quit", "Выйти из shellio"),
    ("mode.toggle", "Переключить режим"),
];

const PANEL_OPEN: &str = "panel.open.";

/// Название продолжения: хост, деление по оси, «Открыть: <название>» плагина реестра (как строка
/// палитры того же действия) и действие плагина с названием палитры.
pub fn title(binding: &Binding, registry: &Registry) -> Option<String> {
    let action = binding.action.as_str();
    if action == "layout.split" {
        let axis = binding.args.as_ref().and_then(|args| args.get("axis"));
        return match axis.and_then(Value::as_str) {
            Some("row") => Some("Разделить зону: части рядом".to_owned()),
            Some("stack") => Some("Разделить зону: части одна над другой".to_owned()),
            _ => None,
        };
    }
    if let Some((_, title)) = HOST.iter().find(|(name, _)| *name == action) {
        return Some((*title).to_owned());
    }
    let manifests = || registry.iter().map(|plugin| plugin.manifest());
    if let Some(plugin) = action.strip_prefix(PANEL_OPEN) {
        return manifests()
            .find(|manifest| manifest.id == plugin)
            .map(|manifest| format!("Открыть: {}", manifest.title));
    }
    manifests()
        .flat_map(|manifest| manifest.commands.iter())
        .find(|spec| spec.name == action && !spec.title.is_empty())
        .map(|spec| spec.title.to_owned())
}

#[cfg(test)]
mod tests {
    use shellio_keymap::{Chord, Keymap, Mode, which_key};

    use super::*;

    fn items(prefix: &str) -> Vec<(String, String, bool)> {
        let registry = crate::plugins::registry().unwrap();
        let keymap = Keymap::defaults(&[]).unwrap();
        let pending: Vec<Chord> = prefix
            .split(' ')
            .map(|chord| Chord::parse(chord).unwrap())
            .collect();
        let contexts = Keymap::contexts(Mode::Navigation, Some("terminal"));
        let found = which_key(&keymap, &contexts, &pending, &|binding| {
            title(binding, &registry)
        })
        .unwrap();
        found
            .items
            .into_iter()
            .map(|item| (item.key, item.label, item.group))
            .collect()
    }

    fn row(key: &str, label: &str, group: bool) -> (String, String, bool) {
        (key.to_owned(), label.to_owned(), group)
    }

    #[test]
    fn leader_continuations_follow_the_prototype() {
        assert_eq!(
            items("Space"),
            [
                row("Space", "Поиск и команды", false),
                row("t", "Открыть: Терминал", false),
                row("z", "Зоны", true),
            ]
        );
        assert_eq!(
            items("Space z"),
            [
                row("h", "Показать или скрыть навигацию", false),
                row("j", "Показать или скрыть нижнюю зону", false),
                row("l", "Показать или скрыть правую зону", false),
            ]
        );
    }

    #[test]
    fn ctrl_w_continuations_follow_the_prototype_and_hide_sizes() {
        let labels: Vec<(String, String)> = items("Ctrl-w")
            .into_iter()
            .map(|(key, label, group)| {
                assert!(!group);
                (key, label)
            })
            .collect();
        let expected = [
            ("h", "Фокус влево"),
            ("H", "В зону: слева"),
            ("j", "Фокус вниз"),
            ("J", "В зону: снизу"),
            ("k", "Фокус вверх"),
            ("K", "В зону: центр"),
            ("l", "Фокус вправо"),
            ("L", "В зону: справа"),
            ("v", "Разделить зону: части рядом"),
            ("s", "Разделить зону: части одна над другой"),
            ("q", "Закрыть панель"),
            ("o", "Только эта панель"),
            ("=", "Выровнять размеры"),
            ("z", "Развернуть панель"),
        ];
        let expected: Vec<(String, String)> = expected
            .iter()
            .map(|(key, label)| ((*key).to_owned(), (*label).to_owned()))
            .collect();
        assert_eq!(labels, expected);
    }

    #[test]
    fn remapped_continuations_keep_the_title_of_their_action() {
        let registry = crate::plugins::registry().unwrap();
        let file = "[navigation]\n\"Ctrl-w m\" = \"layout.zoom\"\n\"Ctrl-w z\" = false\n\
                    \"<leader>t\" = \"panel.open.demo-list\"\n";
        let any = |_: &shellio_keymap::Context,
                   _: &str,
                   _: Option<&Value>|
         -> Result<(), shellio_keymap::ActionError> { Ok(()) };
        let keymap = Keymap::defaults(&[])
            .unwrap()
            .apply(Some(file), &[], &any)
            .unwrap();
        let contexts = Keymap::contexts(Mode::Navigation, Some("terminal"));
        let found = |prefix: &str| {
            let pending: Vec<Chord> = prefix
                .split(' ')
                .map(|chord| Chord::parse(chord).unwrap())
                .collect();
            which_key(&keymap, &contexts, &pending, &|binding| {
                title(binding, &registry)
            })
            .unwrap()
            .items
            .into_iter()
            .map(|item| (item.key, item.label, item.group))
            .collect::<Vec<_>>()
        };
        let ctrl_w = found("Ctrl-w");
        assert!(ctrl_w.contains(&row("m", "Развернуть панель", false)));
        assert!(ctrl_w.iter().all(|(key, _, _)| key != "z"));
        // Строка файла встаёт после строк таблицы по умолчанию: порядок продолжений — порядок таблицы.
        assert_eq!(
            found("Space"),
            [
                row("Space", "Поиск и команды", false),
                row("z", "Зоны", true),
                row("t", "Открыть: Демо: список", false),
            ]
        );
    }

    #[test]
    fn plugin_commands_use_their_palette_title() {
        let registry = crate::plugins::registry().unwrap();
        let keymap = Keymap::defaults(&[]).unwrap();
        let binding = keymap
            .bindings()
            .iter()
            .find(|binding| binding.action == "palette.open")
            .unwrap();
        let mut set = binding.clone();
        set.action = "demo-native.set".to_owned();
        assert_eq!(title(&set, &registry).as_deref(), Some("Применить поле"));
        set.action = "panel.open.demo-list".to_owned();
        assert_eq!(
            title(&set, &registry).as_deref(),
            Some("Открыть: Демо: список")
        );
        set.action = "panel.open.none".to_owned();
        assert_eq!(title(&set, &registry), None);
        set.action = "layout.grow".to_owned();
        assert_eq!(title(&set, &registry), None);
    }
}
