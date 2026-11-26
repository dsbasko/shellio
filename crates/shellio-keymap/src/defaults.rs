//! Привязки по умолчанию хоста и набор `LIST_KEYS` (ADR-0010, «Клавиши по умолчанию»).

use std::fmt;

use serde_json::{Value, json};

use crate::chord::{Chord, KeyCode, Mods};
use crate::keymap::{Binding, Context, Keymap, Source};
use crate::sequence::{Overlap, Sequence};

/// Лидер по умолчанию: `Space`.
pub(crate) const LEADER: Chord = Chord::new(KeyCode(0x31), Mods::NONE);

/// Движения списка: плагин, вид которого их обрабатывает, получает эти привязки в своём
/// контексте `panel.<plugin>`.
pub const LIST_KEYS: [(&str, &str); 9] = [
    ("j", "list.down"),
    ("k", "list.up"),
    ("g g", "list.top"),
    ("G", "list.bottom"),
    ("Ctrl-d", "list.page_down"),
    ("Ctrl-u", "list.page_up"),
    ("h", "list.collapse"),
    ("l", "list.expand"),
    ("Enter", "list.activate"),
];

#[derive(Clone, Copy)]
enum Ctx {
    Global,
    Navigation,
    Input,
}

const HOST: [(Ctx, &str, &str, Option<&str>); 50] = [
    (Ctx::Global, "Cmd-w", "window.close", None),
    (Ctx::Global, "Cmd-q", "app.quit", None),
    (Ctx::Global, "Cmd-0", "layout.toggle.navigator", None),
    (Ctx::Global, "Cmd-j", "layout.toggle.bottom", None),
    (Ctx::Global, "Cmd-Alt-0", "layout.toggle.side", None),
    (Ctx::Global, "Cmd-Alt-t", "ui.appearance.cycle", None),
    (Ctx::Global, "Cmd-k", "palette.open", None),
    (Ctx::Input, "Ctrl-\\", "mode.navigation", None),
    (Ctx::Navigation, "Ctrl-\\", "mode.toggle", None),
    (Ctx::Navigation, "i", "mode.input", None),
    (Ctx::Navigation, "Enter", "mode.input", None),
    (Ctx::Navigation, "Ctrl-w h", "panel.focus.left", None),
    (Ctx::Navigation, "Ctrl-w j", "panel.focus.down", None),
    (Ctx::Navigation, "Ctrl-w k", "panel.focus.up", None),
    (Ctx::Navigation, "Ctrl-w l", "panel.focus.right", None),
    (Ctx::Navigation, "h", "panel.focus.left", None),
    (Ctx::Navigation, "j", "panel.focus.down", None),
    (Ctx::Navigation, "k", "panel.focus.up", None),
    (Ctx::Navigation, "l", "panel.focus.right", None),
    (Ctx::Navigation, "Ctrl-w H", "panel.move.left", None),
    (Ctx::Navigation, "Ctrl-w J", "panel.move.down", None),
    (Ctx::Navigation, "Ctrl-w K", "panel.move.up", None),
    (Ctx::Navigation, "Ctrl-w L", "panel.move.right", None),
    (Ctx::Navigation, "Ctrl-w v", "layout.split", Some("row")),
    (Ctx::Navigation, "Ctrl-w s", "layout.split", Some("stack")),
    (Ctx::Navigation, "Ctrl-w q", "panel.close", None),
    (Ctx::Navigation, "Ctrl-w o", "layout.only", None),
    (Ctx::Navigation, "Ctrl-w =", "layout.equal", None),
    (Ctx::Navigation, "Ctrl-w z", "layout.zoom", None),
    (Ctx::Navigation, "Ctrl-w +", "layout.grow", None),
    (Ctx::Navigation, "Ctrl-w -", "layout.shrink", None),
    (Ctx::Navigation, "Ctrl-w >", "layout.wider", None),
    (Ctx::Navigation, "Ctrl-w <", "layout.narrower", None),
    (Ctx::Navigation, "H", "tab.prev", None),
    (Ctx::Navigation, "L", "tab.next", None),
    (Ctx::Navigation, "1", "tab.select", None),
    (Ctx::Navigation, "2", "tab.select", None),
    (Ctx::Navigation, "3", "tab.select", None),
    (Ctx::Navigation, "4", "tab.select", None),
    (Ctx::Navigation, "5", "tab.select", None),
    (Ctx::Navigation, "6", "tab.select", None),
    (Ctx::Navigation, "7", "tab.select", None),
    (Ctx::Navigation, "8", "tab.select", None),
    (Ctx::Navigation, "9", "tab.select", None),
    (
        Ctx::Navigation,
        "<leader>z h",
        "layout.toggle.navigator",
        None,
    ),
    (Ctx::Navigation, "<leader>z j", "layout.toggle.bottom", None),
    (Ctx::Navigation, "<leader>z l", "layout.toggle.side", None),
    (Ctx::Navigation, "<leader><leader>", "palette.open", None),
    (Ctx::Navigation, "Ctrl-o", "history.back", None),
    (Ctx::Navigation, "Ctrl-i", "history.forward", None),
];

/// Фильтр панели стоит отдельно от таблицы: у него нет ни оси, ни номера.
const FILTER: (&str, &str) = ("/", "panel.filter");

/// Клавиша действия плагина по умолчанию.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyDefault {
    pub sequence: String,
    pub action: String,
}

/// Что приложение знает о плагине для его контекста `panel.<plugin>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanelKeys {
    pub plugin: String,
    /// Вид плагина обрабатывает движения списка: контекст получает [`LIST_KEYS`].
    pub list_keys: bool,
    pub defaults: Vec<KeyDefault>,
}

/// Привязка по умолчанию не собирается: ошибка манифеста плагина, а не файла пользователя.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DefaultsError {
    pub context: Context,
    pub sequence: String,
    pub reason: String,
}

impl fmt::Display for DefaultsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "привязка по умолчанию «{}» контекста {}: {}",
            self.sequence, self.context, self.reason
        )
    }
}

impl std::error::Error for DefaultsError {}

fn args(action: &str, sequence: &str, axis: Option<&str>) -> Option<Value> {
    match (action, axis) {
        ("layout.split", Some(axis)) => Some(json!({ "axis": axis })),
        ("tab.select", _) => sequence
            .parse::<u8>()
            .ok()
            .map(|index| json!({ "index": index })),
        _ => None,
    }
}

fn binding(
    context: &Context,
    sequence: &str,
    action: &str,
    args: Option<Value>,
    leader: Chord,
) -> Result<Binding, DefaultsError> {
    let parsed = Sequence::parse(sequence, leader).map_err(|error| DefaultsError {
        context: context.clone(),
        sequence: sequence.to_owned(),
        reason: error.to_string(),
    })?;
    Ok(Binding {
        context: context.clone(),
        sequence: parsed,
        action: action.to_owned(),
        args,
        source: Source::Default,
    })
}

pub(crate) fn overlap_reason(overlap: &Overlap) -> String {
    match overlap {
        Overlap::Same => "последовательность уже занята".to_owned(),
        Overlap::Prefix(other) => format!("пересекается с привязкой «{other}»"),
    }
}

impl Keymap {
    /// Таблица по умолчанию: привязки хоста и контексты `panel.<plugin>` названных плагинов.
    /// Своя клавиша плагина заменяет такую же из [`LIST_KEYS`].
    pub fn defaults(panels: &[PanelKeys]) -> Result<Self, DefaultsError> {
        Self::defaults_with(LEADER, panels)
    }

    pub(crate) fn defaults_with(
        leader: Chord,
        panels: &[PanelKeys],
    ) -> Result<Self, DefaultsError> {
        let mut bindings = Vec::new();
        for (ctx, sequence, action, axis) in HOST {
            let context = match ctx {
                Ctx::Global => Context::Global,
                Ctx::Navigation => Context::Navigation,
                Ctx::Input => Context::Input,
            };
            let args = args(action, sequence, axis);
            bindings.push(binding(&context, sequence, action, args, leader)?);
        }
        bindings.push(binding(
            &Context::Navigation,
            FILTER.0,
            FILTER.1,
            None,
            leader,
        )?);

        for panel in panels {
            let context = Context::Panel(panel.plugin.clone());
            let own = panel
                .defaults
                .iter()
                .map(|key| binding(&context, &key.sequence, &key.action, None, leader))
                .collect::<Result<Vec<_>, _>>()?;
            let list = if panel.list_keys { &LIST_KEYS[..] } else { &[] };
            for (sequence, action) in list {
                let key = binding(&context, sequence, action, None, leader)?;
                if own.iter().all(|taken| taken.sequence != key.sequence) {
                    bindings.push(key);
                }
            }
            bindings.extend(own);
        }

        let sources: Vec<(Context, String)> = bindings
            .iter()
            .map(|b| (b.context.clone(), b.sequence.to_string()))
            .collect();
        Self::build(1, leader, bindings, Vec::new()).map_err(|(index, overlap)| {
            let (context, sequence) = sources[index].clone();
            DefaultsError {
                context,
                sequence,
                reason: overlap_reason(&overlap),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::{Lookup, Mode};

    fn panels() -> Vec<PanelKeys> {
        vec![
            PanelKeys {
                plugin: "demo-list".to_owned(),
                list_keys: true,
                defaults: vec![
                    KeyDefault {
                        sequence: "o".to_owned(),
                        action: "demo-list.details".to_owned(),
                    },
                    KeyDefault {
                        sequence: "Enter".to_owned(),
                        action: "demo-list.open".to_owned(),
                    },
                ],
            },
            PanelKeys {
                plugin: "demo-native".to_owned(),
                list_keys: false,
                defaults: Vec::new(),
            },
        ]
    }

    fn act(
        keymap: &Keymap,
        plugin: &str,
        mode: Mode,
        typed: &str,
    ) -> Option<(String, Option<Value>)> {
        let contexts = Keymap::contexts(mode, Some(plugin));
        let typed = Sequence::parse(typed, keymap.leader()).unwrap();
        match keymap.lookup(&contexts, typed.chords()) {
            Lookup::Exact(binding) => Some((binding.action.clone(), binding.args.clone())),
            Lookup::Prefix => Some(("…".to_owned(), None)),
            Lookup::None => None,
        }
    }

    fn name(keymap: &Keymap, plugin: &str, mode: Mode, typed: &str) -> Option<String> {
        act(keymap, plugin, mode, typed).map(|(action, _)| action)
    }

    #[test]
    fn host_defaults_follow_the_decision_table() {
        let keymap = Keymap::defaults(&panels()).unwrap();
        assert_eq!(keymap.version(), 1);
        assert!(keymap.diagnostics().is_empty());
        assert!(
            keymap
                .bindings()
                .iter()
                .all(|b| b.source == Source::Default)
        );

        let nav = |typed: &str| act(&keymap, "demo-native", Mode::Navigation, typed);
        let plain = |action: &str| Some((action.to_owned(), None));
        assert_eq!(nav("Cmd-w"), plain("window.close"));
        assert_eq!(nav("Cmd-q"), plain("app.quit"));
        assert_eq!(nav("Cmd-0"), plain("layout.toggle.navigator"));
        assert_eq!(nav("Cmd-j"), plain("layout.toggle.bottom"));
        assert_eq!(nav("Cmd-Alt-0"), plain("layout.toggle.side"));
        assert_eq!(nav("Cmd-Alt-t"), plain("ui.appearance.cycle"));
        assert_eq!(nav("Ctrl-\\"), plain("mode.toggle"));
        assert_eq!(nav("i"), plain("mode.input"));
        assert_eq!(nav("Enter"), plain("mode.input"));
        for (key, direction) in [("h", "left"), ("j", "down"), ("k", "up"), ("l", "right")] {
            let upper = key.to_uppercase();
            assert_eq!(nav(key), plain(&format!("panel.focus.{direction}")));
            assert_eq!(
                nav(&format!("Ctrl-w {key}")),
                plain(&format!("panel.focus.{direction}"))
            );
            assert_eq!(
                nav(&format!("Ctrl-w {upper}")),
                plain(&format!("panel.move.{direction}"))
            );
        }
        assert_eq!(
            nav("Ctrl-w v"),
            Some(("layout.split".to_owned(), Some(json!({"axis": "row"}))))
        );
        assert_eq!(
            nav("Ctrl-w s"),
            Some(("layout.split".to_owned(), Some(json!({"axis": "stack"}))))
        );
        for (key, action) in [
            ("q", "panel.close"),
            ("o", "layout.only"),
            ("=", "layout.equal"),
            ("z", "layout.zoom"),
            ("+", "layout.grow"),
            ("-", "layout.shrink"),
            (">", "layout.wider"),
            ("<", "layout.narrower"),
        ] {
            assert_eq!(nav(&format!("Ctrl-w {key}")), plain(action));
        }
        assert_eq!(nav("H"), plain("tab.prev"));
        assert_eq!(nav("L"), plain("tab.next"));
        for index in 1..=9u8 {
            assert_eq!(
                nav(&index.to_string()),
                Some(("tab.select".to_owned(), Some(json!({"index": index}))))
            );
        }
        assert_eq!(nav("0"), None);
        assert_eq!(nav("Space"), plain("…"));
        assert_eq!(nav("Space z"), plain("…"));
        assert_eq!(nav("Space z h"), plain("layout.toggle.navigator"));
        assert_eq!(nav("Space z j"), plain("layout.toggle.bottom"));
        assert_eq!(nav("Space z l"), plain("layout.toggle.side"));
        assert_eq!(nav("Ctrl-o"), plain("history.back"));
        assert_eq!(nav("Ctrl-i"), plain("history.forward"));
        assert_eq!(nav("/"), plain("panel.filter"));
        assert_eq!(nav("Ctrl-w"), plain("…"));
        assert_eq!(nav("Cmd-1"), None);
    }

    #[test]
    fn input_mode_sees_only_input_and_global() {
        let keymap = Keymap::defaults(&panels()).unwrap();
        let input = |typed: &str| name(&keymap, "demo-list", Mode::Input, typed);
        assert_eq!(input("Ctrl-\\").as_deref(), Some("mode.navigation"));
        assert_eq!(input("Cmd-q").as_deref(), Some("app.quit"));
        for typed in ["j", "h", "i", "Enter", "Ctrl-w", "Space", "/", "Ctrl-o"] {
            assert_eq!(input(typed), None, "{typed}");
        }
    }

    #[test]
    fn list_keys_stay_in_the_panel_that_declares_them() {
        let keymap = Keymap::defaults(&panels()).unwrap();
        let list = |typed: &str| name(&keymap, "demo-list", Mode::Navigation, typed);
        let plain = |typed: &str| name(&keymap, "demo-native", Mode::Navigation, typed);

        for (sequence, action) in LIST_KEYS {
            if sequence != "Enter" {
                assert_eq!(list(sequence).as_deref(), Some(action), "{sequence}");
            }
        }
        assert_eq!(list("g").as_deref(), Some("…"));
        // Своя клавиша плагина заменяет такую же из набора списка.
        assert_eq!(list("Enter").as_deref(), Some("demo-list.open"));
        assert_eq!(list("o").as_deref(), Some("demo-list.details"));
        assert_eq!(list("Ctrl-w j").as_deref(), Some("panel.focus.down"));

        assert_eq!(plain("j").as_deref(), Some("panel.focus.down"));
        assert_eq!(plain("G"), None);
        assert_eq!(plain("g"), None);
        assert_eq!(plain("Ctrl-d"), None);
    }

    #[test]
    fn broken_plugin_defaults_are_an_error() {
        let with = |sequence: &str, list_keys| {
            Keymap::defaults(&[PanelKeys {
                plugin: "p".to_owned(),
                list_keys,
                defaults: vec![KeyDefault {
                    sequence: sequence.to_owned(),
                    action: "p.a".to_owned(),
                }],
            }])
        };
        assert!(with("x", true).is_ok());
        let bad = with("Hyper-x", false).unwrap_err();
        assert_eq!(bad.context, Context::Panel("p".to_owned()));
        assert_eq!(bad.sequence, "Hyper-x");
        // «g» — начало «g g» из набора списка.
        let overlap = with("g", true).unwrap_err();
        assert_eq!(overlap.reason, "пересекается с привязкой «g g»");
        assert!(with("g", false).is_ok());
    }

    #[test]
    fn leader_sequences_move_with_the_leader() {
        let comma = Chord::parse(",").unwrap();
        let keymap = Keymap::defaults_with(comma, &[]).unwrap();
        assert_eq!(
            name(&keymap, "x", Mode::Navigation, ", z j").as_deref(),
            Some("layout.toggle.bottom")
        );
        assert_eq!(name(&keymap, "x", Mode::Navigation, "Space"), None);
        // Лидер, занятый привязкой по умолчанию, таблицу не собирает.
        assert!(Keymap::defaults_with(Chord::parse("h").unwrap(), &[]).is_err());
    }
}
