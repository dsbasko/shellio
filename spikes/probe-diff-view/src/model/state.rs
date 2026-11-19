//! Состояние вида diff: активный файл, режим, якорь прокрутки, курсор, выделение и раскрытые
//! треды. Переходы — чистые функции: состояние, команда и раскладка на входе, новое состояние
//! на выходе. Состояние хранит адреса строк, а не строки показа и не координаты, поэтому смена
//! ширины, режима и раскрытие тредов его не пересчитывают.
//!
//! Прокрутка задаётся якорем: адрес строки под верхним краем и смещение в точках от верха её
//! строки показа. Координата выводится из якоря по текущему индексу высот, а якорь выводится
//! заново из координаты только прокруткой и командами. Если строка якоря стала ниже смещения,
//! показывается её последняя точка, а если якорь дальше предела прокрутки — предел. Сам якорь
//! при этом не меняется, и обратная смена ширины возвращает прежнее положение.

use std::collections::BTreeSet;

use super::heights::HeightIndex;
use super::rows::Mode;
use super::{Diff, FileDiff, LineAddr, LineKind, Side, TextBody};

/// Предел списка положений для возврата `Ctrl-o`.
const JUMPS: usize = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ThreadId(pub u32);

/// Якорь прокрутки: строка `addr` под верхним краем, верхний край на `offset` точек ниже верха
/// её строки показа. Смещение может попасть в блок тредов под строкой.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Anchor {
    pub addr: LineAddr,
    pub offset: u64,
}

/// Курсор: адрес строки и предпочтённая сторона. Сторона выбирает половину строки показа и
/// адрес контекстной строки. Если половины этой стороны у строки нет, курсор стоит на другой,
/// а предпочтение сохраняется для следующего движения.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cursor {
    pub addr: LineAddr,
    pub side: Side,
}

/// Выделение — номера строк одной стороны от `tail`, где оно началось, до `head` у курсора.
/// Сторона задаётся при начале выделения. Строка показа без половины этой стороны конец не
/// двигает: конец встаёт на ближайшую к курсору строку этой стороны между курсором и началом.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub side: Side,
    pub tail: u32,
    pub head: u32,
}

impl Selection {
    pub fn start(&self) -> u32 {
        self.tail.min(self.head)
    }

    pub fn end(&self) -> u32 {
        self.tail.max(self.head)
    }

    pub fn contains(&self, addr: LineAddr) -> bool {
        addr.side == self.side && (self.start()..=self.end()).contains(&addr.line)
    }
}

/// Якорь и курсор вместе: то, что возвращает `Ctrl-o`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Position {
    pub anchor: Anchor,
    pub cursor: Cursor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// `j`: курсор на строку показа ниже.
    Down,
    /// `k`: курсор на строку показа выше.
    Up,
    /// `gg`: курсор на первую строку, верх документа.
    Top,
    /// `G`: курсор на последнюю строку, предел прокрутки.
    Bottom,
    /// `Ctrl-d`: прокрутка и курсор на половину видимой области вниз.
    HalfDown,
    /// `Ctrl-u`: прокрутка и курсор на половину видимой области вверх.
    HalfUp,
    /// `]c`: курсор и верхний край на начало следующего изменения.
    NextChange,
    /// `[c`: курсор и верхний край на начало предыдущего изменения.
    PrevChange,
    /// `v`: начать выделение от курсора или снять его.
    ToggleVisual,
    /// `Escape`: снять выделение.
    ClearSelection,
    /// `Ctrl-o`: положение до последнего перехода `gg`, `G`, `]c` или `[c`.
    JumpBack,
    /// Колесо или полоса прокрутки: верхний край на координате.
    ScrollTo(u64),
    /// Щелчок по половине строки показа: курсор, сторона, снятое выделение.
    Click { row: u32, side: Side },
    /// `Shift`-щелчок или протяжка: выделение от курсора до строки показа.
    ExtendTo { row: u32 },
}

/// Раскладка для команды: текст активного файла, индекс высот режима состояния и высота
/// видимой области в точках.
#[derive(Clone, Copy, Debug)]
pub struct Viewport<'a> {
    pub body: &'a TextBody,
    pub heights: &'a HeightIndex,
    pub height: u64,
}

/// Положение на экране, выведенное из якоря: координата верхнего края, строка показа под ним
/// и смещение от верха этой строки.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shown {
    pub y: u64,
    pub row: u32,
    pub offset: u64,
}

impl Viewport<'_> {
    fn rows(&self) -> u32 {
        self.heights.len()
    }

    fn max_y(&self) -> u64 {
        self.heights.total().saturating_sub(self.height)
    }

    fn clamp(&self, y: u64) -> u64 {
        y.min(self.max_y())
    }

    fn row_of(&self, mode: Mode, addr: LineAddr) -> Option<u32> {
        Some(self.body.row_of_addr(mode, addr)?.index)
    }

    fn shown(&self, mode: Mode, anchor: Anchor) -> Option<Shown> {
        let row = self.row_of(mode, anchor.addr)?;
        let top = self.heights.top(row)?;
        let span = self.heights.span_height(row)?;
        let y = self.clamp(top + anchor.offset.min(span.saturating_sub(1)));
        let hit = self.heights.row_at(y)?;
        Some(Shown {
            y,
            row: hit.row,
            offset: hit.offset,
        })
    }

    fn anchor_at(&self, mode: Mode, y: u64) -> Option<Anchor> {
        let hit = self.heights.row_at(self.clamp(y))?;
        Some(Anchor {
            addr: row_anchor_addr(self.body, mode, hit.row)?,
            offset: hit.offset,
        })
    }

    /// Наименьшая прокрутка от `y`, при которой текст строки `row` виден. Строка выше
    /// видимой области встаёт к верхнему краю.
    fn reveal(&self, y: u64, row: u32) -> u64 {
        let (Some(top), Some(text)) = (self.heights.top(row), self.heights.text_height(row)) else {
            return self.clamp(y);
        };
        let bottom = top + u64::from(text);
        let y = if top < y {
            top
        } else if bottom > y + self.height {
            if u64::from(text) >= self.height {
                top
            } else {
                bottom - self.height
            }
        } else {
            y
        };
        self.clamp(y)
    }

    /// Строка показа под координатой, за концом документа — последняя.
    fn row_near(&self, y: u64) -> Option<u32> {
        let last = self.heights.total().checked_sub(1)?;
        Some(self.heights.row_at(y.min(last))?.row)
    }

    fn is_change(&self, mode: Mode, row: u32) -> bool {
        self.body.row(mode, row).is_some_and(|halves| {
            [halves.old, halves.new]
                .into_iter()
                .flatten()
                .filter_map(|line| self.body.line(line))
                .any(|line| line.kind() != LineKind::Context)
        })
    }

    /// Строка показа открывает изменение: она — изменение, а строка выше — нет или из другого
    /// ханка.
    fn starts_change(&self, mode: Mode, row: u32) -> bool {
        self.is_change(mode, row)
            && (row == 0 || !self.is_change(mode, row - 1) || self.body.starts_hunk(mode, row))
    }
}

/// Адрес якоря строки показа: строка diff, которая в unified стоит первой, у контекстной —
/// новая сторона. Так «Рядом» → unified ставит наверх первую строку пары.
fn row_anchor_addr(body: &TextBody, mode: Mode, row: u32) -> Option<LineAddr> {
    let line = body.line(body.row(mode, row)?.first()?)?;
    line.addr(Side::New).or(line.addr(Side::Old))
}

/// Курсор на строке показа с предпочтённой стороной `side`.
fn cursor_at(body: &TextBody, mode: Mode, row: u32, side: Side) -> Option<Cursor> {
    let halves = body.row(mode, row)?;
    let (line, actual) = match halves.half(side) {
        Some(line) => (line, side),
        None => (halves.half(side.other())?, side.other()),
    };
    let line = body.line(line)?;
    let addr = line.addr(actual).or(line.addr(actual.other()))?;
    Some(Cursor { addr, side })
}

/// Номер строки стороны `side` в строке показа, если эта половина есть.
fn side_line(body: &TextBody, mode: Mode, row: u32, side: Side) -> Option<u32> {
    body.line(body.row(mode, row)?.half(side)?)?.line(side)
}

fn start(body: &TextBody, mode: Mode) -> Option<Position> {
    Some(Position {
        anchor: Anchor {
            addr: row_anchor_addr(body, mode, 0)?,
            offset: 0,
        },
        cursor: cursor_at(body, mode, 0, Side::New)?,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewState {
    file: usize,
    mode: Mode,
    position: Option<Position>,
    selection: Option<Selection>,
    expanded: BTreeSet<ThreadId>,
    jumps: Vec<Position>,
}

impl ViewState {
    /// Вид файла `file` в начале документа. У файла без строк (бинарный, без ханков) нет ни
    /// якоря, ни курсора, команды его не меняют.
    pub fn open(diff: &Diff, file: usize, mode: Mode) -> Self {
        ViewState {
            file,
            mode,
            position: diff
                .file(file)
                .and_then(FileDiff::text)
                .and_then(|body| start(body, mode)),
            selection: None,
            expanded: BTreeSet::new(),
            jumps: Vec::new(),
        }
    }

    pub fn file(&self) -> usize {
        self.file
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn anchor(&self) -> Option<Anchor> {
        self.position.map(|position| position.anchor)
    }

    pub fn cursor(&self) -> Option<Cursor> {
        self.position.map(|position| position.cursor)
    }

    pub fn selection(&self) -> Option<Selection> {
        self.selection
    }

    pub fn expanded(&self) -> &BTreeSet<ThreadId> {
        &self.expanded
    }

    pub fn is_expanded(&self, thread: ThreadId) -> bool {
        self.expanded.contains(&thread)
    }

    /// Положение на экране в раскладке `view`.
    pub fn shown(&self, view: &Viewport<'_>) -> Option<Shown> {
        view.shown(self.mode, self.position?.anchor)
    }

    pub fn cursor_row(&self, view: &Viewport<'_>) -> Option<u32> {
        view.row_of(self.mode, self.position?.cursor.addr)
    }

    /// Следующий файл diff, без перехода по кругу. Вид нового файла начинается сверху,
    /// выделение и возвраты сбрасываются: адреса живут внутри файла. Раскрытые треды
    /// остаются, их якорь содержит путь.
    pub fn next_file(self, diff: &Diff) -> Self {
        match diff.next_file(self.file) {
            Some(file) => self.switch_file(diff, file),
            None => self,
        }
    }

    pub fn prev_file(self, diff: &Diff) -> Self {
        match diff.prev_file(self.file) {
            Some(file) => self.switch_file(diff, file),
            None => self,
        }
    }

    fn switch_file(self, diff: &Diff, file: usize) -> Self {
        ViewState {
            expanded: self.expanded,
            ..ViewState::open(diff, file, self.mode)
        }
    }

    /// Смена режима: якорь, курсор и выделение остаются адресами. Вызывающий строит индекс
    /// высот нового режима, и якорь встаёт на строку показа своего адреса в нём.
    pub fn with_mode(self, mode: Mode) -> Self {
        ViewState { mode, ..self }
    }

    /// Открытие на последнем ханке: курсор на первой строке показа последнего ханка, верхний
    /// край на её начале, прижатом к пределу прокрутки. Переходом не считается, `Ctrl-o` его не
    /// возвращает.
    pub fn at_last_hunk(self, view: &Viewport<'_>) -> Self {
        let Some(position) = self.position else {
            return self;
        };
        let start = (0..view.rows())
            .rev()
            .find(|&row| view.body.starts_hunk(self.mode, row));
        match start.and_then(|row| Some((row, view.heights.top(row)?))) {
            Some((row, top)) => self.go(view, top, row, position.cursor.side, false),
            None => self,
        }
    }

    /// Раскрыть или свернуть тред. Высоту его блока в индексе правит вызывающий, якорь от этого
    /// не меняется.
    pub fn toggle_thread(mut self, thread: ThreadId) -> Self {
        if !self.expanded.remove(&thread) {
            self.expanded.insert(thread);
        }
        self
    }

    pub fn apply(mut self, command: Command, view: &Viewport<'_>) -> Self {
        let (Some(position), Some(shown)) = (self.position, self.shown(view)) else {
            return self;
        };
        let rows = view.rows();
        let Some(last) = rows.checked_sub(1) else {
            return self;
        };
        let cursor = position.cursor;
        let cursor_row = view.row_of(self.mode, cursor.addr).unwrap_or(shown.row);
        let half = view.height / 2;
        match command {
            Command::Down => self.go(
                view,
                shown.y,
                (cursor_row + 1).min(last),
                cursor.side,
                false,
            ),
            Command::Up => self.go(
                view,
                shown.y,
                cursor_row.saturating_sub(1),
                cursor.side,
                false,
            ),
            Command::Top => self.go(view, 0, 0, cursor.side, true),
            Command::Bottom => self.go(view, view.max_y(), last, cursor.side, true),
            Command::HalfDown | Command::HalfUp => {
                let cursor_top = view.heights.top(cursor_row).unwrap_or(shown.y);
                let (y, target) = if command == Command::HalfDown {
                    (
                        shown.y.saturating_add(half),
                        cursor_top.saturating_add(half),
                    )
                } else {
                    (
                        shown.y.saturating_sub(half),
                        cursor_top.saturating_sub(half),
                    )
                };
                let row = view.row_near(target).unwrap_or(cursor_row);
                self.go(view, view.clamp(y), row, cursor.side, false)
            }
            Command::NextChange | Command::PrevChange => {
                let found = if command == Command::NextChange {
                    (cursor_row + 1..rows).find(|&row| view.starts_change(self.mode, row))
                } else {
                    (0..cursor_row)
                        .rev()
                        .find(|&row| view.starts_change(self.mode, row))
                };
                match found.and_then(|row| Some((row, view.heights.top(row)?))) {
                    Some((row, top)) => self.go(view, top, row, cursor.side, true),
                    None => self,
                }
            }
            Command::ToggleVisual => {
                self.selection = match self.selection {
                    Some(_) => None,
                    None => {
                        let side = cursor.addr.side;
                        self.position = Some(Position {
                            cursor: Cursor { side, ..cursor },
                            ..position
                        });
                        Some(Selection {
                            side,
                            tail: cursor.addr.line,
                            head: cursor.addr.line,
                        })
                    }
                };
                self
            }
            Command::ClearSelection => {
                self.selection = None;
                self
            }
            Command::JumpBack => {
                if let Some(previous) = self.jumps.pop() {
                    self.position = Some(previous);
                    self.extend_selection(view);
                }
                self
            }
            Command::ScrollTo(y) => {
                if let Some(anchor) = self.anchor_for(view, position.anchor, view.clamp(y)) {
                    self.position = Some(Position { anchor, ..position });
                }
                self
            }
            Command::Click { row, side } => {
                if let Some(cursor) = cursor_at(view.body, self.mode, row, side) {
                    self.position = Some(Position { cursor, ..position });
                    self.selection = None;
                }
                self
            }
            Command::ExtendTo { row } => {
                let selection = self.selection.unwrap_or(Selection {
                    side: cursor.addr.side,
                    tail: cursor.addr.line,
                    head: cursor.addr.line,
                });
                if let Some(cursor) = cursor_at(view.body, self.mode, row, selection.side) {
                    self.position = Some(Position { cursor, ..position });
                    self.selection = Some(selection);
                    self.extend_selection(view);
                }
                self
            }
        }
    }

    /// Курсор на строку `row`, прокрутка от `y` до наименьшей, при которой строка видна. Переход
    /// (`jump`) запоминает прежнее положение для `Ctrl-o`.
    fn go(mut self, view: &Viewport<'_>, y: u64, row: u32, side: Side, jump: bool) -> Self {
        let (Some(previous), Some(cursor)) =
            (self.position, cursor_at(view.body, self.mode, row, side))
        else {
            return self;
        };
        let y = view.reveal(view.clamp(y), row);
        let Some(anchor) = self.anchor_for(view, previous.anchor, y) else {
            return self;
        };
        let next = Position { anchor, cursor };
        if jump && next != previous {
            if self.jumps.len() == JUMPS {
                self.jumps.remove(0);
            }
            self.jumps.push(previous);
        }
        self.position = Some(next);
        self.extend_selection(view);
        self
    }

    /// Якорь для верхнего края на `y`. Если край не сдвинулся, остаётся прежний якорь: иначе
    /// движение курсора без прокрутки после смены ширины или режима заменило бы сохранённый
    /// якорь показанным положением.
    fn anchor_for(&self, view: &Viewport<'_>, previous: Anchor, y: u64) -> Option<Anchor> {
        if view.shown(self.mode, previous).map(|shown| shown.y) == Some(y) {
            Some(previous)
        } else {
            view.anchor_at(self.mode, y)
        }
    }

    fn extend_selection(&mut self, view: &Viewport<'_>) {
        let (Some(selection), Some(position)) = (self.selection.as_mut(), self.position) else {
            return;
        };
        let side = selection.side;
        let Some(cursor_row) = view.row_of(self.mode, position.cursor.addr) else {
            return;
        };
        let tail_row = view
            .row_of(
                self.mode,
                LineAddr {
                    side,
                    line: selection.tail,
                },
            )
            .unwrap_or(cursor_row);
        let line = |row| side_line(view.body, self.mode, row, side);
        let head = if cursor_row >= tail_row {
            (tail_row..=cursor_row).rev().find_map(line)
        } else {
            (cursor_row..=tail_row).find_map(line)
        };
        if let Some(head) = head {
            selection.head = head;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::model::heights::{self, Metrics};
    use crate::model::parse::parse;
    use crate::model::wrap::{TAB_WIDTH, Wrap};
    use crate::worker::Cancel;

    const LINE: u64 = 10;
    const SCREEN: u64 = 100;

    fn parse_text(text: &str) -> Diff {
        match parse(Arc::from(text.as_bytes()), &Cancel::never()) {
            Ok(diff) => diff,
            Err(failure) => panic!("разбор не прошёл: {failure:?}"),
        }
    }

    /// Один ханк из `blocks` блоков: три строки контекста, удаление длиной `removed` символов и
    /// вставка. Строки unified блока `i` — `5i..5i+5`, изменение — `5i+3`. Строки «Рядом» —
    /// `4i..4i+4`, пара изменения — `4i+3`. Номера строк блока на обеих сторонах — `4i+1..4i+4`.
    fn blocks_diff(blocks: u32, removed: usize) -> Diff {
        let lines = blocks * 4;
        let mut text =
            format!("diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,{lines} +1,{lines} @@\n");
        for i in 0..blocks {
            let old = format!("{i:0>width$}", width = removed);
            text += &format!(" a{i}\n b{i}\n c{i}\n-{old}\n+new{i}\n");
        }
        parse_text(&text)
    }

    fn index(diff: &Diff, mode: Mode, columns: u32) -> HeightIndex {
        let metrics = Metrics {
            line_height: LINE as u32,
            wrap: Wrap::new(columns, TAB_WIDTH),
        };
        let body = diff.files()[0].text().unwrap();
        heights::build(diff, body, mode, metrics, &[], &Cancel::never()).unwrap()
    }

    fn view<'a>(diff: &'a Diff, heights: &'a HeightIndex) -> Viewport<'a> {
        Viewport {
            body: diff.files()[0].text().unwrap(),
            heights,
            height: SCREEN,
        }
    }

    fn run(mut state: ViewState, commands: &[Command], view: &Viewport<'_>) -> ViewState {
        for &command in commands {
            state = state.apply(command, view);
        }
        state
    }

    fn shown(state: &ViewState, view: &Viewport<'_>) -> (u64, u32, u64) {
        let shown = state.shown(view).unwrap();
        (shown.y, shown.row, shown.offset)
    }

    #[test]
    fn opens_at_top_with_cursor_on_first_row() {
        let diff = blocks_diff(40, 4);
        let heights = index(&diff, Mode::Unified, 80);
        let view = view(&diff, &heights);
        let state = ViewState::open(&diff, 0, Mode::Unified);
        assert_eq!(
            state.anchor(),
            Some(Anchor {
                addr: LineAddr::new(1),
                offset: 0
            })
        );
        assert_eq!(
            state.cursor(),
            Some(Cursor {
                addr: LineAddr::new(1),
                side: Side::New
            })
        );
        assert_eq!(shown(&state, &view), (0, 0, 0));
        assert_eq!(state.selection(), None);
    }

    #[test]
    fn j_and_k_move_cursor_and_scroll_minimally() {
        let diff = blocks_diff(40, 4);
        let heights = index(&diff, Mode::Unified, 80);
        let view = view(&diff, &heights);
        let state = ViewState::open(&diff, 0, Mode::Unified);
        let state = run(state, &[Command::Down; 9], &view);
        assert_eq!(state.cursor_row(&view), Some(9));
        assert_eq!(shown(&state, &view), (0, 0, 0));
        let state = state.apply(Command::Down, &view);
        assert_eq!(state.cursor_row(&view), Some(10));
        assert_eq!(shown(&state, &view), (10, 1, 0));
        // Строка 3 — удаление: предпочтение новой стороны остаётся, курсор стоит на старой.
        let state = run(state, &[Command::Up; 7], &view);
        assert_eq!(
            state.cursor(),
            Some(Cursor {
                addr: LineAddr::old(4),
                side: Side::New
            })
        );
        assert_eq!(shown(&state, &view), (10, 1, 0));
        let state = run(state, &[Command::Up; 3], &view);
        assert_eq!(state.cursor_row(&view), Some(0));
        assert_eq!(shown(&state, &view), (0, 0, 0));
        let same = state.clone().apply(Command::Up, &view);
        assert_eq!(same, state);
    }

    #[test]
    fn bottom_top_and_jump_back_restore_position() {
        let diff = blocks_diff(40, 4);
        let heights = index(&diff, Mode::Unified, 80);
        let view = view(&diff, &heights);
        let state = run(
            ViewState::open(&diff, 0, Mode::Unified),
            &[Command::Down; 15],
            &view,
        );
        assert_eq!(shown(&state, &view), (60, 6, 0));
        let before = (state.anchor(), state.cursor());

        let bottom = state.apply(Command::Bottom, &view);
        assert_eq!(bottom.cursor_row(&view), Some(199));
        // Наверху не последняя строка, а предел прокрутки.
        assert_eq!(shown(&bottom, &view), (1900, 190, 0));
        let back = bottom.apply(Command::JumpBack, &view);
        assert_eq!((back.anchor(), back.cursor()), before);

        let top = back.apply(Command::Top, &view);
        assert_eq!(shown(&top, &view), (0, 0, 0));
        assert_eq!(top.cursor_row(&view), Some(0));
        let back = top.apply(Command::JumpBack, &view);
        assert_eq!((back.anchor(), back.cursor()), before);
        let empty = back.clone().apply(Command::JumpBack, &view);
        assert_eq!(empty, back);
    }

    #[test]
    fn half_pages_scroll_and_move_cursor() {
        let diff = blocks_diff(40, 4);
        let heights = index(&diff, Mode::Unified, 80);
        let view = view(&diff, &heights);
        let state = ViewState::open(&diff, 0, Mode::Unified).apply(Command::HalfDown, &view);
        assert_eq!(shown(&state, &view), (50, 5, 0));
        assert_eq!(state.cursor_row(&view), Some(5));
        let state = state.apply(Command::HalfDown, &view);
        assert_eq!(shown(&state, &view), (100, 10, 0));
        assert_eq!(state.cursor_row(&view), Some(10));
        let state = state.apply(Command::HalfUp, &view);
        assert_eq!(shown(&state, &view), (50, 5, 0));
        assert_eq!(state.cursor_row(&view), Some(5));
        let state = run(state, &[Command::HalfUp; 3], &view);
        assert_eq!(shown(&state, &view), (0, 0, 0));
        assert_eq!(state.cursor_row(&view), Some(0));
        // У конца прокрутка упирается в предел, курсор доходит до последней строки.
        let state = run(state, &[Command::Bottom, Command::HalfDown], &view);
        assert_eq!(shown(&state, &view), (1900, 190, 0));
        assert_eq!(state.cursor_row(&view), Some(199));
    }

    #[test]
    fn next_and_prev_change_reach_change_starts() {
        let diff = blocks_diff(40, 4);
        for (mode, first, step) in [(Mode::Unified, 3, 5), (Mode::SideBySide, 3, 4)] {
            let heights = index(&diff, mode, 80);
            let view = view(&diff, &heights);
            let state = ViewState::open(&diff, 0, mode).apply(Command::NextChange, &view);
            assert_eq!(state.cursor_row(&view), Some(first), "{mode:?}");
            assert_eq!(state.cursor().unwrap().addr.line, 4);
            assert_eq!(shown(&state, &view), (first as u64 * LINE, first, 0));
            let second = state.clone().apply(Command::NextChange, &view);
            assert_eq!(second.cursor_row(&view), Some(first + step));
            assert_eq!(shown(&second, &view).1, first + step);
            let back = second.apply(Command::PrevChange, &view);
            assert_eq!(
                (back.anchor(), back.cursor()),
                (state.anchor(), state.cursor())
            );
            // Последнее изменение: в «Рядом» это последняя строка, и `[c` с неё уходит на
            // предыдущее, поэтому путь — `G`, `[c`, `]c`. Верхний край на пределе, дальше `]c`
            // ничего не меняет.
            let last = run(
                ViewState::open(&diff, 0, mode),
                &[Command::Bottom, Command::PrevChange, Command::NextChange],
                &view,
            );
            let start = 39 * step + first;
            assert_eq!(last.cursor_row(&view), Some(start));
            assert_eq!(shown(&last, &view).0, view.max_y());
            let same = last.clone().apply(Command::NextChange, &view);
            assert_eq!(same, last);
        }
        // Внутри изменения `[c` идёт на его начало.
        let heights = index(&diff, Mode::Unified, 80);
        let view = view(&diff, &heights);
        let inside = run(
            ViewState::open(&diff, 0, Mode::Unified),
            &[Command::Down; 4],
            &view,
        );
        assert_eq!(inside.cursor_row(&view), Some(4));
        let start = inside.apply(Command::PrevChange, &view);
        assert_eq!(start.cursor_row(&view), Some(3));
        let none = start.clone().apply(Command::PrevChange, &view);
        assert_eq!(none, start);
    }

    #[test]
    fn visual_selection_keeps_one_side_across_change_block() {
        let diff = blocks_diff(40, 4);
        let heights = index(&diff, Mode::Unified, 80);
        let view = view(&diff, &heights);
        let state = ViewState::open(&diff, 0, Mode::Unified).apply(Command::NextChange, &view);
        let state = state.apply(Command::ToggleVisual, &view);
        assert_eq!(
            state.selection(),
            Some(Selection {
                side: Side::Old,
                tail: 4,
                head: 4
            })
        );
        // Вставка: старой половины нет, конец выделения стоит.
        let state = state.apply(Command::Down, &view);
        assert_eq!(state.cursor().unwrap().addr, LineAddr::new(4));
        assert_eq!(state.selection().unwrap().head, 4);
        // Контекст: конец на его старом номере.
        let state = state.apply(Command::Down, &view);
        let selection = state.selection().unwrap();
        assert_eq!((selection.start(), selection.end()), (4, 5));
        assert!(selection.contains(LineAddr::old(5)));
        assert!(!selection.contains(LineAddr::new(4)));
        let state = run(state, &[Command::Up; 3], &view);
        let selection = state.selection().unwrap();
        assert_eq!(
            (selection.side, selection.start(), selection.end()),
            (Side::Old, 3, 4)
        );
        // Выделение — адреса: переход в конец и возврат его не меняют.
        let state = run(state, &[Command::ClearSelection], &view);
        assert_eq!(state.selection(), None);
        let state = run(state, &[Command::ToggleVisual, Command::Bottom], &view);
        let selection = state.selection().unwrap();
        assert_eq!(
            (selection.side, selection.start(), selection.end()),
            (Side::Old, 3, 160)
        );
        let state = state.apply(Command::ToggleVisual, &view);
        assert_eq!(state.selection(), None);
    }

    #[test]
    fn click_sets_side_and_extend_selects_range() {
        let diff = blocks_diff(40, 4);
        let heights = index(&diff, Mode::SideBySide, 80);
        let view = view(&diff, &heights);
        let state = ViewState::open(&diff, 0, Mode::SideBySide).apply(
            Command::Click {
                row: 3,
                side: Side::Old,
            },
            &view,
        );
        assert_eq!(
            state.cursor(),
            Some(Cursor {
                addr: LineAddr::old(4),
                side: Side::Old
            })
        );
        let state = state.apply(
            Command::Click {
                row: 3,
                side: Side::New,
            },
            &view,
        );
        assert_eq!(state.cursor().unwrap().addr, LineAddr::new(4));
        let state = state.apply(Command::ExtendTo { row: 7 }, &view);
        assert_eq!(
            state.selection(),
            Some(Selection {
                side: Side::New,
                tail: 4,
                head: 8
            })
        );
        let state = state.apply(Command::ExtendTo { row: 1 }, &view);
        let selection = state.selection().unwrap();
        assert_eq!((selection.start(), selection.end()), (2, 4));
        // Щелчок снимает выделение.
        let state = state.apply(
            Command::Click {
                row: 0,
                side: Side::Old,
            },
            &view,
        );
        assert_eq!(state.selection(), None);
        assert_eq!(shown(&state, &view), (0, 0, 0));

        // Щелчок по пустой половине: курсор на строке другой стороны, сторона — щелчка.
        let insertion =
            parse_text("diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,1 +1,2 @@\n a\n+b\n");
        let heights = index(&insertion, Mode::SideBySide, 80);
        let view = Viewport {
            body: insertion.files()[0].text().unwrap(),
            heights: &heights,
            height: SCREEN,
        };
        let state = ViewState::open(&insertion, 0, Mode::SideBySide).apply(
            Command::Click {
                row: 1,
                side: Side::Old,
            },
            &view,
        );
        assert_eq!(
            state.cursor(),
            Some(Cursor {
                addr: LineAddr::new(2),
                side: Side::Old
            })
        );
    }

    #[test]
    fn anchor_survives_width_change() {
        // Удаления по 100 символов: при 80 колонках 2 визуальные строки, при 40 — 3.
        let diff = blocks_diff(40, 100);
        let wide = index(&diff, Mode::Unified, 80);
        let narrow = index(&diff, Mode::Unified, 40);
        let wide_view = view(&diff, &wide);
        let narrow_view = view(&diff, &narrow);
        let top = wide.top(52).unwrap();
        let state =
            ViewState::open(&diff, 0, Mode::Unified).apply(Command::ScrollTo(top + 4), &wide_view);
        assert_eq!(shown(&state, &wide_view), (top + 4, 52, 4));
        let narrow_shown = state.shown(&narrow_view).unwrap();
        assert_eq!((narrow_shown.row, narrow_shown.offset), (52, 4));
        assert_eq!(narrow_shown.y, narrow.top(52).unwrap() + 4);
        assert!(narrow_shown.y > top + 4);
        assert_eq!(shown(&state, &wide_view), (top + 4, 52, 4));

        // Удаление 53: 30 точек при 40 колонках и 20 при 80. Смещение 25 при 80 колонках
        // показывает последнюю точку строки, а обратная смена ширины — прежние 25.
        let top = narrow.top(53).unwrap();
        let state = state.apply(Command::ScrollTo(top + 25), &narrow_view);
        assert_eq!(shown(&state, &narrow_view), (top + 25, 53, 25));
        let clipped = state.shown(&wide_view).unwrap();
        assert_eq!((clipped.row, clipped.offset), (53, 19));
        assert_eq!(shown(&state, &narrow_view), (top + 25, 53, 25));
    }

    #[test]
    fn anchor_survives_thread_above_and_clips_inside_collapsed_block() {
        let diff = blocks_diff(40, 4);
        let mut heights = index(&diff, Mode::Unified, 80);
        let top = heights.top(100).unwrap();
        let state = ViewState::open(&diff, 0, Mode::Unified)
            .apply(Command::ScrollTo(top + 3), &view(&diff, &heights));
        assert_eq!(shown(&state, &view(&diff, &heights)), (top + 3, 100, 3));

        let state = state.toggle_thread(ThreadId(1));
        assert!(state.is_expanded(ThreadId(1)));
        heights.set_block_height(10, 120);
        assert_eq!(shown(&state, &view(&diff, &heights)), (top + 123, 100, 3));
        let state = state.toggle_thread(ThreadId(1));
        assert!(!state.is_expanded(ThreadId(1)));
        heights.set_block_height(10, 0);
        assert_eq!(shown(&state, &view(&diff, &heights)), (top + 3, 100, 3));

        // Верхний край внутри блока треда: якорь — строка блока со смещением в блоке.
        heights.set_block_height(10, 120);
        let inside = heights.top(10).unwrap() + LINE + 50;
        let state = state.apply(Command::ScrollTo(inside), &view(&diff, &heights));
        assert_eq!(state.anchor().unwrap().offset, LINE + 50);
        assert_eq!(heights.row_at(inside).map(|hit| hit.in_block), Some(true));
        heights.set_block_height(10, 0);
        assert_eq!(
            shown(&state, &view(&diff, &heights)),
            (heights.top(10).unwrap() + LINE - 1, 10, LINE - 1)
        );
    }

    #[test]
    fn anchor_round_trips_between_modes() {
        let diff = blocks_diff(40, 4);
        let unified = index(&diff, Mode::Unified, 80);
        let side = index(&diff, Mode::SideBySide, 80);
        let unified_view = view(&diff, &unified);
        let side_view = view(&diff, &side);
        // Строка unified 104 — вставка блока 20 (новая 84), в «Рядом» она в паре строки 83.
        // Строка 103 — удаление того же блока (старая 84) в той же паре.
        for (row, addr) in [
            (104, LineAddr::new(84)),
            (103, LineAddr::old(84)),
            (100, LineAddr::new(81)),
        ] {
            let y = unified.top(row).unwrap() + 3;
            let state =
                ViewState::open(&diff, 0, Mode::Unified).apply(Command::ScrollTo(y), &unified_view);
            assert_eq!(state.anchor().unwrap().addr, addr);
            let state = state.with_mode(Mode::SideBySide);
            let split = side.top(if row == 100 { 80 } else { 83 }).unwrap() + 3;
            assert_eq!(state.shown(&side_view).unwrap().y, split);
            assert_eq!(state.shown(&side_view).unwrap().offset, 3);
            let state = state.with_mode(Mode::Unified);
            assert_eq!(shown(&state, &unified_view), (y, row, 3));
        }
        // Якорь, взятый в «Рядом» на паре, — первая строка пары в unified.
        let y = side.top(83).unwrap();
        let state =
            ViewState::open(&diff, 0, Mode::SideBySide).apply(Command::ScrollTo(y), &side_view);
        assert_eq!(state.anchor().unwrap().addr, LineAddr::old(84));
        let state = state.with_mode(Mode::Unified);
        assert_eq!(shown(&state, &unified_view).1, 103);
    }

    #[test]
    fn moves_without_scroll_keep_preserved_anchor() {
        let diff = blocks_diff(40, 4);
        let unified = index(&diff, Mode::Unified, 80);
        let side = index(&diff, Mode::SideBySide, 80);
        let unified_view = view(&diff, &unified);
        let side_view = view(&diff, &side);
        // Наверху строка 4 — вставка `+new0` (новая 4), курсор на строке 13.
        let state = run(
            ViewState::open(&diff, 0, Mode::Unified),
            &[Command::Down; 13],
            &unified_view,
        );
        assert_eq!(shown(&state, &unified_view), (40, 4, 0));
        // В «Рядом» вставка стоит в паре строки 3, курсор на строке 11 виден: `k` прокрутку не
        // двигает и якорь не меняет.
        let state = state.with_mode(Mode::SideBySide);
        assert_eq!(shown(&state, &side_view), (30, 3, 0));
        let state = state.apply(Command::Up, &side_view);
        assert_eq!(state.cursor_row(&side_view), Some(10));
        assert_eq!(state.anchor().unwrap().addr, LineAddr::new(4));
        let state = state.with_mode(Mode::Unified);
        assert_eq!(shown(&state, &unified_view), (40, 4, 0));

        // Прокрутка на показанное положение тоже не меняет якорь: смещение 25 строки,
        // обрезанное до 19 при широком переносе, возвращается при узком.
        let long = blocks_diff(40, 100);
        let wide = index(&long, Mode::Unified, 80);
        let narrow = index(&long, Mode::Unified, 40);
        let wide_view = view(&long, &wide);
        let narrow_view = view(&long, &narrow);
        let top = narrow.top(53).unwrap();
        let state = ViewState::open(&long, 0, Mode::Unified)
            .apply(Command::ScrollTo(top + 25), &narrow_view);
        let clipped = state.shown(&wide_view).unwrap();
        assert_eq!(clipped.offset, 19);
        let state = state.apply(Command::ScrollTo(clipped.y), &wide_view);
        assert_eq!(shown(&state, &narrow_view), (top + 25, 53, 25));
    }

    #[test]
    fn scroll_clamps_to_document() {
        let diff = blocks_diff(40, 4);
        let heights = index(&diff, Mode::Unified, 80);
        let view = view(&diff, &heights);
        let state =
            ViewState::open(&diff, 0, Mode::Unified).apply(Command::ScrollTo(u64::MAX), &view);
        assert_eq!(shown(&state, &view), (1900, 190, 0));
        let state = state.apply(Command::ScrollTo(0), &view);
        assert_eq!(shown(&state, &view), (0, 0, 0));

        // Документ ниже видимой области: прокрутки нет, `G` ведёт только курсор.
        let short = blocks_diff(1, 4);
        let heights = index(&short, Mode::Unified, 80);
        let view = Viewport {
            body: short.files()[0].text().unwrap(),
            heights: &heights,
            height: SCREEN,
        };
        let state = ViewState::open(&short, 0, Mode::Unified).apply(Command::Bottom, &view);
        assert_eq!(shown(&state, &view), (0, 0, 0));
        assert_eq!(state.cursor_row(&view), Some(4));
    }

    #[test]
    fn file_switch_resets_position_and_keeps_threads() {
        let text = "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1,2 +1,2 @@\n x\n-y\n+z\n\
            diff --git a/b.bin b/b.bin\nindex 1111111..2222222 100644\nBinary files a/b.bin and b/b.bin differ\n\
            diff --git a/c b/c\n--- a/c\n+++ b/c\n@@ -5,1 +5,1 @@\n-p\n+q\n";
        let diff = parse_text(text);
        let heights = index(&diff, Mode::Unified, 80);
        let view = view(&diff, &heights);
        let state = ViewState::open(&diff, 0, Mode::Unified)
            .toggle_thread(ThreadId(7))
            .apply(Command::Down, &view)
            .apply(Command::ToggleVisual, &view);
        assert_eq!(state.clone().prev_file(&diff), state);

        let binary = state.next_file(&diff);
        assert_eq!(binary.file(), 1);
        assert_eq!(
            (binary.anchor(), binary.cursor(), binary.selection()),
            (None, None, None)
        );
        assert!(binary.is_expanded(ThreadId(7)));
        let empty = HeightIndex::default();
        let no_rows = Viewport {
            body: diff.files()[0].text().unwrap(),
            heights: &empty,
            height: SCREEN,
        };
        assert_eq!(binary.clone().apply(Command::Down, &no_rows), binary);

        let last = binary.next_file(&diff);
        assert_eq!(last.file(), 2);
        assert_eq!(last.anchor().unwrap().addr, LineAddr::old(5));
        assert_eq!(last.cursor().unwrap().addr, LineAddr::old(5));
        assert_eq!(last.clone().next_file(&diff), last);
        assert_eq!(last.prev_file(&diff).prev_file(&diff).file(), 0);
    }

    /// Ханки размеров `sizes`: `size` строк контекста и пара удаления и вставки. Строк unified у
    /// ханка `size + 2`, «Рядом» — `size + 1`.
    fn hunks_diff(sizes: &[u32]) -> Diff {
        let mut text = String::from("diff --git a/f b/f\n--- a/f\n+++ b/f\n");
        for (i, &size) in sizes.iter().enumerate() {
            let start = 1000 * i as u32 + 1;
            text += &format!("@@ -{start},{n} +{start},{n} @@\n", n = size + 1);
            for k in 0..size {
                text += &format!(" h{i}l{k}\n");
            }
            text += &format!("-old{i}\n+new{i}\n");
        }
        parse_text(&text)
    }

    #[test]
    fn open_at_last_hunk_clamps_to_the_scroll_limit() {
        let diff = hunks_diff(&[5, 5, 3]);
        let heights = index(&diff, Mode::Unified, 80);
        let view = view(&diff, &heights);
        let state = ViewState::open(&diff, 0, Mode::Unified).at_last_hunk(&view);
        // Последний ханк с 14-й строки показа короче экрана: верх на пределе прокрутки 90.
        assert_eq!(shown(&state, &view).0, 90);
        assert_eq!(state.cursor_row(&view), Some(14));
        assert_eq!(state.cursor().unwrap().addr, LineAddr::new(2001));
        // Переходом открытие не считается: возвращаться некуда.
        assert_eq!(state.clone().apply(Command::JumpBack, &view), state);

        let heights = index(&diff, Mode::SideBySide, 80);
        let view_side = Viewport {
            body: diff.files()[0].text().unwrap(),
            heights: &heights,
            height: SCREEN,
        };
        let side = ViewState::open(&diff, 0, Mode::SideBySide).at_last_hunk(&view_side);
        assert_eq!(shown(&side, &view_side).0, 60);
        assert_eq!(side.cursor_row(&view_side), Some(12));
    }

    #[test]
    fn open_at_long_last_hunk_puts_its_start_on_top() {
        let diff = hunks_diff(&[5, 30]);
        let heights = index(&diff, Mode::Unified, 80);
        let view = view(&diff, &heights);
        let state = ViewState::open(&diff, 0, Mode::Unified).at_last_hunk(&view);
        assert_eq!(shown(&state, &view), (70, 7, 0));
        assert_eq!(state.cursor_row(&view), Some(7));
    }
}
