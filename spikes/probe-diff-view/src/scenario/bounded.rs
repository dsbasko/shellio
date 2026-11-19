//! Оракул сценария `bounded`: работа вида ограничена видимой областью. Видов строк B не больше
//! `2 × видимых + 10`, записей кэша раскладки A не больше чем на 3 экрана, ни одна раскладка
//! Core Text не строится дальше 2 экранов от видимой области, счётчики живых объектов после
//! 10-го и 100-го цикла открытия и закрытия совпадают.

use serde::Serialize;

use super::contract::Check;
use super::counters::{self, Count};

/// Циклов открытия и закрытия.
pub const CYCLES: usize = 100;
/// После какого цикла снимается первый срез счётчиков.
pub const FIRST_CUT: usize = 10;

/// Предел видов строк B при `visible` видимых строках таблицы.
pub fn row_view_limit(visible: usize) -> usize {
    2 * visible + 10
}

/// Наблюдение видов строк B после шага.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct RowViews {
    pub views: usize,
    pub visible: usize,
}

pub fn check_row_views(step: &str, seen: RowViews) -> Check {
    let limit = row_view_limit(seen.visible);
    Check::new(
        step,
        seen.views <= limit,
        format!(
            "видов строк {}, видимых {}, предел {limit}",
            seen.views, seen.visible
        ),
    )
}

/// Наибольшее число записей кэша A за время работы против наибольшего предела 3 экранов.
pub fn check_cache(name: &str, max_entries: usize, max_capacity: usize) -> Check {
    Check::new(
        name,
        max_capacity > 0 && max_entries <= max_capacity,
        format!("записей не больше {max_entries}, предел 3 экранов {max_capacity}"),
    )
}

/// Раскладки дальше 2 экранов от видимой области.
pub fn check_far(name: &str, far_builds: u64) -> Check {
    Check::new(
        name,
        far_builds == 0,
        format!("раскладок дальше 2 экранов {far_builds}"),
    )
}

/// Срезы счётчиков после 10-го и 100-го цикла: совпадают живые экземпляры каждого класса.
pub fn check_cycles(first: &[Count], last: &[Count]) -> Check {
    let changes = counters::live_changes(first, last);
    let detail = if changes.is_empty() {
        format!("живых по классам совпадает: {}", describe(last))
    } else {
        format!(
            "изменились: {}",
            changes
                .iter()
                .map(|(name, before, after)| format!("{name} {before} → {after}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    Check::new(
        format!("cycles {FIRST_CUT}→{CYCLES}"),
        changes.is_empty(),
        detail,
    )
}

fn describe(counts: &[Count]) -> String {
    counts
        .iter()
        .map(|count| format!("{} {}", count.name, count.live()))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_views_limit_is_twice_visible_plus_ten() {
        assert!(
            check_row_views(
                "s",
                RowViews {
                    views: 70,
                    visible: 30
                }
            )
            .ok
        );
        assert!(
            !check_row_views(
                "s",
                RowViews {
                    views: 71,
                    visible: 30
                }
            )
            .ok
        );
    }

    #[test]
    fn cache_and_far_limits() {
        assert!(check_cache("a", 120, 120).ok);
        assert!(!check_cache("a", 121, 120).ok);
        assert!(!check_cache("a", 0, 0).ok);
        assert!(check_far("a", 0).ok);
        assert!(!check_far("a", 1).ok);
    }

    #[test]
    fn cycles_compare_live_counts_per_class() {
        let count = |name: &'static str, created, released| Count {
            name,
            created,
            released,
        };
        let first = [count("A", 10, 9), count("B", 4, 4)];
        let same = [count("A", 100, 99), count("B", 40, 40)];
        let grown = [count("A", 100, 98), count("B", 40, 40)];
        assert!(check_cycles(&first, &same).ok);
        let failed = check_cycles(&first, &grown);
        assert!(!failed.ok);
        assert!(failed.detail.contains("A 1 → 2"), "{}", failed.detail);
    }
}
