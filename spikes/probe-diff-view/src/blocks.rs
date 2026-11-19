//! Блоки тредов и черновика под строкой показа: геометрия, общая для вариантов A и B. Блоки
//! одной строки стоят под её текстом один под другим по порядку раскладки. Текст блока идёт в
//! колонке текста: в unified — строки без гуттера, в «Рядом» — половины стороны якоря без её
//! гуттера. Поэтому перенос комментариев равен переносу строк diff режима и меняется вместе с ним
//! в индексе высот.

use probe_diff_view::model::heights::{HeightIndex, Metrics};
use probe_diff_view::model::layout::DiffGeometry;
use probe_diff_view::model::threads::{BlockItem, BlockMetrics, Entry, ThreadLayout, Threads};
use probe_diff_view::model::{Mode, Side, TextBody};

/// Строк свёрнутого треда.
pub const COLLAPSED_LINES: u32 = 1;
/// Строк поля черновика.
pub const DRAFT_LINES: u32 = 4;
/// Автор комментариев, сохранённых в пробе.
pub const AUTHOR: &str = "probe";

/// Высоты блоков при переносе строк diff `metrics`.
pub fn block_metrics(metrics: Metrics) -> BlockMetrics {
    BlockMetrics {
        line_height: metrics.line_height,
        wrap: metrics.wrap,
        collapsed_lines: COLLAPSED_LINES,
        draft_lines: DRAFT_LINES,
    }
}

/// Раскладка, в которой ищутся блоки.
#[derive(Clone, Copy)]
pub struct Place<'a> {
    pub body: &'a TextBody,
    pub mode: Mode,
    pub geometry: DiffGeometry,
    pub char_width: f64,
    pub heights: &'a HeightIndex,
    pub layout: &'a ThreadLayout,
    pub threads: &'a Threads,
}

/// Блок в координатах документа: колонка текста по x и полоса по y.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BlockFrame {
    pub row: u32,
    pub entry: Entry,
    pub side: Side,
    pub x: f64,
    pub width: f64,
    pub y: f64,
    pub height: f64,
}

impl BlockFrame {
    pub fn contains_y(&self, y: f64) -> bool {
        (self.y..self.y + self.height).contains(&y)
    }
}

impl Place<'_> {
    /// Сторона якоря блока: в «Рядом» она выбирает половину.
    fn side(&self, item: BlockItem) -> Side {
        match item {
            BlockItem::Thread(id) => self.threads.get(id).map(|thread| thread.anchor.side),
            BlockItem::Draft => self.threads.draft().map(|draft| draft.anchor.side),
        }
        .unwrap_or(Side::New)
    }

    /// Колонка текста блока стороны `side`: начало по x и ширина.
    fn column(&self, side: Side) -> (f64, f64) {
        let gutter = f64::from(self.geometry.gutter_cells(self.mode)) * self.char_width;
        let origin = match self.mode {
            Mode::Unified => 0.0,
            Mode::SideBySide => self.geometry.half_origin(side),
        };
        (
            origin + gutter,
            self.geometry.text_width(self.mode).max(0.0),
        )
    }

    /// Блоки под строкой показа `row` по порядку сверху вниз.
    pub fn frames_at_row(&self, row: u32) -> Vec<BlockFrame> {
        let (Some(top), Some(text)) = (self.heights.top(row), self.heights.text_height(row)) else {
            return Vec::new();
        };
        let mut y = top as f64 + f64::from(text);
        self.layout
            .at_row(self.body, self.mode, row)
            .into_iter()
            .map(|entry| {
                let side = self.side(entry.item);
                let (x, width) = self.column(side);
                let frame = BlockFrame {
                    row,
                    entry,
                    side,
                    x,
                    width,
                    y,
                    height: f64::from(entry.height),
                };
                y += frame.height;
                frame
            })
            .collect()
    }

    /// Блок под координатой документа `y` строки `row`.
    pub fn frame_at(&self, row: u32, y: f64) -> Option<BlockFrame> {
        self.frames_at_row(row)
            .into_iter()
            .find(|frame| frame.contains_y(y))
    }

    /// Блок черновика, если черновик стоит в этом файле.
    pub fn draft_frame(&self) -> Option<BlockFrame> {
        let entry = self
            .layout
            .entries()
            .iter()
            .find(|entry| entry.item == BlockItem::Draft)?;
        let row = self.body.row_of_line(self.mode, entry.line)?;
        self.frames_at_row(row)
            .into_iter()
            .find(|frame| frame.entry.item == BlockItem::Draft)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use probe_diff_view::model::heights;
    use probe_diff_view::model::layout::{DiffLayout, Geometry, TextMetrics, Zone};
    use probe_diff_view::model::parse;
    use probe_diff_view::model::state::ThreadId;
    use probe_diff_view::model::threads::{Comment, Thread, ThreadAnchor};
    use probe_diff_view::model::wrap::{TAB_WIDTH, Wrap};
    use probe_diff_view::model::{Diff, LineAddr};
    use probe_diff_view::worker::Cancel;

    use super::*;

    const LINE: u32 = 10;
    const CELL: f64 = 7.0;

    const DIFF: &str = "diff --git a/f.rs b/f.rs\n\
        --- a/f.rs\n\
        +++ b/f.rs\n\
        @@ -1,3 +1,3 @@\n \
        a\n\
        -b\n\
        +c\n \
        d\n";

    fn diff() -> Diff {
        parse::parse(Arc::from(DIFF.as_bytes()), &Cancel::never()).unwrap()
    }

    fn geometry() -> DiffGeometry {
        let metrics = TextMetrics {
            char_width: CELL,
            digits: TextMetrics::digits_of(3),
            divider: 1.0,
        };
        match metrics.adapt(Geometry::new(Zone::Center, 1400.0, 700.0)) {
            DiffLayout::Diff(geometry) => geometry,
            DiffLayout::Placeholder => panic!("ниже min_size"),
        }
    }

    fn threads() -> Threads {
        let comment = |body: &str| Comment {
            author: "anna".to_owned(),
            body: body.to_owned(),
        };
        let mut threads = Threads::new(vec![
            Thread {
                id: ThreadId(0),
                anchor: ThreadAnchor::at("f.rs", LineAddr::old(2)),
                comments: vec![comment("старая")],
            },
            Thread {
                id: ThreadId(1),
                anchor: ThreadAnchor::at("f.rs", LineAddr::new(2)),
                comments: vec![comment("новая")],
            },
        ]);
        threads
            .open_draft(ThreadAnchor::at("f.rs", LineAddr::new(3)))
            .unwrap();
        threads
    }

    #[test]
    fn blocks_stack_under_text_in_their_column() {
        let diff = diff();
        let body = diff.file(0).unwrap().text().unwrap();
        let geometry = geometry();
        let threads = threads();
        for mode in [Mode::Unified, Mode::SideBySide] {
            let wrap = geometry.wrap(mode);
            let metrics = block_metrics(Metrics {
                line_height: LINE,
                wrap,
            });
            let layout = threads.layout(&diff, 0, &BTreeSet::from([ThreadId(1)]), metrics);
            let index = heights::build(
                &diff,
                body,
                mode,
                Metrics {
                    line_height: LINE,
                    wrap,
                },
                &layout.blocks(),
                &Cancel::never(),
            )
            .unwrap();
            let place = Place {
                body,
                mode,
                geometry,
                char_width: CELL,
                heights: &index,
                layout: &layout,
                threads: &threads,
            };
            let gutter = f64::from(geometry.gutter_cells(mode)) * CELL;
            let old_row = body.row_of_line(mode, 1).unwrap();
            let frames = place.frames_at_row(old_row);
            let top = index.top(old_row).unwrap() as f64 + f64::from(LINE);
            match mode {
                Mode::Unified => {
                    // `-b` и `+c` — разные строки показа: под каждой свой блок.
                    assert_eq!(frames.len(), 1);
                    assert_eq!((frames[0].x, frames[0].y), (gutter, top));
                }
                Mode::SideBySide => {
                    // Пара `-b`/`+c`: старый тред в левой колонке, под ним новый в правой.
                    assert_eq!(frames.len(), 2);
                    assert_eq!((frames[0].side, frames[0].x), (Side::Old, gutter));
                    assert_eq!(frames[0].y, top);
                    assert_eq!(frames[1].side, Side::New);
                    assert_eq!(frames[1].x, geometry.half_origin(Side::New) + gutter);
                    assert_eq!(frames[1].y, top + frames[0].height);
                    assert_eq!(
                        frames[1].height,
                        f64::from(metrics.thread_height(threads.get(ThreadId(1)).unwrap(), true))
                    );
                }
            }
            for frame in &frames {
                assert_eq!(frame.width, geometry.text_width(mode));
                assert_eq!(place.frame_at(old_row, frame.y), Some(*frame));
                assert_eq!(
                    place.frame_at(old_row, frame.y + frame.height - 0.5),
                    Some(*frame)
                );
            }
            assert_eq!(place.frame_at(old_row, top - 0.5), None);
            let draft = place.draft_frame().unwrap();
            assert_eq!(draft.entry.item, BlockItem::Draft);
            assert_eq!(draft.height, f64::from(LINE * DRAFT_LINES));
            assert_eq!(draft.row, body.row_of_line(mode, 3).unwrap());
            let wrap_cells = Wrap::from_width(geometry.text_width(mode), CELL, TAB_WIDTH);
            assert_eq!(metrics.wrap, wrap_cells);
        }
    }
}
