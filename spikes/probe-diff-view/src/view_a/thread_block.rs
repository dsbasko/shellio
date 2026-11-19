//! Блоки тредов и черновика варианта A. Геометрия блоков общая (`crate::blocks`), строки треда
//! даёт модель (`BlockMetrics::thread_lines`), вид только рисует: фон блока во всю ширину строки,
//! колонку текста цветом блока и строки треда Core Text. Блок черновика — фон под полем
//! черновика. Строки блоков одной строки показа — запись своего ограниченного кэша, версия
//! записи включает эпоху вида и состав блоков, поэтому раскрытие треда перестраивает только её.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::ops::Range;

use objc2::DefinedClass;
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::CGContext;
use objc2_core_text::CTLine;
use objc2_foundation::{NSPoint, NSRect, NSSize};

use probe_diff_view::model::threads::{BlockItem, LineRole};

use super::layout_cache::Half;
use super::{DocumentView, fill};
use crate::blocks::BlockFrame;
use crate::scene::Model;
use crate::text::Expanded;

/// Визуальные строки блоков одной строки показа, по блоку на запись.
pub struct BlockLayout {
    blocks: Vec<Vec<CFRetained<CTLine>>>,
}

/// Версия записи: эпоха вида и состав блоков строки.
fn version(epoch: u64, frames: &[BlockFrame], expanded: impl Fn(BlockItem) -> bool) -> u64 {
    let mut hasher = DefaultHasher::new();
    epoch.hash(&mut hasher);
    for frame in frames {
        frame.entry.item.hash(&mut hasher);
        frame.entry.height.hash(&mut hasher);
        expanded(frame.entry.item).hash(&mut hasher);
    }
    hasher.finish()
}

impl DocumentView {
    /// Блоки под строкой показа `row`: фон во всю ширину `width`, колонка текста и строки треда.
    pub(super) fn paint_blocks(
        &self,
        context: &CGContext,
        model: &Model,
        row: u32,
        seen: &Range<u32>,
        width: f64,
    ) {
        let Some(place) = model.place() else {
            return;
        };
        let frames = place.frames_at_row(row);
        if frames.is_empty() {
            return;
        }
        let theme = self.theme();
        let font = &self.ivars().font;
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
        let Some(metrics) = model.metrics.map(crate::blocks::block_metrics) else {
            return;
        };
        let version = version(self.ivars().epoch.get(), &frames, expanded);
        let mut cache = self.ivars().blocks.borrow_mut();
        let mut colors = self.ivars().colors.borrow_mut();
        let layout = cache.get_or_build(row, Half::Unified, version, seen, || BlockLayout {
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
                            let expanded = Expanded::new(
                                line.text.as_bytes(),
                                line.segment.clone(),
                                metrics.wrap,
                            );
                            font.line(
                                &expanded.text,
                                &[(0..expanded.text.len(), color)],
                                &mut colors,
                            )
                        })
                        .collect(),
                    BlockItem::Draft => Vec::new(),
                })
                .collect(),
        });
        let line_height = f64::from(metrics.line_height);
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
    }
}
