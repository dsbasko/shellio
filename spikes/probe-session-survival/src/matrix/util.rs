//! Чистые сценарные утилиты headless-матрицы: разрезы последовательности, предусловие ревизии
//! протокола разреза, подсчёт ответов по квитанциям фикстуры и прирост памяти по двум объёмам
//! потока. FFI нет, поэтому тесты идут в `make check`.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::fixture::query::ResponseKind;

/// Разрез последовательности: префикс до границы и остаток. Границы — каждая внутренняя граница
/// байта, `1..len`: пустой префикс или пустой остаток разрезом не является.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cut {
    pub at: usize,
    pub prefix: Vec<u8>,
    pub rest: Vec<u8>,
}

pub fn cuts(sequence: &[u8]) -> Vec<Cut> {
    (1..sequence.len())
        .map(|at| Cut {
            at,
            prefix: sequence[..at].to_vec(),
            rest: sequence[at..].to_vec(),
        })
        .collect()
}

/// Предусловие протокола разреза zmx: между чтением ревизии до префикса и после него ревизия
/// выросла ровно на единицу, а стенд не видел другого вывода, ресайза и подключений. `Err` —
/// граница не доказана, разрез получает `not_run` с этой причиной.
pub fn revision_boundary(before: u64, after: u64, violations: &[String]) -> Result<(), String> {
    if !violations.is_empty() {
        return Err(format!(
            "предусловие разреза нарушено: {}",
            violations.join("; ")
        ));
    }
    match after.checked_sub(before) {
        Some(1) => Ok(()),
        Some(0) => Err(format!("ревизия не выросла: {before}")),
        Some(delta) => Err(format!(
            "ревизия выросла на {delta} ({before} → {after}): префикс прочитан не одним чтением \
             или было другое чтение"
        )),
        None => Err(format!("ревизия уменьшилась: {before} → {after}")),
    }
}

/// Предусловие протокола разреза Rust-хоста: от точки покоя `base` (всё прочитанное хостом
/// разобрано) префикс длиной `len` прочитан и разобран ровно одним куском. `events` — события
/// `Parsed` наблюдателя как пары «прочитано куском, разобрано всего».
pub fn parsed_boundary(
    events: &[(u64, u64)],
    base: u64,
    len: u64,
    violations: &[String],
) -> Result<(), String> {
    if !violations.is_empty() {
        return Err(format!(
            "предусловие разреза нарушено: {}",
            violations.join("; ")
        ));
    }
    let after: Vec<(u64, u64)> = events
        .iter()
        .copied()
        .filter(|(_, offset)| *offset > base)
        .collect();
    match after[..] {
        [] => Err(format!("после точки покоя {base} хост ничего не разобрал")),
        [(read, offset)] if read == len && offset == base + len => Ok(()),
        [(read, offset)] => Err(format!(
            "один кусок {read} байт до {offset}, ожидался префикс {len} байт до {}",
            base + len
        )),
        _ => Err(format!(
            "после точки покоя {base} кусков {}: {after:?}; префикс прочитан не одним куском \
             или было другое чтение",
            after.len()
        )),
    }
}

/// Итог подсчёта ответов: сколько каких ждали и сколько каких пришло по квитанциям фикстуры.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Tally {
    pub expected: BTreeMap<ResponseKind, usize>,
    pub seen: BTreeMap<ResponseKind, usize>,
}

impl Tally {
    pub fn new(expected: &[ResponseKind], seen: &[ResponseKind]) -> Tally {
        let count = |kinds: &[ResponseKind]| {
            let mut map = BTreeMap::new();
            for kind in kinds {
                *map.entry(*kind).or_insert(0) += 1;
            }
            map
        };
        Tally {
            expected: count(expected),
            seen: count(seen),
        }
    }

    /// Каждый ожидаемый ответ пришёл ровно столько раз, сколько его ждали, и лишних нет.
    pub fn exact(&self) -> bool {
        self.expected == self.seen
    }

    /// Виды, которых пришло меньше ожидаемого: вид и недостача.
    pub fn missing(&self) -> Vec<(ResponseKind, usize)> {
        self.expected
            .iter()
            .filter_map(|(kind, want)| {
                let got = self.seen.get(kind).copied().unwrap_or(0);
                (got < *want).then(|| (*kind, want - got))
            })
            .collect()
    }

    /// Виды, которых пришло больше ожидаемого: вид и избыток.
    pub fn extra(&self) -> Vec<(ResponseKind, usize)> {
        self.seen
            .iter()
            .filter_map(|(kind, got)| {
                let want = self.expected.get(kind).copied().unwrap_or(0);
                (*got > want).then(|| (*kind, got - want))
            })
            .collect()
    }

    pub fn describe(&self) -> String {
        if self.exact() {
            return format!("ровно ожидаемые ответы: {:?}", self.expected);
        }
        format!("не хватает {:?}, лишние {:?}", self.missing(), self.extra())
    }
}

/// Прирост памяти хоста по двум объёмам потока: `base` до потока, `small` после объёма `volume`,
/// `large` после объёма `2 × volume` (в отдельной сессии от того же `base`). Если хост копит
/// непрочитанное, второй прирост больше первого примерно на `volume`; ограниченная очередь даёт
/// приросты одного порядка.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Growth {
    pub volume: u64,
    pub small: i64,
    pub large: i64,
}

impl Growth {
    pub fn new(volume: u64, base_small: u64, small: u64, base_large: u64, large: u64) -> Growth {
        let delta = |from: u64, to: u64| to as i64 - from as i64;
        Growth {
            volume,
            small: delta(base_small, small),
            large: delta(base_large, large),
        }
    }

    /// Сколько памяти добавило удвоение объёма.
    pub fn doubling(&self) -> i64 {
        self.large - self.small
    }

    /// Удвоение объёма добавило не больше `limit` байт.
    pub fn within(&self, limit: u64) -> bool {
        self.doubling() <= limit as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cuts_cover_every_inner_byte_boundary() {
        let all = cuts(b"\x1b[c");
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].prefix, b"\x1b");
        assert_eq!(all[0].rest, b"[c");
        assert_eq!(all[1].at, 2);
        for cut in &all {
            let mut joined = cut.prefix.clone();
            joined.extend_from_slice(&cut.rest);
            assert_eq!(joined, b"\x1b[c");
            assert!(!cut.prefix.is_empty() && !cut.rest.is_empty());
        }
        assert!(cuts(b"x").is_empty());
        assert!(cuts(b"").is_empty());
    }

    #[test]
    fn parsed_boundary_requires_one_piece_of_exactly_the_prefix_after_the_quiet_point() {
        // События до точки покоя не считаются.
        assert_eq!(parsed_boundary(&[(16, 100), (3, 103)], 100, 3, &[]), Ok(()));
        assert!(
            parsed_boundary(&[(16, 100)], 100, 3, &[])
                .unwrap_err()
                .contains("ничего не разобрал")
        );
        assert!(
            parsed_boundary(&[(1, 101), (2, 103)], 100, 3, &[])
                .unwrap_err()
                .contains("кусков 2")
        );
        assert!(
            parsed_boundary(&[(5, 105)], 100, 3, &[])
                .unwrap_err()
                .contains("ожидался префикс 3")
        );
        let violations = vec!["сигнал 28".to_owned()];
        assert!(
            parsed_boundary(&[(3, 103)], 100, 3, &violations)
                .unwrap_err()
                .contains("сигнал 28")
        );
    }

    #[test]
    fn revision_boundary_requires_growth_by_exactly_one_and_quiet_stand() {
        assert_eq!(revision_boundary(7, 8, &[]), Ok(()));
        assert!(
            revision_boundary(7, 7, &[])
                .unwrap_err()
                .contains("не выросла")
        );
        assert!(revision_boundary(7, 9, &[]).unwrap_err().contains("на 2"));
        assert!(
            revision_boundary(7, 6, &[])
                .unwrap_err()
                .contains("уменьшилась")
        );
        let violations = vec!["SIGWINCH во время разреза".to_owned()];
        assert!(
            revision_boundary(7, 8, &violations)
                .unwrap_err()
                .contains("SIGWINCH")
        );
    }

    #[test]
    fn tally_counts_missing_and_extra_replies_per_kind() {
        use ResponseKind::{CursorPosition, Da1, Da2};
        let exact = Tally::new(&[Da1, Da2], &[Da2, Da1]);
        assert!(exact.exact());
        assert!(exact.missing().is_empty() && exact.extra().is_empty());

        let doubled = Tally::new(&[Da1, CursorPosition], &[Da1, Da1]);
        assert!(!doubled.exact());
        assert_eq!(doubled.missing(), [(CursorPosition, 1)]);
        assert_eq!(doubled.extra(), [(Da1, 1)]);
        assert!(doubled.describe().contains("лишние"));

        let none = Tally::new(&[Da1], &[]);
        assert_eq!(none.missing(), [(Da1, 1)]);
        assert!(none.extra().is_empty());
    }

    #[test]
    fn growth_compares_doubled_volume_against_the_limit() {
        // Хост копит всё: удвоение добавило объём.
        let unbounded = Growth::new(32 << 20, 10 << 20, 43 << 20, 10 << 20, 76 << 20);
        assert_eq!(unbounded.doubling(), 33 << 20);
        assert!(!unbounded.within(8 << 20));
        // Ограниченная очередь: приросты одного порядка.
        let bounded = Growth::new(32 << 20, 10 << 20, 14 << 20, 10 << 20, 15 << 20);
        assert_eq!(bounded.doubling(), 1 << 20);
        assert!(bounded.within(8 << 20));
        let shrunk = Growth::new(1, 100, 90, 100, 80);
        assert_eq!(shrunk.doubling(), -10);
        assert!(shrunk.within(0));
    }
}
