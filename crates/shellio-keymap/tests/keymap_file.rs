//! Разбор `keymap.toml`: файл дополняет умолчания, неверная строка отключается с номером и
//! причиной, нечитаемый файл оставляет последнюю исправную таблицу (ADR-0010 и ADR-0018,
//! «Уточнено в P1.6 (планирование)»).

// allow-unwrap-in-tests не распространяется на вспомогательные функции интеграционного теста.
#![allow(clippy::unwrap_used)]

use serde_json::{Value, json};
use shellio_keymap::{
    ActionError, Binding, Context, DiagnosticKind, KeyDefault, Keymap, Lookup, Mode, PanelKeys,
    Sequence, Source,
};

fn panels() -> Vec<PanelKeys> {
    vec![
        PanelKeys {
            plugin: "demo-list".to_owned(),
            list_keys: true,
            defaults: vec![KeyDefault {
                sequence: "o".to_owned(),
                action: "demo-list.back".to_owned(),
            }],
        },
        PanelKeys {
            plugin: "demo-native".to_owned(),
            list_keys: false,
            defaults: Vec::new(),
        },
    ]
}

const KNOWN: [&str; 5] = [
    "layout.zoom",
    "layout.toggle.bottom",
    "ui.appearance.cycle",
    "mode.navigation",
    "palette.open",
];

fn check(_: &Context, action: &str, args: Option<&Value>) -> Result<(), ActionError> {
    match action {
        "demo-list.details" => match args.and_then(|args| args.get("open")) {
            Some(Value::Bool(_)) => Ok(()),
            _ => Err(ActionError::Args("нужен open".to_owned())),
        },
        _ if KNOWN.contains(&action) => Ok(()),
        _ => Err(ActionError::Unknown),
    }
}

fn defaults() -> Keymap {
    Keymap::defaults(&panels()).unwrap()
}

fn apply(keymap: &Keymap, file: &str) -> Keymap {
    keymap.apply(Some(file), &panels(), &check).unwrap()
}

fn find<'a>(keymap: &'a Keymap, plugin: &str, mode: Mode, typed: &str) -> Option<&'a Binding> {
    let contexts = Keymap::contexts(mode, Some(plugin));
    let typed = Sequence::parse(typed, keymap.leader()).unwrap();
    match keymap.lookup(&contexts, typed.chords()) {
        Lookup::Exact(binding) => Some(binding),
        Lookup::Prefix | Lookup::None => None,
    }
}

fn nav<'a>(keymap: &'a Keymap, typed: &str) -> Option<&'a str> {
    find(keymap, "demo-native", Mode::Navigation, typed).map(|binding| binding.action.as_str())
}

fn problems(keymap: &Keymap) -> Vec<(Option<u32>, DiagnosticKind)> {
    keymap
        .diagnostics()
        .iter()
        .map(|diagnostic| (diagnostic.line, diagnostic.kind))
        .collect()
}

const EXAMPLE: &str = r#"leader = "Space"

[global]
"Cmd-Alt-t" = "ui.appearance.cycle"

[input]
"Ctrl-\\" = "mode.navigation"

[navigation]
"<leader>zj" = "layout.toggle.bottom"
"Ctrl-w o" = false

[panel.demo-list]
"x" = { action = "demo-list.details", args = { open = true } }
"#;

#[test]
fn no_file_and_an_empty_file_give_the_defaults() {
    let base = defaults();
    assert_eq!(base.apply(None, &panels(), &check).unwrap(), base);
    assert_eq!(apply(&base, ""), base);
    assert_eq!(apply(&base, "# только комментарий\n"), base);
    // Снятие привязки, которой нет, таблицу не меняет.
    let same = apply(&base, "[navigation]\n\"Ctrl-w y\" = false\n");
    assert_eq!(same, base);
}

#[test]
fn the_file_extends_replaces_and_removes_defaults() {
    let base = defaults();
    let keymap = apply(&base, EXAMPLE);

    assert_eq!(keymap.version(), base.version() + 1);
    assert!(keymap.diagnostics().is_empty());

    let zone = find(&keymap, "demo-native", Mode::Navigation, "Space z j").unwrap();
    assert_eq!(
        (zone.action.as_str(), zone.source),
        ("layout.toggle.bottom", Source::File)
    );
    assert_eq!(nav(&keymap, "Ctrl-w o"), None);
    assert_eq!(nav(&keymap, "Ctrl-w z"), Some("layout.zoom"));
    assert_eq!(nav(&base, "Ctrl-w o"), Some("layout.only"));

    let details = find(&keymap, "demo-list", Mode::Navigation, "x").unwrap();
    assert_eq!(details.action, "demo-list.details");
    assert_eq!(details.args, Some(json!({ "open": true })));
    assert_eq!(details.context, Context::Panel("demo-list".to_owned()));
    assert_eq!(find(&keymap, "demo-native", Mode::Navigation, "x"), None);

    let exit = find(&keymap, "demo-list", Mode::Input, "Ctrl-\\").unwrap();
    assert_eq!(
        (exit.action.as_str(), exit.source),
        ("mode.navigation", Source::File)
    );

    // Замена не оставляет второй строки с той же последовательностью.
    let rows = |keymap: &Keymap| keymap.bindings().len();
    assert_eq!(rows(&keymap), rows(&base));
}

#[test]
fn every_bad_line_is_disabled_alone_with_its_line_number() {
    let file = r#"leader = "Space"
[navigation]
"Ctrl-w m" = "layout.zoom"
"Ctrl-w y" = "nope.action"
"Hyper-x" = "layout.zoom"
"a b c d e" = "layout.zoom"
"<leader>zk" = "layout.zoom"
"Space z k" = "layout.zoom"
"Ctrl-w x" = 5
[panel.demo-list]
"x" = { action = "demo-list.details", args = { open = "yes" } }
"y" = { action = "demo-list.details", extra = 1 }
[panel.ghost]
"x" = "layout.zoom"
[modal]
"x" = "layout.zoom"
"#;
    let base = defaults();
    let keymap = apply(&base, file);

    assert_eq!(
        problems(&keymap),
        [
            (Some(4), DiagnosticKind::UnknownAction),
            (Some(5), DiagnosticKind::BadChord),
            (Some(6), DiagnosticKind::TooLong),
            (Some(7), DiagnosticKind::Conflict),
            (Some(8), DiagnosticKind::Conflict),
            (Some(9), DiagnosticKind::BadValue),
            (Some(11), DiagnosticKind::BadArgs),
            (Some(12), DiagnosticKind::BadValue),
            (Some(14), DiagnosticKind::UnknownContext),
            (Some(16), DiagnosticKind::UnknownContext),
        ]
    );
    assert_eq!(
        keymap.diagnostics()[0].message(),
        "keymap.toml, строка 4: неизвестное действие «nope.action» — привязка отключена"
    );
    assert_eq!(keymap.diagnostics()[3].reason, "конфликт со строкой 8");
    assert_eq!(keymap.diagnostics()[4].reason, "конфликт со строкой 7");
    assert_eq!(
        keymap.diagnostics()[8].reason,
        "неизвестный контекст «panel.ghost»"
    );

    // Верная строка действует, остальная таблица прежняя.
    assert_eq!(keymap.version(), base.version() + 1);
    assert_eq!(nav(&keymap, "Ctrl-w m"), Some("layout.zoom"));
    assert_eq!(nav(&keymap, "Ctrl-w z"), Some("layout.zoom"));
    assert_eq!(nav(&keymap, "Space z k"), None);
    assert_eq!(nav(&keymap, "Ctrl-w y"), None);
    assert_eq!(find(&keymap, "demo-list", Mode::Navigation, "x"), None);
    assert_eq!(keymap.bindings().len(), base.bindings().len() + 1);
}

#[test]
fn a_line_that_crosses_a_default_is_disabled_until_the_default_is_removed() {
    let base = defaults();
    let crossed = apply(&base, "[panel.demo-list]\n\"g\" = \"layout.zoom\"\n");
    assert_eq!(problems(&crossed), [(Some(2), DiagnosticKind::Conflict)]);
    assert_eq!(
        crossed.diagnostics()[0].reason,
        "пересекается с привязкой по умолчанию «g g»"
    );
    assert_eq!(crossed.version(), base.version());
    assert_eq!(crossed.bindings(), base.bindings());

    let longer = apply(&base, "[navigation]\n\"i x\" = \"layout.zoom\"\n");
    assert_eq!(problems(&longer), [(Some(2), DiagnosticKind::Conflict)]);

    let freed = apply(
        &base,
        "[panel.demo-list]\n\"g g\" = false\n\"g\" = \"layout.zoom\"\n",
    );
    assert!(freed.diagnostics().is_empty());
    let bound = find(&freed, "demo-list", Mode::Navigation, "g").unwrap();
    assert_eq!(bound.action, "layout.zoom");

    // Две строки файла, одна из которых начало другой, отключаются обе.
    let both = apply(
        &base,
        "[navigation]\n\"Ctrl-p\" = \"layout.zoom\"\n\"Ctrl-p x\" = \"layout.zoom\"\n",
    );
    assert_eq!(
        problems(&both),
        [
            (Some(2), DiagnosticKind::Conflict),
            (Some(3), DiagnosticKind::Conflict)
        ]
    );
    assert_eq!(both.bindings(), base.bindings());
}

#[test]
fn an_unreadable_file_keeps_the_last_good_table() {
    let base = defaults();
    let good = apply(&base, EXAMPLE);
    assert_eq!(good.version(), 2);

    let broken = apply(&good, "[navigation\n\"Ctrl-w m\" = \"layout.zoom\"\n");
    assert_eq!(broken.version(), good.version());
    assert_eq!(broken.bindings(), good.bindings());
    assert_eq!(problems(&broken), [(None, DiagnosticKind::Unreadable)]);
    let message = broken.diagnostics()[0].message();
    assert!(
        message.starts_with("keymap.toml не прочитан: строка 1: "),
        "{message}"
    );
    assert!(
        message.ends_with(" — действует прежняя таблица"),
        "{message}"
    );
    assert_eq!(nav(&broken, "Space z j"), Some("layout.toggle.bottom"));

    // Повторяющийся ключ — тоже ошибка всего файла.
    let twice = apply(
        &good,
        "[navigation]\n\"x\" = \"layout.zoom\"\n\"x\" = \"layout.zoom\"\n",
    );
    assert_eq!(problems(&twice), [(None, DiagnosticKind::Unreadable)]);
    assert_eq!(twice.bindings(), good.bindings());

    // Исправленный файл с теми же строками версию не меняет, диагностика уходит.
    let fixed = apply(&broken, EXAMPLE);
    assert_eq!(fixed, good);

    let edited = apply(&fixed, "[navigation]\n\"Ctrl-w m\" = \"layout.zoom\"\n");
    assert_eq!(edited.version(), 3);
    assert_eq!(nav(&edited, "Ctrl-w m"), Some("layout.zoom"));
    assert_eq!(nav(&edited, "Ctrl-w o"), Some("layout.only"));

    let removed = edited.apply(None, &panels(), &check).unwrap();
    assert_eq!(removed.version(), 4);
    assert_eq!(removed.bindings(), base.bindings());
}

#[test]
fn the_leader_moves_leader_sequences() {
    let base = defaults();
    let comma = apply(
        &base,
        "leader = \",\"\n[navigation]\n\"<leader>m\" = \"layout.zoom\"\n",
    );
    assert!(comma.diagnostics().is_empty());
    assert_eq!(comma.leader().to_string(), ",");
    assert_eq!(nav(&comma, ", z j"), Some("layout.toggle.bottom"));
    assert_eq!(nav(&comma, ", m"), Some("layout.zoom"));
    assert_eq!(nav(&comma, "Space z j"), None);

    for file in ["leader = \"Hyper\"\n", "leader = 5\n", "leader = \"h\"\n"] {
        let keymap = apply(&base, file);
        assert_eq!(
            problems(&keymap),
            [(Some(1), DiagnosticKind::BadLeader)],
            "{file}"
        );
        assert_eq!(keymap.bindings(), base.bindings());
        assert_eq!(keymap.version(), base.version());
    }
}
