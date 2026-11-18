//! Перенос строки diff по ширине в символах моноширинного шрифта.
//!
//! Перенос посимвольный: строка рвётся перед символом, который не помещается в остаток
//! визуальной строки. Ширина в клетках: обычный символ — 1, широкий — 2, комбинирующий и
//! невидимый — 0, он остаётся с предыдущим символом. Табуляция доходит до следующей позиции,
//! кратной ширине табуляции от начала визуальной строки, но не дальше её конца. Управляющий
//! символ и каждый неверный фрагмент UTF-8 занимают одну клетку: вид рисует их знаком замены.
//! Таблица ширин сокращённая, см. [`cell_width`].
//!
//! Вид рисует визуальные строки по разрывам модели ([`segments`]), а не собственным переносом
//! Core Text: иначе число визуальных строк на экране и в индексе высот разойдётся.

use std::num::NonZeroU32;
use std::ops::{Range, RangeInclusive};

/// Ширина табуляции пробы в клетках.
pub const TAB_WIDTH: u32 = 4;

/// Символы нулевой ширины: комбинирующие знаки, невидимые разделители, селекторы вариантов.
const ZERO_WIDTH: &[RangeInclusive<u32>] = &[
    0x0300..=0x036F,
    0x0483..=0x0489,
    0x0591..=0x05BD,
    0x0610..=0x061A,
    0x064B..=0x065F,
    0x0E31..=0x0E31,
    0x0E34..=0x0E3A,
    0x1AB0..=0x1AFF,
    0x1DC0..=0x1DFF,
    0x200B..=0x200F,
    0x202A..=0x202E,
    0x2060..=0x2064,
    0x20D0..=0x20FF,
    0xFE00..=0xFE0F,
    0xFE20..=0xFE2F,
    0xFEFF..=0xFEFF,
    0xE0100..=0xE01EF,
];

/// Широкие символы: основные блоки East Asian Wide и Fullwidth и эмодзи.
const WIDE: &[RangeInclusive<u32>] = &[
    0x1100..=0x115F,
    0x231A..=0x231B,
    0x2329..=0x232A,
    0x23E9..=0x23EC,
    0x2E80..=0x303E,
    0x3041..=0x33FF,
    0x3400..=0x4DBF,
    0x4E00..=0x9FFF,
    0xA000..=0xA4CF,
    0xA960..=0xA97F,
    0xAC00..=0xD7A3,
    0xF900..=0xFAFF,
    0xFE10..=0xFE19,
    0xFE30..=0xFE6F,
    0xFF00..=0xFF60,
    0xFFE0..=0xFFE6,
    0x1F300..=0x1F64F,
    0x1F680..=0x1F6FF,
    0x1F900..=0x1F9FF,
    0x20000..=0x2FFFD,
    0x30000..=0x3FFFD,
];

/// Ширина области текста в клетках и ширина табуляции. Обе не меньше 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Wrap {
    columns: NonZeroU32,
    tab_width: NonZeroU32,
}

impl Wrap {
    pub fn new(columns: u32, tab_width: u32) -> Self {
        Wrap {
            columns: NonZeroU32::new(columns).unwrap_or(NonZeroU32::MIN),
            tab_width: NonZeroU32::new(tab_width).unwrap_or(NonZeroU32::MIN),
        }
    }

    /// Число клеток по ширине области текста и ширине символа шрифта из вида, не меньше 1.
    /// Запас 1e-6 клетки не даёт ошибке округления отнять клетку у точного кратного.
    pub fn from_width(text_width: f64, char_width: f64, tab_width: u32) -> Self {
        let cells = text_width / char_width + 1e-6;
        let columns = if cells.is_finite() && cells >= 1.0 {
            cells.floor() as u32
        } else {
            1
        };
        Wrap::new(columns, tab_width)
    }

    pub fn columns(&self) -> u32 {
        self.columns.get()
    }

    pub fn tab_width(&self) -> u32 {
        self.tab_width.get()
    }
}

/// Ширина символа в клетках, кроме табуляции: её ширина зависит от позиции.
pub fn cell_width(ch: char) -> u32 {
    let code = ch as u32;
    if code < 0x300 {
        return 1;
    }
    let within = |ranges: &[RangeInclusive<u32>]| ranges.iter().any(|range| range.contains(&code));
    if within(ZERO_WIDTH) {
        0
    } else if within(WIDE) {
        2
    } else {
        1
    }
}

/// Число визуальных строк строки diff, не меньше 1.
pub fn visual_lines(text: &[u8], wrap: Wrap) -> u32 {
    let mut lines: u32 = 1;
    breaks(text, wrap, |_| lines = lines.saturating_add(1));
    lines
}

/// Визуальные строки строки diff диапазонами байт текста подряд. У пустой строки одна пустая.
pub fn segments(text: &[u8], wrap: Wrap) -> Vec<Range<usize>> {
    let mut segments = Vec::new();
    let mut start = 0;
    breaks(text, wrap, |at| {
        segments.push(start..at);
        start = at;
    });
    segments.push(start..text.len());
    segments
}

enum Cell {
    Tab,
    Width(u32),
}

/// Вызывает `each` со смещением байта каждого разрыва — начала визуальной строки, кроме первой.
/// Каждый символ продвигает разбор, поэтому разбор конечен при любой ширине.
fn breaks(text: &[u8], wrap: Wrap, mut each: impl FnMut(usize)) {
    let columns = wrap.columns();
    if text.iter().all(|&byte| byte < 0x80 && byte != b'\t') {
        let step = columns as usize;
        let mut at = step;
        while at < text.len() {
            each(at);
            at += step;
        }
        return;
    }
    let tab = wrap.tab_width();
    let mut col: u32 = 0;
    let mut place = |at: usize, cell: Cell| match cell {
        Cell::Tab => {
            if col >= columns {
                each(at);
                col = 0;
            }
            col += (tab - col % tab).min(columns - col);
        }
        Cell::Width(0) => {}
        Cell::Width(width) => {
            if col > 0 && col.saturating_add(width) > columns {
                each(at);
                col = 0;
            }
            col = col.saturating_add(width);
        }
    };
    let mut offset = 0;
    for chunk in text.utf8_chunks() {
        for (index, ch) in chunk.valid().char_indices() {
            let cell = if ch == '\t' {
                Cell::Tab
            } else {
                Cell::Width(cell_width(ch))
            };
            place(offset + index, cell);
        }
        offset += chunk.valid().len();
        if !chunk.invalid().is_empty() {
            place(offset, Cell::Width(1));
            offset += chunk.invalid().len();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wrap(columns: u32) -> Wrap {
        Wrap::new(columns, TAB_WIDTH)
    }

    fn pieces(text: &str, columns: u32) -> Vec<&str> {
        segments(text.as_bytes(), wrap(columns))
            .into_iter()
            .map(|range| &text[range])
            .collect()
    }

    /// Клетки визуальной строки по правилам модуля: табуляция от начала визуальной строки.
    fn segment_cells(text: &[u8], tab: u32, columns: u32) -> u32 {
        let mut col = 0u32;
        for chunk in text.utf8_chunks() {
            for ch in chunk.valid().chars() {
                col += if ch == '\t' {
                    (tab - col % tab).min(columns.saturating_sub(col).max(1))
                } else {
                    cell_width(ch)
                };
            }
            if !chunk.invalid().is_empty() {
                col += 1;
            }
        }
        col
    }

    #[test]
    fn ascii_wraps_by_columns() {
        assert_eq!(visual_lines(b"", wrap(10)), 1);
        let empty = segments(b"", wrap(10));
        assert_eq!((empty.len(), empty.first()), (1, Some(&(0..0))));
        assert_eq!(visual_lines(b"0123456789", wrap(10)), 1);
        assert_eq!(pieces("0123456789a", 10), ["0123456789", "a"]);
        assert_eq!(pieces("abcdefg", 3), ["abc", "def", "g"]);
        assert_eq!(visual_lines(&[b'x'; 4000], wrap(80)), 50);
        assert_eq!(visual_lines(&[b'x'; 4001], wrap(80)), 51);
        assert_eq!(visual_lines(b"abc", wrap(1)), 3);
    }

    #[test]
    fn tab_reaches_next_stop_within_visual_line() {
        assert_eq!(pieces("\tx", 10), ["\tx"]);
        assert_eq!(pieces("abc\tdefgh", 8), ["abc\tdefg", "h"]);
        // Табуляция с 8-й клетки при ширине 10 доходит только до конца строки.
        assert_eq!(pieces("abcdefgh\tz", 10), ["abcdefgh\t", "z"]);
        // Строка заполнена: табуляция начинает новую визуальную строку и заполняет её.
        assert_eq!(pieces("abcd\tz", 4), ["abcd", "\t", "z"]);
        assert_eq!(pieces("abcd\tz", 6), ["abcd\t", "z"]);
        // Табуляция шире строки занимает строку целиком, разбор идёт дальше.
        assert_eq!(pieces("\t\t", 2), ["\t", "\t"]);
        assert_eq!(pieces("\t\t", 1), ["\t", "\t"]);
    }

    #[test]
    fn wide_characters_take_two_cells() {
        assert_eq!(pieces("日本語", 5), ["日本", "語"]);
        assert_eq!(pieces("a日", 2), ["a", "日"]);
        // Узкая строка: широкий символ стоит один и выходит за край, разбор не зацикливается.
        assert_eq!(pieces("日本語", 1), ["日", "本", "語"]);
        assert_eq!(pieces("ab🦀", 3), ["ab", "🦀"]);
    }

    #[test]
    fn zero_width_stays_with_previous_character() {
        assert_eq!(visual_lines("e\u{301}".as_bytes(), wrap(1)), 1);
        assert_eq!(pieces("ab\u{301}c", 2), ["ab\u{301}", "c"]);
        assert_eq!(pieces("\u{200b}ab", 2), ["\u{200b}ab"]);
        assert_eq!(cell_width('\u{fe0f}'), 0);
        assert_eq!(cell_width('a'), 1);
        assert_eq!(cell_width('\u{0}'), 1);
        assert_eq!(cell_width('Ж'), 1);
    }

    #[test]
    fn invalid_utf8_takes_one_cell_per_fragment() {
        let text = b"ab\xffcd";
        assert_eq!(segments(text, wrap(2)), [0..2, 2..4, 4..5]);
        // Обрезанная последовательность — один фрагмент.
        assert_eq!(segments(b"\xe6\x97x", wrap(1)), [0..2, 2..3]);
        assert_eq!(visual_lines(b"\xff\xfe\xfd", wrap(80)), 1);
    }

    #[test]
    fn width_in_points_gives_columns() {
        let columns = |width, char_width| Wrap::from_width(width, char_width, TAB_WIDTH).columns();
        assert_eq!(columns(800.0, 7.2), 111);
        assert_eq!(columns(72.0, 7.2), 10);
        assert_eq!(columns(7.0, 7.2), 1);
        assert_eq!(columns(0.0, 7.2), 1);
        assert_eq!(columns(100.0, 0.0), 1);
        assert_eq!(columns(f64::NAN, 7.2), 1);
        assert_eq!(Wrap::new(0, 0), Wrap::new(1, 1));
    }

    #[test]
    fn segments_agree_with_count_on_random_text() {
        const ALPHABET: &[&str] = &[
            "a", "b", " ", "\t", "日", "🦀", "e\u{301}", "\u{200b}", "Ж", "\u{1}",
        ];
        let mut seed: u64 = 0x5EED_0006;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..2000 {
            let len = (next() % 60) as usize;
            let mut text = Vec::new();
            for _ in 0..len {
                if next() % 23 == 0 {
                    text.push(0xFF);
                } else {
                    let piece = ALPHABET[(next() % ALPHABET.len() as u64) as usize];
                    text.extend_from_slice(piece.as_bytes());
                }
            }
            let columns = (next() % 20 + 1) as u32;
            let wrap = Wrap::new(columns, TAB_WIDTH);
            let parts = segments(&text, wrap);
            assert_eq!(parts.len() as u32, visual_lines(&text, wrap));
            assert_eq!(parts.first().map(|part| part.start), Some(0));
            assert_eq!(parts.last().map(|part| part.end), Some(text.len()));
            for pair in parts.windows(2) {
                assert_eq!(pair[0].end, pair[1].start);
                assert!(pair[1].start < pair[1].end, "пустая визуальная строка");
            }
            for part in &parts {
                let cells = segment_cells(&text[part.clone()], TAB_WIDTH, columns);
                // За край выходит только одиночный символ шире строки.
                assert!(cells <= columns.max(2), "{cells} > {columns}: {text:?}");
            }
        }
    }

    #[test]
    fn ascii_fast_path_matches_general_path() {
        // Невидимый знак в конце уводит разбор с быстрого пути, но клеток не добавляет.
        for len in 0..50usize {
            let ascii: Vec<u8> = (0..len).map(|i| b'a' + (i % 26) as u8).collect();
            let mut general = ascii.clone();
            general.extend_from_slice("\u{200b}".as_bytes());
            for columns in 1..12 {
                let wrap = Wrap::new(columns, TAB_WIDTH);
                assert_eq!(
                    visual_lines(&ascii, wrap),
                    visual_lines(&general, wrap),
                    "{len} {columns}"
                );
                let fast = segments(&ascii, wrap);
                let slow = segments(&general, wrap);
                assert_eq!(fast.len(), slow.len());
                for (fast, slow) in fast.iter().zip(&slow).take(fast.len() - 1) {
                    assert_eq!(fast, slow);
                }
            }
        }
    }
}
