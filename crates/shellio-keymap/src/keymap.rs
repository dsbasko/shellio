//! Таблица keymap: привязки по контекстам, порядок контекстов, поиск действия по набранным
//! аккордам и клавиши по действию.

use std::collections::BTreeMap;
use std::fmt;

use serde::Serialize;
use serde_json::Value;

use crate::chord::Chord;
use crate::sequence::{Match, Overlap, Sequence, SequenceTree};

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Context {
    Global,
    Navigation,
    Input,
    /// Контекст панели плагина: `panel.<plugin>`.
    Panel(String),
}

const PANEL: &str = "panel.";

impl Context {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "global" => Some(Self::Global),
            "navigation" => Some(Self::Navigation),
            "input" => Some(Self::Input),
            _ => name
                .strip_prefix(PANEL)
                .filter(|plugin| !plugin.is_empty())
                .map(|plugin| Self::Panel(plugin.to_owned())),
        }
    }
}

impl fmt::Display for Context {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Global => f.write_str("global"),
            Self::Navigation => f.write_str("navigation"),
            Self::Input => f.write_str("input"),
            Self::Panel(plugin) => write!(f, "{PANEL}{plugin}"),
        }
    }
}

impl Serialize for Context {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// Режим панели с фокусом.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Navigation,
    Input,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Default,
    File,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Binding {
    pub context: Context,
    pub sequence: Sequence,
    pub action: String,
    pub args: Option<Value>,
    pub source: Source,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticKind {
    /// Файл не разобран как TOML или не прочитан: действует прежняя таблица.
    Unreadable,
    UnknownContext,
    UnknownAction,
    BadChord,
    TooLong,
    Conflict,
    BadArgs,
    BadValue,
    BadLeader,
}

/// Запись диагностики последнего разбора `keymap.toml`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Diagnostic {
    /// Номер строки файла с единицы. У нечитаемого файла его нет.
    pub line: Option<u32>,
    pub kind: DiagnosticKind,
    pub reason: String,
}

impl Diagnostic {
    /// Сообщение строки статуса. Путь к файлу в него не входит.
    pub fn message(&self) -> String {
        match self.line {
            Some(line) => format!(
                "keymap.toml, строка {line}: {} — привязка отключена",
                self.reason
            ),
            None => format!(
                "keymap.toml не прочитан: {} — действует прежняя таблица",
                self.reason
            ),
        }
    }
}

/// Ответ таблицы на набранные аккорды.
#[derive(Debug, PartialEq)]
pub enum Lookup<'a> {
    Exact(&'a Binding),
    /// Ждём продолжения последовательности.
    Prefix,
    None,
}

/// Действующая таблица: неизменяемый снимок с версией и диагностикой последнего разбора.
#[derive(Clone, Debug, PartialEq)]
pub struct Keymap {
    version: u64,
    leader: Chord,
    bindings: Vec<Binding>,
    diagnostics: Vec<Diagnostic>,
    trees: BTreeMap<Context, SequenceTree<usize>>,
}

impl Keymap {
    /// Таблица из привязок. Отказ — номер привязки, которая пересеклась с более ранней в своём
    /// контексте.
    pub(crate) fn build(
        version: u64,
        leader: Chord,
        bindings: Vec<Binding>,
        diagnostics: Vec<Diagnostic>,
    ) -> Result<Self, (usize, Overlap)> {
        let mut trees: BTreeMap<Context, SequenceTree<usize>> = BTreeMap::new();
        for (index, binding) in bindings.iter().enumerate() {
            trees
                .entry(binding.context.clone())
                .or_default()
                .insert(&binding.sequence, index)
                .map_err(|overlap| (index, overlap))?;
        }
        Ok(Self {
            version,
            leader,
            bindings,
            diagnostics,
            trees,
        })
    }

    /// Версия таблицы. Действует в пределах процесса владельца и растёт, когда меняются строки.
    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn leader(&self) -> Chord {
        self.leader
    }

    pub fn bindings(&self) -> &[Binding] {
        &self.bindings
    }

    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// Та же таблица и та же версия с новой диагностикой: файл не прочитан или не разобран.
    pub fn rejected(&self, reason: impl Into<String>) -> Self {
        Self {
            diagnostics: vec![Diagnostic {
                line: None,
                kind: DiagnosticKind::Unreadable,
                reason: reason.into(),
            }],
            ..self.clone()
        }
    }

    pub(crate) fn with_diagnostics(mut self, diagnostics: Vec<Diagnostic>) -> Self {
        self.diagnostics = diagnostics;
        self
    }

    pub(crate) fn tree(&self, context: &Context) -> Option<&SequenceTree<usize>> {
        self.trees.get(context)
    }

    /// Контексты панели с фокусом сверху вниз. В `Input` контекст панели и `navigation` не
    /// участвуют: клавиши, кроме `input` и `global`, уходят получателю.
    pub fn contexts(mode: Mode, plugin: Option<&str>) -> Vec<Context> {
        match mode {
            Mode::Navigation => plugin
                .map(|plugin| Context::Panel(plugin.to_owned()))
                .into_iter()
                .chain([Context::Navigation, Context::Global])
                .collect(),
            Mode::Input => vec![Context::Input, Context::Global],
        }
    }

    /// Решает первый сверху контекст, который знает набранное: его привязка скрывает такую же
    /// последовательность ниже, а его более длинная последовательность — более короткую ниже.
    pub fn lookup(&self, contexts: &[Context], typed: &[Chord]) -> Lookup<'_> {
        for context in contexts {
            match self.tree(context).map(|tree| tree.find(typed)) {
                Some(Match::Exact(&index)) => return Lookup::Exact(&self.bindings[index]),
                Some(Match::Prefix) => return Lookup::Prefix,
                Some(Match::None) | None => {}
            }
        }
        Lookup::None
    }

    /// Последовательности действия с такими `args` в названных контекстах: сначала короткие,
    /// затем по порядку контекстов и строк таблицы.
    pub fn sequences_for(
        &self,
        contexts: &[Context],
        action: &str,
        args: Option<&Value>,
    ) -> Vec<&Sequence> {
        let mut found: Vec<(usize, usize, usize)> = self
            .bindings
            .iter()
            .enumerate()
            .filter(|(_, binding)| binding.action == action && binding.args.as_ref() == args)
            .filter_map(|(index, binding)| {
                let order = contexts.iter().position(|c| *c == binding.context)?;
                Some((binding.sequence.chords().len(), order, index))
            })
            .collect();
        found.sort_unstable();
        found
            .into_iter()
            .map(|(.., index)| &self.bindings[index].sequence)
            .collect()
    }

    /// Клавиша действия для подсказки: первая из [`Keymap::sequences_for`].
    pub fn key_for(
        &self,
        contexts: &[Context],
        action: &str,
        args: Option<&Value>,
    ) -> Option<&Sequence> {
        self.sequences_for(contexts, action, args)
            .into_iter()
            .next()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn leader() -> Chord {
        Chord::parse("Space").unwrap()
    }

    fn seq(text: &str) -> Sequence {
        Sequence::parse(text, leader()).unwrap()
    }

    fn bind(context: Context, sequence: &str, action: &str) -> Binding {
        Binding {
            context,
            sequence: seq(sequence),
            action: action.to_owned(),
            args: None,
            source: Source::Default,
        }
    }

    fn panel() -> Context {
        Context::Panel("demo-list".to_owned())
    }

    fn keymap(bindings: Vec<Binding>) -> Keymap {
        Keymap::build(1, leader(), bindings, Vec::new()).unwrap()
    }

    fn action(keymap: &Keymap, contexts: &[Context], typed: &str) -> Option<String> {
        match keymap.lookup(contexts, seq(typed).chords()) {
            Lookup::Exact(binding) => Some(binding.action.clone()),
            Lookup::Prefix => Some("…".to_owned()),
            Lookup::None => None,
        }
    }

    #[test]
    fn context_names_round_trip() {
        for name in ["global", "navigation", "input", "panel.demo-list"] {
            assert_eq!(Context::parse(name).unwrap().to_string(), name);
        }
        for name in ["", "panel", "panel.", "modal", "Global"] {
            assert_eq!(Context::parse(name), None);
        }
    }

    #[test]
    fn contexts_follow_the_mode() {
        assert_eq!(
            Keymap::contexts(Mode::Navigation, Some("demo-list")),
            [panel(), Context::Navigation, Context::Global]
        );
        assert_eq!(
            Keymap::contexts(Mode::Navigation, None),
            [Context::Navigation, Context::Global]
        );
        assert_eq!(
            Keymap::contexts(Mode::Input, Some("demo-list")),
            [Context::Input, Context::Global]
        );
    }

    #[test]
    fn upper_context_hides_the_same_sequence_below() {
        let keymap = keymap(vec![
            bind(panel(), "j", "list.down"),
            bind(panel(), "g g", "list.top"),
            bind(Context::Navigation, "j", "panel.focus.down"),
            bind(Context::Global, "g", "global.g"),
            bind(Context::Navigation, "g x", "nav.gx"),
            bind(Context::Navigation, "k", "panel.focus.up"),
            bind(Context::Global, "k", "global.k"),
            bind(Context::Global, "Cmd-w", "window.close"),
            bind(Context::Input, "Ctrl-\\", "mode.navigation"),
        ]);
        let list = Keymap::contexts(Mode::Navigation, Some("demo-list"));
        let plain = Keymap::contexts(Mode::Navigation, Some("demo-native"));
        let input = Keymap::contexts(Mode::Input, Some("demo-list"));

        assert_eq!(action(&keymap, &list, "j").as_deref(), Some("list.down"));
        assert_eq!(
            action(&keymap, &plain, "j").as_deref(),
            Some("panel.focus.down")
        );
        assert_eq!(
            action(&keymap, &list, "k").as_deref(),
            Some("panel.focus.up")
        );
        assert_eq!(
            action(&keymap, &list, "Cmd-w").as_deref(),
            Some("window.close")
        );
        // Более длинная последовательность панели скрывает короткую ниже, продолжение вне
        // панели находит нижний контекст.
        assert_eq!(action(&keymap, &list, "g").as_deref(), Some("…"));
        assert_eq!(action(&keymap, &list, "g g").as_deref(), Some("list.top"));
        assert_eq!(action(&keymap, &list, "g x").as_deref(), Some("nav.gx"));
        assert_eq!(action(&keymap, &plain, "g").as_deref(), Some("…"));
        assert_eq!(
            action(&keymap, &[Context::Global], "g").as_deref(),
            Some("global.g")
        );
        assert_eq!(action(&keymap, &list, "x"), None);

        assert_eq!(action(&keymap, &input, "j"), None);
        assert_eq!(
            action(&keymap, &input, "Ctrl-\\").as_deref(),
            Some("mode.navigation")
        );
        assert_eq!(
            action(&keymap, &input, "Cmd-w").as_deref(),
            Some("window.close")
        );
        assert_eq!(action(&keymap, &list, "Ctrl-\\"), None);
    }

    #[test]
    fn one_context_holds_one_action_per_sequence() {
        let same = Keymap::build(
            1,
            leader(),
            vec![
                bind(Context::Navigation, "j", "a"),
                bind(Context::Global, "j", "b"),
                bind(Context::Navigation, "j", "c"),
            ],
            Vec::new(),
        );
        assert_eq!(same, Err((2, Overlap::Same)));
        let prefix = Keymap::build(
            1,
            leader(),
            vec![
                bind(Context::Navigation, "Ctrl-w h", "a"),
                bind(Context::Navigation, "Ctrl-w", "b"),
            ],
            Vec::new(),
        );
        assert_eq!(prefix, Err((1, Overlap::Prefix(seq("Ctrl-w h")))));
    }

    #[test]
    fn keys_of_an_action_come_short_first_and_match_args() {
        let split = |sequence, axis| Binding {
            args: Some(json!({ "axis": axis })),
            ..bind(Context::Navigation, sequence, "layout.split")
        };
        let keymap = keymap(vec![
            bind(Context::Navigation, "Ctrl-w h", "panel.focus.left"),
            bind(Context::Navigation, "h", "panel.focus.left"),
            split("Ctrl-w v", "row"),
            split("Ctrl-w s", "stack"),
            bind(Context::Navigation, "Space Space", "palette.open"),
            bind(Context::Global, "Cmd-k", "palette.open"),
        ]);
        let nav = [Context::Navigation, Context::Global];
        let names = |action, args: Option<&Value>| -> Vec<String> {
            keymap
                .sequences_for(&nav, action, args)
                .into_iter()
                .map(Sequence::to_string)
                .collect()
        };

        assert_eq!(names("panel.focus.left", None), ["h", "Ctrl-w h"]);
        assert_eq!(
            names("layout.split", Some(&json!({"axis": "stack"}))),
            ["Ctrl-w s"]
        );
        assert_eq!(names("layout.split", None), Vec::<String>::new());
        assert_eq!(names("palette.open", None), ["Cmd-k", "Space Space"]);
        assert_eq!(
            keymap
                .key_for(&[Context::Navigation], "palette.open", None)
                .map(Sequence::to_string)
                .as_deref(),
            Some("Space Space")
        );
        assert_eq!(keymap.key_for(&nav, "layout.zoom", None), None);
    }

    #[test]
    fn diagnostic_messages_never_name_the_path() {
        let line = Diagnostic {
            line: Some(7),
            kind: DiagnosticKind::UnknownAction,
            reason: "неизвестное действие «x»".to_owned(),
        };
        assert_eq!(
            line.message(),
            "keymap.toml, строка 7: неизвестное действие «x» — привязка отключена"
        );
        let keymap = keymap(vec![bind(Context::Global, "Cmd-w", "window.close")]);
        let rejected = keymap.rejected("строка 2: нет значения");
        assert_eq!(rejected.version(), keymap.version());
        assert_eq!(rejected.bindings(), keymap.bindings());
        assert_eq!(
            rejected.diagnostics()[0].message(),
            "keymap.toml не прочитан: строка 2: нет значения — действует прежняя таблица"
        );
    }

    #[test]
    fn rows_serialize_for_the_wire() {
        let row = Binding {
            args: Some(json!({ "axis": "row" })),
            source: Source::File,
            ..bind(
                Context::Panel("demo-list".to_owned()),
                "<leader>zj",
                "layout.split",
            )
        };
        assert_eq!(
            serde_json::to_value(&row).unwrap(),
            json!({
                "context": "panel.demo-list",
                "sequence": "Space z j",
                "action": "layout.split",
                "args": { "axis": "row" },
                "source": "file",
            })
        );
    }
}
