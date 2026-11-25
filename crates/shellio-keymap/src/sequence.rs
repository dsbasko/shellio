//! Последовательность аккордов и дерево последовательностей одного контекста.

use std::collections::BTreeMap;
use std::fmt;
use std::ops::Bound;

use crate::chord::{Chord, ChordError};

/// Наибольшая длина последовательности в аккордах.
pub const MAX_SEQUENCE: usize = 4;

const LEADER: &str = "<leader>";

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Sequence(Vec<Chord>);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SequenceError {
    Empty,
    TooLong(usize),
    Chord(ChordError),
}

impl fmt::Display for SequenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("пустая последовательность"),
            Self::TooLong(len) => write!(
                f,
                "последовательность из {len} аккордов длиннее {MAX_SEQUENCE}"
            ),
            Self::Chord(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for SequenceError {}

impl Sequence {
    /// Аккорды через пробел. `<leader>` раскрывается в `leader`, после него аккорды из одного
    /// знака можно писать слитно: `<leader>gb`.
    pub fn parse(text: &str, leader: Chord) -> Result<Self, SequenceError> {
        let mut chords = Vec::new();
        for token in text.split(' ').filter(|token| !token.is_empty()) {
            match token.strip_prefix(LEADER) {
                Some(mut tail) => {
                    chords.push(leader);
                    while let Some(rest) = tail.strip_prefix(LEADER) {
                        chords.push(leader);
                        tail = rest;
                    }
                    let mut buffer = [0u8; 4];
                    for sign in tail.chars() {
                        let chord = Chord::parse(sign.encode_utf8(&mut buffer))
                            .map_err(SequenceError::Chord)?;
                        chords.push(chord);
                    }
                }
                None => chords.push(Chord::parse(token).map_err(SequenceError::Chord)?),
            }
        }
        Self::new(chords)
    }

    pub fn new(chords: Vec<Chord>) -> Result<Self, SequenceError> {
        match chords.len() {
            0 => Err(SequenceError::Empty),
            len if len > MAX_SEQUENCE => Err(SequenceError::TooLong(len)),
            _ => Ok(Self(chords)),
        }
    }

    pub fn chords(&self) -> &[Chord] {
        &self.0
    }
}

/// Нормализованная запись: аккорды через пробел, лидер раскрыт.
impl fmt::Display for Sequence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, chord) in self.0.iter().enumerate() {
            if index > 0 {
                f.write_str(" ")?;
            }
            chord.fmt(f)?;
        }
        Ok(())
    }
}

impl serde::Serialize for Sequence {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// Ответ дерева на набранные аккорды.
#[derive(Debug, PartialEq, Eq)]
pub enum Match<'a, T> {
    Exact(&'a T),
    /// Набранное — начало более длинной последовательности: ждём продолжения.
    Prefix,
    None,
}

/// Почему последовательность не входит в дерево.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Overlap {
    /// Такая последовательность уже есть.
    Same,
    /// Одна последовательность — начало другой.
    Prefix(Sequence),
}

/// Дерево последовательностей одного контекста. Ни одна последовательность не начало другой,
/// поэтому ответы `Exact` и `Prefix` не пересекаются и ожидание по времени не нужно.
#[derive(Clone, Debug, PartialEq)]
pub struct SequenceTree<T> {
    entries: BTreeMap<Vec<Chord>, T>,
}

impl<T> Default for SequenceTree<T> {
    fn default() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }
}

impl<T> SequenceTree<T> {
    /// С чем пересекается последовательность, если её добавить.
    pub fn overlap(&self, sequence: &Sequence) -> Option<Overlap> {
        let chords = sequence.chords();
        if self.entries.contains_key(chords) {
            return Some(Overlap::Same);
        }
        let shorter = (1..chords.len()).find_map(|len| self.entries.get_key_value(&chords[..len]));
        shorter
            .map(|(key, _)| key)
            .or_else(|| self.longer(chords))
            .map(|key| Overlap::Prefix(Sequence(key.clone())))
    }

    pub fn insert(&mut self, sequence: &Sequence, value: T) -> Result<(), Overlap> {
        match self.overlap(sequence) {
            Some(overlap) => Err(overlap),
            None => {
                self.entries.insert(sequence.chords().to_vec(), value);
                Ok(())
            }
        }
    }

    pub fn remove(&mut self, sequence: &Sequence) -> Option<T> {
        self.entries.remove(sequence.chords())
    }

    pub fn get(&self, sequence: &Sequence) -> Option<&T> {
        self.entries.get(sequence.chords())
    }

    pub fn find(&self, typed: &[Chord]) -> Match<'_, T> {
        if let Some(value) = self.entries.get(typed) {
            return Match::Exact(value);
        }
        if !typed.is_empty() && self.longer(typed).is_some() {
            return Match::Prefix;
        }
        Match::None
    }

    pub fn values(&self) -> impl Iterator<Item = &T> {
        self.entries.values()
    }

    /// Первая последовательность, которая начинается с `typed` и длиннее его: в порядке ключей
    /// она идёт сразу за ним.
    fn longer(&self, typed: &[Chord]) -> Option<&Vec<Chord>> {
        self.entries
            .range::<[Chord], _>((Bound::Excluded(typed), Bound::Unbounded))
            .next()
            .map(|(key, _)| key)
            .filter(|key| key.starts_with(typed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leader() -> Chord {
        Chord::parse("Space").unwrap()
    }

    fn seq(text: &str) -> Sequence {
        Sequence::parse(text, leader()).unwrap()
    }

    fn chords(text: &str) -> Vec<Chord> {
        seq(text).chords().to_vec()
    }

    #[test]
    fn sequences_parse_and_normalize() {
        assert_eq!(seq("Ctrl-w h").to_string(), "Ctrl-w h");
        assert_eq!(seq("<leader>zj"), seq("Space z j"));
        assert_eq!(seq("<leader> z j"), seq("Space z j"));
        assert_eq!(seq("<leader>gb").to_string(), "Space g b");
        assert_eq!(seq("<leader><leader>").to_string(), "Space Space");
        assert_eq!(seq("g  g").to_string(), "g g");
        assert_eq!(seq("Ctrl-w <").to_string(), "Ctrl-w <");
        assert_eq!(seq("Ctrl-w -").to_string(), "Ctrl-w -");
        assert_eq!(seq("<").chords().len(), 1);
        let comma = Chord::parse(",").unwrap();
        assert_eq!(
            Sequence::parse("<leader>zj", comma).unwrap().to_string(),
            ", z j"
        );
        assert_eq!(
            serde_json::to_string(&seq("<leader>zj")).unwrap(),
            "\"Space z j\""
        );
    }

    #[test]
    fn bad_sequences_are_refused() {
        let parse = |text| Sequence::parse(text, leader());
        assert_eq!(parse(""), Err(SequenceError::Empty));
        assert_eq!(parse("  "), Err(SequenceError::Empty));
        assert_eq!(parse("a b c d e"), Err(SequenceError::TooLong(5)));
        assert_eq!(parse("<leader>abcd"), Err(SequenceError::TooLong(5)));
        assert_eq!(seq("a b c d").chords().len(), MAX_SEQUENCE);
        assert_eq!(
            parse("Ctrl-w hh"),
            Err(SequenceError::Chord(ChordError("hh".to_owned())))
        );
        assert!(matches!(parse("<leader>й"), Err(SequenceError::Chord(_))));
    }

    #[test]
    fn tree_answers_exact_prefix_and_none() {
        let mut tree = SequenceTree::default();
        tree.insert(&seq("Ctrl-w h"), 1).unwrap();
        tree.insert(&seq("Ctrl-w l"), 2).unwrap();
        tree.insert(&seq("g g"), 3).unwrap();
        tree.insert(&seq("j"), 4).unwrap();

        assert_eq!(tree.find(&chords("Ctrl-w h")), Match::Exact(&1));
        assert_eq!(tree.find(&chords("j")), Match::Exact(&4));
        assert_eq!(tree.find(&chords("Ctrl-w")), Match::Prefix);
        assert_eq!(tree.find(&chords("g")), Match::Prefix);
        assert_eq!(tree.find(&chords("Ctrl-w x")), Match::None);
        assert_eq!(tree.find(&chords("g g g")), Match::None);
        assert_eq!(tree.find(&chords("k")), Match::None);
        assert_eq!(tree.find(&[]), Match::None);
    }

    #[test]
    fn tree_keeps_one_action_per_sequence_and_no_prefixes() {
        let mut tree = SequenceTree::default();
        tree.insert(&seq("Ctrl-w h"), 1).unwrap();
        tree.insert(&seq("g"), 2).unwrap();

        assert_eq!(tree.insert(&seq("Ctrl-w h"), 9), Err(Overlap::Same));
        assert_eq!(
            tree.insert(&seq("Ctrl-w"), 9),
            Err(Overlap::Prefix(seq("Ctrl-w h")))
        );
        assert_eq!(tree.insert(&seq("g g"), 9), Err(Overlap::Prefix(seq("g"))));
        assert_eq!(tree.get(&seq("Ctrl-w h")), Some(&1));

        assert_eq!(tree.remove(&seq("g")), Some(2));
        tree.insert(&seq("g g"), 3).unwrap();
        assert_eq!(tree.find(&chords("g")), Match::Prefix);
        assert_eq!(tree.values().copied().collect::<Vec<_>>().len(), 2);
    }
}
