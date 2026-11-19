//! Core Text для обоих вариантов отрисовки: моноширинный шрифт и его метрики, раскладка одной
//! визуальной строки строки diff в `CTLine` с цветами классов подсветки.
//!
//! Визуальную строку задаёт перенос модели ([`wrap::segments`]), а не Core Text. Табуляция
//! раскрывается пробелами до клеток модели, управляющий символ и неверный фрагмент UTF-8
//! рисуются знаком замены: так число клеток на экране совпадает с индексом высот. Отрезки
//! подсветки заданы байтами строки, атрибуты Core Text — единицами UTF-16, перевод — по таблице
//! смещений [`Expanded`].

use std::collections::HashMap;
use std::ops::Range;

use objc2::MainThreadMarker;
use objc2_app_kit::{NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSApplication, NSWorkspace};
use objc2_core_foundation::{
    CFAttributedString, CFIndex, CFMutableAttributedString, CFRange, CFRetained, CFString, CFType,
};
use objc2_core_graphics::CGColor;
use objc2_core_text::{CTFont, CTLine, kCTFontAttributeName, kCTForegroundColorAttributeName};
use objc2_foundation::NSArray;

use probe_diff_view::highlight::Run;
use probe_diff_view::model::wrap::{self, Wrap};
use probe_diff_view::tokens::{Appearance, Rgb, Theme, ThemeKey};

/// Тема вида по оформлению приложения и Increase Contrast.
pub fn current_theme(mtm: MainThreadMarker) -> ThemeKey {
    let app = NSApplication::sharedApplication(mtm);
    // SAFETY: имена оформлений — неизменяемые константы AppKit.
    let names = unsafe { NSArray::from_slice(&[NSAppearanceNameAqua, NSAppearanceNameDarkAqua]) };
    let dark = app
        .effectiveAppearance()
        .bestMatchFromAppearancesWithNames(&names)
        // SAFETY: сравнение с неизменяемой константой AppKit.
        .is_some_and(|name| unsafe { &*name == NSAppearanceNameDarkAqua });
    ThemeKey {
        appearance: if dark {
            Appearance::Dark
        } else {
            Appearance::Light
        },
        increase_contrast: NSWorkspace::sharedWorkspace()
            .accessibilityDisplayShouldIncreaseContrast(),
    }
}

/// Шрифт вида: имя и кегль записаны в отчёт до первого прогона.
pub const FONT_NAME: &str = "Menlo";
pub const FONT_SIZE: f64 = 12.0;
const REPLACEMENT: char = '\u{FFFD}';

/// Текст визуальной строки для Core Text: единицы UTF-16 и смещение каждого байта сегмента в
/// них. `units[i]` — начало символа, в котором стоит байт `start + i`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Expanded {
    pub text: Vec<u16>,
    start: usize,
    units: Vec<usize>,
}

impl Expanded {
    /// Раскрывает сегмент `segment` текста строки по правилам переноса `wrap`.
    pub fn new(text: &[u8], segment: Range<usize>, wrap: Wrap) -> Expanded {
        let start = segment.start.min(text.len());
        let end = segment.end.clamp(start, text.len());
        let bytes = &text[start..end];
        let mut out = Expanded {
            text: Vec::with_capacity(bytes.len()),
            start,
            units: vec![0; bytes.len() + 1],
        };
        let columns = wrap.columns();
        let tab = wrap.tab_width();
        let mut col: u32 = 0;
        let mut offset = 0;
        let push = |out: &mut Expanded, at: usize, len: usize, ch: char, col: &mut u32| {
            let unit = out.text.len();
            for slot in &mut out.units[at..at + len] {
                *slot = unit;
            }
            if ch == '\t' {
                let spaces = (tab - *col % tab).min(columns.saturating_sub(*col)).max(1);
                out.text
                    .extend(std::iter::repeat_n(u16::from(b' '), spaces as usize));
                *col += spaces;
                return;
            }
            let shown = if ch.is_control() { REPLACEMENT } else { ch };
            let mut buffer = [0u16; 2];
            out.text.extend_from_slice(shown.encode_utf16(&mut buffer));
            *col += if ch.is_control() {
                1
            } else {
                wrap::cell_width(ch)
            };
        };
        for chunk in bytes.utf8_chunks() {
            for (index, ch) in chunk.valid().char_indices() {
                push(&mut out, offset + index, ch.len_utf8(), ch, &mut col);
            }
            offset += chunk.valid().len();
            let invalid = chunk.invalid().len();
            if invalid > 0 {
                push(&mut out, offset, invalid, REPLACEMENT, &mut col);
                offset += invalid;
            }
        }
        if let Some(last) = out.units.last_mut() {
            *last = out.text.len();
        }
        out
    }

    /// Единица UTF-16 байта строки `byte`, обрезанного до сегмента.
    pub fn unit(&self, byte: usize) -> usize {
        let index = byte.saturating_sub(self.start).min(self.units.len() - 1);
        self.units[index]
    }

    /// Диапазон единиц отрезка подсветки, обрезанного до сегмента. Пустой — отрезка в сегменте
    /// нет.
    pub fn run_units(&self, run: &Run) -> Range<usize> {
        let end_byte = self.start + self.units.len() - 1;
        let start = (run.start as usize).clamp(self.start, end_byte);
        let end = (run.end as usize).clamp(self.start, end_byte);
        self.unit(start)..self.unit(end)
    }
}

/// Моноширинный шрифт и его метрики.
pub struct Font {
    pub ct: CFRetained<CTFont>,
    /// Ширина клетки в точках.
    pub char_width: f64,
    /// Высота визуальной строки в целых точках.
    pub line_height: u32,
    pub ascent: f64,
}

impl Font {
    pub fn monospaced() -> Font {
        let name = CFString::from_str(FONT_NAME);
        // SAFETY: имя — живая строка CF, матрица не передаётся.
        let ct = unsafe { CTFont::with_name(&name, FONT_SIZE, std::ptr::null()) };
        // SAFETY: шрифт жив, функции метрик его не сохраняют.
        let (ascent, descent, leading) = unsafe { (ct.ascent(), ct.descent(), ct.leading()) };
        let mut font = Font {
            ct,
            char_width: 0.0,
            line_height: (ascent + descent + leading).ceil().max(1.0) as u32,
            ascent,
        };
        let probe: Vec<u16> = "0000000000".encode_utf16().collect();
        let line = font.line(&probe, &[], &mut ColorCache::default());
        // SAFETY: строка жива, указатели метрик могут быть нулевыми.
        let width = unsafe {
            line.typographic_bounds(
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        font.char_width = width / 10.0;
        font
    }

    /// `CTLine` текста `text` с отрезками цвета: диапазоны единиц UTF-16 и цвет.
    pub fn line(
        &self,
        text: &[u16],
        colors: &[(Range<usize>, Rgb)],
        cache: &mut ColorCache,
    ) -> CFRetained<CTLine> {
        // SAFETY: буфер жив на время вызова, CF копирует символы.
        let string =
            unsafe { CFString::with_characters(None, text.as_ptr(), text.len() as CFIndex) };
        let attributed = CFMutableAttributedString::new(None, 0);
        if let (Some(string), Some(attributed)) = (string, attributed.as_deref()) {
            let whole = CFRange::new(0, text.len() as CFIndex);
            // SAFETY: строка и атрибутированная строка живы, ключи — константы Core Text, значения
            // нужного типа: шрифт и `CGColor`.
            unsafe {
                CFMutableAttributedString::replace_string(
                    Some(attributed),
                    CFRange::new(0, 0),
                    Some(&string),
                );
                CFMutableAttributedString::set_attribute(
                    Some(attributed),
                    whole,
                    Some(kCTFontAttributeName),
                    Some(cf_type(&*self.ct)),
                );
                for (units, rgb) in colors {
                    if units.is_empty() || units.end > text.len() {
                        continue;
                    }
                    let color = cache.get(*rgb);
                    CFMutableAttributedString::set_attribute(
                        Some(attributed),
                        CFRange::new(units.start as CFIndex, units.len() as CFIndex),
                        Some(kCTForegroundColorAttributeName),
                        Some(cf_type(&*color)),
                    );
                }
            }
        }
        let attributed: Option<&CFAttributedString> = attributed.as_deref().map(|value| &**value);
        match attributed {
            // SAFETY: атрибутированная строка жива, `CTLine` её копирует.
            Some(attributed) => unsafe { CTLine::with_attributed_string(attributed) },
            None => {
                let empty = CFMutableAttributedString::new(None, 0);
                // SAFETY: пустая строка допустима для `CTLineCreateWithAttributedString`.
                match empty.as_deref() {
                    Some(empty) => unsafe { CTLine::with_attributed_string(empty) },
                    None => unreachable!("CFAttributedStringCreateMutable вернул NULL"),
                }
            }
        }
    }

    /// Визуальная строка строки diff: сегмент `segment` текста `text`, отрезки подсветки `runs`
    /// и цвета темы. Текст без отрезка рисуется цветом текста темы.
    pub fn segment_line(
        &self,
        text: &[u8],
        segment: Range<usize>,
        wrap: Wrap,
        runs: &[Run],
        theme: &Theme,
        cache: &mut ColorCache,
    ) -> CFRetained<CTLine> {
        let expanded = Expanded::new(text, segment, wrap);
        let mut colors = vec![(0..expanded.text.len(), theme.text)];
        colors.extend(
            runs.iter()
                .map(|run| (expanded.run_units(run), theme.class(run.class)))
                .filter(|(units, _)| !units.is_empty()),
        );
        self.line(&expanded.text, &colors, cache)
    }
}

fn cf_type<T: AsRef<CFType>>(value: &T) -> &CFType {
    value.as_ref()
}

/// `CGColor` токенов темы: один объект на цвет.
#[derive(Default)]
pub struct ColorCache {
    colors: HashMap<Rgb, CFRetained<CGColor>>,
}

impl ColorCache {
    pub fn get(&mut self, rgb: Rgb) -> CFRetained<CGColor> {
        self.colors
            .entry(rgb)
            .or_insert_with(|| {
                let [r, g, b, a] = rgb.components();
                CGColor::new_srgb(r, g, b, a)
            })
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use probe_diff_view::highlight::TokenClass;
    use probe_diff_view::model::wrap::TAB_WIDTH;

    use super::*;

    fn text(expanded: &Expanded) -> String {
        String::from_utf16_lossy(&expanded.text)
    }

    #[test]
    fn ascii_maps_byte_to_unit_one_to_one() {
        let line = b"let x = 1;";
        let expanded = Expanded::new(line, 4..10, Wrap::new(80, TAB_WIDTH));
        assert_eq!(text(&expanded), "x = 1;");
        assert_eq!(expanded.unit(4), 0);
        assert_eq!(expanded.unit(9), 5);
        assert_eq!(expanded.unit(10), 6);
        // Отрезок до сегмента обрезается.
        let run = Run {
            start: 0,
            end: 5,
            class: TokenClass::Keyword,
        };
        assert_eq!(expanded.run_units(&run), 0..1);
    }

    #[test]
    fn tabs_expand_to_model_cells_from_segment_start() {
        let line = b"a\tbc\td";
        let expanded = Expanded::new(line, 0..line.len(), Wrap::new(80, TAB_WIDTH));
        assert_eq!(text(&expanded), "a   bc  d");
        assert_eq!(expanded.unit(2), 4);
        assert_eq!(expanded.unit(5), 8);
        // Табуляция у края визуальной строки не выходит за неё.
        let expanded = Expanded::new(b"abcd\t", 0..5, Wrap::new(5, TAB_WIDTH));
        assert_eq!(text(&expanded), "abcd ");
    }

    #[test]
    fn cyrillic_and_wide_characters_map_by_char_start() {
        let line = "й = \"日本\";".as_bytes();
        let expanded = Expanded::new(line, 0..line.len(), Wrap::new(80, TAB_WIDTH));
        assert_eq!(text(&expanded), "й = \"日本\";");
        // `й` — два байта, одна единица UTF-16; байт внутри символа даёт его начало.
        assert_eq!(expanded.unit(1), 0);
        assert_eq!(expanded.unit(2), 1);
        let string = Run {
            start: 5,
            end: 13,
            class: TokenClass::String,
        };
        assert_eq!(expanded.run_units(&string), 4..8);
    }

    #[test]
    fn controls_and_invalid_utf8_become_one_replacement_each() {
        let line = b"a\x01b\xff\xfec";
        let expanded = Expanded::new(line, 0..line.len(), Wrap::new(80, TAB_WIDTH));
        assert_eq!(text(&expanded), "a\u{FFFD}b\u{FFFD}\u{FFFD}c");
        assert_eq!(expanded.unit(3), 3);
        assert_eq!(expanded.unit(5), 5);
        assert_eq!(expanded.unit(6), 6);
    }

    #[test]
    fn astral_characters_take_two_units() {
        let line = "a😀b".as_bytes();
        let expanded = Expanded::new(line, 0..line.len(), Wrap::new(80, TAB_WIDTH));
        assert_eq!(expanded.text.len(), 4);
        assert_eq!(expanded.unit(1), 1);
        assert_eq!(expanded.unit(5), 3);
    }

    #[test]
    fn segments_of_the_model_cover_the_line_once() {
        let line = "fn x() { let s = \"а\tб\"; }".repeat(5);
        let wrap = Wrap::new(17, TAB_WIDTH);
        let mut cells = 0;
        for segment in wrap::segments(line.as_bytes(), wrap) {
            let expanded = Expanded::new(line.as_bytes(), segment, wrap);
            let shown: u32 = String::from_utf16_lossy(&expanded.text)
                .chars()
                .map(wrap::cell_width)
                .sum();
            assert!(shown <= wrap.columns(), "{shown}");
            cells += shown;
        }
        assert!(cells >= line.chars().count() as u32);
    }
}
