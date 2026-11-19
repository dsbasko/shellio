//! Стили строк, которые показывает вид, и разница снимков подсветки. Снимок приносит стили
//! видимых строк обеих сторон, хранилище сравнивает их с прежними и отдаёт строки, у которых
//! отрезки изменились: вид перерисовывает только их. Строка без стилей рисуется как строка без
//! отрезков, поэтому снимок, где у строки отрезков нет, её не меняет. Общий контракт обоих
//! вариантов отрисовки.

use std::collections::BTreeMap;

use super::{Run, Sides, Snapshot, Source};
use crate::model::Side;

/// Стили одной строки и источник снимка, который их принёс.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Styled {
    pub runs: Box<[Run]>,
    pub source: Source,
}

#[derive(Debug, Default)]
pub struct StyleStore {
    document: u64,
    lines: Sides<BTreeMap<u32, Styled>>,
}

/// Строки сторон, у которых изменились отрезки.
pub type Changed = Sides<Vec<u32>>;

impl StyleStore {
    /// Новый документ: прежние стили забываются.
    pub fn reset(&mut self, document: u64) {
        self.document = document;
        self.lines = Sides::default();
    }

    pub fn document(&self) -> u64 {
        self.document
    }

    pub fn get(&self, side: Side, line: u32) -> Option<&Styled> {
        self.lines.get(side).get(&line)
    }

    pub fn runs(&self, side: Side, line: u32) -> &[Run] {
        self.get(side, line).map_or(&[], |styled| &styled.runs)
    }

    pub fn len(&self) -> usize {
        self.lines.old.len() + self.lines.new.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Применяет снимок своего документа. `None` — снимок чужого документа, он не применён.
    pub fn apply(&mut self, snapshot: &Snapshot) -> Option<Changed> {
        if snapshot.document != self.document {
            return None;
        }
        let mut changed = Changed::default();
        for side in Side::BOTH {
            let store = self.lines.get_mut(side);
            let out = changed.get_mut(side);
            for (line, runs) in snapshot.styles.get(side).lines() {
                let before = store.get(&line).map_or(&[][..], |styled| &styled.runs[..]);
                if before != runs {
                    out.push(line);
                }
                store.insert(
                    line,
                    Styled {
                        runs: runs.into(),
                        source: snapshot.source,
                    },
                );
            }
        }
        Some(changed)
    }

    /// Оставляет только строки, для которых `keep` истинно: стили далеко от видимой области не
    /// держатся.
    pub fn retain(&mut self, mut keep: impl FnMut(Side, u32) -> bool) {
        for side in Side::BOTH {
            self.lines.get_mut(side).retain(|&line, _| keep(side, line));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::super::{SideStyles, TokenClass};
    use super::*;

    fn run(start: u32, end: u32, class: TokenClass) -> Run {
        Run { start, end, class }
    }

    fn snapshot(
        document: u64,
        source: Source,
        old: &[(u32, Vec<Run>)],
        new: &[(u32, Vec<Run>)],
    ) -> Snapshot {
        let side = |lines: &[(u32, Vec<Run>)]| {
            SideStyles::from_lines(lines.iter().map(|(line, runs)| (*line, runs.as_slice())))
                .unwrap_or_default()
        };
        Snapshot {
            document,
            request: 1,
            source,
            styles: Sides {
                old: side(old),
                new: side(new),
            },
            compute: Duration::ZERO,
        }
    }

    #[test]
    fn first_snapshot_changes_only_lines_with_runs() {
        let mut store = StyleStore::default();
        store.reset(3);
        let keyword = vec![run(0, 2, TokenClass::Keyword)];
        let changed = store
            .apply(&snapshot(
                3,
                Source::Local,
                &[],
                &[(1, keyword.clone()), (2, vec![]), (3, keyword.clone())],
            ))
            .unwrap();
        assert_eq!(changed.new, vec![1, 3]);
        assert!(changed.old.is_empty());
        assert_eq!(store.runs(Side::New, 1), keyword.as_slice());
        assert_eq!(
            store.get(Side::New, 2).map(|s| s.source),
            Some(Source::Local)
        );
        assert_eq!(store.len(), 3);
    }

    #[test]
    fn correction_changes_only_differing_lines() {
        let mut store = StyleStore::default();
        store.reset(1);
        let comment = vec![run(0, 9, TokenClass::Comment)];
        let string = vec![run(0, 9, TokenClass::String)];
        store.apply(&snapshot(
            1,
            Source::Local,
            &[(7, comment.clone())],
            &[(7, comment.clone()), (8, comment.clone())],
        ));
        let changed = store
            .apply(&snapshot(
                1,
                Source::Full,
                &[(7, comment.clone())],
                &[(7, string.clone()), (8, comment.clone())],
            ))
            .unwrap();
        assert!(changed.old.is_empty());
        assert_eq!(changed.new, vec![7]);
        assert_eq!(
            store.get(Side::New, 8).map(|s| s.source),
            Some(Source::Full)
        );
        assert_eq!(store.runs(Side::New, 7), string.as_slice());
    }

    #[test]
    fn foreign_document_is_not_applied_and_reset_forgets() {
        let mut store = StyleStore::default();
        store.reset(2);
        let keyword = vec![run(0, 2, TokenClass::Keyword)];
        assert!(
            store
                .apply(&snapshot(1, Source::Full, &[], &[(1, keyword.clone())]))
                .is_none()
        );
        assert!(store.is_empty());
        store.apply(&snapshot(2, Source::Full, &[(4, keyword.clone())], &[]));
        assert_eq!(store.len(), 1);
        store.reset(3);
        assert!(store.is_empty());
        assert_eq!(store.document(), 3);
    }

    #[test]
    fn retain_prunes_by_side_and_line() {
        let mut store = StyleStore::default();
        store.reset(1);
        let keyword = vec![run(0, 2, TokenClass::Keyword)];
        let lines: Vec<(u32, Vec<Run>)> = (1..=10).map(|line| (line, keyword.clone())).collect();
        store.apply(&snapshot(1, Source::Full, &lines, &lines));
        store.retain(|side, line| side == Side::New && line > 5);
        assert_eq!(store.len(), 5);
        assert!(store.get(Side::Old, 6).is_none());
        assert!(store.get(Side::New, 6).is_some());
    }
}
