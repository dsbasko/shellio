//! Подсветка синтаксиса пробы на worker: tree-sitter с грамматикой Rust. Модуль не зависит от
//! AppKit, стили — классы токенов, цвета им даёт вид из токенов темы.
//!
//! Устройство [`HighlightWorker`]:
//! - два исполнителя полного разбора, по одному на сторону, разбирают старый и новый blob целиком
//!   параллельно. Каждый полный разбор идёт свежим разборщиком и останавливается по поколению
//!   через `progress_callback` tree-sitter: флага отмены в tree-sitter 0.27 нет;
//! - поток запросов держит деревья обеих сторон и отвечает на запрос видимых строк. Пока деревьев
//!   нет, он даёт предварительную подсветку локальным разбором окна [`LOCAL_WINDOW_LINES`] строк
//!   файла выше и ниже каждого непрерывного участка видимых строк стороны. Когда готовы оба
//!   дерева, он один раз даёт исправление — снимок полного разбора последнего запроса, — дальше
//!   отвечает на запросы по деревьям через `QueryCursor::set_byte_range`.
//!
//! Правила раскраски одни для всех путей: внутренний захват перекрывает внешний, у захватов
//! одного диапазона действует ранний шаблон запроса, как у `tree-sitter-highlight`. Стили строки —
//! отрезки байт от начала строки без перевода строки, соседние отрезки одного класса слиты.

pub mod reference;

use std::cmp::Reverse;
use std::fmt;
use std::io;
use std::ops::{ControlFlow, Range};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tree_sitter::{Language, ParseOptions, Parser, Query, QueryCursor, StreamingIterator, Tree};

use crate::model::Side;
use crate::worker::{Cancel, Delivery, Stopped, Worker};

/// Окно локального разбора: строк файла выше и ниже участка видимых строк. Записано в план и
/// отчёт до первого прогона и после результатов не меняется.
pub const LOCAL_WINDOW_LINES: u32 = 200;

/// Класс токена — имя захвата запроса подсветки `tree-sitter-rust`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum TokenClass {
    #[serde(rename = "attribute")]
    Attribute,
    #[serde(rename = "comment")]
    Comment,
    #[serde(rename = "comment.documentation")]
    CommentDoc,
    #[serde(rename = "constant")]
    Constant,
    #[serde(rename = "constant.builtin")]
    ConstantBuiltin,
    #[serde(rename = "constructor")]
    Constructor,
    #[serde(rename = "escape")]
    Escape,
    #[serde(rename = "function")]
    Function,
    #[serde(rename = "function.macro")]
    FunctionMacro,
    #[serde(rename = "function.method")]
    FunctionMethod,
    #[serde(rename = "keyword")]
    Keyword,
    #[serde(rename = "label")]
    Label,
    #[serde(rename = "operator")]
    Operator,
    #[serde(rename = "property")]
    Property,
    #[serde(rename = "punctuation.bracket")]
    PunctuationBracket,
    #[serde(rename = "punctuation.delimiter")]
    PunctuationDelimiter,
    #[serde(rename = "string")]
    String,
    #[serde(rename = "type")]
    Type,
    #[serde(rename = "type.builtin")]
    TypeBuiltin,
    #[serde(rename = "variable.builtin")]
    VariableBuiltin,
    #[serde(rename = "variable.parameter")]
    VariableParameter,
}

impl TokenClass {
    pub const ALL: [TokenClass; 21] = [
        TokenClass::Attribute,
        TokenClass::Comment,
        TokenClass::CommentDoc,
        TokenClass::Constant,
        TokenClass::ConstantBuiltin,
        TokenClass::Constructor,
        TokenClass::Escape,
        TokenClass::Function,
        TokenClass::FunctionMacro,
        TokenClass::FunctionMethod,
        TokenClass::Keyword,
        TokenClass::Label,
        TokenClass::Operator,
        TokenClass::Property,
        TokenClass::PunctuationBracket,
        TokenClass::PunctuationDelimiter,
        TokenClass::String,
        TokenClass::Type,
        TokenClass::TypeBuiltin,
        TokenClass::VariableBuiltin,
        TokenClass::VariableParameter,
    ];

    pub fn name(self) -> &'static str {
        match self {
            TokenClass::Attribute => "attribute",
            TokenClass::Comment => "comment",
            TokenClass::CommentDoc => "comment.documentation",
            TokenClass::Constant => "constant",
            TokenClass::ConstantBuiltin => "constant.builtin",
            TokenClass::Constructor => "constructor",
            TokenClass::Escape => "escape",
            TokenClass::Function => "function",
            TokenClass::FunctionMacro => "function.macro",
            TokenClass::FunctionMethod => "function.method",
            TokenClass::Keyword => "keyword",
            TokenClass::Label => "label",
            TokenClass::Operator => "operator",
            TokenClass::Property => "property",
            TokenClass::PunctuationBracket => "punctuation.bracket",
            TokenClass::PunctuationDelimiter => "punctuation.delimiter",
            TokenClass::String => "string",
            TokenClass::Type => "type",
            TokenClass::TypeBuiltin => "type.builtin",
            TokenClass::VariableBuiltin => "variable.builtin",
            TokenClass::VariableParameter => "variable.parameter",
        }
    }

    pub fn from_capture(name: &str) -> Option<TokenClass> {
        TokenClass::ALL
            .into_iter()
            .find(|class| class.name() == name)
    }

    /// Код в буфере раскраски: 0 — без класса.
    fn code(self) -> u8 {
        TokenClass::ALL
            .iter()
            .position(|&class| class == self)
            .map_or(0, |index| index as u8 + 1)
    }

    fn from_code(code: u8) -> Option<TokenClass> {
        TokenClass::ALL
            .get(usize::from(code).checked_sub(1)?)
            .copied()
    }
}

/// Значение для каждой стороны diff.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sides<T> {
    pub old: T,
    pub new: T,
}

impl<T> Sides<T> {
    pub fn get(&self, side: Side) -> &T {
        match side {
            Side::Old => &self.old,
            Side::New => &self.new,
        }
    }

    pub fn get_mut(&mut self, side: Side) -> &mut T {
        match side {
            Side::Old => &mut self.old,
            Side::New => &mut self.new,
        }
    }

    pub fn map<U>(&self, mut f: impl FnMut(Side, &T) -> U) -> Sides<U> {
        Sides {
            old: f(Side::Old, &self.old),
            new: f(Side::New, &self.new),
        }
    }
}

/// Множество номеров строк файла одной стороны (с 1): отсортированные полуинтервалы без
/// пересечений и без смежных.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LineSet {
    ranges: Vec<Range<u32>>,
}

impl LineSet {
    pub fn from_ranges(ranges: impl IntoIterator<Item = Range<u32>>) -> LineSet {
        let mut ranges: Vec<Range<u32>> = ranges
            .into_iter()
            .filter(|range| range.start >= 1 && range.start < range.end)
            .collect();
        ranges.sort_unstable_by_key(|range| range.start);
        let mut merged: Vec<Range<u32>> = Vec::with_capacity(ranges.len());
        for range in ranges {
            match merged.last_mut() {
                Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
                _ => merged.push(range),
            }
        }
        LineSet { ranges: merged }
    }

    pub fn from_lines(lines: impl IntoIterator<Item = u32>) -> LineSet {
        LineSet::from_ranges(lines.into_iter().map(|line| line..line.saturating_add(1)))
    }

    pub fn ranges(&self) -> &[Range<u32>] {
        &self.ranges
    }

    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    pub fn len(&self) -> usize {
        self.ranges
            .iter()
            .map(|range| (range.end - range.start) as usize)
            .sum()
    }

    pub fn contains(&self, line: u32) -> bool {
        let index = self.ranges.partition_point(|range| range.end <= line);
        self.ranges
            .get(index)
            .is_some_and(|range| range.contains(&line))
    }

    pub fn lines(&self) -> impl Iterator<Item = u32> + '_ {
        self.ranges.iter().flat_map(Clone::clone)
    }

    /// Строки, которые есть в файле из `count` строк.
    pub fn clamp(&self, count: u32) -> LineSet {
        let end = count.saturating_add(1);
        LineSet::from_ranges(
            self.ranges
                .iter()
                .map(|range| range.start.min(end)..range.end.min(end)),
        )
    }

    /// Пересечение с полуинтервалом строк.
    pub fn intersect(&self, window: &Range<u32>) -> LineSet {
        LineSet::from_ranges(
            self.ranges
                .iter()
                .map(|range| range.start.max(window.start)..range.end.min(window.end)),
        )
    }

    /// Окна локального разбора: каждый участок, расширенный на `margin` строк в обе стороны в
    /// пределах файла из `count` строк. Пересекающиеся и смежные окна слиты.
    pub fn windows(&self, margin: u32, count: u32) -> Vec<Range<u32>> {
        let end = count.saturating_add(1);
        LineSet::from_ranges(self.clamp(count).ranges.iter().map(|range| {
            range.start.saturating_sub(margin).max(1)..range.end.saturating_add(margin).min(end)
        }))
        .ranges
    }
}

/// Начала строк текста. Строки нумеруются с 1, конечный перевод строки новой строки не
/// начинает: у `"a\nb\n"` две строки, у пустого текста ни одной.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineIndex {
    starts: Vec<u32>,
    len: u32,
    trailing_newline: bool,
}

impl LineIndex {
    /// Текст длиннее `u32::MAX` байт в пробе не встречается: смещения обрезаются.
    pub fn new(text: &[u8]) -> LineIndex {
        let mut starts = Vec::with_capacity(text.len() / 32 + 1);
        if !text.is_empty() {
            starts.push(0);
            starts.extend(
                memchr::memchr_iter(b'\n', text)
                    .map(|newline| newline + 1)
                    .filter(|&start| start < text.len())
                    .map(|start| start as u32),
            );
        }
        LineIndex {
            starts,
            len: text.len() as u32,
            trailing_newline: text.last() == Some(&b'\n'),
        }
    }

    pub fn count(&self) -> u32 {
        self.starts.len() as u32
    }

    /// Ёмкость буфера начал строк в байтах: для оценки памяти данных модели.
    pub fn capacity_bytes(&self) -> usize {
        self.starts.capacity() * std::mem::size_of::<u32>()
    }

    fn start(&self, line: u32) -> Option<usize> {
        self.starts
            .get(line.checked_sub(1)? as usize)
            .map(|&start| start as usize)
    }

    /// Конец содержимого строки без перевода строки.
    fn content_end(&self, line: u32) -> Option<usize> {
        self.start(line)?;
        Some(match self.starts.get(line as usize) {
            Some(&next) => next as usize - 1,
            None if self.trailing_newline => self.len as usize - 1,
            None => self.len as usize,
        })
    }

    /// Байты строки без перевода строки.
    pub fn line(&self, line: u32) -> Option<Range<usize>> {
        Some(self.start(line)?..self.content_end(line)?)
    }

    /// Байты строк вместе с их переводами строки: кусок для локального разбора.
    pub fn bytes(&self, lines: Range<u32>) -> Option<Range<usize>> {
        let start = self.start(lines.start)?;
        let last = lines
            .end
            .checked_sub(1)
            .filter(|&last| last >= lines.start)?;
        self.start(last)?;
        let end = self
            .start(lines.end)
            .unwrap_or(self.len as usize)
            .max(start);
        Some(start..end)
    }

    /// От начала первой строки до конца содержимого последней.
    pub fn content(&self, lines: Range<u32>) -> Option<Range<usize>> {
        let start = self.start(lines.start)?;
        let last = lines
            .end
            .checked_sub(1)
            .filter(|&last| last >= lines.start)?;
        Some(start..self.content_end(last)?.max(start))
    }
}

/// Отрезок одного класса в строке: байты от начала строки.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    pub start: u32,
    pub end: u32,
    pub class: TokenClass,
}

/// Стили строк одной стороны. Строки идут по возрастанию номера.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SideStyles {
    lines: Vec<u32>,
    offsets: Vec<u32>,
    runs: Vec<Run>,
}

impl SideStyles {
    /// Строки с отрезками по возрастанию номера; повтор или убывание номера — ошибка.
    pub fn from_lines<'a>(
        lines: impl IntoIterator<Item = (u32, &'a [Run])>,
    ) -> Result<SideStyles, String> {
        let mut styles = SideStyles::default();
        for (line, runs) in lines {
            if styles.lines.last().is_some_and(|&last| last >= line) {
                return Err(format!("строка {line} не по возрастанию"));
            }
            styles.push_line(line, runs.iter().copied());
        }
        Ok(styles)
    }

    fn push_line(&mut self, line: u32, runs: impl IntoIterator<Item = Run>) {
        if self.offsets.is_empty() {
            self.offsets.push(0);
        }
        self.lines.push(line);
        self.runs.extend(runs);
        self.offsets.push(self.runs.len() as u32);
    }

    pub fn line(&self, line: u32) -> Option<&[Run]> {
        let index = self.lines.binary_search(&line).ok()?;
        let start = *self.offsets.get(index)? as usize;
        let end = *self.offsets.get(index + 1)? as usize;
        self.runs.get(start..end)
    }

    pub fn lines(&self) -> impl Iterator<Item = (u32, &[Run])> + '_ {
        self.lines.iter().enumerate().map(|(index, &line)| {
            let start = self.offsets[index] as usize;
            let end = self.offsets[index + 1] as usize;
            (line, &self.runs[start..end])
        })
    }

    pub fn covered(&self) -> LineSet {
        LineSet::from_lines(self.lines.iter().copied())
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub fn run_count(&self) -> usize {
        self.runs.len()
    }
}

/// Сверка стилей с ожидаемыми по набору строк.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Comparison {
    /// Строки с другими отрезками.
    pub mismatched: Vec<u32>,
    /// Строк нет в ожидаемом: ожидание не покрывает проверку.
    pub missing_expected: Vec<u32>,
    /// Строк нет в проверяемом снимке.
    pub missing_actual: Vec<u32>,
}

impl Comparison {
    pub fn matches(&self) -> bool {
        self.mismatched.is_empty()
            && self.missing_expected.is_empty()
            && self.missing_actual.is_empty()
    }
}

pub fn compare(expected: &SideStyles, actual: &SideStyles, lines: &LineSet) -> Comparison {
    let mut comparison = Comparison::default();
    for line in lines.lines() {
        match (expected.line(line), actual.line(line)) {
            (None, _) => comparison.missing_expected.push(line),
            (Some(_), None) => comparison.missing_actual.push(line),
            (Some(want), Some(got)) if want != got => comparison.mismatched.push(line),
            (Some(_), Some(_)) => {}
        }
    }
    comparison
}

/// Захват: границы узла в байтах текста, шаблон запроса и класс.
#[derive(Clone, Copy, Debug)]
struct Capture {
    start: usize,
    end: usize,
    pattern: usize,
    class: TokenClass,
}

pub fn language() -> Language {
    tree_sitter_rust::LANGUAGE.into()
}

/// Скомпилированный запрос подсветки и курсор. Один на поток.
pub struct Styler {
    query: Query,
    classes: Vec<Option<TokenClass>>,
    cursor: QueryCursor,
    captures: Vec<Capture>,
    paint: Vec<u8>,
    runs: Vec<Run>,
}

impl fmt::Debug for Styler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Styler")
            .field("patterns", &self.query.pattern_count())
            .finish_non_exhaustive()
    }
}

impl Styler {
    pub fn new() -> Result<Styler, String> {
        let query = Query::new(&language(), tree_sitter_rust::HIGHLIGHTS_QUERY)
            .map_err(|err| format!("запрос подсветки не скомпилирован: {err}"))?;
        let classes = query
            .capture_names()
            .iter()
            .map(|name| TokenClass::from_capture(name))
            .collect();
        Ok(Styler {
            query,
            classes,
            cursor: QueryCursor::new(),
            captures: Vec::new(),
            paint: Vec::new(),
            runs: Vec::new(),
        })
    }

    /// Имена захватов запроса без класса: их отрезки потерялись бы молча.
    pub fn unmapped_captures(&self) -> Vec<&str> {
        self.query
            .capture_names()
            .iter()
            .zip(&self.classes)
            .filter_map(|(name, class)| class.is_none().then_some(*name))
            .collect()
    }

    /// Стили строк `lines` по дереву всего текста: запрос по диапазону байт каждого участка.
    pub fn styles(
        &mut self,
        tree: &Tree,
        text: &[u8],
        index: &LineIndex,
        lines: &LineSet,
    ) -> SideStyles {
        let mut out = SideStyles::default();
        for range in lines.clamp(index.count()).ranges() {
            self.ranged(tree, text, index, range.clone(), 0, &mut out);
        }
        out
    }

    /// Предварительные стили: каждый кусок окна разбирается отдельно, без остального файла.
    pub fn preliminary(
        &mut self,
        parser: &mut Parser,
        text: &[u8],
        index: &LineIndex,
        lines: &LineSet,
    ) -> SideStyles {
        let mut out = SideStyles::default();
        let lines = lines.clamp(index.count());
        for window in lines.windows(LOCAL_WINDOW_LINES, index.count()) {
            let Some(bytes) = index.bytes(window.clone()) else {
                continue;
            };
            let slice = &text[bytes];
            // Локальный разбор не отменяется, поэтому разборщик можно держать между вызовами.
            let Some(tree) = parser.parse(slice, None) else {
                continue;
            };
            let local = LineIndex::new(slice);
            let shift = window.start - 1;
            for range in lines.intersect(&window).ranges() {
                self.ranged(
                    &tree,
                    slice,
                    &local,
                    range.start - shift..range.end - shift,
                    shift,
                    &mut out,
                );
            }
        }
        out
    }

    /// Стили строк `lines` запросом без диапазона по всему дереву: путь эталона, независимый от
    /// `set_byte_range`.
    pub fn unranged(
        &mut self,
        tree: &Tree,
        text: &[u8],
        index: &LineIndex,
        lines: &LineSet,
    ) -> SideStyles {
        let lines = lines.clamp(index.count());
        let spans: Vec<Range<usize>> = lines
            .ranges()
            .iter()
            .filter_map(|range| index.content(range.clone()))
            .collect();
        let mut buckets: Vec<Vec<Capture>> = vec![Vec::new(); spans.len()];
        self.cursor.set_byte_range(0..u32::MAX as usize);
        let mut captures = self.cursor.captures(&self.query, tree.root_node(), text);
        while let Some((found, index)) = captures.next() {
            let Some(capture) = found.captures().get(*index) else {
                continue;
            };
            let Some(Some(class)) = self.classes.get(capture.index as usize) else {
                continue;
            };
            let bytes = capture.node.byte_range();
            let first = spans.partition_point(|span| span.end < bytes.start);
            for (bucket, span) in buckets[first..].iter_mut().zip(&spans[first..]) {
                if span.start > bytes.end {
                    break;
                }
                bucket.push(Capture {
                    start: bytes.start,
                    end: bytes.end,
                    pattern: found.pattern_index,
                    class: *class,
                });
            }
        }
        let mut out = SideStyles::default();
        for ((range, span), bucket) in lines.ranges().iter().zip(&spans).zip(&mut buckets) {
            self.captures.clear();
            self.captures.append(bucket);
            self.emit(text, index, range.clone(), span.clone(), 0, &mut out);
        }
        out
    }

    fn ranged(
        &mut self,
        tree: &Tree,
        text: &[u8],
        index: &LineIndex,
        lines: Range<u32>,
        shift: u32,
        out: &mut SideStyles,
    ) {
        let Some(span) = index.content(lines.clone()) else {
            return;
        };
        self.captures.clear();
        self.cursor.set_byte_range(span.clone());
        let mut captures = self.cursor.captures(&self.query, tree.root_node(), text);
        while let Some((found, index)) = captures.next() {
            let Some(capture) = found.captures().get(*index) else {
                continue;
            };
            let Some(Some(class)) = self.classes.get(capture.index as usize) else {
                continue;
            };
            let bytes = capture.node.byte_range();
            self.captures.push(Capture {
                start: bytes.start,
                end: bytes.end,
                pattern: found.pattern_index,
                class: *class,
            });
        }
        self.emit(text, index, lines, span, shift, out);
    }

    /// Раскрашивает участок захватами из `self.captures` и дописывает строки в `out`.
    fn emit(
        &mut self,
        text: &[u8],
        index: &LineIndex,
        lines: Range<u32>,
        span: Range<usize>,
        shift: u32,
        out: &mut SideStyles,
    ) {
        debug_assert!(span.end <= text.len());
        self.paint.clear();
        self.paint.resize(span.len(), 0);
        self.captures
            .sort_unstable_by_key(|capture| (capture.start, Reverse(capture.end), capture.pattern));
        let mut last = None;
        for capture in &self.captures {
            let bounds = (capture.start, capture.end);
            if last == Some(bounds) {
                continue;
            }
            last = Some(bounds);
            let start = capture.start.clamp(span.start, span.end) - span.start;
            let end = capture.end.clamp(span.start, span.end) - span.start;
            self.paint[start..end].fill(capture.class.code());
        }
        for line in lines {
            let Some(bytes) = index.line(line) else {
                continue;
            };
            let codes = &self.paint[bytes.start - span.start..bytes.end - span.start];
            self.runs.clear();
            push_runs(&mut self.runs, codes);
            out.push_line(line + shift, self.runs.drain(..));
        }
    }
}

fn push_runs(out: &mut Vec<Run>, codes: &[u8]) {
    let mut start = 0;
    while start < codes.len() {
        let code = codes[start];
        let end = codes[start..]
            .iter()
            .position(|&other| other != code)
            .map_or(codes.len(), |len| start + len);
        if let Some(class) = TokenClass::from_code(code) {
            out.push(Run {
                start: start as u32,
                end: end as u32,
                class,
            });
        }
        start = end;
    }
}

/// Почему полный разбор не дал дерева.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseStop {
    /// Отмена: смещение разборщика в момент остановки.
    Cancelled {
        at_byte: usize,
    },
    Grammar(String),
}

/// Полный разбор свежим разборщиком: остановленный разборщик продолжил бы прерванный разбор на
/// следующем вызове. `stop` проверяется в `progress_callback` каждые 100 операций разборщика.
pub fn parse_full(text: &[u8], stop: &dyn Fn() -> bool) -> Result<Tree, ParseStop> {
    let mut parser = Parser::new();
    parser
        .set_language(&language())
        .map_err(|err| ParseStop::Grammar(err.to_string()))?;
    let mut at_byte = 0;
    let mut progress = |state: &tree_sitter::ParseState| {
        if stop() {
            at_byte = state.current_byte_offset();
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let mut read = |offset: usize, _| text.get(offset..).unwrap_or_default();
    let tree = parser.parse_with_options(
        &mut read,
        None,
        Some(ParseOptions::new().progress_callback(&mut progress)),
    );
    match tree {
        Some(tree) if !stop() => Ok(tree),
        Some(_) => Err(ParseStop::Cancelled {
            at_byte: text.len(),
        }),
        None => Err(ParseStop::Cancelled { at_byte }),
    }
}

/// Откуда стили снимка.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// Предварительные: локальный разбор окна.
    Local,
    /// Полный разбор обеих сторон. Первый такой снимок документа — исправление.
    Full,
}

/// Неизменяемый снимок стилей видимых строк обеих сторон.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    /// Документ — поколение открытия.
    pub document: u64,
    /// Номер запроса видимых строк, на который отвечает снимок.
    pub request: u64,
    pub source: Source,
    pub styles: Sides<SideStyles>,
    /// Время расчёта снимка на потоке запросов.
    pub compute: Duration,
}

/// События разбора для стенда: замеры и проверка отмены.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    QueryCompiled {
        elapsed: Duration,
    },
    QueryFailed {
        reason: String,
    },
    Indexed {
        document: u64,
        elapsed: Duration,
    },
    ParseStarted {
        document: u64,
        side: Side,
    },
    ParseFinished {
        document: u64,
        side: Side,
        elapsed: Duration,
        bytes: usize,
    },
    ParseCancelled {
        document: u64,
        side: Side,
        at_byte: usize,
    },
    ParseFailed {
        document: u64,
        side: Side,
        reason: String,
    },
}

/// Наблюдатель событий. Вызывается на потоках worker и не должен ждать их владельца.
pub type Observer = Arc<dyn Fn(&Event) + Send + Sync>;

fn notify(observer: &Option<Observer>, event: Event) {
    if let Some(observer) = observer {
        observer(&event);
    }
}

struct ParseJob {
    document: u64,
    side: Side,
    text: Arc<[u8]>,
}

struct Parsed {
    document: u64,
    side: Side,
    tree: Tree,
}

enum Message {
    Open {
        document: u64,
        request: u64,
        texts: Sides<Arc<[u8]>>,
        visible: Sides<LineSet>,
    },
    Visible {
        document: u64,
        request: u64,
        visible: Sides<LineSet>,
    },
    Parsed(Parsed),
    /// Закрытие вида: документ и его деревья освобождаются.
    Close,
}

/// Подсветка на worker. `Drop` отменяет разбор и ждёт все потоки.
pub struct HighlightWorker {
    current: Arc<AtomicU64>,
    requests: AtomicU64,
    messages: Option<Sender<Message>>,
    parsers: Option<Sides<Worker<ParseJob>>>,
    thread: Option<JoinHandle<()>>,
}

impl HighlightWorker {
    /// `deliver` вызывается на потоке запросов.
    pub fn spawn(
        deliver: impl Fn(Snapshot) + Send + 'static,
        observer: Option<Observer>,
    ) -> io::Result<HighlightWorker> {
        let current = Arc::new(AtomicU64::new(0));
        let (messages, inbox) = mpsc::channel::<Message>();
        let parser = |side: Side| {
            let observer = observer.clone();
            let messages = messages.clone();
            let name = match side {
                Side::Old => "probe-diff-highlight-old",
                Side::New => "probe-diff-highlight-new",
            };
            Worker::spawn(
                name,
                move |job: ParseJob, cancel: &Cancel| parse_job(job, cancel, &observer),
                move |delivery: Delivery<Parsed>| {
                    let _ = messages.send(Message::Parsed(delivery.value));
                },
            )
        };
        let parsers = Sides {
            old: parser(Side::Old)?,
            new: parser(Side::New)?,
        };
        let shared = Arc::clone(&current);
        let thread = thread::Builder::new()
            .name("probe-diff-highlight".to_owned())
            .spawn(move || query_loop(&inbox, &shared, &deliver, &observer))?;
        Ok(HighlightWorker {
            current,
            requests: AtomicU64::new(0),
            messages: Some(messages),
            parsers: Some(parsers),
            thread: Some(thread),
        })
    }

    /// Новый документ: прежний разбор отменяется, ответ на `visible` придёт предварительным
    /// снимком, если полный разбор не готов. Возвращает номер документа и запроса.
    pub fn open(
        &self,
        texts: Sides<Arc<[u8]>>,
        visible: Sides<LineSet>,
    ) -> Result<(u64, u64), Stopped> {
        let document = self.current.fetch_add(1, Ordering::AcqRel) + 1;
        let request = self.requests.fetch_add(1, Ordering::AcqRel) + 1;
        let messages = self.messages.as_ref().ok_or(Stopped)?;
        let parsers = self.parsers.as_ref().ok_or(Stopped)?;
        // Открытие уходит в очередь раньше заданий разбора, поэтому деревья приходят после него.
        messages
            .send(Message::Open {
                document,
                request,
                texts: texts.clone(),
                visible,
            })
            .map_err(|_| Stopped)?;
        for side in Side::BOTH {
            parsers.get(side).submit(ParseJob {
                document,
                side,
                text: Arc::clone(texts.get(side)),
            })?;
        }
        Ok((document, request))
    }

    /// Запрос видимых строк текущего документа. Из очереди отвечается только последний.
    pub fn visible(&self, visible: Sides<LineSet>) -> Result<u64, Stopped> {
        let request = self.requests.fetch_add(1, Ordering::AcqRel) + 1;
        let messages = self.messages.as_ref().ok_or(Stopped)?;
        messages
            .send(Message::Visible {
                document: self.current.load(Ordering::Acquire),
                request,
                visible,
            })
            .map_err(|_| Stopped)?;
        Ok(request)
    }

    /// Закрытие вида: разбор отменяется, снимков текущего документа больше нет.
    pub fn close(&self) {
        self.current.fetch_add(1, Ordering::AcqRel);
        if let Some(parsers) = &self.parsers {
            parsers.old.cancel();
            parsers.new.cancel();
        }
        if let Some(messages) = &self.messages {
            let _ = messages.send(Message::Close);
        }
    }

    pub fn document(&self) -> u64 {
        self.current.load(Ordering::Acquire)
    }
}

impl Drop for HighlightWorker {
    fn drop(&mut self) {
        self.close();
        // Исполнители разбора держат копии отправителя: поток запросов выйдет после них.
        drop(self.parsers.take());
        drop(self.messages.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn parse_job(job: ParseJob, cancel: &Cancel, observer: &Option<Observer>) -> Option<Parsed> {
    let ParseJob {
        document,
        side,
        text,
    } = job;
    notify(observer, Event::ParseStarted { document, side });
    let started = Instant::now();
    match parse_full(&text, &|| cancel.is_cancelled()) {
        Ok(tree) => {
            notify(
                observer,
                Event::ParseFinished {
                    document,
                    side,
                    elapsed: started.elapsed(),
                    bytes: text.len(),
                },
            );
            Some(Parsed {
                document,
                side,
                tree,
            })
        }
        Err(ParseStop::Cancelled { at_byte }) => {
            notify(
                observer,
                Event::ParseCancelled {
                    document,
                    side,
                    at_byte,
                },
            );
            None
        }
        Err(ParseStop::Grammar(reason)) => {
            notify(
                observer,
                Event::ParseFailed {
                    document,
                    side,
                    reason,
                },
            );
            None
        }
    }
}

struct Document {
    id: u64,
    texts: Sides<Arc<[u8]>>,
    index: Sides<LineIndex>,
    trees: Sides<Option<Tree>>,
    /// Последний запрос видимых строк: на него отвечает исправление.
    visible: (u64, Sides<LineSet>),
    corrected: bool,
}

fn query_loop(
    inbox: &Receiver<Message>,
    current: &AtomicU64,
    deliver: &dyn Fn(Snapshot),
    observer: &Option<Observer>,
) {
    let started = Instant::now();
    let mut styler = match Styler::new() {
        Ok(styler) => styler,
        Err(reason) => {
            notify(observer, Event::QueryFailed { reason });
            return;
        }
    };
    notify(
        observer,
        Event::QueryCompiled {
            elapsed: started.elapsed(),
        },
    );
    let mut parser = Parser::new();
    if let Err(err) = parser.set_language(&language()) {
        notify(
            observer,
            Event::QueryFailed {
                reason: err.to_string(),
            },
        );
        return;
    }
    let mut document: Option<Document> = None;
    while let Ok(first) = inbox.recv() {
        // Всё, что успело прийти, разбирается разом: отвечается только последний запрос.
        // Деревья прежних документов освобождаются после ответа: освобождение дерева на 100 000
        // строк стоит десятки миллисекунд и задержало бы первый экран нового документа.
        let mut retired: Vec<Tree> = Vec::new();
        let mut answer = false;
        for message in std::iter::once(first).chain(inbox.try_iter()) {
            match message {
                Message::Open {
                    document: id,
                    request,
                    texts,
                    visible,
                } => {
                    if current.load(Ordering::Acquire) != id {
                        continue;
                    }
                    let indexed = Instant::now();
                    let index = texts.map(|_, text| LineIndex::new(text));
                    notify(
                        observer,
                        Event::Indexed {
                            document: id,
                            elapsed: indexed.elapsed(),
                        },
                    );
                    let opened = Document {
                        id,
                        texts,
                        index,
                        trees: Sides::default(),
                        visible: (request, visible),
                        corrected: false,
                    };
                    retired.extend(
                        document
                            .replace(opened)
                            .into_iter()
                            .flat_map(Document::into_trees),
                    );
                    answer = true;
                }
                Message::Visible {
                    document: id,
                    request,
                    visible,
                } => {
                    if let Some(document) = document.as_mut().filter(|document| document.id == id) {
                        document.visible = (request, visible);
                        answer = true;
                    }
                }
                Message::Parsed(parsed) => match document
                    .as_mut()
                    .filter(|document| document.id == parsed.document)
                {
                    Some(document) => {
                        *document.trees.get_mut(parsed.side) = Some(parsed.tree);
                        if document.trees.old.is_some()
                            && document.trees.new.is_some()
                            && !document.corrected
                        {
                            answer = true;
                        }
                    }
                    None => retired.push(parsed.tree),
                },
                Message::Close => {
                    retired.extend(document.take().into_iter().flat_map(Document::into_trees));
                    answer = false;
                }
            }
        }
        if answer
            && let Some(document) = document.as_mut()
            && current.load(Ordering::Acquire) == document.id
        {
            let snapshot = document.answer(&mut styler, &mut parser);
            if current.load(Ordering::Acquire) == document.id {
                deliver(snapshot);
            }
        }
        drop(retired);
    }
}

impl Document {
    fn into_trees(self) -> impl Iterator<Item = Tree> {
        [self.trees.old, self.trees.new].into_iter().flatten()
    }

    /// Снимок последнего запроса: по деревьям, если готовы оба, иначе локальным разбором.
    fn answer(&mut self, styler: &mut Styler, parser: &mut Parser) -> Snapshot {
        let computed = Instant::now();
        let (request, visible) = &self.visible;
        let (source, styles) = match (&self.trees.old, &self.trees.new) {
            (Some(old), Some(new)) => {
                self.corrected = true;
                let trees = Sides { old, new };
                (
                    Source::Full,
                    visible.map(|side, lines| {
                        styler.styles(
                            trees.get(side),
                            self.texts.get(side),
                            self.index.get(side),
                            lines,
                        )
                    }),
                )
            }
            _ => (
                Source::Local,
                visible.map(|side, lines| {
                    styler.preliminary(parser, self.texts.get(side), self.index.get(side), lines)
                }),
            ),
        };
        Snapshot {
            document: self.id,
            request: *request,
            source,
            styles,
            compute: computed.elapsed(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::sync::mpsc::RecvTimeoutError;
    use std::sync::{Mutex, OnceLock};

    use super::reference::{EDGE_DIFF_LINES, diff_lines, edge_lines};
    use super::*;
    use crate::fixtures::{self, Fixture, FixtureName, SPARSE_LAST_HUNK};
    use crate::model::{Diff, TextBody, parse};

    /// Предел ожидания доставки: защита от зависания теста, а не синхронизация.
    const WAIT: Duration = Duration::from_secs(60);

    const SAMPLE: &str = "fn main() {\n    let s = r#\"x\"#; /* a\nb */ let t = 1;\n}\n";

    fn recv<T>(rx: &Receiver<T>) -> T {
        match rx.recv_timeout(WAIT) {
            Ok(value) => value,
            Err(RecvTimeoutError::Timeout) => panic!("доставки нет за {WAIT:?}"),
            Err(RecvTimeoutError::Disconnected) => panic!("канал доставки закрыт"),
        }
    }

    fn run(start: u32, end: u32, class: TokenClass) -> Run {
        Run { start, end, class }
    }

    fn full_styles(text: &[u8], lines: &LineSet) -> SideStyles {
        let tree = parse_full(text, &|| false).unwrap();
        let index = LineIndex::new(text);
        Styler::new().unwrap().styles(&tree, text, &index, lines)
    }

    struct Sparse {
        fixture: Fixture,
        diff: Diff,
        trees: Sides<Tree>,
        index: Sides<LineIndex>,
    }

    impl Sparse {
        fn body(&self) -> &TextBody {
            self.diff.files()[0].text().unwrap()
        }

        fn texts(&self) -> Sides<Arc<[u8]>> {
            Sides {
                old: Arc::clone(&self.fixture.old),
                new: Arc::clone(&self.fixture.new),
            }
        }

        /// Строки последнего ханка: экран открытия `sparse`.
        fn last_hunk(&self) -> Sides<LineSet> {
            let body = self.body();
            let hunk = body.hunks().last().unwrap();
            diff_lines(body, hunk.lines.start as usize..hunk.lines.end as usize)
        }
    }

    fn sparse() -> &'static Sparse {
        static SPARSE: OnceLock<Sparse> = OnceLock::new();
        SPARSE.get_or_init(|| {
            let fixture = fixtures::generate(FixtureName::Sparse);
            let diff = parse::parse(fixture.diff.clone(), &Cancel::never()).unwrap();
            let trees = Sides {
                old: parse_full(&fixture.old, &|| false).unwrap(),
                new: parse_full(&fixture.new, &|| false).unwrap(),
            };
            let index = Sides {
                old: LineIndex::new(&fixture.old),
                new: LineIndex::new(&fixture.new),
            };
            Sparse {
                fixture,
                diff,
                trees,
                index,
            }
        })
    }

    #[test]
    fn every_capture_has_a_class() {
        let styler = Styler::new().unwrap();
        assert_eq!(styler.unmapped_captures(), Vec::<&str>::new());
        for class in TokenClass::ALL {
            assert_eq!(TokenClass::from_code(class.code()), Some(class));
            assert_eq!(TokenClass::from_capture(class.name()), Some(class));
            assert_eq!(
                serde_json::to_string(&class).unwrap(),
                format!("\"{}\"", class.name())
            );
        }
        assert_eq!(TokenClass::from_code(0), None);
    }

    #[test]
    fn line_index_counts_lines_like_git() {
        let cases: [(&str, &[(usize, usize)]); 7] = [
            ("", &[]),
            ("a", &[(0, 1)]),
            ("a\n", &[(0, 1)]),
            ("a\nbc", &[(0, 1), (2, 4)]),
            ("a\nbc\n", &[(0, 1), (2, 4)]),
            ("\n\n", &[(0, 0), (1, 1)]),
            ("a\r\nb", &[(0, 2), (3, 4)]),
        ];
        for (text, lines) in cases {
            let index = LineIndex::new(text.as_bytes());
            assert_eq!(index.count() as usize, lines.len(), "{text:?}");
            for (number, &(start, end)) in (1..).zip(lines) {
                assert_eq!(index.line(number), Some(start..end), "{text:?} {number}");
            }
            assert_eq!(index.line(0), None);
            assert_eq!(index.line(lines.len() as u32 + 1), None);
        }
        let index = LineIndex::new(b"ab\ncd\nef\n");
        assert_eq!(index.bytes(2..4), Some(3..9));
        assert_eq!(index.content(2..4), Some(3..8));
        assert_eq!(index.bytes(1..2), Some(0..3));
        assert_eq!(index.content(3..3), None);
        assert_eq!(index.bytes(4..5), None);
    }

    #[test]
    fn line_set_normalizes_and_builds_windows() {
        let set = LineSet::from_ranges([5..7, 1..3, 3..4, 6..9, 0..2, 12..12]);
        assert_eq!(set.ranges(), &[1..4, 5..9]);
        assert_eq!(set.len(), 7);
        assert!(set.contains(3) && set.contains(8) && !set.contains(4) && !set.contains(9));
        assert_eq!(
            LineSet::from_lines([4, 2, 3, 9, 3]).ranges(),
            &[2..5, 9..10]
        );
        assert_eq!(set.clamp(6).ranges(), &[1..4, 5..7]);
        assert_eq!(set.intersect(&(3..6)).ranges(), &[3..4, 5..6]);
        let visible = LineSet::from_ranges([10..20, 300..310, 800..805]);
        assert_eq!(visible.windows(200, 1_000), vec![1..510, 600..1_001]);
        assert_eq!(visible.windows(200, 802), vec![1..510, 600..803]);
        assert!(LineSet::default().windows(200, 10).is_empty());
    }

    #[test]
    fn sample_styles_follow_highlight_rules() {
        let styles = full_styles(SAMPLE.as_bytes(), &LineSet::from_lines(1..=4));
        use TokenClass::*;
        assert_eq!(
            styles.line(1).unwrap(),
            &[
                run(0, 2, Keyword),
                run(3, 7, Function),
                run(7, 9, PunctuationBracket),
                run(10, 11, PunctuationBracket),
            ]
        );
        assert_eq!(
            styles.line(2).unwrap(),
            &[
                run(4, 7, Keyword),
                run(12, 18, String),
                run(18, 19, PunctuationDelimiter),
                run(20, 24, Comment),
            ]
        );
        // Строка после конца многострочного комментария: комментарий до `*/`, дальше код.
        assert_eq!(
            styles.line(3).unwrap(),
            &[
                run(0, 4, Comment),
                run(5, 8, Keyword),
                run(13, 14, ConstantBuiltin),
                run(14, 15, PunctuationDelimiter),
            ]
        );
        assert_eq!(styles.line(4).unwrap(), &[run(0, 1, PunctuationBracket)]);
        assert_eq!(styles.covered(), LineSet::from_lines(1..=4));
    }

    #[test]
    fn ranged_unranged_and_preliminary_agree_without_outer_context() {
        let text = SAMPLE.as_bytes();
        let tree = parse_full(text, &|| false).unwrap();
        let index = LineIndex::new(text);
        let mut styler = Styler::new().unwrap();
        let lines = LineSet::from_lines([3]);
        let ranged = styler.styles(&tree, text, &index, &lines);
        assert_eq!(ranged, styler.unranged(&tree, text, &index, &lines));
        let mut parser = Parser::new();
        parser.set_language(&language()).unwrap();
        assert_eq!(
            ranged,
            styler.preliminary(&mut parser, text, &index, &lines)
        );

        // Строки без многострочных конструкций: окно в середине длинного текста даёт те же стили,
        // номера строк окна переводятся в номера файла.
        let long: std::string::String = (1..=1_000).map(|n| format!("let x{n} = {n};\n")).collect();
        let long = long.as_bytes();
        let tree = parse_full(long, &|| false).unwrap();
        let index = LineIndex::new(long);
        let lines = LineSet::from_ranges([500..511, 990..1_001]);
        let full = styler.styles(&tree, long, &index, &lines);
        assert_eq!(full.line_count(), 22);
        assert_eq!(full, styler.preliminary(&mut parser, long, &index, &lines));
        assert_eq!(full, styler.unranged(&tree, long, &index, &lines));
    }

    #[test]
    fn reference_matches_full_parse_at_sparse_last_hunk() {
        let sparse = sparse();
        let mut styler = Styler::new().unwrap();
        let lines = edge_lines(sparse.body(), EDGE_DIFF_LINES);
        for side in Side::BOTH {
            let text = sparse.texts().get(side).clone();
            let index = sparse.index.get(side);
            let tree = sparse.trees.get(side);
            let reference = styler.unranged(tree, &text, index, lines.get(side));
            let ranged = styler.styles(tree, &text, index, lines.get(side));
            assert_eq!(reference.line_count(), lines.get(side).len(), "{side:?}");
            assert_eq!(
                compare(&reference, &ranged, lines.get(side)),
                Comparison::default()
            );
            // Строка 99 900 целиком внутри комментария старой стороны и raw-строки новой.
            let line = index.line(SPARSE_LAST_HUNK).unwrap();
            let class = match side {
                Side::Old => TokenClass::Comment,
                Side::New => TokenClass::String,
            };
            assert_eq!(
                reference.line(SPARSE_LAST_HUNK).unwrap(),
                &[run(0, line.len() as u32, class)],
                "{side:?}"
            );
        }
    }

    #[test]
    fn preliminary_differs_from_reference_at_sparse_last_hunk() {
        let sparse = sparse();
        let mut styler = Styler::new().unwrap();
        let mut parser = Parser::new();
        parser.set_language(&language()).unwrap();
        let visible = sparse.last_hunk();
        for side in Side::BOTH {
            let text = sparse.texts().get(side).clone();
            let index = sparse.index.get(side);
            let lines = visible.get(side);
            assert!(lines.contains(SPARSE_LAST_HUNK), "{side:?}");
            let reference = styler.unranged(sparse.trees.get(side), &text, index, lines);
            let preliminary = styler.preliminary(&mut parser, &text, index, lines);
            let comparison = compare(&reference, &preliminary, lines);
            assert!(comparison.missing_expected.is_empty() && comparison.missing_actual.is_empty());
            assert!(
                comparison.mismatched.contains(&SPARSE_LAST_HUNK),
                "{side:?}: {comparison:?}"
            );
        }
    }

    #[test]
    fn full_parse_stops_on_cancel_and_fresh_parse_is_whole() {
        let text: &[u8] = &sparse().fixture.new;
        let calls = Cell::new(0u32);
        let stop = || {
            calls.set(calls.get() + 1);
            calls.get() > 10
        };
        match parse_full(text, &stop) {
            Err(ParseStop::Cancelled { at_byte }) => {
                assert!(at_byte > 0 && at_byte < text.len(), "{at_byte}");
            }
            other => panic!("разбор не остановлен: {:?}", other.map(|_| ())),
        }
        // Отмена до начала видна на первой проверке, через 100 операций разборщика.
        let cancelled = Cancel::cancelled();
        match parse_full(text, &|| cancelled.is_cancelled()) {
            Err(ParseStop::Cancelled { at_byte }) => assert!(at_byte < 4_096, "{at_byte}"),
            other => panic!("разбор не остановлен: {:?}", other.map(|_| ())),
        }
        let tree = parse_full(text, &|| false).unwrap();
        assert_eq!(tree.root_node().end_byte(), text.len());
        assert!(!tree.root_node().has_error());
    }

    /// Наблюдатель, который держит начало полного разбора документа `gated`, пока тест не отпустит.
    fn gated(gated: u64) -> (Observer, Receiver<Event>, Sender<()>) {
        let (events_tx, events) = mpsc::channel();
        let (release, gate) = mpsc::channel::<()>();
        let gate = Mutex::new(gate);
        let observer: Observer = Arc::new(move |event: &Event| {
            let _ = events_tx.send(event.clone());
            if matches!(event, Event::ParseStarted { document, .. } if *document == gated) {
                let _ = gate.lock().unwrap().recv_timeout(WAIT);
            }
        });
        (observer, events, release)
    }

    fn wait_event(events: &Receiver<Event>, mut wanted: impl FnMut(&Event) -> bool) -> Event {
        loop {
            let event = recv(events);
            if wanted(&event) {
                return event;
            }
        }
    }

    fn spawn(observer: Observer) -> (HighlightWorker, Receiver<Snapshot>) {
        let (tx, snapshots) = mpsc::channel();
        let worker = HighlightWorker::spawn(
            move |snapshot| {
                let _ = tx.send(snapshot);
            },
            Some(observer),
        )
        .unwrap();
        (worker, snapshots)
    }

    fn expected(sparse: &Sparse, visible: &Sides<LineSet>) -> Sides<SideStyles> {
        let mut styler = Styler::new().unwrap();
        visible.map(|side, lines| {
            styler.unranged(
                sparse.trees.get(side),
                sparse.texts().get(side),
                sparse.index.get(side),
                lines,
            )
        })
    }

    #[test]
    fn worker_gives_preliminary_then_one_correction() {
        let sparse = sparse();
        let visible = sparse.last_hunk();
        let (observer, events, release) = gated(1);
        let (worker, snapshots) = spawn(observer);
        let (document, request) = worker.open(sparse.texts(), visible.clone()).unwrap();
        assert_eq!((document, request), (1, 1));
        // Разбор держится в начале, поэтому первый снимок обязан быть предварительным.
        let preliminary = recv(&snapshots);
        assert_eq!(
            (
                preliminary.document,
                preliminary.request,
                preliminary.source
            ),
            (1, 1, Source::Local)
        );
        let want = expected(sparse, &visible);
        assert!(!compare(&want.old, &preliminary.styles.old, &visible.old).matches());
        release.send(()).unwrap();
        release.send(()).unwrap();
        let correction = recv(&snapshots);
        assert_eq!(
            (correction.document, correction.request, correction.source),
            (1, 1, Source::Full)
        );
        assert_eq!(correction.styles, want);
        let mut finished = 0;
        wait_event(&events, |event| {
            finished += usize::from(matches!(event, Event::ParseFinished { document: 1, .. }));
            finished == 2
        });
        // Запрос после исправления отвечается по деревьям, без повторного исправления.
        let top = diff_lines(sparse.body(), 0..40);
        let next = worker.visible(top.clone()).unwrap();
        let answer = recv(&snapshots);
        assert_eq!(
            (answer.document, answer.request, answer.source),
            (1, next, Source::Full)
        );
        assert_eq!(answer.styles, expected(sparse, &top));
        drop(worker);
        assert_eq!(snapshots.try_iter().count(), 0);
    }

    #[test]
    fn replacing_document_cancels_its_parse() {
        let sparse = sparse();
        let (observer, events, release) = gated(1);
        let (worker, snapshots) = spawn(observer);
        worker.open(sparse.texts(), sparse.last_hunk()).unwrap();
        let mut started = 0;
        wait_event(&events, |event| {
            started += usize::from(matches!(event, Event::ParseStarted { document: 1, .. }));
            started == 2
        });
        let small = Sides {
            old: Arc::from(&b"fn old() {}\n"[..]),
            new: Arc::from(SAMPLE.as_bytes()),
        };
        let visible = Sides {
            old: LineSet::from_lines([1]),
            new: LineSet::from_lines(1..=4),
        };
        assert_eq!(worker.open(small.clone(), visible.clone()).unwrap().0, 2);
        release.send(()).unwrap();
        release.send(()).unwrap();
        let mut cancelled = Vec::new();
        wait_event(&events, |event| {
            if let Event::ParseCancelled {
                document: 1, side, ..
            } = event
            {
                cancelled.push(*side);
            }
            assert!(
                !matches!(event, Event::ParseFinished { document: 1, .. }),
                "{event:?}"
            );
            cancelled.len() == 2
        });
        let mut seen_second = false;
        let correction = loop {
            let snapshot = recv(&snapshots);
            if snapshot.document == 2 {
                seen_second = true;
                if snapshot.source == Source::Full {
                    break snapshot;
                }
            } else {
                assert!(!seen_second, "снимок заменённого документа после нового");
                assert_eq!(snapshot.source, Source::Local);
            }
        };
        assert_eq!(correction.styles.new, full_styles(&small.new, &visible.new));
        assert_eq!(correction.styles.old, full_styles(&small.old, &visible.old));
    }

    #[test]
    fn closing_stops_parse_and_deliveries() {
        let sparse = sparse();
        let (observer, events, release) = gated(1);
        let (worker, snapshots) = spawn(observer);
        worker.open(sparse.texts(), sparse.last_hunk()).unwrap();
        let mut started = 0;
        wait_event(&events, |event| {
            started += usize::from(matches!(event, Event::ParseStarted { document: 1, .. }));
            started == 2
        });
        worker.close();
        release.send(()).unwrap();
        release.send(()).unwrap();
        let mut cancelled = 0;
        wait_event(&events, |event| {
            cancelled += usize::from(matches!(event, Event::ParseCancelled { document: 1, .. }));
            cancelled == 2
        });
        worker.visible(sparse.last_hunk()).unwrap();
        drop(worker);
        assert!(
            snapshots
                .try_iter()
                .all(|snapshot| snapshot.source == Source::Local)
        );
    }
}
