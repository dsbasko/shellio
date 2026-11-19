//! Оракул сценария `anchor`: адрес строки под верхним краем и её смещение из модели против
//! прямоугольника строки, который вид отдал при отрисовке, и фактического верхнего края clip
//! view. Допуск — 0 строк и не больше 1 точки (план, «Контракт корректности»).

use serde::Serialize;

use probe_diff_view::model::state::Anchor;
use probe_diff_view::model::{LineAddr, Mode};

use super::contract::{Check, Kind, Raw};

/// Допуск смещения, точки.
pub const TOLERANCE: f64 = 1.0;

/// Прямоугольник строки показа, отданный видом при отрисовке: поколение содержимого, верх и
/// высота в координатах документа.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Drawn {
    pub generation: u64,
    pub row: u32,
    pub top: f64,
    pub height: f64,
}

/// Сверка якоря на одном шаге.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AnchorCheck {
    pub expected: Option<String>,
    pub model: String,
    pub model_row: u32,
    pub drawn: Option<Drawn>,
    pub viewport_y: f64,
    /// Верх видимой области минус верх нарисованной строки якоря.
    pub drawn_offset: Option<f64>,
    pub ok: bool,
    pub reason: String,
}

/// Сверяет якорь модели `model` (строка показа `model_row`) с отрисованной строкой `drawn`
/// поколения не младше `generation` и верхним краем `viewport_y`. `expected` — адрес, которого
/// должен достичь шаг, `None` — адрес не задан шагом.
pub fn check(
    expected: Option<LineAddr>,
    model: Anchor,
    model_row: u32,
    drawn: Option<Drawn>,
    generation: u64,
    viewport_y: f64,
) -> AnchorCheck {
    let drawn_offset = drawn.map(|drawn| viewport_y - drawn.top);
    let reason = match (expected, drawn, drawn_offset) {
        (Some(expected), _, _) if expected != model.addr => {
            format!("адрес модели {:?}, ожидался {expected:?}", model.addr)
        }
        (_, None, _) | (_, _, None) => format!("строка {model_row} не отрисована"),
        (_, Some(drawn), _) if drawn.row != model_row => format!(
            "нарисована строка {}, а якорь модели в строке {model_row}",
            drawn.row
        ),
        (_, Some(drawn), _) if drawn.generation < generation => format!(
            "строка {model_row} отрисована поколением {}, текущее {generation}",
            drawn.generation
        ),
        (_, Some(drawn), Some(offset)) if offset < -TOLERANCE || offset >= drawn.height => {
            format!(
                "верх {viewport_y:.1} вне строки {model_row} [{:.1}; {:.1})",
                drawn.top,
                drawn.top + drawn.height
            )
        }
        (_, _, Some(offset)) if (offset - model.offset as f64).abs() > TOLERANCE => format!(
            "смещение по отрисовке {offset:.1}, по модели {}",
            model.offset
        ),
        _ => String::new(),
    };
    AnchorCheck {
        expected: expected.map(|addr| format!("{addr:?}")),
        model: format!("{model:?}"),
        model_row,
        drawn,
        viewport_y,
        drawn_offset,
        ok: reason.is_empty(),
        reason,
    }
}

/// Стимул подействовал: верх строки якоря в документе сдвинулся больше чем на допуск. Иначе
/// шаг ничего не проверил, и это ошибка исполнения, а не `pass`.
pub fn moved(before_top: f64, after_top: f64) -> bool {
    (before_top - after_top).abs() > TOLERANCE
}

/// Адреса `a` и `b` принадлежат одной строке сырого diff: у контекстной строки их два.
pub fn same_line(raw: &Raw, a: LineAddr, b: LineAddr) -> bool {
    a == b
        || raw.lines.iter().any(|line| {
            let addrs = line.addrs();
            addrs.contains(&a) && addrs.contains(&b)
        })
}

/// Ожидаемый адрес строки показа `row` по сырому diff: адрес модели, если он среди адресов
/// строки, иначе первый из них (строка отчёта «Якорь `anchor`»).
pub fn expected_addr(raw: &Raw, mode: Mode, row: usize, model: LineAddr) -> Option<LineAddr> {
    let addrs = raw.row_addrs(mode, row);
    if addrs.contains(&model) {
        Some(model)
    } else {
        addrs.first().copied()
    }
}

/// Исходная строка якоря: первая контекстная строка показа не раньше середины, которую
/// принимает `accept`.
pub fn start_row(raw: &Raw, mode: Mode, accept: impl Fn(usize) -> bool) -> Option<usize> {
    let rows = raw.rows(mode);
    (rows / 2..rows).find(|&row| {
        let lines = raw.row_lines(mode, row);
        !lines.is_empty()
            && lines
                .iter()
                .all(|&index| raw.lines[index].kind == Kind::Context)
            && accept(row)
    })
}

/// Сверка якоря на шаге как проверка контракта: сырые значения в подробностях.
pub fn as_check(step: &str, checked: &AnchorCheck) -> Check {
    let raw = serde_json::to_string(checked).unwrap_or_default();
    let detail = if checked.ok {
        raw
    } else {
        format!("{}; {raw}", checked.reason)
    };
    Check::new(step, checked.ok, detail)
}

/// Якорь модели сохранил строку и смещение исходного якоря.
pub fn check_kept(step: &str, raw: &Raw, initial: Anchor, model: Anchor) -> Check {
    let ok = same_line(raw, initial.addr, model.addr) && initial.offset == model.offset;
    Check::new(
        format!("{step} kept"),
        ok,
        format!("якорь модели {model:?}, исходный {initial:?}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use probe_diff_view::model::Side;

    fn anchor(line: u32, offset: u64) -> Anchor {
        Anchor {
            addr: LineAddr {
                side: Side::New,
                line,
            },
            offset,
        }
    }

    fn drawn(row: u32, top: f64) -> Drawn {
        Drawn {
            generation: 5,
            row,
            top,
            height: 30.0,
        }
    }

    #[test]
    fn offset_within_one_point_passes() {
        let ok = check(None, anchor(7, 6), 9, Some(drawn(9, 300.0)), 5, 306.8);
        assert!(ok.ok, "{}", ok.reason);
        let off = check(None, anchor(7, 6), 9, Some(drawn(9, 300.0)), 5, 307.5);
        assert!(!off.ok);
        assert!(off.reason.contains("смещение"));
    }

    #[test]
    fn wrong_address_row_or_generation_fails() {
        let expected = Some(LineAddr {
            side: Side::New,
            line: 8,
        });
        assert!(!check(expected, anchor(7, 0), 9, Some(drawn(9, 0.0)), 5, 0.0).ok);
        assert!(!check(None, anchor(7, 0), 9, Some(drawn(8, 0.0)), 5, 0.0).ok);
        assert!(!check(None, anchor(7, 0), 9, Some(drawn(9, 0.0)), 6, 0.0).ok);
        assert!(!check(None, anchor(7, 0), 9, None, 5, 0.0).ok);
    }

    #[test]
    fn top_edge_outside_the_drawn_row_fails() {
        let below = check(None, anchor(7, 31), 9, Some(drawn(9, 0.0)), 5, 31.0);
        assert!(!below.ok);
        assert!(below.reason.contains("вне строки"));
    }

    fn raw() -> Raw {
        Raw::parse(b"--- a/f\n+++ b/f\n@@ -1,6 +1,6 @@\n a\n b\n-c\n+C\n d\n e\n f\n").unwrap()
    }

    #[test]
    fn context_lines_share_both_addresses() {
        let raw = raw();
        assert!(same_line(&raw, LineAddr::old(4), LineAddr::new(4)));
        assert!(!same_line(&raw, LineAddr::old(3), LineAddr::new(3)));
        assert!(same_line(&raw, LineAddr::new(3), LineAddr::new(3)));
    }

    #[test]
    fn expected_address_prefers_the_model_side_of_the_row() {
        let raw = raw();
        assert_eq!(
            expected_addr(&raw, Mode::Unified, 0, LineAddr::old(1)),
            Some(LineAddr::old(1))
        );
        assert_eq!(
            expected_addr(&raw, Mode::Unified, 0, LineAddr::new(5)),
            Some(LineAddr::old(1))
        );
        assert_eq!(
            expected_addr(&raw, Mode::SideBySide, 2, LineAddr::new(3)),
            Some(LineAddr::new(3))
        );
    }

    #[test]
    fn start_row_is_the_first_context_row_from_the_middle() {
        let raw = raw();
        // unified: a b -c +C d e f — 7 строк, середина 3 (+C), первая контекстная — 4 (d).
        assert_eq!(start_row(&raw, Mode::Unified, |_| true), Some(4));
        assert_eq!(start_row(&raw, Mode::Unified, |row| row > 4), Some(5));
        // «Рядом»: a b c|C d e f — 6 строк, середина 3 (d).
        assert_eq!(start_row(&raw, Mode::SideBySide, |_| true), Some(3));
        assert_eq!(start_row(&raw, Mode::SideBySide, |_| false), None);
    }

    #[test]
    fn kept_anchor_accepts_the_other_address_of_a_context_line() {
        let raw = raw();
        let initial = Anchor {
            addr: LineAddr::new(4),
            offset: 7,
        };
        let other = Anchor {
            addr: LineAddr::old(4),
            offset: 7,
        };
        assert!(check_kept("mode", &raw, initial, other).ok);
        assert!(!check_kept("mode", &raw, initial, Anchor { offset: 8, ..other }).ok);
        assert!(!check_kept("mode", &raw, initial, anchor(5, 7)).ok);
    }

    #[test]
    fn stimulus_must_move_the_anchor_row() {
        assert!(!moved(100.0, 100.5));
        assert!(moved(100.0, 112.0));
    }
}
