//! Смещения строк Rust (байты UTF-8) и диапазоны `NSRange` AppKit (единицы UTF-16).
//!
//! `NSRange` считает единицы UTF-16, поэтому символ вне базовой плоскости (эмодзи) занимает две
//! единицы — суррогатную пару. Смещение внутри пары не соответствует ни одной границе символа
//! строки Rust и переводится в `None`. `NSNotFound` — `NSIntegerMax`.

use std::ops::Range;

use serde::Serialize;

/// `NSNotFound`.
pub const NOT_FOUND: usize = isize::MAX as usize;

/// Диапазон в единицах UTF-16, как `NSRange`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Range16 {
    pub location: usize,
    pub length: usize,
}

impl Default for Range16 {
    fn default() -> Range16 {
        Range16::NONE
    }
}

impl Range16 {
    /// `{NSNotFound, 0}`: диапазона нет.
    pub const NONE: Range16 = Range16 {
        location: NOT_FOUND,
        length: 0,
    };

    pub const fn new(location: usize, length: usize) -> Range16 {
        Range16 { location, length }
    }

    pub fn is_none(self) -> bool {
        self.location == NOT_FOUND
    }

    /// Конец диапазона; `None` для `NSNotFound` и переполнения.
    pub fn end(self) -> Option<usize> {
        if self.is_none() {
            return None;
        }
        self.location.checked_add(self.length)
    }
}

/// Длина строки в единицах UTF-16.
pub fn len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// Смещение UTF-16 для смещения UTF-8. `None`, если байт вне строки или не на границе символа.
pub fn from_utf8(text: &str, byte: usize) -> Option<usize> {
    if byte > text.len() || !text.is_char_boundary(byte) {
        return None;
    }
    Some(len(&text[..byte]))
}

/// Смещение UTF-8 для смещения UTF-16. `None`, если оно за концом строки или внутри суррогатной
/// пары.
pub fn to_utf8(text: &str, unit: usize) -> Option<usize> {
    let mut units = 0;
    for (byte, ch) in text.char_indices() {
        if units == unit {
            return Some(byte);
        }
        units += ch.len_utf16();
        if units > unit {
            return None;
        }
    }
    (units == unit).then_some(text.len())
}

/// Байтовый диапазон строки для диапазона UTF-16. `None` для `NSNotFound`, выхода за конец и
/// границы внутри суррогатной пары.
pub fn range_to_utf8(text: &str, range: Range16) -> Option<Range<usize>> {
    let end = range.end()?;
    Some(to_utf8(text, range.location)?..to_utf8(text, end)?)
}

/// Диапазон UTF-16 для байтового диапазона строки.
pub fn range_from_utf8(text: &str, range: Range<usize>) -> Option<Range16> {
    let start = from_utf8(text, range.start)?;
    let end = from_utf8(text, range.end)?;
    Some(Range16::new(start, end.checked_sub(start)?))
}

/// Диапазон внутри документа длины `len16`: начало и длина обрезаются по концу. `NSNotFound`
/// остаётся `NSNotFound`.
pub fn clamp(range: Range16, len16: usize) -> Range16 {
    if range.is_none() {
        return Range16::NONE;
    }
    let location = range.location.min(len16);
    Range16::new(location, range.length.min(len16 - location))
}

/// Сборка текста `insertText:` из единиц UTF-16. Ввод символа вне базовой плоскости может прийти
/// двумя вызовами по половине пары: ведущая половина ждёт следующего вызова, одиночная
/// замыкающая отбрасывается, как в Terminal.app.
#[derive(Debug, Default)]
pub struct Surrogates {
    lead: Option<u16>,
}

impl Surrogates {
    pub fn push(&mut self, units: &[u16]) -> String {
        if let [unit] = units {
            if is_lead(*unit) {
                self.lead = Some(*unit);
                return String::new();
            }
            if is_trail(*unit) {
                return match self.lead.take() {
                    Some(lead) => String::from_utf16_lossy(&[lead, *unit]),
                    None => String::new(),
                };
            }
        }
        self.lead = None;
        String::from_utf16_lossy(units)
    }
}

fn is_lead(unit: u16) -> bool {
    (0xD800..=0xDBFF).contains(&unit)
}

fn is_trail(unit: u16) -> bool {
    (0xDC00..=0xDFFF).contains(&unit)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIXED: &str = "aж🙂b";

    #[test]
    fn lengths_count_utf16_units() {
        assert_eq!(len(""), 0);
        assert_eq!(len("abc"), 3);
        assert_eq!(len("жук"), 3, "кириллица — одна единица на букву");
        assert_eq!(len("🙂"), 2, "эмодзи — суррогатная пара");
        assert_eq!(len(MIXED), 5);
    }

    #[test]
    fn utf8_to_utf16_on_boundaries_only() {
        // a=1 байт, ж=2, 🙂=4, b=1.
        assert_eq!(from_utf8(MIXED, 0), Some(0));
        assert_eq!(from_utf8(MIXED, 1), Some(1));
        assert_eq!(from_utf8(MIXED, 2), None, "внутри «ж»");
        assert_eq!(from_utf8(MIXED, 3), Some(2));
        assert_eq!(from_utf8(MIXED, 5), None, "внутри эмодзи");
        assert_eq!(from_utf8(MIXED, 7), Some(4));
        assert_eq!(from_utf8(MIXED, 8), Some(5));
        assert_eq!(from_utf8(MIXED, 9), None, "за концом");
    }

    #[test]
    fn utf16_to_utf8_rejects_split_surrogate_pairs() {
        assert_eq!(to_utf8(MIXED, 0), Some(0));
        assert_eq!(to_utf8(MIXED, 1), Some(1));
        assert_eq!(to_utf8(MIXED, 2), Some(3));
        assert_eq!(to_utf8(MIXED, 3), None, "между половинами пары");
        assert_eq!(to_utf8(MIXED, 4), Some(7));
        assert_eq!(to_utf8(MIXED, 5), Some(8));
        assert_eq!(to_utf8(MIXED, 6), None);
        assert_eq!(to_utf8("", 0), Some(0));
        assert_eq!(to_utf8("жук", 2), Some(4));
    }

    #[test]
    fn ranges_round_trip_and_not_found() {
        let emoji = Range16::new(2, 2);
        assert_eq!(range_to_utf8(MIXED, emoji), Some(3..7));
        assert_eq!(&MIXED[3..7], "🙂");
        assert_eq!(range_from_utf8(MIXED, 3..7), Some(emoji));
        assert_eq!(range_to_utf8(MIXED, Range16::new(3, 1)), None);
        assert_eq!(range_to_utf8(MIXED, Range16::NONE), None);
        assert_eq!(range_to_utf8(MIXED, Range16::new(4, 9)), None);
        assert_eq!(range_from_utf8(MIXED, 2..3), None);
        assert!(Range16::NONE.is_none());
        assert_eq!(Range16::NONE.end(), None);
        assert_eq!(Range16::new(usize::MAX - 1, 5).end(), None);
        assert_eq!(NOT_FOUND, 0x7fff_ffff_ffff_ffff);
    }

    #[test]
    fn clamp_keeps_not_found_and_cuts_at_end() {
        assert_eq!(clamp(Range16::NONE, 4), Range16::NONE);
        assert_eq!(clamp(Range16::new(1, 2), 4), Range16::new(1, 2));
        assert_eq!(clamp(Range16::new(3, 9), 4), Range16::new(3, 1));
        assert_eq!(clamp(Range16::new(9, 1), 4), Range16::new(4, 0));
    }

    #[test]
    fn surrogate_halves_join_across_calls() {
        let units: Vec<u16> = "🙂".encode_utf16().collect();
        let mut joiner = Surrogates::default();
        assert_eq!(joiner.push(&units[..1]), "");
        assert_eq!(joiner.push(&units[1..]), "🙂");
        assert_eq!(joiner.push(&units[1..]), "", "одиночная замыкающая");
        assert_eq!(joiner.push(&units[..1]), "");
        let text: Vec<u16> = "ж".encode_utf16().collect();
        assert_eq!(joiner.push(&text), "ж", "другой текст снимает ведущую");
        assert_eq!(joiner.push(&units[1..]), "");
        assert_eq!(joiner.push(&units), "🙂", "целая пара за один вызов");
    }
}
