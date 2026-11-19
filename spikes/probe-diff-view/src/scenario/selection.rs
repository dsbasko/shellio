//! Оракул сценария `selection`: `v` с `j` и `k`, протяжка мышью и `Shift`-щелчок дают диапазон
//! адресов одной стороны, в «Рядом» щелчок по половине задаёт сторону и строку, выделение
//! переживает перенос, раскрытие треда и уход строк за экран. Ожидаемые адреса выводятся из
//! сырого diff по строкам показа стимула, а не из состояния модели.

use probe_diff_view::model::state::Selection;
use probe_diff_view::model::{LineAddr, Mode, Side};

use super::contract::{Check, Kind, Raw};

/// Строк контекста подряд, внутри которых идут шаги выделения: `v j j`, `k`, протяжка и
/// `Shift`-щелчок на 3 строки ниже начала не выходят из участка.
pub const CONTEXT_RUN: usize = 4;

/// Первая строка показа не раньше `from`, с которой идут `CONTEXT_RUN` контекстных строк
/// одного ханка.
pub fn context_run(raw: &Raw, mode: Mode, from: usize) -> Option<usize> {
    let context = |row: usize| {
        let lines = raw.row_lines(mode, row);
        !lines.is_empty()
            && lines
                .iter()
                .all(|&index| raw.lines[index].kind == Kind::Context)
    };
    let hunk = |row: usize| raw.row_lines(mode, row).first().map(|&i| raw.lines[i].hunk);
    (from..=raw.rows(mode).checked_sub(CONTEXT_RUN)?).find(|&row| {
        (row..row + CONTEXT_RUN).all(context)
            && (row..row + CONTEXT_RUN).all(|other| hunk(other) == hunk(row))
    })
}

/// Адрес строки показа на стороне `side` по сырому diff.
pub fn addr_at(raw: &Raw, mode: Mode, row: usize, side: Side) -> Option<LineAddr> {
    raw.row_addrs(mode, row)
        .into_iter()
        .find(|addr| addr.side == side)
}

/// Выделение стороны `side` от строки показа `tail` до `head` по сырому diff.
pub fn expected(raw: &Raw, mode: Mode, side: Side, tail: usize, head: usize) -> Option<Selection> {
    Some(Selection {
        side,
        tail: addr_at(raw, mode, tail, side)?.line,
        head: addr_at(raw, mode, head, side)?.line,
    })
}

/// Выделение модели против ожидаемого.
pub fn check(step: &str, expected: Option<Selection>, model: Option<Selection>) -> Check {
    Check::new(
        step,
        expected == model,
        format!("модель {model:?}, ожидалось {expected:?}"),
    )
}

/// Курсор модели против ожидаемого адреса: щелчок по половине задаёт сторону и строку.
pub fn check_cursor(step: &str, expected: Option<LineAddr>, model: Option<LineAddr>) -> Check {
    Check::new(
        step,
        expected.is_some() && expected == model,
        format!("курсор модели {model:?}, ожидался {expected:?}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIFF: &str = "@@ -1,7 +1,7 @@\n a\n-b\n+B\n c\n d\n e\n f\n g\n";

    #[test]
    fn context_run_skips_changes_and_short_runs() {
        let raw = Raw::parse(DIFF.as_bytes()).expect("разбор");
        assert_eq!(context_run(&raw, Mode::Unified, 0), Some(3));
        assert_eq!(context_run(&raw, Mode::SideBySide, 0), Some(2));
        assert_eq!(context_run(&raw, Mode::Unified, 5), None);
    }

    #[test]
    fn expected_selection_uses_addresses_of_one_side() {
        let raw = Raw::parse(DIFF.as_bytes()).expect("разбор");
        let selection = expected(&raw, Mode::Unified, Side::New, 3, 5).expect("выделение");
        assert_eq!(
            selection,
            Selection {
                side: Side::New,
                tail: 3,
                head: 5
            }
        );
        assert!(check("v j j", Some(selection), Some(selection)).ok);
        let other = Selection {
            side: Side::Old,
            ..selection
        };
        assert!(!check("v j j", Some(selection), Some(other)).ok);
        assert!(!check("v j j", Some(selection), None).ok);
        assert_eq!(expected(&raw, Mode::Unified, Side::New, 1, 3), None);
    }

    #[test]
    fn half_click_cursor_needs_side_and_line() {
        let raw = Raw::parse(DIFF.as_bytes()).expect("разбор");
        let old = addr_at(&raw, Mode::SideBySide, 1, Side::Old);
        assert_eq!(
            old,
            Some(LineAddr {
                side: Side::Old,
                line: 2
            })
        );
        assert!(check_cursor("half", old, old).ok);
        let new = addr_at(&raw, Mode::SideBySide, 1, Side::New);
        assert!(!check_cursor("half", old, new).ok);
        assert!(!check_cursor("half", None, None).ok);
    }
}
