//! Общие части сценариев контракта корректности (задача 14): оракул адресов, независимый от
//! модели, — наивный разбор сырого текста unified diff одного файла, пары «Рядом» по правилу
//! плана и начала изменений, — и запись проверки для `result.json`.
//!
//! Модель строит строки показа своим разбором (`model::parse`, `model::rows`). Оракул их не
//! вызывает: ожидаемые адреса выводятся заново из байтов diff, иначе проверка сравнивала бы код
//! сам с собой.

use serde::Serialize;

use probe_diff_view::model::{LineAddr, Mode, Side};

/// Вид строки сырого diff.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Kind {
    Context,
    Removed,
    Added,
}

/// Строка тела ханка: вид, номера сторон и номер ханка.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct RawLine {
    pub kind: Kind,
    pub old: Option<u32>,
    pub new: Option<u32>,
    pub hunk: usize,
}

impl RawLine {
    /// Адрес строки на стороне `side`, если он у неё есть.
    pub fn addr(&self, side: Side) -> Option<LineAddr> {
        let line = match side {
            Side::Old => self.old,
            Side::New => self.new,
        }?;
        Some(LineAddr { side, line })
    }

    /// Все адреса строки: у контекстной два.
    pub fn addrs(&self) -> Vec<LineAddr> {
        Side::BOTH
            .into_iter()
            .filter_map(|side| self.addr(side))
            .collect()
    }
}

/// Пара «Рядом»: индексы строк сырого diff старой и новой половины.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct RawPair {
    pub old: Option<usize>,
    pub new: Option<usize>,
}

/// Наивный разбор сырого diff одного файла: строки тел ханков по порядку.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Raw {
    pub lines: Vec<RawLine>,
    pub pairs: Vec<RawPair>,
}

/// Начало и число строк стороны в заголовке ханка.
type Span = (u32, u32);

fn number(text: &str) -> Result<Span, String> {
    let mut parts = text.splitn(2, ',');
    let start = parts
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or_else(|| format!("нет начала в «{text}»"))?;
    let count = match parts.next() {
        Some(value) => value
            .parse::<u32>()
            .map_err(|_| format!("нет числа строк в «{text}»"))?,
        None => 1,
    };
    Ok((start, count))
}

/// Заголовок ханка `@@ -a[,b] +c[,d] @@`: начала и числа строк сторон.
fn hunk_header(line: &str) -> Result<(Span, Span), String> {
    let mut words = line.split(' ');
    let (Some("@@"), Some(old), Some(new)) = (words.next(), words.next(), words.next()) else {
        return Err(format!("не заголовок ханка: «{line}»"));
    };
    let old = old
        .strip_prefix('-')
        .ok_or_else(|| format!("нет старой стороны: «{line}»"))?;
    let new = new
        .strip_prefix('+')
        .ok_or_else(|| format!("нет новой стороны: «{line}»"))?;
    Ok((number(old)?, number(new)?))
}

impl Raw {
    /// Разбирает текст `git diff` одного файла. Заголовки файла до первого ханка пропускаются,
    /// тело ханка читается по числам строк заголовка, `\ No newline` не строка.
    pub fn parse(diff: &[u8]) -> Result<Raw, String> {
        let text = std::str::from_utf8(diff).map_err(|err| format!("diff не UTF-8: {err}"))?;
        let mut lines = Vec::new();
        let mut hunk: Option<usize> = None;
        let (mut old, mut new) = (0u32, 0u32);
        let (mut old_left, mut new_left) = (0u32, 0u32);
        for line in text.split('\n') {
            if old_left > 0 || new_left > 0 {
                let Some(index) = hunk else {
                    return Err("тело ханка без заголовка".to_owned());
                };
                let (kind, body_old, body_new) = match line.as_bytes().first() {
                    Some(b' ') if old_left > 0 && new_left > 0 => {
                        old_left -= 1;
                        new_left -= 1;
                        (Kind::Context, Some(old), Some(new))
                    }
                    Some(b'-') if old_left > 0 => {
                        old_left -= 1;
                        (Kind::Removed, Some(old), None)
                    }
                    Some(b'+') if new_left > 0 => {
                        new_left -= 1;
                        (Kind::Added, None, Some(new))
                    }
                    Some(b'\\') => continue,
                    _ => return Err(format!("строка тела ханка {index} не по счёту: «{line}»")),
                };
                if body_old.is_some() {
                    old += 1;
                }
                if body_new.is_some() {
                    new += 1;
                }
                lines.push(RawLine {
                    kind,
                    old: body_old,
                    new: body_new,
                    hunk: index,
                });
                continue;
            }
            if line.starts_with("@@") {
                let ((old_start, old_count), (new_start, new_count)) = hunk_header(line)?;
                hunk = Some(hunk.map_or(0, |index| index + 1));
                old = if old_count == 0 {
                    old_start + 1
                } else {
                    old_start
                };
                new = if new_count == 0 {
                    new_start + 1
                } else {
                    new_start
                };
                old_left = old_count;
                new_left = new_count;
            }
        }
        if old_left > 0 || new_left > 0 {
            return Err(format!(
                "diff оборван: осталось строк {old_left} старой и {new_left} новой стороны"
            ));
        }
        let pairs = side_pairs(&lines);
        Ok(Raw { lines, pairs })
    }

    /// Строк показа в режиме.
    pub fn rows(&self, mode: Mode) -> usize {
        match mode {
            Mode::Unified => self.lines.len(),
            Mode::SideBySide => self.pairs.len(),
        }
    }

    /// Строки сырого diff в строке показа `row`: в unified одна, в «Рядом» до двух.
    pub fn row_lines(&self, mode: Mode, row: usize) -> Vec<usize> {
        match mode {
            Mode::Unified => (row < self.lines.len())
                .then_some(row)
                .into_iter()
                .collect(),
            Mode::SideBySide => self
                .pairs
                .get(row)
                .map(|pair| {
                    let mut lines: Vec<usize> = pair.old.into_iter().chain(pair.new).collect();
                    lines.dedup();
                    lines
                })
                .unwrap_or_default(),
        }
    }

    /// Адреса строки показа: все адреса её строк сырого diff.
    pub fn row_addrs(&self, mode: Mode, row: usize) -> Vec<LineAddr> {
        let mut addrs: Vec<LineAddr> = self
            .row_lines(mode, row)
            .into_iter()
            .flat_map(|index| self.lines[index].addrs())
            .collect();
        addrs.dedup();
        addrs
    }

    /// Строка показа, в которой есть адрес.
    pub fn row_of(&self, mode: Mode, addr: LineAddr) -> Option<usize> {
        (0..self.rows(mode)).find(|&row| self.row_addrs(mode, row).contains(&addr))
    }

    /// Строки показа, с которых начинается изменение: первая строка сплошного участка
    /// удалений и добавлений внутри ханка.
    pub fn change_starts(&self, mode: Mode) -> Vec<usize> {
        let starts = self.lines.iter().enumerate().filter(|(index, line)| {
            line.kind != Kind::Context
                && (*index == 0 || {
                    let previous = &self.lines[index - 1];
                    previous.kind == Kind::Context || previous.hunk != line.hunk
                })
        });
        let mut rows: Vec<usize> = starts
            .filter_map(|(index, _)| match mode {
                Mode::Unified => Some(index),
                Mode::SideBySide => self
                    .pairs
                    .iter()
                    .position(|pair| pair.old == Some(index) || pair.new == Some(index)),
            })
            .collect();
        rows.dedup();
        rows
    }

    /// Начало следующего (`forward`) или предыдущего изменения от строки показа `row`.
    pub fn change_from(&self, mode: Mode, row: usize, forward: bool) -> Option<usize> {
        let starts = self.change_starts(mode);
        if forward {
            starts.into_iter().find(|&start| start > row)
        } else {
            starts.into_iter().rev().find(|&start| start < row)
        }
    }
}

/// Пары «Рядом» по правилу плана: контекстная строка — пара сама с собой, в сплошном участке
/// изменений `i`-я удалённая строка в паре с `i`-й добавленной, у лишних одна сторона пустая.
pub fn side_pairs(lines: &[RawLine]) -> Vec<RawPair> {
    let mut pairs = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        if line.kind == Kind::Context {
            pairs.push(RawPair {
                old: Some(index),
                new: Some(index),
            });
            index += 1;
            continue;
        }
        let (mut removed, mut added) = (Vec::new(), Vec::new());
        while index < lines.len()
            && lines[index].kind != Kind::Context
            && lines[index].hunk == line.hunk
        {
            match lines[index].kind {
                Kind::Removed => removed.push(index),
                Kind::Added => added.push(index),
                Kind::Context => {}
            }
            index += 1;
        }
        for slot in 0..removed.len().max(added.len()) {
            pairs.push(RawPair {
                old: removed.get(slot).copied(),
                new: added.get(slot).copied(),
            });
        }
    }
    pairs
}

/// Итог одной проверки контракта: имя шага, выполнено ли условие и сырые значения.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Check {
    pub step: String,
    pub ok: bool,
    pub detail: String,
}

impl Check {
    pub fn new(step: impl Into<String>, ok: bool, detail: impl Into<String>) -> Check {
        Check {
            step: step.into(),
            ok,
            detail: detail.into(),
        }
    }
}

/// Вердикт части контракта по её проверкам: `fail`, если не выполнена хоть одна, с первыми
/// расхождениями в причине.
pub fn judge(part: &str, checks: &[Check]) -> (crate::results::Verdict, String) {
    use crate::results::Verdict;
    let failed: Vec<&Check> = checks.iter().filter(|check| !check.ok).collect();
    if checks.is_empty() {
        return (Verdict::Fail, format!("{part}: нет проверок"));
    }
    if failed.is_empty() {
        return (
            Verdict::Pass,
            format!("{part}: выполнено {} проверок", checks.len()),
        );
    }
    let first: Vec<String> = failed
        .iter()
        .take(3)
        .map(|check| format!("{}: {}", check.step, check.detail))
        .collect();
    (
        Verdict::Fail,
        format!(
            "{part}: не выполнено {} из {}; {}",
            failed.len(),
            checks.len(),
            first.join("; ")
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIFF: &str = "diff --git a/x.rs b/x.rs\n--- a/x.rs\n+++ b/x.rs\n\
@@ -1,4 +1,4 @@\n a\n-b\n-c\n+B\n d\n+e\n@@ -10,3 +11,2 @@\n x\n-y\n z\n\\ No newline at end of file\n";

    fn addr(side: Side, line: u32) -> LineAddr {
        LineAddr { side, line }
    }

    #[test]
    fn raw_parse_numbers_lines_by_hunk_headers() {
        let raw = Raw::parse(DIFF.as_bytes()).expect("разбор");
        let kinds: Vec<Kind> = raw.lines.iter().map(|line| line.kind).collect();
        use Kind::*;
        assert_eq!(
            kinds,
            [
                Context, Removed, Removed, Added, Context, Added, Context, Removed, Context
            ]
        );
        assert_eq!(raw.lines[1].old, Some(2));
        assert_eq!(raw.lines[3].new, Some(2));
        assert_eq!(raw.lines[5].new, Some(4));
        assert_eq!((raw.lines[6].old, raw.lines[6].new), (Some(10), Some(11)));
        assert_eq!(raw.lines[7].hunk, 1);
        assert_eq!((raw.lines[8].old, raw.lines[8].new), (Some(12), Some(12)));
    }

    #[test]
    fn added_file_starts_at_line_one() {
        let raw = Raw::parse(b"@@ -0,0 +1,2 @@\n+a\n+b\n").expect("разбор");
        assert_eq!(raw.lines[0].new, Some(1));
        assert_eq!(raw.lines[1].new, Some(2));
        assert_eq!(raw.pairs.len(), 2);
        assert!(raw.pairs.iter().all(|pair| pair.old.is_none()));
    }

    #[test]
    fn truncated_diff_is_an_error() {
        assert!(Raw::parse(b"@@ -1,3 +1,3 @@\n a\n").is_err());
        assert!(Raw::parse(b"@@ -1,1 +1,1 @@\n?a\n").is_err());
    }

    #[test]
    fn side_pairs_match_removed_and_added_by_order() {
        let raw = Raw::parse(DIFF.as_bytes()).expect("разбор");
        assert_eq!(
            raw.pairs,
            [
                RawPair {
                    old: Some(0),
                    new: Some(0)
                },
                RawPair {
                    old: Some(1),
                    new: Some(3)
                },
                RawPair {
                    old: Some(2),
                    new: None
                },
                RawPair {
                    old: Some(4),
                    new: Some(4)
                },
                RawPair {
                    old: None,
                    new: Some(5)
                },
                RawPair {
                    old: Some(6),
                    new: Some(6)
                },
                RawPair {
                    old: Some(7),
                    new: None
                },
                RawPair {
                    old: Some(8),
                    new: Some(8)
                },
            ]
        );
        assert_eq!(
            raw.row_addrs(Mode::SideBySide, 1),
            [addr(Side::Old, 2), addr(Side::New, 2)]
        );
        assert_eq!(raw.row_of(Mode::SideBySide, addr(Side::New, 4)), Some(4));
        assert_eq!(raw.row_of(Mode::Unified, addr(Side::Old, 11)), Some(7));
    }

    #[test]
    fn change_starts_and_jumps_in_both_modes() {
        let raw = Raw::parse(DIFF.as_bytes()).expect("разбор");
        assert_eq!(raw.change_starts(Mode::Unified), [1, 5, 7]);
        assert_eq!(raw.change_starts(Mode::SideBySide), [1, 4, 6]);
        assert_eq!(raw.change_from(Mode::Unified, 4, true), Some(5));
        assert_eq!(raw.change_from(Mode::Unified, 4, false), Some(1));
        assert_eq!(raw.change_from(Mode::SideBySide, 5, true), Some(6));
        assert_eq!(raw.change_from(Mode::SideBySide, 0, false), None);
    }

    #[test]
    fn judge_fails_on_any_unmet_check_and_on_none() {
        use crate::results::Verdict;
        let checks = [Check::new("a", true, ""), Check::new("b", false, "x")];
        assert_eq!(judge("part", &checks).0, Verdict::Fail);
        assert!(judge("part", &checks).1.contains("b: x"));
        assert_eq!(judge("part", &checks[..1]).0, Verdict::Pass);
        assert_eq!(judge("part", &[]).0, Verdict::Fail);
    }
    /// Адреса строк показа модели и оракула совпадают на фикстурах сценариев: иначе
    /// расхождение в GUI-прогоне было бы ошибкой оракула, а не выводом о виде.
    #[test]
    fn oracle_agrees_with_the_model_on_scenario_fixtures() {
        use probe_diff_view::fixtures::{FixtureName, generate};
        use probe_diff_view::model::parse;
        use probe_diff_view::worker::Cancel;
        let fixtures = [
            generate(FixtureName::Threads),
            generate(FixtureName::Wrap),
            super::super::pairs::pair31(),
        ];
        for fixture in fixtures {
            let raw = Raw::parse(&fixture.diff).expect("разбор оракула");
            let diff = parse::parse(fixture.diff.clone(), &Cancel::never()).expect("разбор модели");
            let body = diff.file(0).and_then(|file| file.text()).expect("текст");
            for mode in [Mode::Unified, Mode::SideBySide] {
                assert_eq!(
                    raw.rows(mode),
                    body.row_count(mode),
                    "{} {mode:?}",
                    fixture.name
                );
                for row in 0..raw.rows(mode) {
                    let halves = body.row(mode, row as u32).expect("строка");
                    let mut model: Vec<LineAddr> = Vec::new();
                    for half in Side::BOTH {
                        let Some(line) = halves.half(half).and_then(|index| body.line(index))
                        else {
                            continue;
                        };
                        model.extend(Side::BOTH.into_iter().filter_map(|side| {
                            line.line(side).map(|line| LineAddr { side, line })
                        }));
                    }
                    model.sort_by_key(|addr| (addr.side == Side::New, addr.line));
                    model.dedup();
                    let mut oracle = raw.row_addrs(mode, row);
                    oracle.sort_by_key(|addr| (addr.side == Side::New, addr.line));
                    assert_eq!(model, oracle, "{} {mode:?} строка {row}", fixture.name);
                }
            }
        }
    }
}
