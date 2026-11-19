//! Треды и черновик комментария с якорем ADR-0015.
//!
//! Якорь — путь, сторона и номера первой и последней строки диапазона на этой стороне. Номера
//! ищутся в diff только точно: если строки нет, тред «устарел» и блока не получает, номер строки
//! молча не подбирается. Строки внутри диапазона, которых нет в diff (диапазон через границу
//! ханка), устаревания не дают: нужны только первая и последняя.
//!
//! Блок треда стоит после последней строки якоря, то есть под строкой показа, в которой эта
//! строка diff стоит в текущей раскладке. Поэтому одна раскладка блоков в строках diff годится
//! обоим режимам, а индекс высот получает её как список [`Block`].
//!
//! Черновик один на модель: вид держит одно поле черновика и ставит его над блоком черновика.

use std::collections::BTreeSet;
use std::ops::Range;

use super::heights::Block;
use super::rows::Mode;
use super::state::{Cursor, Selection, ThreadId};
use super::wrap::{self, Wrap};
use super::{Diff, LineAddr, Side, TextBody};

/// Якорь треда: строки `start_line..=line` стороны `side` файла `path`. Вторая сторона
/// диапазона ADR-0015 (`start_side`) в пробе совпадает с `side`: выделение модели — строки
/// одной стороны.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ThreadAnchor {
    pub path: String,
    pub side: Side,
    pub start_line: u32,
    pub line: u32,
}

impl ThreadAnchor {
    pub fn at(path: impl Into<String>, addr: LineAddr) -> Self {
        ThreadAnchor {
            path: path.into(),
            side: addr.side,
            start_line: addr.line,
            line: addr.line,
        }
    }

    /// Диапазон строк одной стороны, концы в любом порядке.
    pub fn range(path: impl Into<String>, side: Side, from: u32, to: u32) -> Self {
        ThreadAnchor {
            path: path.into(),
            side,
            start_line: from.min(to),
            line: from.max(to),
        }
    }

    /// Якорь черновика по виду: выделение, если оно есть, иначе строка курсора.
    pub fn from_view(
        path: impl Into<String>,
        cursor: Cursor,
        selection: Option<Selection>,
    ) -> Self {
        match selection {
            Some(selection) => {
                ThreadAnchor::range(path, selection.side, selection.start(), selection.end())
            }
            None => ThreadAnchor::at(path, cursor.addr),
        }
    }

    pub fn first(&self) -> LineAddr {
        LineAddr {
            side: self.side,
            line: self.start_line,
        }
    }

    pub fn last(&self) -> LineAddr {
        LineAddr {
            side: self.side,
            line: self.line,
        }
    }

    pub fn is_range(&self) -> bool {
        self.start_line != self.line
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Comment {
    pub author: String,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Thread {
    pub id: ThreadId,
    pub anchor: ThreadAnchor,
    pub comments: Vec<Comment>,
}

/// Почему тред устарел.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stale {
    /// Файла с путём якоря в diff нет.
    NoFile,
    /// Файл бинарный или без ханков: строк у него нет.
    NoText,
    /// Строки якоря нет в diff: первая, если нет её, иначе последняя.
    Missing(LineAddr),
}

/// Место треда в diff: файл и строки diff первой и последней строки якоря. Блок стоит после
/// строки `last`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    Placed { file: usize, first: u32, last: u32 },
    Stale(Stale),
}

/// Ищет якорь в diff только по точным адресам.
pub fn place(diff: &Diff, anchor: &ThreadAnchor) -> Placement {
    let Some((file, body)) = diff
        .files()
        .iter()
        .enumerate()
        .find(|(_, file)| file.path() == anchor.path)
    else {
        return Placement::Stale(Stale::NoFile);
    };
    let Some(body) = body.text() else {
        return Placement::Stale(Stale::NoText);
    };
    let Some(first) = body.line_of_addr(anchor.first()) else {
        return Placement::Stale(Stale::Missing(anchor.first()));
    };
    let Some(last) = body.line_of_addr(anchor.last()) else {
        return Placement::Stale(Stale::Missing(anchor.last()));
    };
    Placement::Placed { file, first, last }
}

/// Черновик: якорь и текст поля.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Draft {
    pub anchor: ThreadAnchor,
    text: String,
}

impl Draft {
    pub fn text(&self) -> &str {
        &self.text
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DraftError {
    /// Черновика нет.
    NoDraft,
    /// Открыт черновик с текстом: поле одно, текст не теряется, сначала сохранить или отменить.
    Busy,
    /// Текст пуст или из одних пробелов, сохранять нечего.
    Empty,
}

/// Высоты блоков в точках. `wrap` — ширина области блока в клетках, комментарии переносятся
/// по ней так же, как строки diff.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockMetrics {
    pub line_height: u32,
    pub wrap: Wrap,
    /// Строк свёрнутого треда: полоса с числом комментариев.
    pub collapsed_lines: u32,
    /// Строк поля черновика.
    pub draft_lines: u32,
}

impl BlockMetrics {
    /// Свёрнутый тред — `collapsed_lines`. Раскрытый — строка заголовка и у каждого комментария
    /// строка автора и визуальные строки текста.
    pub fn thread_height(&self, thread: &Thread, expanded: bool) -> u32 {
        let lines = if expanded {
            thread.comments.iter().fold(1u32, |sum, comment| {
                let body = comment
                    .body
                    .split('\n')
                    .map(|line| wrap::visual_lines(line.as_bytes(), self.wrap))
                    .fold(0u32, u32::saturating_add);
                sum.saturating_add(1).saturating_add(body)
            })
        } else {
            self.collapsed_lines
        };
        self.line_height.saturating_mul(lines)
    }

    pub fn draft_height(&self) -> u32 {
        self.line_height.saturating_mul(self.draft_lines)
    }

    /// Визуальные строки блока треда, ровно столько, сколько считает [`Self::thread_height`]:
    /// у свёрнутого — заголовок и пустые строки до `collapsed_lines`, у раскрытого — заголовок,
    /// у каждого комментария автор и визуальные строки текста. Заголовок и автор длиннее строки
    /// обрезаются её первой визуальной строкой.
    pub fn thread_lines(&self, thread: &Thread, expanded: bool) -> Vec<BlockLine> {
        let first = |role: LineRole, text: String| {
            let segment = wrap::segments(text.as_bytes(), self.wrap)
                .into_iter()
                .next()
                .unwrap_or(0..0);
            BlockLine {
                role,
                text,
                segment,
            }
        };
        let anchor = &thread.anchor;
        let side = match anchor.side {
            Side::Old => "старая",
            Side::New => "новая",
        };
        let lines = if anchor.is_range() {
            format!("{}–{}", anchor.start_line, anchor.line)
        } else {
            anchor.line.to_string()
        };
        let header = format!(
            "{} тред {}: {side} {lines}, комментариев {}",
            if expanded { "▾" } else { "▸" },
            thread.id.0,
            thread.comments.len()
        );
        let mut out = vec![first(LineRole::Header, header)];
        if !expanded {
            out.extend((1..self.collapsed_lines).map(|_| first(LineRole::Header, String::new())));
            return out;
        }
        for comment in &thread.comments {
            out.push(first(LineRole::Author, comment.author.clone()));
            for line in comment.body.split('\n') {
                out.extend(
                    wrap::segments(line.as_bytes(), self.wrap)
                        .into_iter()
                        .map(|segment| BlockLine {
                            role: LineRole::Body,
                            text: line.to_owned(),
                            segment,
                        }),
                );
            }
        }
        out
    }
}

/// Роль визуальной строки блока треда.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineRole {
    Header,
    Author,
    Body,
}

/// Визуальная строка блока треда: логическая строка и байты визуальной строки в ней.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockLine {
    pub role: LineRole,
    pub text: String,
    pub segment: Range<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BlockItem {
    Thread(ThreadId),
    Draft,
}

/// Блок после строки diff `line`. Блоки одной строки идут по порядку [`BlockItem`]: треды по
/// возрастанию id, черновик последним.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub line: u32,
    pub item: BlockItem,
    pub height: u32,
}

/// Блоки тредов и черновика одного файла и устаревшие треды этого файла.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ThreadLayout {
    entries: Vec<Entry>,
    stale: Vec<(ThreadId, Stale)>,
}

impl ThreadLayout {
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn stale(&self) -> &[(ThreadId, Stale)] {
        &self.stale
    }

    /// Блоки для индекса высот: блоки одной строки индекс складывает сам.
    pub fn blocks(&self) -> Vec<Block> {
        self.entries
            .iter()
            .map(|entry| Block {
                line: entry.line,
                height: entry.height,
            })
            .collect()
    }

    pub fn at_line(&self, line: u32) -> &[Entry] {
        let start = self.entries.partition_point(|entry| entry.line < line);
        let end = self.entries.partition_point(|entry| entry.line <= line);
        self.entries.get(start..end).unwrap_or_default()
    }

    /// Блоки под строкой показа: в «Рядом» у пары две строки diff, блоки идут по порядку строк
    /// diff, то есть как в unified.
    pub fn at_row(&self, body: &TextBody, mode: Mode, row: u32) -> Vec<Entry> {
        let Some(halves) = body.row(mode, row) else {
            return Vec::new();
        };
        let mut lines: Vec<u32> = [halves.old, halves.new].into_iter().flatten().collect();
        lines.sort_unstable();
        lines.dedup();
        lines
            .into_iter()
            .flat_map(|line| self.at_line(line).iter().copied())
            .collect()
    }
}

/// Треды diff и единственный черновик.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Threads {
    threads: Vec<Thread>,
    draft: Option<Draft>,
    next_id: u32,
}

impl Threads {
    pub fn new(mut threads: Vec<Thread>) -> Self {
        threads.sort_by_key(|thread| thread.id);
        let next_id = threads
            .last()
            .map_or(0, |thread| thread.id.0.saturating_add(1));
        Threads {
            threads,
            draft: None,
            next_id,
        }
    }

    pub fn threads(&self) -> &[Thread] {
        &self.threads
    }

    pub fn get(&self, id: ThreadId) -> Option<&Thread> {
        self.threads
            .binary_search_by_key(&id, |thread| thread.id)
            .ok()
            .and_then(|index| self.threads.get(index))
    }

    pub fn draft(&self) -> Option<&Draft> {
        self.draft.as_ref()
    }

    /// Открывает черновик на якоре. Пустой открытый черновик переносится на новый якорь,
    /// черновик с текстом остаётся, пока его не сохранят или не отменят.
    pub fn open_draft(&mut self, anchor: ThreadAnchor) -> Result<(), DraftError> {
        if self
            .draft
            .as_ref()
            .is_some_and(|draft| !draft.text.is_empty() && draft.anchor != anchor)
        {
            return Err(DraftError::Busy);
        }
        match &mut self.draft {
            Some(draft) => draft.anchor = anchor,
            None => {
                self.draft = Some(Draft {
                    anchor,
                    text: String::new(),
                })
            }
        }
        Ok(())
    }

    /// Текст поля черновика целиком: поле правит вид, модель получает его состояние.
    pub fn set_draft_text(&mut self, text: impl Into<String>) -> Result<(), DraftError> {
        let draft = self.draft.as_mut().ok_or(DraftError::NoDraft)?;
        draft.text = text.into();
        Ok(())
    }

    /// Сохраняет черновик новым тредом из одного комментария и закрывает поле.
    pub fn save_draft(&mut self, author: &str) -> Result<ThreadId, DraftError> {
        let draft = self.draft.as_ref().ok_or(DraftError::NoDraft)?;
        if draft.text.trim().is_empty() {
            return Err(DraftError::Empty);
        }
        let draft = self.draft.take().ok_or(DraftError::NoDraft)?;
        let id = ThreadId(self.next_id);
        self.next_id = self.next_id.saturating_add(1);
        self.threads.push(Thread {
            id,
            anchor: draft.anchor,
            comments: vec![Comment {
                author: author.to_owned(),
                body: draft.text,
            }],
        });
        Ok(id)
    }

    /// Закрывает поле без сохранения.
    pub fn cancel_draft(&mut self) -> Option<Draft> {
        self.draft.take()
    }

    /// Блоки файла `file`: треды его пути и черновик, если он на этом файле. Тред из
    /// `expanded` раскрыт.
    pub fn layout(
        &self,
        diff: &Diff,
        file: usize,
        expanded: &BTreeSet<ThreadId>,
        metrics: BlockMetrics,
    ) -> ThreadLayout {
        let mut layout = ThreadLayout::default();
        let Some(path) = diff.file(file).map(|file| file.path()) else {
            return layout;
        };
        let placed = |anchor: &ThreadAnchor| match place(diff, anchor) {
            Placement::Placed {
                file: found, last, ..
            } if found == file => Ok(last),
            Placement::Placed { .. } => Err(None),
            Placement::Stale(stale) => Err(Some(stale)),
        };
        for thread in self
            .threads
            .iter()
            .filter(|thread| thread.anchor.path == path)
        {
            match placed(&thread.anchor) {
                Ok(line) => layout.entries.push(Entry {
                    line,
                    item: BlockItem::Thread(thread.id),
                    height: metrics.thread_height(thread, expanded.contains(&thread.id)),
                }),
                Err(Some(stale)) => layout.stale.push((thread.id, stale)),
                Err(None) => {}
            }
        }
        if let Some(draft) = self
            .draft
            .as_ref()
            .filter(|draft| draft.anchor.path == path)
            && let Ok(line) = placed(&draft.anchor)
        {
            layout.entries.push(Entry {
                line,
                item: BlockItem::Draft,
                height: metrics.draft_height(),
            });
        }
        layout.entries.sort_by_key(|entry| (entry.line, entry.item));
        layout
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::model::heights::{self, HeightIndex, Metrics};
    use crate::model::parse::parse;
    use crate::model::state::{Command, ViewState, Viewport};
    use crate::model::wrap::TAB_WIDTH;
    use crate::worker::Cancel;

    const LINE: u32 = 10;

    fn parse_text(text: &str) -> Diff {
        match parse(Arc::from(text.as_bytes()), &Cancel::never()) {
            Ok(diff) => diff,
            Err(failure) => panic!("разбор не прошёл: {failure:?}"),
        }
    }

    /// Два ханка файла `f.rs` и бинарный файл. Ханк 1: старые 1..=5, новые 1..=4, удаление
    /// старых 2..=3 и вставка новой 2. Ханк 2: старые 20..=23, новые 19..=23, удаление старой 21
    /// и вставка новых 20..=21.
    fn two_hunks() -> Diff {
        parse_text(concat!(
            "diff --git a/f.rs b/f.rs\n",
            "--- a/f.rs\n",
            "+++ b/f.rs\n",
            "@@ -1,5 +1,4 @@\n",
            " a\n",
            "-b\n",
            "-c\n",
            "+B\n",
            " d\n",
            " e\n",
            "@@ -20,4 +19,5 @@ fn f()\n",
            " t\n",
            "-u\n",
            "+U1\n",
            "+U2\n",
            " v\n",
            " w\n",
            "diff --git a/img.png b/img.png\n",
            "index 1111111..2222222 100644\n",
            "Binary files a/img.png and b/img.png differ\n",
        ))
    }

    fn body(diff: &Diff) -> &TextBody {
        diff.files()[0].text().unwrap()
    }

    fn metrics() -> BlockMetrics {
        BlockMetrics {
            line_height: LINE,
            wrap: Wrap::new(20, TAB_WIDTH),
            collapsed_lines: 1,
            draft_lines: 4,
        }
    }

    fn thread(id: u32, anchor: ThreadAnchor, bodies: &[&str]) -> Thread {
        Thread {
            id: ThreadId(id),
            anchor,
            comments: bodies
                .iter()
                .map(|body| Comment {
                    author: "anna".to_owned(),
                    body: (*body).to_owned(),
                })
                .collect(),
        }
    }

    #[test]
    fn anchors_resolve_only_exact_lines() {
        let diff = two_hunks();
        let single = ThreadAnchor::at("f.rs", LineAddr::old(3));
        assert_eq!(
            place(&diff, &single),
            Placement::Placed {
                file: 0,
                first: 2,
                last: 2
            }
        );
        // Контекстная строка доступна с обеих сторон.
        assert_eq!(
            place(&diff, &ThreadAnchor::at("f.rs", LineAddr::new(3))),
            Placement::Placed {
                file: 0,
                first: 4,
                last: 4
            }
        );
        // Старой строки 10 нет: ближайшая не подбирается.
        assert_eq!(
            place(&diff, &ThreadAnchor::at("f.rs", LineAddr::old(10))),
            Placement::Stale(Stale::Missing(LineAddr::old(10)))
        );
        // Новой строки 5 нет, хотя старая 5 есть.
        assert_eq!(
            place(&diff, &ThreadAnchor::range("f.rs", Side::New, 3, 5)),
            Placement::Stale(Stale::Missing(LineAddr::new(5)))
        );
        assert_eq!(
            place(&diff, &ThreadAnchor::range("f.rs", Side::Old, 7, 21)),
            Placement::Stale(Stale::Missing(LineAddr::old(7)))
        );
        assert_eq!(
            place(&diff, &ThreadAnchor::at("g.rs", LineAddr::new(1))),
            Placement::Stale(Stale::NoFile)
        );
        assert_eq!(
            place(&diff, &ThreadAnchor::at("img.png", LineAddr::new(1))),
            Placement::Stale(Stale::NoText)
        );
    }

    #[test]
    fn range_across_hunk_boundary_is_placed_after_its_last_line() {
        let diff = two_hunks();
        let body = body(&diff);
        // Старые 4..=21: середины 6..=19 в diff нет, концы есть в разных ханках.
        let anchor = ThreadAnchor::range("f.rs", Side::Old, 21, 4);
        assert_eq!((anchor.start_line, anchor.line), (4, 21));
        assert!(anchor.is_range());
        assert_eq!(
            place(&diff, &anchor),
            Placement::Placed {
                file: 0,
                first: 4,
                last: 7
            }
        );
        assert_ne!(body.hunk_of_line(4), body.hunk_of_line(7));
        let threads = Threads::new(vec![thread(0, anchor, &["через ханк"])]);
        let layout = threads.layout(&diff, 0, &BTreeSet::new(), metrics());
        assert_eq!(
            layout.entries(),
            [Entry {
                line: 7,
                item: BlockItem::Thread(ThreadId(0)),
                height: LINE
            }]
        );
        // Блок под строкой показа `-u`: в unified это строка 7, в «Рядом» пара `-u`/`+U1`.
        assert_eq!(body.row_of_line(Mode::Unified, 7), Some(7));
        let pair = body.row_of_line(Mode::SideBySide, 7).unwrap();
        assert_eq!(body.row_of_line(Mode::SideBySide, 8), Some(pair));
        assert_eq!(
            layout.at_row(body, Mode::SideBySide, pair),
            layout.entries()
        );
        assert_eq!(layout.at_row(body, Mode::Unified, 10), []);
    }

    #[test]
    fn blocks_follow_last_line_in_both_modes() {
        let diff = two_hunks();
        let body = body(&diff);
        let threads = Threads::new(vec![
            thread(3, ThreadAnchor::at("f.rs", LineAddr::new(2)), &["новая"]),
            thread(1, ThreadAnchor::range("f.rs", Side::Old, 1, 2), &["старая"]),
            thread(2, ThreadAnchor::at("f.rs", LineAddr::old(99)), &["устарел"]),
            thread(
                4,
                ThreadAnchor::at("img.png", LineAddr::new(1)),
                &["другой файл"],
            ),
        ]);
        let layout = threads.layout(&diff, 0, &BTreeSet::new(), metrics());
        let lines: Vec<(u32, BlockItem)> = layout
            .entries()
            .iter()
            .map(|entry| (entry.line, entry.item))
            .collect();
        assert_eq!(
            lines,
            [
                (1, BlockItem::Thread(ThreadId(1))),
                (3, BlockItem::Thread(ThreadId(3))),
            ]
        );
        assert_eq!(
            layout.stale(),
            [(ThreadId(2), Stale::Missing(LineAddr::old(99)))]
        );
        // `-b` (строка diff 1) и `+B` (строка diff 3) — одна пара «Рядом»: оба блока под ней,
        // по порядку строк diff. В unified они под разными строками.
        let pair = body.row_of_line(Mode::SideBySide, 1).unwrap();
        assert_eq!(body.row_of_line(Mode::SideBySide, 3), Some(pair));
        assert_eq!(
            layout.at_row(body, Mode::SideBySide, pair),
            layout.entries()
        );
        assert_eq!(layout.at_row(body, Mode::Unified, 1).len(), 1);
        assert_eq!(layout.at_row(body, Mode::Unified, 3).len(), 1);

        for mode in [Mode::Unified, Mode::SideBySide] {
            let index = heights::build(
                &diff,
                body,
                mode,
                Metrics {
                    line_height: LINE,
                    wrap: Wrap::new(80, TAB_WIDTH),
                },
                &layout.blocks(),
                &Cancel::never(),
            )
            .unwrap();
            for entry in layout.entries() {
                let row = body.row_of_line(mode, entry.line).unwrap();
                let sum: u32 = layout
                    .at_row(body, mode, row)
                    .iter()
                    .map(|entry| entry.height)
                    .sum();
                assert_eq!(index.block_height(row), Some(sum), "{mode:?}");
            }
        }
        // Бинарный файл: тред его пути устарел.
        let binary = threads.layout(&diff, 1, &BTreeSet::new(), metrics());
        assert_eq!(binary.entries(), []);
        assert_eq!(binary.stale(), [(ThreadId(4), Stale::NoText)]);
        assert_eq!(
            threads.layout(&diff, 7, &BTreeSet::new(), metrics()),
            ThreadLayout::default()
        );
    }

    #[test]
    fn expanded_height_counts_header_authors_and_wrapped_bodies() {
        let long = "x".repeat(45);
        let item = thread(
            0,
            ThreadAnchor::at("f.rs", LineAddr::new(1)),
            &["одна строка", &format!("две\n{long}")],
        );
        let metrics = metrics();
        assert_eq!(metrics.thread_height(&item, false), LINE);
        // Заголовок 1, первый комментарий 1 + 1, второй 1 + 1 + 3 (45 символов по 20).
        assert_eq!(metrics.thread_height(&item, true), 8 * LINE);
        assert_eq!(metrics.draft_height(), 4 * LINE);

        for expanded in [false, true] {
            let lines = metrics.thread_lines(&item, expanded);
            assert_eq!(
                lines.len() as u32 * LINE,
                metrics.thread_height(&item, expanded),
                "{expanded}"
            );
        }
        let lines = metrics.thread_lines(&item, true);
        let roles: Vec<LineRole> = lines.iter().map(|line| line.role).collect();
        assert_eq!(
            roles,
            [
                LineRole::Header,
                LineRole::Author,
                LineRole::Body,
                LineRole::Author,
                LineRole::Body,
                LineRole::Body,
                LineRole::Body,
                LineRole::Body,
            ]
        );
        // Заголовок длиннее 20 клеток обрезан первой визуальной строкой.
        assert!(lines[0].text.starts_with("▾ тред 0: новая 1"));
        assert_eq!(
            wrap::visual_lines(
                &lines[0].text.as_bytes()[lines[0].segment.clone()],
                metrics.wrap
            ),
            1
        );
        let tail: Vec<&str> = lines[5..]
            .iter()
            .map(|line| &line.text[line.segment.clone()])
            .collect();
        assert_eq!(tail, [&long[..20], &long[20..40], &long[40..]]);
        // Пустая строка и пустой текст — по визуальной строке, как в высоте блока.
        let blank = thread(
            1,
            ThreadAnchor::at("f.rs", LineAddr::new(1)),
            &["a\n\nb", ""],
        );
        assert_eq!(
            metrics.thread_lines(&blank, true).len() as u32 * LINE,
            metrics.thread_height(&blank, true)
        );
        let collapsed = BlockMetrics {
            collapsed_lines: 2,
            ..metrics
        };
        assert_eq!(collapsed.thread_lines(&item, false).len(), 2);

        let diff = two_hunks();
        let threads = Threads::new(vec![item]);
        let expanded = BTreeSet::from([ThreadId(0)]);
        let layout = threads.layout(&diff, 0, &expanded, metrics);
        assert_eq!(layout.entries()[0].height, 8 * LINE);
    }

    #[test]
    fn draft_opens_on_line_or_selection_and_saves_into_model() {
        let diff = two_hunks();
        let body = body(&diff);
        let index = heights::build(
            &diff,
            body,
            Mode::Unified,
            Metrics {
                line_height: LINE,
                wrap: Wrap::new(80, TAB_WIDTH),
            },
            &[],
            &Cancel::never(),
        )
        .unwrap();
        let view = Viewport {
            body,
            heights: &index,
            height: 100,
        };
        let state = ViewState::open(&diff, 0, Mode::Unified);
        let path = diff.files()[0].path();
        let line = ThreadAnchor::from_view(path, state.cursor().unwrap(), state.selection());
        assert_eq!(line, ThreadAnchor::at("f.rs", LineAddr::new(1)));
        let state = [
            Command::Down,
            Command::ToggleVisual,
            Command::Down,
            Command::Down,
        ]
        .into_iter()
        .fold(state, |state, command| state.apply(command, &view));
        let range = ThreadAnchor::from_view(path, state.cursor().unwrap(), state.selection());
        assert_eq!(range, ThreadAnchor::range("f.rs", Side::Old, 2, 3));

        let mut threads = Threads::new(vec![thread(
            5,
            ThreadAnchor::at("f.rs", LineAddr::new(1)),
            &["есть"],
        )]);
        assert_eq!(threads.set_draft_text("x"), Err(DraftError::NoDraft));
        assert_eq!(threads.save_draft("anna"), Err(DraftError::NoDraft));
        assert_eq!(threads.open_draft(line.clone()), Ok(()));
        // Пустой черновик переносится на диапазон.
        assert_eq!(threads.open_draft(range.clone()), Ok(()));
        assert_eq!(threads.draft().unwrap().anchor, range);
        assert_eq!(threads.save_draft("anna"), Err(DraftError::Empty));
        assert_eq!(threads.set_draft_text("  \n "), Ok(()));
        assert_eq!(threads.save_draft("anna"), Err(DraftError::Empty));
        assert_eq!(threads.set_draft_text("Проверить границу"), Ok(()));
        // Черновик с текстом не уступает поле другому якорю, тот же якорь его не трогает.
        assert_eq!(threads.open_draft(line.clone()), Err(DraftError::Busy));
        assert_eq!(threads.open_draft(range.clone()), Ok(()));
        assert_eq!(threads.draft().unwrap().text(), "Проверить границу");

        // Блок черновика стоит после последней строки диапазона, после тредов той же строки.
        let layout = threads.layout(&diff, 0, &BTreeSet::new(), metrics());
        assert_eq!(
            layout.at_line(2),
            [Entry {
                line: 2,
                item: BlockItem::Draft,
                height: 4 * LINE
            }]
        );

        let id = threads.save_draft("boris").unwrap();
        assert_eq!(id, ThreadId(6));
        assert_eq!(threads.draft(), None);
        let saved = threads.get(id).unwrap();
        assert_eq!(saved.anchor, range);
        assert_eq!(
            saved.comments,
            [Comment {
                author: "boris".to_owned(),
                body: "Проверить границу".to_owned()
            }]
        );
        let layout = threads.layout(&diff, 0, &BTreeSet::from([id]), metrics());
        assert_eq!(layout.at_line(2)[0].item, BlockItem::Thread(id));
        assert_eq!(layout.at_line(2)[0].height, 3 * LINE);
    }

    #[test]
    fn cancel_closes_draft_without_thread() {
        let diff = two_hunks();
        let mut threads = Threads::default();
        let anchor = ThreadAnchor::at("f.rs", LineAddr::new(20));
        threads.open_draft(anchor.clone()).unwrap();
        threads.set_draft_text("лишнее").unwrap();
        let cancelled = threads.cancel_draft().unwrap();
        assert_eq!((&cancelled.anchor, cancelled.text()), (&anchor, "лишнее"));
        assert_eq!(threads.cancel_draft(), None);
        assert!(threads.threads().is_empty());
        assert_eq!(
            threads.layout(&diff, 0, &BTreeSet::new(), metrics()),
            ThreadLayout::default()
        );
        // Сто циклов открытия и отмены не создают тредов, черновик всегда один.
        for k in 0..100 {
            threads
                .open_draft(ThreadAnchor::at("f.rs", LineAddr::new(1 + k % 4)))
                .unwrap();
            threads.set_draft_text(format!("цикл {k}")).unwrap();
            assert!(threads.cancel_draft().is_some());
        }
        assert!(threads.threads().is_empty());
        let first = threads.save_draft("anna");
        assert_eq!(first, Err(DraftError::NoDraft));
        threads
            .open_draft(ThreadAnchor::at("f.rs", LineAddr::new(1)))
            .unwrap();
        threads.set_draft_text("первый").unwrap();
        assert_eq!(threads.save_draft("anna"), Ok(ThreadId(0)));
    }

    #[test]
    fn expanding_thread_above_anchor_keeps_anchor() {
        let diff = two_hunks();
        let body = body(&diff);
        let threads = Threads::new(vec![thread(
            0,
            ThreadAnchor::at("f.rs", LineAddr::old(1)),
            &["раз", "два", "три"],
        )]);
        let height_metrics = Metrics {
            line_height: LINE,
            wrap: Wrap::new(80, TAB_WIDTH),
        };
        let build = |expanded: &BTreeSet<ThreadId>| -> HeightIndex {
            let layout = threads.layout(&diff, 0, expanded, metrics());
            heights::build(
                &diff,
                body,
                Mode::Unified,
                height_metrics,
                &layout.blocks(),
                &Cancel::never(),
            )
            .unwrap()
        };
        let collapsed = build(&BTreeSet::new());
        let state = ViewState::open(&diff, 0, Mode::Unified).apply(
            Command::ScrollTo(collapsed.top(6).unwrap() + 2),
            &Viewport {
                body,
                heights: &collapsed,
                height: 30,
            },
        );
        let state = state.toggle_thread(ThreadId(0));
        let expanded = build(state.expanded());
        let shown = state
            .shown(&Viewport {
                body,
                heights: &expanded,
                height: 30,
            })
            .unwrap();
        assert_eq!((shown.row, shown.offset), (6, 2));
        assert_eq!(shown.y, expanded.top(6).unwrap() + 2);
        assert_eq!(
            expanded.top(6).unwrap() - collapsed.top(6).unwrap(),
            6 * LINE as u64
        );
    }
}
