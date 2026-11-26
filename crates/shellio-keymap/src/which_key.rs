//! Подсказка which-key (ADR-0010, «Уточнено в P1.6 (планирование): наложения»): набранный
//! префикс, название группы и продолжения из действующей таблицы. Своего ввода у неё нет:
//! продолжение получает маршрутизатор.

use std::time::Duration;

use serde::Serialize;

use crate::chord::Chord;
use crate::keymap::{Binding, Context, Keymap, Lookup};

/// Задержка появления после лидера и `Ctrl-w`.
pub const WHICH_KEY_SHORT: Duration = Duration::from_millis(90);
/// Задержка появления после других префиксов.
pub const WHICH_KEY_LONG: Duration = Duration::from_millis(380);

/// Название группы без своего названия.
const OTHER: &str = "Ещё";

fn ctrl_w() -> Option<Chord> {
    Chord::parse("Ctrl-w").ok()
}

/// Через сколько показать подсказку набранного префикса; без префикса — никогда. Первый аккорд
/// сравнивается с лидером таблицы, а не с буквальным `Space`.
pub fn which_key_delay(pending: &[Chord], leader: Chord) -> Option<Duration> {
    let first = *pending.first()?;
    Some(if first == leader || Some(first) == ctrl_w() {
        WHICH_KEY_SHORT
    } else {
        WHICH_KEY_LONG
    })
}

/// Название группы префикса (прототип `S.keyGroups`, `keyboard.js:35`), лидер — из таблицы.
pub fn group_title(leader: Chord, prefix: &[Chord]) -> Option<&'static str> {
    let rest: Vec<String> = prefix.iter().skip(1).map(Chord::to_string).collect();
    let rest: Vec<&str> = rest.iter().map(String::as_str).collect();
    let first = *prefix.first()?;
    if first == leader {
        return match rest.as_slice() {
            [] => Some("Команды"),
            ["g"] => Some("Git"),
            ["r"] => Some("Ревью"),
            ["w"] => Some("Рабочая область"),
            ["z"] => Some("Зоны"),
            _ => None,
        };
    }
    if !rest.is_empty() {
        return None;
    }
    if Some(first) == ctrl_w() {
        return Some("Панели и зоны");
    }
    match first.to_string().as_str() {
        "g" => Some("Переход"),
        "]" => Some("Вперёд"),
        "[" => Some("Назад"),
        _ => None,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WhichItem {
    /// Следующий аккорд.
    pub key: String,
    pub label: String,
    /// Группа: за аккордом есть более длинные последовательности.
    pub group: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WhichKey {
    /// Набранные аккорды по одному.
    pub prefix: Vec<String>,
    pub title: Option<String>,
    pub items: Vec<WhichItem>,
}

/// Продолжения набранного `pending` в контекстах `contexts` сверху вниз, как их решит
/// маршрутизатор: аккорд, после которого таблица ждёт ещё, — группа, иначе конечная команда с
/// названием из `title` или именем действия. Порядок — первое появление аккорда по контекстам и
/// строкам таблицы. `None` — префикса нет или продолжать нечем.
pub fn which_key(
    keymap: &Keymap,
    contexts: &[Context],
    pending: &[Chord],
    title: &dyn Fn(&Binding) -> Option<String>,
) -> Option<WhichKey> {
    if pending.is_empty() {
        return None;
    }
    let mut next: Vec<Chord> = Vec::new();
    for context in contexts {
        for binding in keymap.bindings() {
            let chords = binding.sequence.chords();
            if binding.context == *context
                && chords.len() > pending.len()
                && chords.starts_with(pending)
                && !next.contains(&chords[pending.len()])
            {
                next.push(chords[pending.len()]);
            }
        }
    }
    let leader = keymap.leader();
    let items: Vec<WhichItem> = next
        .into_iter()
        .filter_map(|chord| {
            let typed: Vec<Chord> = pending.iter().copied().chain([chord]).collect();
            let (label, group) = match keymap.lookup(contexts, &typed) {
                Lookup::Exact(binding) => (
                    title(binding).unwrap_or_else(|| binding.action.clone()),
                    false,
                ),
                Lookup::Prefix => (
                    group_title(leader, &typed).unwrap_or(OTHER).to_owned(),
                    true,
                ),
                Lookup::None => return None,
            };
            Some(WhichItem {
                key: chord.to_string(),
                label,
                group,
            })
        })
        .collect();
    (!items.is_empty()).then(|| WhichKey {
        prefix: pending.iter().map(Chord::to_string).collect(),
        title: group_title(leader, pending).map(str::to_owned),
        items,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::defaults::{KeyDefault, PanelKeys};
    use crate::keymap::Mode;
    use crate::sequence::Sequence;
    use crate::toml::ActionError;

    fn chords(keymap: &Keymap, text: &str) -> Vec<Chord> {
        Sequence::parse(text, keymap.leader())
            .unwrap()
            .chords()
            .to_vec()
    }

    fn titles(binding: &Binding) -> Option<String> {
        let title = match binding.action.as_str() {
            "layout.toggle.navigator" => "Навигация",
            "layout.toggle.bottom" => "Нижняя зона",
            "layout.toggle.side" => "Правая зона",
            "layout.zoom" => "Развернуть панель",
            "panel.focus.left" => "Фокус влево",
            "layout.split" => match binding.args.as_ref().and_then(|a| a.get("axis")) {
                Some(Value::String(axis)) if axis == "row" => "Разделить: рядом",
                _ => "Разделить: одна над другой",
            },
            _ => return None,
        };
        Some(title.to_owned())
    }

    fn items(found: &WhichKey) -> Vec<(&str, &str, bool)> {
        found
            .items
            .iter()
            .map(|item| (item.key.as_str(), item.label.as_str(), item.group))
            .collect()
    }

    fn nav() -> Vec<Context> {
        Keymap::contexts(Mode::Navigation, Some("demo-native"))
    }

    #[test]
    fn delay_is_short_after_the_leader_and_ctrl_w() {
        let keymap = Keymap::defaults(&[]).unwrap();
        let leader = keymap.leader();
        let delay = |text| which_key_delay(&chords(&keymap, text), leader);
        assert_eq!(delay("Space"), Some(WHICH_KEY_SHORT));
        assert_eq!(delay("Space z"), Some(WHICH_KEY_SHORT));
        assert_eq!(delay("Ctrl-w"), Some(WHICH_KEY_SHORT));
        assert_eq!(delay("g"), Some(WHICH_KEY_LONG));
        assert_eq!(which_key_delay(&[], leader), None);
        assert_eq!(WHICH_KEY_SHORT.as_millis(), 90);
        assert_eq!(WHICH_KEY_LONG.as_millis(), 380);
        // Лидер переназначен: короткая задержка у нового лидера, у `Space` — длинная.
        let comma = Chord::parse(",").unwrap();
        assert_eq!(which_key_delay(&[comma], comma), Some(WHICH_KEY_SHORT));
        assert_eq!(
            which_key_delay(&[Chord::parse("Space").unwrap()], comma),
            Some(WHICH_KEY_LONG)
        );
    }

    #[test]
    fn leader_shows_the_zones_group_and_its_commands() {
        let keymap = Keymap::defaults(&[]).unwrap();
        let leader = which_key(&keymap, &nav(), &chords(&keymap, "Space"), &titles).unwrap();
        assert_eq!(leader.prefix, ["Space"]);
        assert_eq!(leader.title.as_deref(), Some("Команды"));
        assert_eq!(items(&leader), [("z", "Зоны", true)]);

        let zones = which_key(&keymap, &nav(), &chords(&keymap, "Space z"), &titles).unwrap();
        assert_eq!(zones.prefix, ["Space", "z"]);
        assert_eq!(zones.title.as_deref(), Some("Зоны"));
        assert_eq!(
            items(&zones),
            [
                ("h", "Навигация", false),
                ("j", "Нижняя зона", false),
                ("l", "Правая зона", false),
            ]
        );
    }

    #[test]
    fn ctrl_w_lists_the_layout_continuations_in_table_order() {
        let keymap = Keymap::defaults(&[]).unwrap();
        let found = which_key(&keymap, &nav(), &chords(&keymap, "Ctrl-w"), &titles).unwrap();
        assert_eq!(found.title.as_deref(), Some("Панели и зоны"));
        let keys: Vec<&str> = found.items.iter().map(|item| item.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "h", "j", "k", "l", "H", "J", "K", "L", "v", "s", "q", "o", "=", "z", "+", "-",
                ">", "<"
            ]
        );
        assert!(found.items.iter().all(|item| !item.group));
        let item = |key: &str| found.items.iter().find(|item| item.key == key).unwrap();
        assert_eq!(item("v").label, "Разделить: рядом");
        assert_eq!(item("s").label, "Разделить: одна над другой");
        assert_eq!(item("z").label, "Развернуть панель");
        // Без названия — имя действия.
        assert_eq!(item("q").label, "panel.close");
    }

    #[test]
    fn upper_context_decides_the_continuation() {
        let panels = [PanelKeys {
            plugin: "demo-list".to_owned(),
            list_keys: true,
            defaults: vec![KeyDefault {
                sequence: "g x".to_owned(),
                action: "demo-list.extra".to_owned(),
            }],
        }];
        let keymap = Keymap::defaults(&panels).unwrap();
        let list = Keymap::contexts(Mode::Navigation, Some("demo-list"));
        let found = which_key(&keymap, &list, &chords(&keymap, "g"), &titles).unwrap();
        assert_eq!(found.title.as_deref(), Some("Переход"));
        assert_eq!(
            items(&found),
            [("g", "list.top", false), ("x", "demo-list.extra", false)]
        );
        // В панели без набора списка `g` не префикс.
        assert_eq!(
            which_key(&keymap, &nav(), &chords(&keymap, "g"), &titles),
            None
        );
        // В `Input` префиксов `navigation` нет.
        let input = Keymap::contexts(Mode::Input, Some("demo-list"));
        assert_eq!(
            which_key(&keymap, &input, &chords(&keymap, "Ctrl-w"), &titles),
            None
        );
        assert_eq!(which_key(&keymap, &list, &[], &titles), None);
    }

    #[test]
    fn remapped_keys_follow_the_effective_table() {
        let file = "[navigation]\n\"Ctrl-w m\" = \"layout.zoom\"\n\"Ctrl-w z\" = false\n";
        let any = |_: &Context, _: &str, _: Option<&Value>| -> Result<(), ActionError> { Ok(()) };
        let keymap = Keymap::defaults(&[])
            .unwrap()
            .apply(Some(file), &[], &any)
            .unwrap();
        let found = which_key(&keymap, &nav(), &chords(&keymap, "Ctrl-w"), &titles).unwrap();
        let item = found.items.iter().find(|item| item.key == "m").unwrap();
        assert_eq!(item.label, "Развернуть панель");
        assert!(!item.group);
        assert!(found.items.iter().all(|item| item.key != "z"));
    }

    #[test]
    fn group_names_follow_the_leader() {
        let comma = Chord::parse(",").unwrap();
        let keymap = Keymap::defaults_with(comma, &[]).unwrap();
        let found = which_key(&keymap, &nav(), &chords(&keymap, ","), &titles).unwrap();
        assert_eq!(found.prefix, [","]);
        assert_eq!(found.title.as_deref(), Some("Команды"));
        assert_eq!(items(&found), [("z", "Зоны", true)]);
        let space = Chord::parse("Space").unwrap();
        assert_eq!(group_title(comma, &[space]), None);
        assert_eq!(group_title(comma, &[]), None);
        // Префикс без названия — группа «Ещё».
        let deep = [PanelKeys {
            plugin: "demo-native".to_owned(),
            list_keys: false,
            defaults: vec![KeyDefault {
                sequence: "<leader>q x y".to_owned(),
                action: "demo-native.deep".to_owned(),
            }],
        }];
        let keymap = Keymap::defaults(&deep).unwrap();
        let found = which_key(&keymap, &nav(), &chords(&keymap, "Space"), &titles).unwrap();
        assert_eq!(items(&found), [("q", "Ещё", true), ("z", "Зоны", true)]);
        let found = which_key(&keymap, &nav(), &chords(&keymap, "Space q"), &titles).unwrap();
        assert_eq!(found.title, None);
        assert_eq!(
            serde_json::to_value(&found).unwrap()["items"],
            json!([
                {"key": "x", "label": "Ещё", "group": true}
            ])
        );
    }
}
