//! Отрисовка строки показа и блоков тредов Core Text, общая для вариантов A и B. Строка и блоки
//! рисуются в координатах документа по индексу высот модели: A рисует их прямо в document view,
//! B — в виде строки таблицы, сдвинутом на её начало. Где хранится раскладка, решает вариант
//! ([`Store`]): у A — ограниченный кэш вида, у B — сам вид строки.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::ops::Range;

use objc2_core_foundation::{CFRetained, CGAffineTransform};
use objc2_core_graphics::CGContext;
use objc2_core_text::CTLine;
use objc2_foundation::{NSPoint, NSRect, NSSize};

use probe_diff_view::model::layout::Gutter;
use probe_diff_view::model::threads::{BlockItem, LineRole};
use probe_diff_view::model::wrap;
use probe_diff_view::model::{LineKind, Mode, Side};
use probe_diff_view::tokens::{Rgb, Theme};

use crate::blocks::{self, BlockFrame};
use crate::scenario::drawmark;
use crate::scene::{Model, Shown, drawn_side};
use crate::text::{ColorCache, Expanded, Font};

/// Экранов от видимых строк, дальше которых раскладка не строится.
pub const FAR_SCREENS: u32 = 2;

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

/// Половина строки показа: в unified одна, в «Рядом» старая и новая.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Half {
    Unified,
    Old,
    New,
}

impl Half {
    /// Половина «Рядом» стороны `side`.
    pub fn of(side: Side) -> Half {
        match side {
            Side::Old => Half::Old,
            Side::New => Half::New,
        }
    }
}

/// Раскладка половины строки показа: гуттер и визуальные строки текста.
pub struct RowLayout {
    gutter: CFRetained<CTLine>,
    /// Текст гуттера раскладки: журнал контракта пишет то, что нарисовано, а не пересчёт.
    gutter_text: String,
    segments: Vec<CFRetained<CTLine>>,
}

/// Визуальные строки блоков одной строки показа, по блоку на запись.
pub struct BlockLayout {
    blocks: Vec<Vec<CFRetained<CTLine>>>,
}

/// Хранилище раскладок варианта: отдаёт в `draw` раскладку строки `row` и половины `half`
/// версии `version`, построив её `build`, если её нет или версия другая.
pub trait Store<V> {
    fn with(
        &mut self,
        row: u32,
        half: Half,
        version: u64,
        build: &mut dyn FnMut() -> V,
        draw: &mut dyn FnMut(&V),
    );
}

/// Контекст отрисовки: тема, шрифт, цвета и эпоха вида — версия раскладок строк.
pub struct Painter<'a> {
    pub context: &'a CGContext,
    pub theme: &'a Theme,
    pub font: &'a Font,
    pub colors: &'a mut ColorCache,
    pub epoch: u64,
}

/// Полоса текста строки показа в координатах документа.
struct Band {
    row: u32,
    top: f64,
    height: f64,
}

pub fn fill(context: &CGContext, color: Rgb, area: NSRect) {
    let [r, g, b, a] = color.components();
    CGContext::set_rgb_fill_color(Some(context), r, g, b, a);
    CGContext::fill_rect(Some(context), area);
}

/// Матрица текста для перевёрнутого вида: строки Core Text рисуются головой вверх.
pub fn flip_text(context: &CGContext) {
    CGContext::set_text_matrix(
        Some(context),
        CGAffineTransform {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: -1.0,
            tx: 0.0,
            ty: 0.0,
        },
    );
}

/// Строка показа `row` шириной `width`: в unified одна половина во всю ширину, в «Рядом» старая
/// слева и новая справа за разделителем, пустая половина залита фоном гуттера. Граница ханка —
/// линия поверх строки.
pub fn paint_row(
    painter: &mut Painter<'_>,
    model: &Model,
    row: u32,
    width: f64,
    store: &mut dyn Store<RowLayout>,
) {
    let Some(shown) = model.shown() else {
        return;
    };
    let mode = model.mode;
    let (Some(top), Some(text_height), Some(halves)) = (
        shown.heights.top(row),
        shown.heights.text_height(row),
        shown.body.row(mode, row),
    ) else {
        return;
    };
    let theme = painter.theme;
    let band = Band {
        row,
        top: top as f64,
        height: f64::from(text_height),
    };
    match mode {
        Mode::Unified => {
            if let Some(index) = halves.first() {
                paint_half(
                    painter, model, &shown, &band, index, None, 0.0, width, store,
                );
            }
        }
        Mode::SideBySide => {
            for side in Side::BOTH {
                let x = shown.geometry.half_origin(side);
                let half_width = match side {
                    Side::Old => shown.geometry.half_width(),
                    Side::New => (width - x).max(0.0),
                };
                match halves.half(side) {
                    Some(index) => paint_half(
                        painter,
                        model,
                        &shown,
                        &band,
                        index,
                        Some(side),
                        x,
                        half_width,
                        store,
                    ),
                    None => fill(
                        painter.context,
                        theme.gutter_background,
                        NSRect::new(
                            NSPoint::new(x, band.top),
                            NSSize::new(half_width, band.height),
                        ),
                    ),
                }
            }
            let (x, divider) = shown.geometry.divider();
            fill(
                painter.context,
                theme.separator,
                NSRect::new(NSPoint::new(x, band.top), NSSize::new(divider, band.height)),
            );
        }
    }
    if row > 0 && shown.body.starts_hunk(mode, row) {
        fill(
            painter.context,
            theme.separator,
            NSRect::new(NSPoint::new(0.0, band.top), NSSize::new(width, 1.0)),
        );
    }
}

/// Половина строки показа от `x` шириной `width`: фон по виду строки, гуттер с номером и
/// знаком, визуальные строки текста. `side` — половина «Рядом», `None` — unified. Курсор
/// отмечен фоном гуттера, выделение — фоном текста половины своей стороны.
#[allow(clippy::too_many_arguments)]
fn paint_half(
    painter: &mut Painter<'_>,
    model: &Model,
    shown: &Shown<'_>,
    band: &Band,
    index: u32,
    side: Option<Side>,
    x: f64,
    width: f64,
    store: &mut dyn Store<RowLayout>,
) {
    let Some(line) = shown.body.line(index) else {
        return;
    };
    let mode = model.mode;
    let (context, theme, font) = (painter.context, painter.theme, painter.font);
    let colors = &mut *painter.colors;
    let gutter_cells = shown.geometry.gutter_cells(mode);
    let gutter_width = f64::from(gutter_cells) * font.char_width;
    let kind = line.kind();
    let state = model.state.as_ref();
    let cursor = state
        .and_then(|state| state.cursor())
        .is_some_and(|cursor| match side {
            Some(side) => line.addr(side) == Some(cursor.addr),
            None => shown
                .body
                .row_of_addr(mode, cursor.addr)
                .is_some_and(|located| located.index == band.row),
        });
    let selected = state
        .and_then(|state| state.selection())
        .is_some_and(|selection| {
            side.is_none_or(|side| side == selection.side)
                && line
                    .addr(selection.side)
                    .is_some_and(|addr| selection.contains(addr))
        });
    let (text_fill, gutter_fill) = match kind {
        LineKind::Context => (theme.background, theme.gutter_background),
        LineKind::Added => (theme.added_background, theme.added_gutter),
        LineKind::Removed => (theme.removed_background, theme.removed_gutter),
    };
    let text_fill = if selected {
        theme.selection
    } else if cursor && kind == LineKind::Context {
        theme.cursor_line
    } else {
        text_fill
    };
    let gutter_fill = if cursor { theme.selection } else { gutter_fill };
    fill(
        context,
        gutter_fill,
        NSRect::new(
            NSPoint::new(x, band.top),
            NSSize::new(gutter_width.min(width), band.height),
        ),
    );
    fill(
        context,
        text_fill,
        NSRect::new(
            NSPoint::new(x + gutter_width, band.top),
            NSSize::new((width - gutter_width).max(0.0), band.height),
        ),
    );
    let text = shown.diff.line_text(line);
    let drawn = drawn_side(kind, mode, side.unwrap_or(Side::New));
    let runs = line
        .line(drawn)
        .map_or(&[][..], |number| model.styles.runs(drawn, number));
    let (gutter, numbers, half) = match side {
        None => (
            shown.geometry.gutter,
            (line.line(Side::Old), line.line(Side::New)),
            Half::Unified,
        ),
        Some(side) => (
            Gutter::Single,
            match side {
                Side::Old => (line.line(Side::Old), None),
                Side::New => (None, line.line(Side::New)),
            },
            Half::of(side),
        ),
    };
    let wrap = shown.metrics.wrap;
    let mut build = || {
        let gutter = gutter_text(gutter, gutter_cells, numbers.0, numbers.1, kind);
        let gutter_units: Vec<u16> = gutter.encode_utf16().collect();
        RowLayout {
            gutter_text: gutter.clone(),
            gutter: font.line(
                &gutter_units,
                &[(0..gutter_units.len(), theme.gutter_text)],
                colors,
            ),
            segments: wrap::segments(text, wrap)
                .into_iter()
                .map(|segment| font.segment_line(text, segment, wrap, runs, theme, colors))
                .collect(),
        }
    };
    let line_height = f64::from(shown.metrics.line_height);
    let record = drawmark::recording();
    let baseline = |visual: usize| band.top + visual as f64 * line_height + font.ascent;
    let mut draw = |layout: &RowLayout| {
        CGContext::set_text_position(Some(context), x, baseline(0));
        // SAFETY: строка Core Text и контекст живы на время вызова.
        unsafe { layout.gutter.draw(context) };
        if record {
            drawmark::drawn_gutter(model.generation, band.row, side, &layout.gutter_text);
        }
        for (visual, segment) in layout.segments.iter().enumerate() {
            CGContext::set_text_position(Some(context), x + gutter_width, baseline(visual));
            // SAFETY: строка Core Text и контекст живы на время вызова.
            unsafe { segment.draw(context) };
        }
    };
    store.with(band.row, half, painter.epoch, &mut build, &mut draw);
}

/// Версия раскладки блоков строки: эпоха вида и состав блоков.
fn block_version(epoch: u64, frames: &[BlockFrame], expanded: impl Fn(BlockItem) -> bool) -> u64 {
    let mut hasher = DefaultHasher::new();
    epoch.hash(&mut hasher);
    for frame in frames {
        frame.entry.item.hash(&mut hasher);
        frame.entry.height.hash(&mut hasher);
        expanded(frame.entry.item).hash(&mut hasher);
    }
    hasher.finish()
}

/// Блоки под строкой показа `row`: фон во всю ширину `width`, колонка текста цветом блока и
/// строки треда. Блок черновика — фон под полем черновика.
pub fn paint_blocks(
    painter: &mut Painter<'_>,
    model: &Model,
    row: u32,
    width: f64,
    store: &mut dyn Store<BlockLayout>,
) {
    let Some(place) = model.place() else {
        return;
    };
    let frames = place.frames_at_row(row);
    if frames.is_empty() {
        return;
    }
    let (context, theme, font) = (painter.context, painter.theme, painter.font);
    let colors = &mut *painter.colors;
    let state = model.state.as_ref();
    let expanded = |item: BlockItem| match item {
        BlockItem::Thread(id) => state.is_some_and(|state| state.is_expanded(id)),
        BlockItem::Draft => false,
    };
    for frame in &frames {
        fill(
            context,
            theme.gutter_background,
            NSRect::new(NSPoint::new(0.0, frame.y), NSSize::new(width, frame.height)),
        );
        fill(
            context,
            theme.thread_background,
            NSRect::new(
                NSPoint::new(frame.x, frame.y),
                NSSize::new(frame.width, frame.height),
            ),
        );
    }
    let Some(metrics) = model.metrics.map(blocks::block_metrics) else {
        return;
    };
    let version = block_version(painter.epoch, &frames, expanded);
    let mut build = || BlockLayout {
        blocks: frames
            .iter()
            .map(|frame| match frame.entry.item {
                BlockItem::Thread(id) => model
                    .threads
                    .get(id)
                    .map(|thread| metrics.thread_lines(thread, expanded(frame.entry.item)))
                    .unwrap_or_default()
                    .iter()
                    .map(|line| {
                        let color = match line.role {
                            LineRole::Header | LineRole::Author => theme.gutter_text,
                            LineRole::Body => theme.text,
                        };
                        let expanded =
                            Expanded::new(line.text.as_bytes(), line.segment.clone(), metrics.wrap);
                        font.line(&expanded.text, &[(0..expanded.text.len(), color)], colors)
                    })
                    .collect(),
                BlockItem::Draft => Vec::new(),
            })
            .collect(),
    };
    let line_height = f64::from(metrics.line_height);
    let mut draw = |layout: &BlockLayout| {
        for (frame, lines) in frames.iter().zip(&layout.blocks) {
            for (visual, line) in lines.iter().enumerate() {
                CGContext::set_text_position(
                    Some(context),
                    frame.x,
                    frame.y + visual as f64 * line_height + font.ascent,
                );
                // SAFETY: строка Core Text и контекст живы на время вызова.
                unsafe { line.draw(context) };
            }
        }
    };
    store.with(row, Half::Unified, version, &mut build, &mut draw);
}

/// Текст гуттера unified: номера сторон по своим колонкам и знак. Колонок номеров — по
/// [`Gutter`], ширина — `cells` клеток.
pub fn gutter_text(
    gutter: Gutter,
    cells: u32,
    old: Option<u32>,
    new: Option<u32>,
    kind: LineKind,
) -> String {
    let sign = match kind {
        LineKind::Context => ' ',
        LineKind::Added => '+',
        LineKind::Removed => '-',
    };
    let numbers = match gutter {
        Gutter::Both => 2,
        Gutter::Single => 1,
    };
    // Клетки знака и пробела после него; остальное — колонки номеров с пробелом после каждой.
    let digits = (cells.saturating_sub(2) / numbers).saturating_sub(1) as usize;
    let number = |value: Option<u32>| {
        value.map_or_else(|| " ".repeat(digits), |value| format!("{value:>digits$}"))
    };
    match gutter {
        Gutter::Both => format!("{} {} {sign} ", number(old), number(new)),
        Gutter::Single => format!("{} {sign} ", number(new.or(old))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gutter_text_fills_its_cells() {
        // 6 цифр: две колонки по 7 клеток и знак с пробелом — 16 клеток.
        let text = gutter_text(Gutter::Both, 16, Some(12), Some(345_678), LineKind::Context);
        assert_eq!(text, "    12 345678   ");
        assert_eq!(text.chars().count(), 16);
        let text = gutter_text(Gutter::Both, 16, None, Some(7), LineKind::Added);
        assert_eq!(text, "            7 + ");
        let text = gutter_text(Gutter::Both, 16, Some(9), None, LineKind::Removed);
        assert_eq!(text, "     9        - ");
        let text = gutter_text(Gutter::Single, 9, Some(9), None, LineKind::Removed);
        assert_eq!(text, "     9 - ");
    }
}
