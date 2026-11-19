//! Ограниченный кэш раскладки варианта A. Запись — раскладка одной половины строки показа
//! (визуальные строки Core Text), ключ — строка показа и половина. Запись помнит версию стилей и
//! переноса, с которой построена: другая версия перестраивает её.
//!
//! Предел — записи на 3 экрана. Сверх предела вытесняется запись, дальняя от видимых строк.
//! Построение дальше 2 экранов от видимых строк считается: вид рисует только видимую область с
//! запасом в экран, и такая раскладка — нарушение контракта ограниченности работы. Кэш обобщён по
//! значению, поэтому тесты идут без Core Text.

use std::collections::BTreeMap;
use std::ops::Range;

use serde::Serialize;

/// Экранов в пределе кэша.
pub const SCREENS: u32 = 3;
/// Экранов от видимых строк, дальше которых раскладка не строится.
pub const FAR_SCREENS: u32 = 2;

/// Половина строки показа: в unified одна, в «Рядом» старая и новая.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Half {
    Unified,
    // Половины «Рядом» рисует задача 8.
    #[allow(dead_code)]
    Old,
    #[allow(dead_code)]
    New,
}

/// Счётчики кэша для `result.json`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Counters {
    /// Построено раскладок, включая перестройки.
    pub built: u64,
    /// Из них перестроено из-за другой версии.
    pub rebuilt: u64,
    pub hits: u64,
    pub evicted: u64,
    /// Построено дальше [`FAR_SCREENS`] экранов от видимых строк.
    pub far_builds: u64,
    /// Наибольшее число записей.
    pub max_entries: usize,
    /// Наибольший предел за время работы: предел меняется с высотой видимой области.
    pub max_capacity: usize,
    pub cleared: u64,
}

struct Entry<V> {
    version: u64,
    value: V,
}

pub struct LayoutCache<V> {
    entries: BTreeMap<(u32, Half), Entry<V>>,
    capacity: usize,
    counters: Counters,
}

/// Предел записей на [`SCREENS`] экрана: визуальных строк на экран с запасом в строку,
/// умноженных на число половин.
pub fn capacity(view_height: f64, line_height: u32, halves: usize) -> usize {
    let per_screen = (view_height.max(0.0) / f64::from(line_height.max(1))).ceil() as usize + 1;
    SCREENS as usize * per_screen * halves.max(1)
}

/// Расстояние строки от видимых строк в строках показа: 0 внутри.
pub fn distance(row: u32, visible: &Range<u32>) -> u32 {
    if row < visible.start {
        visible.start - row
    } else if row >= visible.end {
        row + 1 - visible.end.max(visible.start)
    } else {
        0
    }
}

impl<V> LayoutCache<V> {
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        LayoutCache {
            entries: BTreeMap::new(),
            capacity,
            counters: Counters {
                max_capacity: capacity,
                ..Counters::default()
            },
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn counters(&self) -> Counters {
        self.counters
    }

    /// Новый предел. Лишние записи вытесняются при следующем построении.
    pub fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity.max(1);
        self.counters.max_capacity = self.counters.max_capacity.max(self.capacity);
    }

    /// Раскладка половины строки `row` версии `version`. Нет записи или другая версия —
    /// `build` строит её, и запись встаёт в кэш, вытеснив дальние от `visible` сверх предела.
    pub fn get_or_build(
        &mut self,
        row: u32,
        half: Half,
        version: u64,
        visible: &Range<u32>,
        build: impl FnOnce() -> V,
    ) -> &V {
        let key = (row, half);
        let stale = match self.entries.get(&key) {
            Some(entry) if entry.version == version => {
                self.counters.hits += 1;
                false
            }
            Some(_) => true,
            None => false,
        };
        if !self.entries.contains_key(&key) || stale {
            let screen = visible.end.saturating_sub(visible.start).max(1);
            if distance(row, visible) > FAR_SCREENS * screen {
                self.counters.far_builds += 1;
            }
            self.counters.built += 1;
            if stale {
                self.counters.rebuilt += 1;
                self.entries.remove(&key);
            }
            while self.entries.len() >= self.capacity {
                if !self.evict_farthest(visible) {
                    break;
                }
            }
            self.entries.insert(
                key,
                Entry {
                    version,
                    value: build(),
                },
            );
            self.counters.max_entries = self.counters.max_entries.max(self.entries.len());
        }
        match self.entries.get(&key) {
            Some(entry) => &entry.value,
            None => unreachable!("запись только что вставлена или найдена"),
        }
    }

    /// Вытесняет запись дальней от видимых строк строки: первую или последнюю по порядку.
    fn evict_farthest(&mut self, visible: &Range<u32>) -> bool {
        let first = self.entries.first_key_value().map(|(key, _)| *key);
        let last = self.entries.last_key_value().map(|(key, _)| *key);
        let victim = match (first, last) {
            (Some(first), Some(last)) => {
                if distance(first.0, visible) >= distance(last.0, visible) {
                    first
                } else {
                    last
                }
            }
            _ => return false,
        };
        self.entries.remove(&victim);
        self.counters.evicted += 1;
        true
    }

    /// Убирает записи строк: их содержимое или стили изменились.
    pub fn invalidate(&mut self, rows: Range<u32>) {
        let keys: Vec<(u32, Half)> = self
            .entries
            .range((rows.start, Half::Unified)..(rows.end, Half::Unified))
            .map(|(key, _)| *key)
            .collect();
        for key in keys {
            self.entries.remove(&key);
        }
    }

    /// Убирает все записи: другой diff, перенос или тема.
    pub fn clear(&mut self) {
        if !self.is_empty() {
            self.counters.cleared += 1;
        }
        self.entries.clear();
    }

    /// Строки показа записей: для проверки, что кэш держит строки у видимой области.
    pub fn rows(&self) -> impl Iterator<Item = u32> + '_ {
        self.entries.keys().map(|(row, _)| *row)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    #[test]
    fn capacity_is_three_screens_of_visual_lines() {
        // 720 точек, строка 15 точек: 48 строк на экран и запас в строку.
        assert_eq!(capacity(720.0, 15, 1), 3 * 49);
        assert_eq!(capacity(720.0, 15, 2), 2 * 3 * 49);
        assert_eq!(capacity(0.0, 15, 1), 3);
        assert_eq!(capacity(10.0, 0, 0), 3 * 11);
    }

    #[test]
    fn distance_is_zero_inside_and_counts_rows_outside() {
        let visible = 10..20;
        assert_eq!(distance(10, &visible), 0);
        assert_eq!(distance(19, &visible), 0);
        assert_eq!(distance(9, &visible), 1);
        assert_eq!(distance(0, &visible), 10);
        assert_eq!(distance(20, &visible), 1);
        assert_eq!(distance(29, &visible), 10);
        assert_eq!(distance(5, &(7..7)), 2);
    }

    #[test]
    fn hit_does_not_build_and_version_rebuilds() {
        let mut cache = LayoutCache::new(10);
        let builds = Cell::new(0);
        let visible = 0..5;
        let build = |value: u32| {
            builds.set(builds.get() + 1);
            value
        };
        assert_eq!(
            *cache.get_or_build(1, Half::Unified, 1, &visible, || build(10)),
            10
        );
        assert_eq!(
            *cache.get_or_build(1, Half::Unified, 1, &visible, || build(11)),
            10
        );
        assert_eq!(builds.get(), 1);
        assert_eq!(
            *cache.get_or_build(1, Half::Unified, 2, &visible, || build(12)),
            12
        );
        assert_eq!(builds.get(), 2);
        let counters = cache.counters();
        assert_eq!((counters.built, counters.rebuilt, counters.hits), (2, 1, 1));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn eviction_keeps_the_limit_and_drops_the_farthest_row() {
        let mut cache = LayoutCache::new(4);
        let visible = 10..12;
        for row in [3, 10, 11, 20] {
            cache.get_or_build(row, Half::Unified, 1, &visible, || row);
        }
        assert_eq!(cache.len(), 4);
        // Строка 3 на 7 от видимых, строка 20 на 9: вытесняется 20.
        cache.get_or_build(12, Half::Unified, 1, &visible, || 12);
        assert_eq!(cache.rows().collect::<Vec<_>>(), vec![3, 10, 11, 12]);
        // Прокрутка вниз: теперь дальше всех строка 3.
        let visible = 12..14;
        cache.get_or_build(13, Half::Unified, 1, &visible, || 13);
        assert_eq!(cache.rows().collect::<Vec<_>>(), vec![10, 11, 12, 13]);
        assert_eq!(cache.counters().evicted, 2);
        assert_eq!(cache.counters().max_entries, 4);
    }

    #[test]
    fn long_scroll_never_exceeds_the_limit() {
        let mut cache = LayoutCache::new(capacity(720.0, 15, 2));
        let limit = cache.capacity();
        for top in (0..10_000u32).step_by(7) {
            let visible = top..top + 48;
            for row in visible.clone() {
                for half in [Half::Old, Half::New] {
                    cache.get_or_build(row, half, 1, &visible, || row);
                }
            }
            assert!(cache.len() <= limit);
            // Сверх видимых строк кэш держит только строки над ними, не больше остатка предела.
            let rows_kept = (limit / 2) as u32;
            assert!(
                cache
                    .rows()
                    .all(|row| distance(row, &visible) <= rows_kept - 48)
            );
        }
        let counters = cache.counters();
        assert_eq!(counters.max_entries, limit);
        assert_eq!(counters.far_builds, 0);
    }

    #[test]
    fn far_build_is_counted() {
        let mut cache = LayoutCache::new(100);
        let visible = 100..110;
        // Экран — 10 строк: строка 130 в 21 строке от видимых, 120 — в 11, 79 — в 21.
        cache.get_or_build(130, Half::Unified, 1, &visible, || 0);
        cache.get_or_build(120, Half::Unified, 1, &visible, || 0);
        cache.get_or_build(79, Half::Unified, 1, &visible, || 0);
        assert_eq!(cache.counters().far_builds, 2);
    }

    #[test]
    fn invalidate_and_clear_drop_entries() {
        let mut cache = LayoutCache::new(20);
        let visible = 0..10;
        for row in 0..6 {
            cache.get_or_build(row, Half::Old, 1, &visible, || row);
            cache.get_or_build(row, Half::New, 1, &visible, || row);
        }
        cache.invalidate(2..4);
        assert_eq!(
            cache.rows().collect::<Vec<_>>(),
            vec![0, 0, 1, 1, 4, 4, 5, 5]
        );
        cache.clear();
        assert!(cache.is_empty());
        assert_eq!(cache.counters().cleared, 1);
    }

    #[test]
    fn shrinking_capacity_evicts_on_next_build() {
        let mut cache = LayoutCache::new(6);
        let visible = 0..3;
        for row in 0..6 {
            cache.get_or_build(row, Half::Unified, 1, &visible, || row);
        }
        cache.set_capacity(3);
        cache.get_or_build(1, Half::Unified, 2, &visible, || 1);
        assert_eq!(cache.len(), 3);
        assert_eq!(cache.counters().max_capacity, 6);
    }
}
