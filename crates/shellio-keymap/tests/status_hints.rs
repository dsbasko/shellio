//! Выбор подсказок строки статуса (ADR-0010, «Уточнено в P1.6 (планирование)», «Строка
//! статуса»): префикс, сообщение, не больше трёх подсказок из действующей таблицы.

// allow-unwrap-in-tests не распространяется на вспомогательные функции интеграционного теста.
#![allow(clippy::unwrap_used)]

use serde_json::Value;
use shellio_keymap::{
    ActionError, Chord, Context, HintQuery, KeyDefault, Keymap, Mode, PanelHint, PanelKeys,
    StatusText, status_hints,
};

fn panels() -> Vec<PanelKeys> {
    let key = |sequence: &str, action: &str| KeyDefault {
        sequence: sequence.to_owned(),
        action: action.to_owned(),
    };
    vec![PanelKeys {
        plugin: "demo-list".to_owned(),
        list_keys: true,
        defaults: vec![
            key("o", "demo-list.details"),
            key("b", "demo-list.back"),
            key("s", "demo-list.select"),
            key("r", "demo-list.reload"),
        ],
    }]
}

fn any(_: &Context, _: &str, _: Option<&Value>) -> Result<(), ActionError> {
    Ok(())
}

fn defaults() -> Keymap {
    Keymap::defaults(&panels()).unwrap()
}

fn with(file: &str) -> Keymap {
    let keymap = defaults().apply(Some(file), &panels(), &any).unwrap();
    assert!(
        keymap.diagnostics().is_empty(),
        "{:?}",
        keymap.diagnostics()
    );
    keymap
}

fn query<'a>() -> HintQuery<'a> {
    HintQuery {
        mode: Mode::Navigation,
        field: false,
        pending: &[],
        message: None,
        plugin: Some("demo-native"),
        panel_hints: &[],
        input_receiver: false,
        palette: false,
    }
}

fn hints(keymap: &Keymap, query: &HintQuery<'_>) -> Vec<(String, String)> {
    match status_hints(keymap, query) {
        StatusText::Hints(hints) => hints
            .into_iter()
            .map(|hint| (hint.keys.join(" "), hint.label))
            .collect(),
        other => panic!("ждали подсказки: {other:?}"),
    }
}

fn pairs(expected: &[(&str, &str)]) -> Vec<(String, String)> {
    expected
        .iter()
        .map(|(keys, label)| ((*keys).to_owned(), (*label).to_owned()))
        .collect()
}

#[test]
fn a_typed_prefix_and_a_message_replace_the_hints() {
    let keymap = defaults();
    let pending = [Chord::parse("Ctrl-w").unwrap()];
    let leader = [Chord::parse("Space").unwrap(), Chord::parse("z").unwrap()];

    let typed = HintQuery {
        pending: &pending,
        message: Some("отказ"),
        ..query()
    };
    assert_eq!(
        status_hints(&keymap, &typed),
        StatusText::Prefix("Ctrl-w".to_owned())
    );
    let typed = HintQuery {
        pending: &leader,
        ..query()
    };
    assert_eq!(
        status_hints(&keymap, &typed),
        StatusText::Prefix("Space z".to_owned())
    );

    let refused = HintQuery {
        message: Some("Правая зона делится одна над другой: Ctrl-w s"),
        mode: Mode::Input,
        ..query()
    };
    assert_eq!(
        status_hints(&keymap, &refused),
        StatusText::Message("Правая зона делится одна над другой: Ctrl-w s".to_owned())
    );
}

#[test]
fn navigation_shows_panels_then_tabs_or_input_then_search() {
    let keymap = defaults();
    assert_eq!(
        hints(&keymap, &query()),
        pairs(&[("h j k l", "панели"), ("1 2 3", "вкладки")])
    );
    // Палитры нет в таблице: подсказки «поиск» нет, даже если действие названо.
    let named = HintQuery {
        palette: true,
        ..query()
    };
    assert_eq!(hints(&keymap, &named).len(), 2);

    let typing = HintQuery {
        input_receiver: true,
        ..query()
    };
    assert_eq!(
        hints(&keymap, &typing),
        pairs(&[("h j k l", "панели"), ("i", "ввод")])
    );

    let palette = with(
        "[global]\n\"Cmd-k\" = \"palette.open\"\n[navigation]\n\"<leader><leader>\" = \"palette.open\"\n",
    );
    assert_eq!(
        hints(&palette, &named),
        pairs(&[
            ("h j k l", "панели"),
            ("1 2 3", "вкладки"),
            ("Cmd-k", "поиск")
        ])
    );
    let both = HintQuery {
        palette: true,
        input_receiver: true,
        ..query()
    };
    assert_eq!(
        hints(&palette, &both),
        pairs(&[("h j k l", "панели"), ("i", "ввод"), ("Cmd-k", "поиск")])
    );
    // Действие не зарегистрировано — клавиша в таблице подсказки не даёт.
    assert_eq!(hints(&palette, &query()).len(), 2);
}

#[test]
fn hints_name_the_keys_of_the_current_table() {
    let moved = with(
        "[navigation]\n\"h\" = false\n\"i\" = false\n\"1\" = false\n\"Alt-1\" = { action = \"tab.select\", args = { index = 1 } }\n",
    );
    let typing = HintQuery {
        input_receiver: true,
        ..query()
    };
    assert_eq!(
        hints(&moved, &query()),
        pairs(&[("Ctrl-w h j k l", "панели"), ("Alt-1 2 3", "вкладки")])
    );
    assert_eq!(
        hints(&moved, &typing),
        pairs(&[("Ctrl-w h j k l", "панели"), ("Enter", "ввод")])
    );

    let bare = with(
        "[navigation]\n\"h\" = false\n\"j\" = false\n\"k\" = false\n\"l\" = false\n\"Ctrl-w h\" = false\n\"Ctrl-w j\" = false\n\"Ctrl-w k\" = false\n\"Ctrl-w l\" = false\n",
    );
    assert_eq!(hints(&bare, &query()), pairs(&[("1 2 3", "вкладки")]));
}

#[test]
fn input_mode_names_the_way_back() {
    let keymap = defaults();
    let field = HintQuery {
        mode: Mode::Input,
        field: true,
        input_receiver: true,
        ..query()
    };
    assert_eq!(hints(&keymap, &field), pairs(&[("Esc", "навигация")]));

    let surface = HintQuery {
        field: false,
        ..field
    };
    assert_eq!(hints(&keymap, &surface), pairs(&[("Ctrl-\\", "навигация")]));

    let moved = with("[input]\n\"Ctrl-\\\\\" = false\n\"Ctrl-]\" = \"mode.navigation\"\n");
    assert_eq!(hints(&moved, &surface), pairs(&[("Ctrl-]", "навигация")]));
    assert_eq!(hints(&moved, &field), pairs(&[("Esc", "навигация")]));
}

#[test]
fn a_panel_shows_up_to_three_hints_of_its_own() {
    let keymap = defaults();
    let hint = |action, label| PanelHint {
        action,
        args: None,
        label,
    };
    let own = [
        hint("demo-list.details", "детали"),
        hint("demo-list.unbound", "без клавиши"),
        hint("demo-list.back", "назад"),
        hint("demo-list.select", "выбрать"),
        hint("demo-list.reload", "обновить"),
    ];
    let list = HintQuery {
        plugin: Some("demo-list"),
        panel_hints: &own,
        ..query()
    };
    assert_eq!(
        hints(&keymap, &list),
        pairs(&[("o", "детали"), ("b", "назад"), ("s", "выбрать")])
    );

    // Ни у одной своей подсказки нет клавиши — общие подсказки.
    let unbound = HintQuery {
        panel_hints: &own[1..2],
        ..list
    };
    assert_eq!(
        hints(&keymap, &unbound),
        pairs(&[("h j k l", "панели"), ("1 2 3", "вкладки")])
    );
    // Во `Input` свои подсказки панели не показываются.
    let input = HintQuery {
        mode: Mode::Input,
        field: true,
        ..list
    };
    assert_eq!(hints(&keymap, &input), pairs(&[("Esc", "навигация")]));
}
