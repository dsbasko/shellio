//! Строки показа двух раскладок и отображение адресов в строки показа и обратно.
//!
//! Заголовок ханка — не строка показа: строк показа unified ровно столько, сколько строк diff,
//! а границу ханка вид рисует по [`TextBody::starts_hunk`]. Строки «Рядом» строятся по ханкам:
//! непрерывный блок удалений и вставок делится на пары по порядку, остаток длинной стороны
//! получает пустую половину. Маркер `\ No newline at end of file` — признак строки, а не строка,
//! поэтому блок он не разрывает.

use super::{DiffLine, Hunk, LineAddr, LineKind, Side, TextBody};
use crate::worker::Cancel;

/// Отмена проверяется раз в столько строк показа.
const CANCEL_CHECK_ROWS: usize = 4096;
const NO_LINE: u32 = u32::MAX;

/// Раскладка показа.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mode {
    Unified,
    SideBySide,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cancelled;

/// Строка показа: индексы строк diff в старой и новой половине. В unified у контекстной строки
/// обе половины — одна строка diff, у удалённой есть только старая, у добавленной только новая.
/// В «Рядом» половины — пара старой и новой строки модели.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row {
    pub old: Option<u32>,
    pub new: Option<u32>,
}

impl Row {
    pub fn half(&self, side: Side) -> Option<u32> {
        match side {
            Side::Old => self.old,
            Side::New => self.new,
        }
    }

    /// Первая строка diff строки показа в порядке unified.
    pub fn first(&self) -> Option<u32> {
        match (self.old, self.new) {
            (Some(old), Some(new)) => Some(old.min(new)),
            (old, new) => old.or(new),
        }
    }

    pub fn contains(&self, line: u32) -> bool {
        self.old == Some(line) || self.new == Some(line)
    }
}

/// Результат поиска по адресу. `exact == false` — адреса в diff нет и взята ближайшая строка той
/// же стороны: для якоря прокрутки это допустимо, для якоря треда нет (ADR-0015).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Located {
    pub index: u32,
    pub exact: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Pair {
    old: u32,
    new: u32,
}

impl Pair {
    fn row(self) -> Row {
        let half = |line| (line != NO_LINE).then_some(line);
        Row {
            old: half(self.old),
            new: half(self.new),
        }
    }
}

/// Строки «Рядом» и индексы адресов одного текстового файла. Строки unified совпадают со
/// строками diff и отдельно не хранятся.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Rows {
    split: Vec<Pair>,
    line_to_split: Vec<u32>,
    /// Индексы строк diff со старым адресом по возрастанию номера.
    old_lines: Vec<u32>,
    /// Индексы строк diff с новым адресом по возрастанию номера.
    new_lines: Vec<u32>,
}

impl Rows {
    /// Байты буферов строк «Рядом» и индексов адресов по ёмкости.
    pub(super) fn heap_bytes(&self) -> u64 {
        use crate::estimate::vec_bytes;
        vec_bytes(&self.split)
            + vec_bytes(&self.line_to_split)
            + vec_bytes(&self.old_lines)
            + vec_bytes(&self.new_lines)
    }

    fn side(&self, side: Side) -> &[u32] {
        match side {
            Side::Old => &self.old_lines,
            Side::New => &self.new_lines,
        }
    }
}

pub(super) fn build(
    lines: &[DiffLine],
    hunks: &[Hunk],
    cancel: &Cancel,
) -> Result<Rows, Cancelled> {
    let mut split: Vec<Pair> = Vec::with_capacity(lines.len());
    let mut line_to_split = vec![0u32; lines.len()];
    let mut removed: Vec<u32> = Vec::new();
    let mut added: Vec<u32> = Vec::new();
    let mut push = |split: &mut Vec<Pair>, pair: Pair| -> Result<(), Cancelled> {
        let row = split.len() as u32;
        for line in [pair.old, pair.new] {
            if let Some(slot) = line_to_split.get_mut(line as usize) {
                *slot = row;
            }
        }
        split.push(pair);
        if split.len().is_multiple_of(CANCEL_CHECK_ROWS) && cancel.is_cancelled() {
            return Err(Cancelled);
        }
        Ok(())
    };
    for hunk in hunks {
        let start = hunk.lines.start;
        let hunk_lines = lines
            .get(start as usize..hunk.lines.end as usize)
            .unwrap_or_default();
        let mut offset = 0;
        while let Some(line) = hunk_lines.get(offset) {
            let index = start + offset as u32;
            if line.kind() == LineKind::Context {
                push(
                    &mut split,
                    Pair {
                        old: index,
                        new: index,
                    },
                )?;
                offset += 1;
                continue;
            }
            removed.clear();
            added.clear();
            while let Some(line) = hunk_lines.get(offset)
                && line.kind() != LineKind::Context
            {
                let index = start + offset as u32;
                match line.kind() {
                    LineKind::Removed => removed.push(index),
                    LineKind::Added => added.push(index),
                    LineKind::Context => {}
                }
                offset += 1;
            }
            for k in 0..removed.len().max(added.len()) {
                let pair = Pair {
                    old: removed.get(k).copied().unwrap_or(NO_LINE),
                    new: added.get(k).copied().unwrap_or(NO_LINE),
                };
                push(&mut split, pair)?;
            }
        }
    }
    split.shrink_to_fit();
    let side_lines = |side: Side| -> Vec<u32> {
        let mut found: Vec<u32> = (0..lines.len() as u32)
            .filter(|&index| lines.get(index as usize).is_some_and(|line| line.has(side)))
            .collect();
        found.shrink_to_fit();
        found
    };
    Ok(Rows {
        split,
        line_to_split,
        old_lines: side_lines(Side::Old),
        new_lines: side_lines(Side::New),
    })
}

impl TextBody {
    pub fn row_count(&self, mode: Mode) -> usize {
        match mode {
            Mode::Unified => self.lines.len(),
            Mode::SideBySide => self.rows.split.len(),
        }
    }

    pub fn row(&self, mode: Mode, row: u32) -> Option<Row> {
        match mode {
            Mode::Unified => {
                let line = self.line(row)?;
                let half = |side| line.has(side).then_some(row);
                Some(Row {
                    old: half(Side::Old),
                    new: half(Side::New),
                })
            }
            Mode::SideBySide => self.rows.split.get(row as usize).map(|pair| pair.row()),
        }
    }

    /// Строка показа, в которой стоит строка diff.
    pub fn row_of_line(&self, mode: Mode, line: u32) -> Option<u32> {
        match mode {
            Mode::Unified => ((line as usize) < self.lines.len()).then_some(line),
            Mode::SideBySide => self.rows.line_to_split.get(line as usize).copied(),
        }
    }

    /// Адрес половины строки показа.
    pub fn row_addr(&self, mode: Mode, row: u32, side: Side) -> Option<LineAddr> {
        let line = self.row(mode, row)?.half(side)?;
        self.line(line)?.addr(side)
    }

    /// Строка diff по точному адресу.
    pub fn line_of_addr(&self, addr: LineAddr) -> Option<u32> {
        self.locate(addr)
            .filter(|located| located.exact)
            .map(|located| located.index)
    }

    /// Строка diff по адресу или ближайшая строка той же стороны по номеру, при равенстве
    /// расстояний — предыдущая. `None`, если у стороны нет строк.
    pub fn locate(&self, addr: LineAddr) -> Option<Located> {
        let index = self.rows.side(addr.side);
        let number = |k: usize| {
            let line = *index.get(k)?;
            Some((line, self.line(line)?.line(addr.side)?))
        };
        let after = index.partition_point(|&line| {
            self.line(line)
                .and_then(|line| line.line(addr.side))
                .is_some_and(|number| number < addr.line)
        });
        let next = number(after);
        if let Some((line, found)) = next
            && found == addr.line
        {
            return Some(Located {
                index: line,
                exact: true,
            });
        }
        let prev = after.checked_sub(1).and_then(number);
        let nearest = match (prev, next) {
            (Some(prev), Some(next)) => {
                if addr.line - prev.1 <= next.1 - addr.line {
                    prev
                } else {
                    next
                }
            }
            (Some(only), None) | (None, Some(only)) => only,
            (None, None) => return None,
        };
        Some(Located {
            index: nearest.0,
            exact: false,
        })
    }

    /// Строка показа по адресу, ближайшая — если адреса в diff нет.
    pub fn row_of_addr(&self, mode: Mode, addr: LineAddr) -> Option<Located> {
        let located = self.locate(addr)?;
        Some(Located {
            index: self.row_of_line(mode, located.index)?,
            exact: located.exact,
        })
    }

    /// Строка показа другой раскладки для строки показа `row`. Из пары «Рядом» берётся
    /// половина `prefer`, а если она пуста — другая.
    pub fn convert_row(&self, row: u32, from: Mode, to: Mode, prefer: Side) -> Option<u32> {
        let row = self.row(from, row)?;
        let line = row.half(prefer).or(row.half(prefer.other()))?;
        self.row_of_line(to, line)
    }

    pub fn hunk_of_line(&self, line: u32) -> Option<u32> {
        let hunk = self.hunks.partition_point(|hunk| hunk.lines.end <= line);
        self.hunks
            .get(hunk)
            .filter(|hunk| hunk.lines.contains(&line))
            .map(|_| hunk as u32)
    }

    pub fn hunk_of_row(&self, mode: Mode, row: u32) -> Option<u32> {
        self.hunk_of_line(self.row(mode, row)?.first()?)
    }

    /// Строка показа открывает ханк: над ней вид рисует границу ханка.
    pub fn starts_hunk(&self, mode: Mode, row: u32) -> bool {
        let Some(first) = self.row(mode, row).and_then(|row| row.first()) else {
            return false;
        };
        self.hunk_of_line(first)
            .and_then(|hunk| self.hunks.get(hunk as usize))
            .is_some_and(|hunk| hunk.lines.start == first)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::model::parse::{Failure, parse};
    use crate::model::{Diff, LineKind};

    fn parse_lines(lines: &[&str]) -> Diff {
        let mut text = lines.join("\n");
        text.push('\n');
        match parse(Arc::from(text.as_bytes()), &Cancel::never()) {
            Ok(diff) => diff,
            Err(failure) => panic!("разбор не прошёл: {failure:?}"),
        }
    }

    fn text_of(diff: &Diff) -> &TextBody {
        diff.files()[0].text().unwrap()
    }

    fn split_rows(body: &TextBody) -> Vec<(Option<u32>, Option<u32>)> {
        (0..body.row_count(Mode::SideBySide) as u32)
            .map(|row| {
                let row = body.row(Mode::SideBySide, row).unwrap();
                (row.old, row.new)
            })
            .collect()
    }

    fn header(hunk: &str) -> Vec<&str> {
        vec!["diff --git a/f b/f", "--- a/f", "+++ b/f", hunk]
    }

    /// Два ханка: замена 3 на 1 с контекстом и замена 1 на 3.
    fn two_hunks() -> Diff {
        let mut lines = header("@@ -1,5 +1,3 @@");
        lines.extend([
            " a",
            "-b",
            "-c",
            "-d",
            "+B",
            " e",
            "@@ -21,2 +19,4 @@ fn f()",
        ]);
        lines.extend([" t", "-u", "+U1", "+U2", "+U3"]);
        parse_lines(&lines)
    }

    #[test]
    fn replacement_block_pairs_in_order_with_remainder() {
        let diff = two_hunks();
        let body = text_of(&diff);
        assert_eq!(
            split_rows(body),
            [
                (Some(0), Some(0)),
                (Some(1), Some(4)),
                (Some(2), None),
                (Some(3), None),
                (Some(5), Some(5)),
                (Some(6), Some(6)),
                (Some(7), Some(8)),
                (None, Some(9)),
                (None, Some(10)),
            ]
        );
        assert_eq!(body.row_count(Mode::Unified), 11);
    }

    #[test]
    fn interleaved_block_pairs_by_side_order() {
        let mut lines = header("@@ -1,2 +1,2 @@");
        lines.extend(["-a", "+x", "-b", "+y"]);
        let diff = parse_lines(&lines);
        assert_eq!(
            split_rows(text_of(&diff)),
            [(Some(0), Some(1)), (Some(2), Some(3))]
        );
    }

    #[test]
    fn pure_insertion_and_deletion_have_empty_half() {
        let mut lines = header("@@ -1,2 +1,1 @@");
        lines.extend([" a", "-b", "@@ -5,1 +4,2 @@", " e", "+f"]);
        let diff = parse_lines(&lines);
        assert_eq!(
            split_rows(text_of(&diff)),
            [
                (Some(0), Some(0)),
                (Some(1), None),
                (Some(2), Some(2)),
                (None, Some(3)),
            ]
        );
    }

    #[test]
    fn blocks_do_not_cross_hunks() {
        let mut lines = header("@@ -2 +1,0 @@");
        lines.extend(["-b", "@@ -4,0 +3 @@", "+X"]);
        let diff = parse_lines(&lines);
        assert_eq!(
            split_rows(text_of(&diff)),
            [(Some(0), None), (None, Some(1))]
        );
    }

    #[test]
    fn no_newline_marker_does_not_split_block() {
        let mut lines = header("@@ -1,2 +1,2 @@");
        lines.extend([
            " p",
            "-q",
            "\\ No newline at end of file",
            "+r",
            "\\ No newline at end of file",
        ]);
        let diff = parse_lines(&lines);
        assert_eq!(
            split_rows(text_of(&diff)),
            [(Some(0), Some(0)), (Some(1), Some(2))]
        );
    }

    #[test]
    fn unified_row_halves_follow_line_kind() {
        let diff = two_hunks();
        let body = text_of(&diff);
        for row in 0..body.row_count(Mode::Unified) as u32 {
            let halves = body.row(Mode::Unified, row).unwrap();
            let expected = match body.line(row).unwrap().kind() {
                LineKind::Context => (Some(row), Some(row)),
                LineKind::Removed => (Some(row), None),
                LineKind::Added => (None, Some(row)),
            };
            assert_eq!((halves.old, halves.new), expected, "строка {row}");
        }
        assert_eq!(body.row(Mode::Unified, 11), None);
        assert_eq!(body.row(Mode::SideBySide, 9), None);
    }

    #[test]
    fn addresses_round_trip_in_both_modes() {
        let diff = two_hunks();
        let body = text_of(&diff);
        for (index, line) in body.lines().iter().enumerate() {
            let index = index as u32;
            for side in Side::BOTH {
                let Some(addr) = line.addr(side) else {
                    continue;
                };
                assert_eq!(body.line_of_addr(addr), Some(index), "{addr:?}");
                for mode in [Mode::Unified, Mode::SideBySide] {
                    let located = body.row_of_addr(mode, addr).unwrap();
                    assert!(located.exact);
                    assert_eq!(body.row_of_line(mode, index), Some(located.index));
                    let row = body.row(mode, located.index).unwrap();
                    assert_eq!(row.half(side), Some(index), "{addr:?} {mode:?}");
                    assert_eq!(body.row_addr(mode, located.index, side), Some(addr));
                }
            }
        }
        for mode in [Mode::Unified, Mode::SideBySide] {
            for row in 0..body.row_count(mode) as u32 {
                let halves = body.row(mode, row).unwrap();
                for line in [halves.old, halves.new].into_iter().flatten() {
                    assert_eq!(body.row_of_line(mode, line), Some(row));
                }
            }
        }
    }

    #[test]
    fn rows_convert_between_modes() {
        let diff = two_hunks();
        let body = text_of(&diff);
        // Строка unified 4 — `+B`, в «Рядом» она в паре со `-b` (строка 1).
        assert_eq!(
            body.convert_row(4, Mode::Unified, Mode::SideBySide, Side::New),
            Some(1)
        );
        assert_eq!(
            body.convert_row(1, Mode::SideBySide, Mode::Unified, Side::New),
            Some(4)
        );
        assert_eq!(
            body.convert_row(1, Mode::SideBySide, Mode::Unified, Side::Old),
            Some(1)
        );
        // У пары `-c` новая половина пуста: берётся старая.
        assert_eq!(
            body.convert_row(2, Mode::SideBySide, Mode::Unified, Side::New),
            Some(2)
        );
        for row in 0..body.row_count(Mode::Unified) as u32 {
            for prefer in Side::BOTH {
                let split = body
                    .convert_row(row, Mode::Unified, Mode::SideBySide, prefer)
                    .unwrap();
                assert!(body.row(Mode::SideBySide, split).unwrap().contains(row));
            }
        }
    }

    #[test]
    fn missing_address_gives_nearest_row() {
        let diff = two_hunks();
        let body = text_of(&diff);
        // Старая сторона: 1..=5 в первом ханке и 21..=22 во втором.
        let nearest = |line, mode| body.row_of_addr(mode, LineAddr::old(line)).unwrap();
        assert_eq!(
            nearest(12, Mode::Unified),
            Located {
                index: 5,
                exact: false
            }
        );
        assert_eq!(
            nearest(13, Mode::Unified),
            Located {
                index: 5,
                exact: false
            },
            "равное расстояние — предыдущая строка"
        );
        assert_eq!(
            nearest(14, Mode::Unified),
            Located {
                index: 6,
                exact: false
            }
        );
        assert_eq!(
            nearest(500, Mode::SideBySide),
            Located {
                index: 6,
                exact: false
            }
        );
        assert_eq!(body.line_of_addr(LineAddr::old(13)), None);
        // Новая сторона без строки 0: ближайшая — первая.
        assert_eq!(
            body.row_of_addr(Mode::Unified, LineAddr::new(0)),
            Some(Located {
                index: 0,
                exact: false
            })
        );

        let mut lines = header("@@ -0,0 +1,2 @@");
        lines.extend(["+a", "+b"]);
        let added = parse_lines(&lines);
        assert_eq!(text_of(&added).locate(LineAddr::old(1)), None);
    }

    #[test]
    fn hunk_boundaries_in_both_modes() {
        let diff = two_hunks();
        let body = text_of(&diff);
        let starts = |mode| -> Vec<u32> {
            (0..body.row_count(mode) as u32)
                .filter(|&row| body.starts_hunk(mode, row))
                .collect()
        };
        assert_eq!(starts(Mode::Unified), [0, 6]);
        assert_eq!(starts(Mode::SideBySide), [0, 5]);
        assert_eq!(body.hunk_of_row(Mode::Unified, 5), Some(0));
        assert_eq!(body.hunk_of_row(Mode::SideBySide, 8), Some(1));
        assert_eq!(body.hunk_of_line(11), None);
    }

    #[test]
    fn cancelled_build_stops() {
        let mut lines = vec![
            "diff --git a/f b/f".to_owned(),
            "--- a/f".to_owned(),
            "+++ b/f".to_owned(),
        ];
        let count = 2 * CANCEL_CHECK_ROWS;
        lines.push(format!("@@ -1,{count} +1,{count} @@"));
        lines.extend((0..count).map(|i| format!(" {i}")));
        let text = lines.join("\n") + "\n";
        let diff = parse(Arc::from(text.as_bytes()), &Cancel::never()).unwrap();
        let body = text_of(&diff);
        assert_eq!(
            build(body.lines(), body.hunks(), &Cancel::cancelled()),
            Err(Cancelled)
        );
        assert_eq!(
            parse(Arc::from(text.as_bytes()), &Cancel::cancelled()).err(),
            Some(Failure::Cancelled)
        );
    }
}
