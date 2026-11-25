//! Разбор `keymap.toml` (ADR-0010 и ADR-0018, «Уточнено в P1.6 (планирование)»): файл дополняет
//! привязки по умолчанию, каждая строка разбирается отдельно, нечитаемый файл оставляет прежнюю
//! таблицу.

use serde_json::Value;
use toml::de::DeTable;

use crate::chord::Chord;
use crate::defaults::{DefaultsError, LEADER, PanelKeys, overlap_reason};
use crate::keymap::{Binding, Context, Diagnostic, DiagnosticKind, Keymap, Source};
use crate::sequence::{Sequence, SequenceError};

/// Почему действие строки не принято.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActionError {
    Unknown,
    /// `args` отвергнуты проверкой действия, с причиной.
    Args(String),
}

/// Проверка имени действия и `args` по реестру плагинов и командам хоста: её даёт приложение.
pub trait ActionCheck {
    fn check(
        &self,
        context: &Context,
        action: &str,
        args: Option<&Value>,
    ) -> Result<(), ActionError>;
}

impl<F> ActionCheck for F
where
    F: Fn(&Context, &str, Option<&Value>) -> Result<(), ActionError>,
{
    fn check(
        &self,
        context: &Context,
        action: &str,
        args: Option<&Value>,
    ) -> Result<(), ActionError> {
        self(context, action, args)
    }
}

const BAD_VALUE: &str = "значение привязки — имя действия, false или таблица с action и args";

enum Op {
    Remove,
    Bind { action: String, args: Option<Value> },
}

struct Line {
    line: u32,
    context: Context,
    sequence: Sequence,
    op: Op,
}

struct Raw<'a> {
    line: u32,
    context: Context,
    key: &'a str,
    value: &'a toml::Value,
}

struct Report(Vec<Diagnostic>);

impl Report {
    fn add(&mut self, line: u32, kind: DiagnosticKind, reason: impl Into<String>) {
        self.0.push(Diagnostic {
            line: Some(line),
            kind,
            reason: reason.into(),
        });
    }
}

fn line_at(text: &str, offset: usize) -> u32 {
    let before = &text.as_bytes()[..offset.min(text.len())];
    let breaks = before.iter().filter(|&&byte| byte == b'\n').count();
    u32::try_from(breaks + 1).unwrap_or(u32::MAX)
}

/// Строка ключа по пути от корня. Ключ, которого разбор с местами не нашёл, получает строку 1.
fn line_of(text: &str, spans: &DeTable<'_>, path: &[&str]) -> u32 {
    let mut table = spans;
    let mut line = 1;
    for (depth, name) in path.iter().enumerate() {
        let Some((key, value)) = table.iter().find(|(key, _)| key.get_ref() == name) else {
            break;
        };
        line = line_at(text, key.span().start);
        match value.get_ref().as_table() {
            Some(inner) if depth + 1 < path.len() => table = inner,
            _ => break,
        }
    }
    line
}

fn op(value: &toml::Value) -> Option<Op> {
    match value {
        toml::Value::Boolean(false) => Some(Op::Remove),
        toml::Value::String(action) => Some(Op::Bind {
            action: action.clone(),
            args: None,
        }),
        toml::Value::Table(table) => {
            let action = table.get("action")?.as_str()?.to_owned();
            if table.keys().any(|key| key != "action" && key != "args") {
                return None;
            }
            let args = match table.get("args") {
                Some(args) => Some(serde_json::to_value(args).ok()?),
                None => None,
            };
            Some(Op::Bind { action, args })
        }
        _ => None,
    }
}

impl Keymap {
    /// Следующая таблица по содержимому файла. `None` — файла нет: таблица по умолчанию без
    /// диагностики. Нечитаемый TOML оставляет строки и версию, меняется только диагностика.
    /// Версия растёт, когда строки таблицы или лидер стали другими.
    pub fn apply(
        &self,
        file: Option<&str>,
        panels: &[PanelKeys],
        check: &dyn ActionCheck,
    ) -> Result<Self, DefaultsError> {
        let defaults = Self::defaults(panels)?;
        let Some(text) = file else {
            return Ok(self.next(defaults.leader(), defaults.bindings().to_vec(), Vec::new()));
        };
        let spans = match DeTable::parse(text) {
            Ok(spans) => spans.into_inner(),
            Err(error) => {
                let place = error
                    .span()
                    .map(|span| format!("строка {}: ", line_at(text, span.start)));
                let reason = format!("{}{}", place.unwrap_or_default(), error.message());
                return Ok(self.rejected(reason));
            }
        };
        let values: toml::Table = match toml::from_str(text) {
            Ok(values) => values,
            Err(error) => return Ok(self.rejected(error.message())),
        };

        let mut report = Report(Vec::new());
        let mut leader = LEADER;
        let mut raws: Vec<Raw<'_>> = Vec::new();
        for (name, value) in &values {
            let line = line_of(text, &spans, &[name]);
            if name == "leader" {
                match value.as_str().map(Chord::parse) {
                    Some(Ok(chord)) => leader = chord,
                    _ => report.add(
                        line,
                        DiagnosticKind::BadLeader,
                        "лидер не аккорд, действует Space",
                    ),
                }
                continue;
            }
            let unknown = |report: &mut Report, line, context: &str| {
                report.add(
                    line,
                    DiagnosticKind::UnknownContext,
                    format!("неизвестный контекст «{context}»"),
                );
            };
            let Some(table) = value.as_table() else {
                unknown(&mut report, line, name);
                continue;
            };
            if name != "panel" {
                let Some(context) =
                    Context::parse(name).filter(|c| !matches!(c, Context::Panel(_)))
                else {
                    for key in table.keys() {
                        unknown(&mut report, line_of(text, &spans, &[name, key]), name);
                    }
                    if table.is_empty() {
                        unknown(&mut report, line, name);
                    }
                    continue;
                };
                raws.extend(table.iter().map(|(key, value)| Raw {
                    line: line_of(text, &spans, &[name, key]),
                    context: context.clone(),
                    key,
                    value,
                }));
                continue;
            }
            for (plugin, inner) in table {
                let context = format!("panel.{plugin}");
                let line = line_of(text, &spans, &[name, plugin]);
                let known = panels.iter().any(|panel| panel.plugin == *plugin);
                match inner.as_table() {
                    Some(inner) if known => raws.extend(inner.iter().map(|(key, value)| Raw {
                        line: line_of(text, &spans, &[name, plugin, key]),
                        context: Context::Panel(plugin.clone()),
                        key,
                        value,
                    })),
                    Some(inner) if !inner.is_empty() => {
                        for key in inner.keys() {
                            let line = line_of(text, &spans, &[name, plugin, key]);
                            unknown(&mut report, line, &context);
                        }
                    }
                    _ => unknown(&mut report, line, &context),
                }
            }
        }

        let defaults = if leader == LEADER {
            defaults
        } else {
            match Self::defaults_with(leader, panels) {
                Ok(moved) => moved,
                Err(error) => {
                    report.add(
                        line_of(text, &spans, &["leader"]),
                        DiagnosticKind::BadLeader,
                        format!("лидер «{leader}» занят: {}, действует Space", error.reason),
                    );
                    leader = LEADER;
                    defaults
                }
            }
        };

        let mut lines: Vec<Line> = Vec::new();
        for raw in raws {
            let sequence = match Sequence::parse(raw.key, leader) {
                Ok(sequence) => sequence,
                Err(error) => {
                    let kind = match error {
                        SequenceError::TooLong(_) => DiagnosticKind::TooLong,
                        SequenceError::Empty | SequenceError::Chord(_) => DiagnosticKind::BadChord,
                    };
                    report.add(raw.line, kind, error.to_string());
                    continue;
                }
            };
            let Some(op) = op(raw.value) else {
                report.add(raw.line, DiagnosticKind::BadValue, BAD_VALUE);
                continue;
            };
            if let Op::Bind { action, args } = &op {
                match check.check(&raw.context, action, args.as_ref()) {
                    Ok(()) => {}
                    Err(ActionError::Unknown) => {
                        report.add(
                            raw.line,
                            DiagnosticKind::UnknownAction,
                            format!("неизвестное действие «{action}»"),
                        );
                        continue;
                    }
                    Err(ActionError::Args(why)) => {
                        report.add(
                            raw.line,
                            DiagnosticKind::BadArgs,
                            format!("аргументы действия «{action}» отвергнуты: {why}"),
                        );
                        continue;
                    }
                }
            }
            lines.push(Line {
                line: raw.line,
                context: raw.context,
                sequence,
                op,
            });
        }
        lines.sort_by_key(|line| line.line);

        // Строки одного контекста с одной последовательностью или с началом другой отключаются
        // обе.
        // Снятие умолчания началу другой строки не мешает: так освобождают место под неё.
        let clash = |a: &Line, b: &Line| {
            let binds = matches!((&a.op, &b.op), (Op::Bind { .. }, Op::Bind { .. }));
            let (a, b) = (a.sequence.chords(), b.sequence.chords());
            a == b || (binds && (a.starts_with(b) || b.starts_with(a)))
        };
        let mut disabled = vec![false; lines.len()];
        for (i, first) in lines.iter().enumerate() {
            for (j, second) in lines.iter().enumerate().skip(i + 1) {
                if first.context == second.context && clash(first, second) {
                    for (index, own, other) in [(i, first, second), (j, second, first)] {
                        if !disabled[index] {
                            disabled[index] = true;
                            report.add(
                                own.line,
                                DiagnosticKind::Conflict,
                                format!("конфликт со строкой {}", other.line),
                            );
                        }
                    }
                }
            }
        }
        let lines: Vec<Line> = lines
            .into_iter()
            .zip(disabled)
            .filter_map(|(line, off)| (!off).then_some(line))
            .collect();

        // Строка с той же последовательностью и контекстом заменяет умолчание, `false` снимает.
        let mut bindings: Vec<Binding> = defaults
            .bindings()
            .iter()
            .filter(|binding| {
                lines.iter().all(|line| {
                    line.context != binding.context || line.sequence != binding.sequence
                })
            })
            .cloned()
            .collect();
        let kept = bindings.len();
        for line in lines {
            let Op::Bind { action, args } = line.op else {
                continue;
            };
            // Умолчание, начало которого совпало со строкой, остаётся: строка отключается.
            let crossed = bindings[..kept].iter().find(|binding| {
                let (a, b) = (binding.sequence.chords(), line.sequence.chords());
                binding.context == line.context && (a.starts_with(b) || b.starts_with(a))
            });
            if let Some(crossed) = crossed {
                report.add(
                    line.line,
                    DiagnosticKind::Conflict,
                    format!(
                        "пересекается с привязкой по умолчанию «{}»",
                        crossed.sequence
                    ),
                );
                continue;
            }
            bindings.push(Binding {
                context: line.context,
                sequence: line.sequence,
                action,
                args,
                source: Source::File,
            });
        }

        report.0.sort_by_key(|diagnostic| diagnostic.line);
        Ok(self.next(leader, bindings, report.0))
    }

    fn next(&self, leader: Chord, bindings: Vec<Binding>, diagnostics: Vec<Diagnostic>) -> Self {
        let same = leader == self.leader() && bindings == self.bindings();
        let version = self.version() + u64::from(!same);
        match Self::build(version, leader, bindings, Vec::new()) {
            Ok(keymap) => keymap.with_diagnostics(diagnostics),
            // Пересечений после проверок выше нет; если проверка их пропустила, действует
            // прежняя таблица.
            Err((_, overlap)) => self.rejected(overlap_reason(&overlap)),
        }
    }
}
