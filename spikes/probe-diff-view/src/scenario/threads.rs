//! Оракул сценария `threads`: `c` на строке или диапазоне открывает черновик в одном
//! переиспользуемом поле, `Cmd-Enter` сохраняет его в модель, блок треда стоит сразу после
//! последней строки якоря, у диапазона через границу ханка — после последней строки диапазона.
//! За 100 циклов открытия и закрытия черновика создано одно поле.

use probe_diff_view::model::threads::ThreadAnchor;
use probe_diff_view::model::{LineAddr, Mode, Side};

use super::anchor::Drawn;
use super::contract::{Check, Raw};

/// Циклов открытия и закрытия черновика.
pub const DRAFT_CYCLES: usize = 100;
/// Допуск стыка блока и строки, точки.
const JOINT: f64 = 0.5;

/// Последняя строка показа якоря по сырому diff: наибольшая строка среди адресов стороны якоря
/// от `start_line` до `line`, которые есть в diff.
pub fn last_row(raw: &Raw, mode: Mode, anchor: &ThreadAnchor) -> Option<usize> {
    let (from, to) = (
        anchor.start_line.min(anchor.line),
        anchor.start_line.max(anchor.line),
    );
    (from..=to)
        .filter_map(|line| {
            raw.row_of(
                mode,
                LineAddr {
                    side: anchor.side,
                    line,
                },
            )
        })
        .max()
}

/// Ханки начала и конца диапазона якоря различаются.
pub fn crosses_hunk(raw: &Raw, anchor: &ThreadAnchor) -> bool {
    let hunk = |line: u32| {
        raw.lines
            .iter()
            .find(|raw_line| {
                raw_line.addr(anchor.side)
                    == Some(LineAddr {
                        side: anchor.side,
                        line,
                    })
            })
            .map(|raw_line| raw_line.hunk)
    };
    matches!((hunk(anchor.start_line), hunk(anchor.line)), (Some(a), Some(b)) if a != b)
}

/// Граница ханков на стороне `side`: последняя строка показа ханка и первая следующего, обе с
/// адресом этой стороны. Первая граница не раньше строки `from`.
pub fn hunk_boundary(raw: &Raw, mode: Mode, side: Side, from: usize) -> Option<(usize, usize)> {
    let hunk_of = |row: usize| {
        raw.row_lines(mode, row)
            .into_iter()
            .find(|&index| raw.lines[index].addr(side).is_some())
            .map(|index| raw.lines[index].hunk)
    };
    (from..raw.rows(mode).saturating_sub(1)).find_map(|row| {
        let (here, next) = (hunk_of(row)?, hunk_of(row + 1)?);
        (here != next).then_some((row, row + 1))
    })
}

/// Блок треда начинается там, где кончается нарисованный текст последней строки якоря.
pub fn check_block(step: &str, text: Option<Drawn>, block: Option<Drawn>) -> Check {
    let detail = format!("текст {text:?}, блок {block:?}");
    let ok = match (text, block) {
        (Some(text), Some(block)) => {
            block.row == text.row && (block.top - (text.top + text.height)).abs() <= JOINT
        }
        _ => false,
    };
    Check::new(step, ok, detail)
}

/// Поле черновика одно: создано за процесс одно и одно живо.
pub fn check_field(step: &str, created: usize, live: usize) -> Check {
    Check::new(
        step,
        created == 1 && live == 1,
        format!("создано полей черновика {created}, живых {live}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIFF: &str = "@@ -1,4 +1,4 @@\n a\n-b\n+B\n c\n d\n@@ -20,3 +20,3 @@\n x\n-y\n+Y\n z\n";

    fn anchor(side: Side, start_line: u32, line: u32) -> ThreadAnchor {
        ThreadAnchor {
            path: "x.rs".to_owned(),
            side,
            start_line,
            line,
        }
    }

    #[test]
    fn last_row_of_a_range_across_the_hunk_boundary() {
        let raw = Raw::parse(DIFF.as_bytes()).expect("разбор");
        let range = anchor(Side::New, 3, 21);
        assert!(crosses_hunk(&raw, &range));
        assert_eq!(last_row(&raw, Mode::Unified, &range), Some(7));
        assert_eq!(last_row(&raw, Mode::SideBySide, &range), Some(5));
        let single = anchor(Side::Old, 2, 2);
        assert!(!crosses_hunk(&raw, &single));
        assert_eq!(last_row(&raw, Mode::Unified, &single), Some(1));
    }

    #[test]
    fn hunk_boundary_on_a_side() {
        let raw = Raw::parse(DIFF.as_bytes()).expect("разбор");
        assert_eq!(
            hunk_boundary(&raw, Mode::Unified, Side::New, 0),
            Some((4, 5))
        );
        assert_eq!(
            hunk_boundary(&raw, Mode::SideBySide, Side::Old, 0),
            Some((3, 4))
        );
        assert_eq!(hunk_boundary(&raw, Mode::Unified, Side::New, 5), None);
    }

    #[test]
    fn block_must_start_at_the_end_of_the_text_row() {
        let text = Drawn {
            generation: 1,
            row: 4,
            top: 100.0,
            height: 15.0,
        };
        let block = Drawn { top: 115.2, ..text };
        assert!(check_block("b", Some(text), Some(block)).ok);
        assert!(
            !check_block(
                "b",
                Some(text),
                Some(Drawn {
                    top: 130.0,
                    ..block
                })
            )
            .ok
        );
        assert!(!check_block("b", Some(text), Some(Drawn { row: 5, ..block })).ok);
        assert!(!check_block("b", Some(text), None).ok);
    }

    #[test]
    fn one_draft_field_for_all_cycles() {
        assert!(check_field("f", 1, 1).ok);
        assert!(!check_field("f", 2, 1).ok);
        assert!(!check_field("f", 1, 0).ok);
    }
}
