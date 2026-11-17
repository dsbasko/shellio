//! Чистая часть сценария `bridge` (план P0.4, задача 15): клетки моста в настоящую поверхность и
//! их части. Процесс сценария — водитель без поверхности, обе GUI — дочерние процессы
//! `probe-session-surface --gui`; FFI и окна живут в `surface::bridge` под функцией `surface`.

use crate::matrix::evidence;
use crate::results::Cell;

/// Часть клетки, которую сценарий `bridge` добавляет к headless-частям той же клетки.
pub const SURFACE_PART: &str = "surface";

/// Клетки сценария и их часть: у C1–C4 и F2 одна часть, у D1, D3–D5 — `surface` рядом с
/// headless-частями сценария `queries`.
pub const CELLS: [(&str, Option<&str>); 9] = [
    ("C1", None),
    ("C2", None),
    ("C3", None),
    ("C4", None),
    ("D1", Some(SURFACE_PART)),
    ("D3", Some(SURFACE_PART)),
    ("D4", Some(SURFACE_PART)),
    ("D5", Some(SURFACE_PART)),
    ("F2", None),
];

/// Клетки, которые сценарий ещё не построил: `not_run` с причиной стенда.
pub fn pending(reason: &str) -> Vec<Cell> {
    CELLS
        .iter()
        .map(|(id, part)| evidence::later(id, *part, reason))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matrix::is_matrix_cell;
    use crate::results::Status;

    #[test]
    fn cells_are_matrix_cells_without_duplicates() {
        for (index, (id, part)) in CELLS.iter().enumerate() {
            assert!(is_matrix_cell(id), "{id}");
            assert!(
                !CELLS[..index].contains(&(*id, *part)),
                "клетка {id} повторяется"
            );
        }
    }

    #[test]
    fn pending_cells_are_not_run_with_reason() {
        let cells = pending("срез 1");
        assert_eq!(cells.len(), CELLS.len());
        for cell in &cells {
            assert_eq!(cell.status, Status::NotRun);
            assert!(cell.reason.contains("срез 1"), "{}", cell.reason);
        }
        let d1 = cells.iter().find(|cell| cell.id == "D1").unwrap();
        assert_eq!(d1.part.as_deref(), Some(SURFACE_PART));
    }
}
