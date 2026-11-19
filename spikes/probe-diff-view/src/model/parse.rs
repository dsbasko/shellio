//! Разбор unified diff в формате `git diff`: заголовки файлов, расширенные заголовки git, ханки
//! и `\ No newline at end of file`. Повреждённый ввод даёт [`ParseError`] с номером строки, а не
//! панику.
//!
//! Тело ханка читается по счётчикам заголовка, а не по префиксу строки: удалённая строка
//! `-- x` выглядит как `--- x`, и только счётчики отличают её от заголовка следующего файла.

use std::fmt;
use std::num::NonZeroU32;
use std::sync::Arc;

use super::rows::{self, Cancelled};
use super::{Diff, DiffLine, FileBody, FileChange, FileDiff, Hunk, LineKind, TextBody};
use crate::worker::Cancel;

/// Отмена проверяется раз в столько строк текста diff.
const CANCEL_CHECK_LINES: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseErrorKind {
    /// Текст diff больше 4 ГиБ: смещения строк хранятся в `u32`.
    TooLarge,
    /// Строка вне файла: ожидался `diff --git`.
    ExpectedFileHeader,
    /// По заголовкам файла не определить ни одного пути.
    MissingPath,
    BadPath,
    BadMode,
    BadSimilarity,
    UnknownHeader,
    /// За `---` нет `+++`.
    MissingNewFileLine,
    /// После `---` и `+++` нет ни одного ханка.
    MissingHunk,
    BadHunkHeader,
    /// Ханк начинается раньше конца предыдущего.
    HunkOutOfOrder,
    /// Текст кончился раньше, чем строки ханка по счётчикам.
    HunkTruncated,
    /// Строка не помещается в счётчики ханка или не начинается с ` `, `-`, `+`, `\`.
    UnexpectedHunkLine,
    /// `\ No newline at end of file` не после строки ханка.
    StrayNoNewline,
    /// Строка после ханка не начинает ни ханк, ни файл.
    UnexpectedLine,
}

impl fmt::Display for ParseErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            ParseErrorKind::TooLarge => "текст diff больше 4 ГиБ",
            ParseErrorKind::ExpectedFileHeader => "ожидался заголовок файла diff --git",
            ParseErrorKind::MissingPath => "по заголовкам файла не определить путь",
            ParseErrorKind::BadPath => "повреждённый путь в кавычках",
            ParseErrorKind::BadMode => "повреждённый режим файла",
            ParseErrorKind::BadSimilarity => "повреждённый индекс сходства",
            ParseErrorKind::UnknownHeader => "неизвестная строка заголовка файла",
            ParseErrorKind::MissingNewFileLine => "за строкой --- нет строки +++",
            ParseErrorKind::MissingHunk => "после --- и +++ нет ханков",
            ParseErrorKind::BadHunkHeader => "повреждённый заголовок ханка",
            ParseErrorKind::HunkOutOfOrder => "ханк начинается раньше конца предыдущего",
            ParseErrorKind::HunkTruncated => "текст кончился внутри ханка",
            ParseErrorKind::UnexpectedHunkLine => "строка не помещается в счётчики ханка",
            ParseErrorKind::StrayNoNewline => "маркер «No newline» не после строки ханка",
            ParseErrorKind::UnexpectedLine => "строка вне ханка и вне заголовка файла",
        };
        f.write_str(text)
    }
}

/// Ошибка разбора. `line` — номер строки текста diff с 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub kind: ParseErrorKind,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "строка {}: {}", self.line, self.kind)
    }
}

impl std::error::Error for ParseError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    Cancelled,
    Invalid(ParseError),
}

impl From<Cancelled> for Failure {
    fn from(_: Cancelled) -> Self {
        Failure::Cancelled
    }
}

/// Разбирает весь текст и строит строки показа каждого текстового файла.
pub fn parse(source: Arc<[u8]>, cancel: &Cancel) -> Result<Diff, Failure> {
    if u32::try_from(source.len()).is_err() {
        return Err(Failure::Invalid(ParseError {
            line: 1,
            kind: ParseErrorKind::TooLarge,
        }));
    }
    let mut parser = Parser {
        cursor: Cursor {
            src: &source,
            pos: 0,
            line_no: 0,
        },
        cancel,
    };
    let mut files = Vec::new();
    while let Some(line) = parser.cursor.peek() {
        if !line.text.starts_with(b"diff --git ") {
            return Err(parser.error_at(&line, ParseErrorKind::ExpectedFileHeader));
        }
        files.push(parser.file()?);
    }
    Ok(Diff { source, files })
}

#[derive(Clone, Copy)]
struct Line<'a> {
    no: usize,
    start: usize,
    text: &'a [u8],
    next: usize,
}

struct Cursor<'a> {
    src: &'a [u8],
    pos: usize,
    line_no: usize,
}

impl<'a> Cursor<'a> {
    fn peek(&self) -> Option<Line<'a>> {
        let rest = self.src.get(self.pos..)?;
        if rest.is_empty() {
            return None;
        }
        let len = rest.iter().position(|&b| b == b'\n').unwrap_or(rest.len());
        Some(Line {
            no: self.line_no + 1,
            start: self.pos,
            text: rest.get(..len)?,
            next: (self.pos + len + 1).min(self.src.len()),
        })
    }

    fn advance(&mut self, line: &Line<'a>) {
        self.pos = line.next;
        self.line_no = line.no;
    }
}

struct Parser<'a, 'c> {
    cursor: Cursor<'a>,
    cancel: &'c Cancel,
}

/// Пути и состояния, собранные по заголовкам одного файла.
#[derive(Default)]
struct Header {
    git_paths: Option<(String, String)>,
    minus: Option<Option<String>>,
    plus: Option<Option<String>>,
    rename_from: Option<String>,
    rename_to: Option<String>,
    copy_from: Option<String>,
    copy_to: Option<String>,
    similarity: Option<u8>,
    added: bool,
    deleted: bool,
    old_mode: Option<u32>,
    new_mode: Option<u32>,
}

impl<'a> Parser<'a, '_> {
    fn error_at(&self, line: &Line<'_>, kind: ParseErrorKind) -> Failure {
        Failure::Invalid(ParseError {
            line: line.no,
            kind,
        })
    }

    fn error_here(&self, kind: ParseErrorKind) -> Failure {
        Failure::Invalid(ParseError {
            line: self.cursor.line_no + 1,
            kind,
        })
    }

    fn advance(&mut self, line: &Line<'a>) -> Result<(), Failure> {
        self.cursor.advance(line);
        if line.no.is_multiple_of(CANCEL_CHECK_LINES) && self.cancel.is_cancelled() {
            return Err(Failure::Cancelled);
        }
        Ok(())
    }

    fn file(&mut self) -> Result<FileDiff, Failure> {
        let mut header = Header::default();
        let Some(first) = self.cursor.peek() else {
            return Err(self.error_here(ParseErrorKind::ExpectedFileHeader));
        };
        let rest = first.text.get(b"diff --git ".len()..).unwrap_or_default();
        header.git_paths = git_paths(rest).map_err(|kind| self.error_at(&first, kind))?;
        self.advance(&first)?;

        let mut body = FileBody::Empty;
        while let Some(line) = self.cursor.peek() {
            let text = line.text;
            if text.starts_with(b"diff --git ") {
                break;
            }
            let bad = |kind| self.error_at(&line, kind);
            if let Some(mode) = text.strip_prefix(b"old mode ") {
                header.old_mode =
                    Some(parse_mode(mode).ok_or_else(|| bad(ParseErrorKind::BadMode))?);
            } else if let Some(mode) = text.strip_prefix(b"new mode ") {
                header.new_mode =
                    Some(parse_mode(mode).ok_or_else(|| bad(ParseErrorKind::BadMode))?);
            } else if let Some(mode) = text.strip_prefix(b"deleted file mode ") {
                header.deleted = true;
                header.old_mode =
                    Some(parse_mode(mode).ok_or_else(|| bad(ParseErrorKind::BadMode))?);
            } else if let Some(mode) = text.strip_prefix(b"new file mode ") {
                header.added = true;
                header.new_mode =
                    Some(parse_mode(mode).ok_or_else(|| bad(ParseErrorKind::BadMode))?);
            } else if let Some(path) = text.strip_prefix(b"rename from ") {
                header.rename_from = Some(plain_path(path).map_err(bad)?);
            } else if let Some(path) = text.strip_prefix(b"rename to ") {
                header.rename_to = Some(plain_path(path).map_err(bad)?);
            } else if let Some(path) = text.strip_prefix(b"copy from ") {
                header.copy_from = Some(plain_path(path).map_err(bad)?);
            } else if let Some(path) = text.strip_prefix(b"copy to ") {
                header.copy_to = Some(plain_path(path).map_err(bad)?);
            } else if let Some(value) = text.strip_prefix(b"similarity index ") {
                header.similarity =
                    Some(parse_percent(value).ok_or_else(|| bad(ParseErrorKind::BadSimilarity))?);
            } else if let Some(value) = text.strip_prefix(b"dissimilarity index ") {
                parse_percent(value).ok_or_else(|| bad(ParseErrorKind::BadSimilarity))?;
            } else if let Some(value) = text.strip_prefix(b"index ") {
                if let Some(mode) = index_mode(value).map_err(bad)? {
                    header.old_mode.get_or_insert(mode);
                    header.new_mode.get_or_insert(mode);
                }
            } else if text.starts_with(b"Binary files ") {
                self.advance(&line)?;
                body = FileBody::Binary;
                break;
            } else if text == b"GIT binary patch" {
                self.advance(&line)?;
                self.skip_binary_patch()?;
                body = FileBody::Binary;
                break;
            } else if let Some(path) = text.strip_prefix(b"--- ") {
                header.minus = Some(side_path(path, b"a/").map_err(bad)?);
                self.advance(&line)?;
                let plus = self
                    .cursor
                    .peek()
                    .filter(|line| line.text.starts_with(b"+++ "))
                    .ok_or_else(|| self.error_here(ParseErrorKind::MissingNewFileLine))?;
                let path = plus.text.get(4..).unwrap_or_default();
                header.plus =
                    Some(side_path(path, b"b/").map_err(|kind| self.error_at(&plus, kind))?);
                self.advance(&plus)?;
                body = FileBody::Text(self.hunks()?);
                break;
            } else {
                return Err(bad(ParseErrorKind::UnknownHeader));
            }
            self.advance(&line)?;
        }
        finish_file(header, body).map_err(|kind| self.error_here(kind))
    }

    fn skip_binary_patch(&mut self) -> Result<(), Failure> {
        while let Some(line) = self.cursor.peek() {
            if line.text.starts_with(b"diff --git ") {
                break;
            }
            self.advance(&line)?;
        }
        Ok(())
    }

    fn hunks(&mut self) -> Result<TextBody, Failure> {
        let mut lines: Vec<DiffLine> = Vec::new();
        let mut hunks: Vec<Hunk> = Vec::new();
        let mut old_next: u32 = 1;
        let mut new_next: u32 = 1;
        while let Some(line) = self.cursor.peek() {
            if line.text.starts_with(b"diff --git ") {
                break;
            }
            if !line.text.starts_with(b"@@ ") {
                return Err(self.error_at(&line, ParseErrorKind::UnexpectedLine));
            }
            let header = HunkHeader::parse(line.text)
                .ok_or_else(|| self.error_at(&line, ParseErrorKind::BadHunkHeader))?;
            let old_first = header.first_line(header.old_start, header.old_count);
            let new_first = header.first_line(header.new_start, header.new_count);
            let (Some(old_first), Some(new_first)) = (old_first, new_first) else {
                return Err(self.error_at(&line, ParseErrorKind::BadHunkHeader));
            };
            if old_first < old_next || new_first < new_next {
                return Err(self.error_at(&line, ParseErrorKind::HunkOutOfOrder));
            }
            self.advance(&line)?;
            let first_line = lines.len();
            let (old_end, new_end) = self.hunk_body(&mut lines, old_first, new_first, &header)?;
            old_next = old_end;
            new_next = new_end;
            let section_start = line.start + header.section.start;
            hunks.push(Hunk {
                old_start: header.old_start,
                old_count: header.old_count,
                new_start: header.new_start,
                new_count: header.new_count,
                lines: index(first_line)..index(lines.len()),
                section_start: index(section_start),
                section_len: index(header.section.len()),
            });
        }
        if hunks.is_empty() {
            return Err(self.error_here(ParseErrorKind::MissingHunk));
        }
        let rows = rows::build(&lines, &hunks, self.cancel)?;
        Ok(TextBody { lines, hunks, rows })
    }

    /// Строки одного ханка по его счётчикам. Возвращает номера строк после ханка.
    fn hunk_body(
        &mut self,
        lines: &mut Vec<DiffLine>,
        old_first: u32,
        new_first: u32,
        header: &HunkHeader,
    ) -> Result<(u32, u32), Failure> {
        let mut old_next = old_first;
        let mut new_next = new_first;
        let mut old_left = header.old_count;
        let mut new_left = header.new_count;
        let hunk_start = lines.len();
        while old_left > 0 || new_left > 0 {
            let Some(line) = self.cursor.peek() else {
                return Err(self.error_here(ParseErrorKind::HunkTruncated));
            };
            // Пустая строка — контекстная строка, у которой редактор срезал пробел-префикс.
            let (prefix, text_start) = match line.text.first() {
                Some(&b) => (b, line.start + 1),
                None => (b' ', line.start),
            };
            let text_len = line.text.len().saturating_sub(1);
            let kind = match prefix {
                b' ' if old_left > 0 && new_left > 0 => LineKind::Context,
                b'-' if old_left > 0 => LineKind::Removed,
                b'+' if new_left > 0 => LineKind::Added,
                b'\\' if lines.len() > hunk_start => {
                    mark_no_newline(lines);
                    self.advance(&line)?;
                    continue;
                }
                b'\\' => return Err(self.error_at(&line, ParseErrorKind::StrayNoNewline)),
                _ => return Err(self.error_at(&line, ParseErrorKind::UnexpectedHunkLine)),
            };
            let old = (kind != LineKind::Added).then(|| take(&mut old_next, &mut old_left));
            let new = (kind != LineKind::Removed).then(|| take(&mut new_next, &mut new_left));
            lines.push(DiffLine {
                kind,
                no_newline: false,
                old: old.flatten(),
                new: new.flatten(),
                text_start: index(text_start),
                text_len: index(text_len),
            });
            self.advance(&line)?;
        }
        if let Some(line) = self.cursor.peek()
            && line.text.first() == Some(&b'\\')
        {
            if lines.len() == hunk_start {
                return Err(self.error_at(&line, ParseErrorKind::StrayNoNewline));
            }
            mark_no_newline(lines);
            self.advance(&line)?;
        }
        Ok((old_next, new_next))
    }
}

/// Номер текущей строки стороны и сдвиг счётчиков. Номера проверены в заголовке ханка:
/// `start + count` помещается в `u32`, поэтому сдвиг не переполняется.
fn take(next: &mut u32, left: &mut u32) -> Option<NonZeroU32> {
    let line = NonZeroU32::new(*next);
    *next = next.saturating_add(1);
    *left = left.saturating_sub(1);
    line
}

fn mark_no_newline(lines: &mut [DiffLine]) {
    if let Some(last) = lines.last_mut() {
        last.no_newline = true;
    }
}

/// Смещение в тексте, который короче 4 ГиБ: проверено в начале [`parse`].
fn index(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

struct HunkHeader {
    old_start: u32,
    old_count: u32,
    new_start: u32,
    new_count: u32,
    /// Диапазон текста раздела внутри строки заголовка.
    section: std::ops::Range<usize>,
}

impl HunkHeader {
    /// `@@ -l[,s] +l[,s] @@[ раздел]`.
    fn parse(text: &[u8]) -> Option<HunkHeader> {
        let rest = text.strip_prefix(b"@@ -")?;
        let space = rest.iter().position(|&b| b == b' ')?;
        let (old_start, old_count) = parse_range(rest.get(..space)?)?;
        let rest = rest.get(space + 1..)?.strip_prefix(b"+")?;
        let space = rest.iter().position(|&b| b == b' ')?;
        let (new_start, new_count) = parse_range(rest.get(..space)?)?;
        let tail = rest.get(space + 1..)?.strip_prefix(b"@@")?;
        let section_offset = text.len() - tail.len();
        let section = match tail.first() {
            None => section_offset..section_offset,
            Some(b' ') => section_offset + 1..text.len(),
            Some(_) => return None,
        };
        Some(HunkHeader {
            old_start,
            old_count,
            new_start,
            new_count,
            section,
        })
    }

    /// Номер первой строки стороны. При нулевом счётчике `start` — строка перед вставкой.
    /// `None`, если диапазон не помещается в `u32` или ненулевой счётчик начинается с нуля.
    fn first_line(&self, start: u32, count: u32) -> Option<u32> {
        let first = if count == 0 {
            start.checked_add(1)?
        } else {
            start
        };
        if first == 0 {
            return None;
        }
        first.checked_add(count)?;
        Some(first)
    }
}

fn parse_range(text: &[u8]) -> Option<(u32, u32)> {
    match text.iter().position(|&b| b == b',') {
        Some(comma) => Some((
            parse_u32(text.get(..comma)?)?,
            parse_u32(text.get(comma + 1..)?)?,
        )),
        None => Some((parse_u32(text)?, 1)),
    }
}

fn parse_u32(text: &[u8]) -> Option<u32> {
    if text.is_empty() || !text.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(text).ok()?.parse().ok()
}

fn parse_mode(text: &[u8]) -> Option<u32> {
    if text.is_empty() || !text.iter().all(|b| (b'0'..=b'7').contains(b)) {
        return None;
    }
    u32::from_str_radix(std::str::from_utf8(text).ok()?, 8).ok()
}

fn parse_percent(text: &[u8]) -> Option<u8> {
    let value = parse_u32(text.strip_suffix(b"%")?)?;
    u8::try_from(value).ok().filter(|&value| value <= 100)
}

/// Режим в конце `index <old>..<new>[ <mode>]`, если он есть.
fn index_mode(text: &[u8]) -> Result<Option<u32>, ParseErrorKind> {
    let mut parts = text.split(|&b| b == b' ');
    let hashes = parts.next().unwrap_or_default();
    if !hashes.windows(2).any(|pair| pair == b"..") {
        return Err(ParseErrorKind::UnknownHeader);
    }
    let mode = parts
        .next()
        .map(|mode| parse_mode(mode).ok_or(ParseErrorKind::BadMode))
        .transpose()?;
    if parts.next().is_some() {
        return Err(ParseErrorKind::UnknownHeader);
    }
    Ok(mode)
}

/// Пути из `diff --git a/<старый> b/<новый>`. Без кавычек разделить пути с пробелами можно,
/// только если они совпадают; иначе `None`, и путь дают другие заголовки файла.
fn git_paths(text: &[u8]) -> Result<Option<(String, String)>, ParseErrorKind> {
    if text.first() == Some(&b'"') {
        let (old, used) = unquote(text)?;
        let rest = text.get(used..).unwrap_or_default();
        let rest = rest.strip_prefix(b" ").ok_or(ParseErrorKind::BadPath)?;
        let new = if rest.first() == Some(&b'"') {
            let (new, used) = unquote(rest)?;
            if used != rest.len() {
                return Err(ParseErrorKind::BadPath);
            }
            new
        } else {
            rest.to_vec()
        };
        return Ok(Some((lossy(strip(&old, b"a/")), lossy(strip(&new, b"b/")))));
    }
    if let Some(quote) = text.windows(2).position(|pair| pair == b" \"") {
        let old = text.get(..quote).unwrap_or_default();
        let rest = text.get(quote + 1..).unwrap_or_default();
        let (new, used) = unquote(rest)?;
        if used != rest.len() {
            return Err(ParseErrorKind::BadPath);
        }
        return Ok(Some((lossy(strip(old, b"a/")), lossy(strip(&new, b"b/")))));
    }
    if text.len() < 5 || !(text.len() - 5).is_multiple_of(2) {
        return Ok(None);
    }
    let name_len = (text.len() - 5) / 2;
    let (Some(old), Some(sep), Some(new)) = (
        text.get(2..2 + name_len),
        text.get(2 + name_len..5 + name_len),
        text.get(5 + name_len..),
    ) else {
        return Ok(None);
    };
    if text.starts_with(b"a/") && sep == b" b/" && old == new {
        Ok(Some((lossy(old), lossy(new))))
    } else {
        Ok(None)
    }
}

/// Путь стороны в `--- ` или `+++ `: `None` для `/dev/null`. Без кавычек путь кончается перед
/// табуляцией: git ставит её после путей с пробелами.
fn side_path(text: &[u8], prefix: &[u8]) -> Result<Option<String>, ParseErrorKind> {
    if text.first() == Some(&b'"') {
        let (path, used) = unquote(text)?;
        let rest = text.get(used..).unwrap_or_default();
        if !rest.is_empty() && rest.first() != Some(&b'\t') {
            return Err(ParseErrorKind::BadPath);
        }
        return Ok(Some(lossy(strip(&path, prefix))));
    }
    let end = text.iter().position(|&b| b == b'\t').unwrap_or(text.len());
    let path = text.get(..end).unwrap_or_default();
    if path == b"/dev/null" {
        return Ok(None);
    }
    if path.is_empty() {
        return Err(ParseErrorKind::BadPath);
    }
    Ok(Some(lossy(strip(path, prefix))))
}

/// Путь в `rename from`, `rename to`, `copy from`, `copy to`: без префикса стороны.
fn plain_path(text: &[u8]) -> Result<String, ParseErrorKind> {
    if text.first() == Some(&b'"') {
        let (path, used) = unquote(text)?;
        if used != text.len() {
            return Err(ParseErrorKind::BadPath);
        }
        return Ok(lossy(&path));
    }
    if text.is_empty() {
        return Err(ParseErrorKind::BadPath);
    }
    Ok(lossy(text))
}

fn strip<'p>(path: &'p [u8], prefix: &[u8]) -> &'p [u8] {
    path.strip_prefix(prefix).unwrap_or(path)
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Путь в кавычках с экранированием C, как его пишет git (`core.quotePath`). Возвращает байты
/// пути и длину разобранного текста вместе с кавычками.
fn unquote(text: &[u8]) -> Result<(Vec<u8>, usize), ParseErrorKind> {
    let mut out = Vec::new();
    let mut i = 1;
    loop {
        let &b = text.get(i).ok_or(ParseErrorKind::BadPath)?;
        i += 1;
        match b {
            b'"' => return Ok((out, i)),
            b'\\' => {
                let &e = text.get(i).ok_or(ParseErrorKind::BadPath)?;
                i += 1;
                let byte = match e {
                    b'a' => 0x07,
                    b'b' => 0x08,
                    b't' => b'\t',
                    b'n' => b'\n',
                    b'v' => 0x0b,
                    b'f' => 0x0c,
                    b'r' => b'\r',
                    b'"' => b'"',
                    b'\\' => b'\\',
                    b'0'..=b'3' => {
                        let digits = text.get(i - 1..i + 2).ok_or(ParseErrorKind::BadPath)?;
                        if !digits.iter().all(|d| (b'0'..=b'7').contains(d)) {
                            return Err(ParseErrorKind::BadPath);
                        }
                        i += 2;
                        digits.iter().fold(0u8, |acc, d| (acc << 3) | (d - b'0'))
                    }
                    _ => return Err(ParseErrorKind::BadPath),
                };
                out.push(byte);
            }
            _ => out.push(b),
        }
    }
}

fn finish_file(header: Header, body: FileBody) -> Result<FileDiff, ParseErrorKind> {
    let (git_old, git_new) = match header.git_paths {
        Some((old, new)) => (Some(old), Some(new)),
        None => (None, None),
    };
    let old_path = match header.minus {
        Some(path) => path,
        None if header.added => None,
        None => header
            .rename_from
            .clone()
            .or_else(|| header.copy_from.clone())
            .or(git_old),
    };
    let new_path = match header.plus {
        Some(path) => path,
        None if header.deleted => None,
        None => header
            .rename_to
            .clone()
            .or_else(|| header.copy_to.clone())
            .or(git_new),
    };
    if old_path.is_none() && new_path.is_none() {
        return Err(ParseErrorKind::MissingPath);
    }
    let change = if header.rename_from.is_some() || header.rename_to.is_some() {
        FileChange::Renamed {
            similarity: header.similarity,
        }
    } else if header.copy_from.is_some() || header.copy_to.is_some() {
        FileChange::Copied {
            similarity: header.similarity,
        }
    } else if header.added || old_path.is_none() {
        FileChange::Added
    } else if header.deleted || new_path.is_none() {
        FileChange::Deleted
    } else {
        FileChange::Modified
    };
    Ok(FileDiff {
        old_path,
        new_path,
        change,
        old_mode: header.old_mode,
        new_mode: header.new_mode,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{LineAddr, Side};

    fn parse_text(text: &str) -> Result<Diff, Failure> {
        parse(Arc::from(text.as_bytes()), &Cancel::never())
    }

    fn parse_ok(text: &str) -> Diff {
        match parse_text(text) {
            Ok(diff) => diff,
            Err(failure) => panic!("разбор не прошёл: {failure:?}"),
        }
    }

    fn parse_err(text: &str) -> ParseError {
        match parse_text(text) {
            Err(Failure::Invalid(error)) => error,
            other => panic!("ожидалась ошибка разбора, получено {other:?}"),
        }
    }

    fn join(lines: &[&str]) -> String {
        let mut text = lines.join("\n");
        text.push('\n');
        text
    }

    fn texts(diff: &Diff, file: &FileDiff) -> Vec<String> {
        file.text()
            .unwrap()
            .lines()
            .iter()
            .map(|line| String::from_utf8_lossy(diff.line_text(line)).into_owned())
            .collect()
    }

    /// Вывод `git diff -U3 --inter-hunk-context=0 --no-ext-diff --no-textconv --no-color`
    /// настоящего git 2.x между двумя коммитами временного репозитория.
    const GIT_SAMPLE: &str = concat!(
        "diff --git a/blob.bin b/blob.bin\n",
        "index 6772730..8d0cd2a 100644\n",
        "Binary files a/blob.bin and b/blob.bin differ\n",
        "diff --git a/empty.txt b/empty.txt\n",
        "new file mode 100644\n",
        "index 0000000..e69de29\n",
        "diff --git a/gone.txt b/gone.txt\n",
        "deleted file mode 100644\n",
        "index 587be6b..0000000\n",
        "--- a/gone.txt\n",
        "+++ /dev/null\n",
        "@@ -1 +0,0 @@\n",
        "-x\n",
        "diff --git a/keep.txt b/keep.txt\n",
        "index c9e9e05..d96039a 100644\n",
        "--- a/keep.txt\n",
        "+++ b/keep.txt\n",
        "@@ -1,4 +1,4 @@\n",
        "-one\n",
        "+ONE\n",
        " two\n",
        " three\n",
        " four\n",
        "@@ -7,4 +7,4 @@ six\n",
        " seven\n",
        " eight\n",
        " nine\n",
        "-ten\n",
        "+TEN\n",
        "diff --git a/pure.txt b/moved.txt\n",
        "similarity index 100%\n",
        "rename from pure.txt\n",
        "rename to moved.txt\n",
        "diff --git a/old_name.txt b/new_name.txt\n",
        "similarity index 80%\n",
        "rename from old_name.txt\n",
        "rename to new_name.txt\n",
        "index 600d48a..cdafba1 100644\n",
        "--- a/old_name.txt\n",
        "+++ b/new_name.txt\n",
        "@@ -1,5 +1,5 @@\n",
        " alpha\n",
        " beta\n",
        "-gamma\n",
        "+GAMMA\n",
        " delta\n",
        " epsilon\n",
        "diff --git a/script.sh b/script.sh\n",
        "old mode 100644\n",
        "new mode 100755\n",
        "diff --git a/tail.rs b/tail.rs\n",
        "index f328e4d..8afd733 100644\n",
        "--- a/tail.rs\n",
        "+++ b/tail.rs\n",
        "@@ -1 +1 @@\n",
        "-fn main() {}\n",
        "+fn main() { run() }\n",
        "\\ No newline at end of file\n",
        "diff --git a/with space.txt b/with space.txt\n",
        "index 7898192..6178079 100644\n",
        "--- a/with space.txt\t\n",
        "+++ b/with space.txt\t\n",
        "@@ -1 +1 @@\n",
        "-a\n",
        "+b\n",
        "diff --git \"a/\\303\\274n\\303\\257.txt\" \"b/\\303\\274n\\303\\257.txt\"\n",
        "new file mode 100644\n",
        "index 0000000..c600332\n",
        "--- /dev/null\n",
        "+++ \"b/\\303\\274n\\303\\257.txt\"\n",
        "@@ -0,0 +1 @@\n",
        "+é\n",
    );

    /// Тот же git: строка без перевода строки на одной и на обеих сторонах.
    const GIT_NO_NEWLINE: &str = concat!(
        "diff --git a/both.txt b/both.txt\n",
        "index 8d7864f..9dd2e36 100644\n",
        "--- a/both.txt\n",
        "+++ b/both.txt\n",
        "@@ -1,2 +1,2 @@\n",
        " p\n",
        "-q\n",
        "\\ No newline at end of file\n",
        "+r\n",
        "\\ No newline at end of file\n",
        "diff --git a/nonl.txt b/nonl.txt\n",
        "index 0a207c0..0f7bc76 100644\n",
        "--- a/nonl.txt\n",
        "+++ b/nonl.txt\n",
        "@@ -1,2 +1,2 @@\n",
        " a\n",
        "-b\n",
        "\\ No newline at end of file\n",
        "+c\n",
    );

    #[test]
    fn git_sample_gives_explicit_file_states() {
        let diff = parse_ok(GIT_SAMPLE);
        let files = diff.files();
        let paths: Vec<_> = files.iter().map(FileDiff::path).collect();
        assert_eq!(
            paths,
            [
                "blob.bin",
                "empty.txt",
                "gone.txt",
                "keep.txt",
                "moved.txt",
                "new_name.txt",
                "script.sh",
                "tail.rs",
                "with space.txt",
                "ünï.txt",
            ]
        );

        assert_eq!(files[0].body, FileBody::Binary);
        assert_eq!(files[0].change, FileChange::Modified);
        assert_eq!(files[0].old_mode, Some(0o100644));

        assert_eq!(files[1].change, FileChange::Added);
        assert_eq!(files[1].body, FileBody::Empty);
        assert_eq!(files[1].old_path, None);
        assert_eq!(files[1].new_mode, Some(0o100644));

        assert_eq!(files[2].change, FileChange::Deleted);
        assert_eq!(files[2].new_path, None);
        assert_eq!(texts(&diff, &files[2]), ["x"]);

        let keep = files[3].text().unwrap();
        assert_eq!(keep.hunks().len(), 2);
        assert_eq!(diff.section(&keep.hunks()[0]), b"");
        assert_eq!(diff.section(&keep.hunks()[1]), b"six");
        assert_eq!(keep.hunks()[1].lines, 5..10);

        assert_eq!(
            files[4].change,
            FileChange::Renamed {
                similarity: Some(100)
            }
        );
        assert_eq!(files[4].old_path.as_deref(), Some("pure.txt"));
        assert_eq!(files[4].body, FileBody::Empty);

        assert_eq!(
            files[5].change,
            FileChange::Renamed {
                similarity: Some(80)
            }
        );
        assert_eq!(files[5].old_path.as_deref(), Some("old_name.txt"));
        assert_eq!(texts(&diff, &files[5])[2], "gamma");

        assert_eq!(files[6].change, FileChange::Modified);
        assert_eq!(files[6].body, FileBody::Empty);
        assert_eq!(
            (files[6].old_mode, files[6].new_mode),
            (Some(0o100644), Some(0o100755))
        );

        let tail = files[7].text().unwrap();
        let flags: Vec<_> = tail.lines().iter().map(DiffLine::no_newline).collect();
        assert_eq!(flags, [false, true]);

        assert_eq!(files[8].old_path.as_deref(), Some("with space.txt"));
        assert_eq!(texts(&diff, &files[8]), ["a", "b"]);

        assert_eq!(files[9].change, FileChange::Added);
        assert_eq!(files[9].old_path, None);
        assert_eq!(texts(&diff, &files[9]), ["é"]);
    }

    #[test]
    fn regular_hunks_number_both_sides() {
        let diff = parse_ok(GIT_SAMPLE);
        let keep = diff.files()[3].text().unwrap();
        let numbers: Vec<_> = keep
            .lines()
            .iter()
            .map(|line| (line.kind(), line.line(Side::Old), line.line(Side::New)))
            .collect();
        use LineKind::{Added, Context, Removed};
        assert_eq!(
            numbers,
            [
                (Removed, Some(1), None),
                (Added, None, Some(1)),
                (Context, Some(2), Some(2)),
                (Context, Some(3), Some(3)),
                (Context, Some(4), Some(4)),
                (Context, Some(7), Some(7)),
                (Context, Some(8), Some(8)),
                (Context, Some(9), Some(9)),
                (Removed, Some(10), None),
                (Added, None, Some(10)),
            ]
        );
    }

    #[test]
    fn empty_input_has_no_files() {
        assert!(parse_ok("").files().is_empty());
    }

    #[test]
    fn no_newline_marks_line_on_its_side() {
        let diff = parse_ok(GIT_NO_NEWLINE);
        let both = diff.files()[0].text().unwrap();
        let flags: Vec<_> = both
            .lines()
            .iter()
            .map(|line| (line.kind(), line.no_newline()))
            .collect();
        assert_eq!(
            flags,
            [
                (LineKind::Context, false),
                (LineKind::Removed, true),
                (LineKind::Added, true),
            ]
        );
        let nonl = diff.files()[1].text().unwrap();
        let flags: Vec<_> = nonl.lines().iter().map(DiffLine::no_newline).collect();
        assert_eq!(flags, [false, true, false]);
    }

    #[test]
    fn last_line_without_trailing_newline_in_diff_text() {
        let text = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n+b";
        let diff = parse_ok(text);
        assert_eq!(texts(&diff, &diff.files()[0]), ["a", "b"]);
    }

    #[test]
    fn hunks_on_first_and_last_lines() {
        let text = join(&[
            "diff --git a/f b/f",
            "--- a/f",
            "+++ b/f",
            "@@ -1,2 +1,3 @@",
            "+zero",
            " one",
            " two",
            "@@ -9,2 +10,1 @@ fn tail()",
            " nine",
            "-ten",
            "\\ No newline at end of file",
        ]);
        let diff = parse_ok(&text);
        let body = diff.files()[0].text().unwrap();
        let first = body.lines()[0];
        assert_eq!(first.addr(Side::New), Some(LineAddr::new(1)));
        assert_eq!(first.addr(Side::Old), None);
        let last = body.lines()[body.lines().len() - 1];
        assert_eq!(last.addr(Side::Old), Some(LineAddr::old(10)));
        assert!(last.no_newline());
        assert_eq!(body.hunks()[1].new_start, 10);
        assert_eq!(diff.section(&body.hunks()[1]), b"fn tail()");
    }

    #[test]
    fn line_text_may_look_like_file_header() {
        let text = join(&[
            "diff --git a/f b/f",
            "--- a/f",
            "+++ b/f",
            "@@ -1,2 +1,2 @@",
            "--- not a header",
            "+++ not a header",
            " x",
        ]);
        let diff = parse_ok(&text);
        assert_eq!(
            texts(&diff, &diff.files()[0]),
            ["-- not a header", "++ not a header", "x"]
        );
    }

    #[test]
    fn empty_line_inside_hunk_is_empty_context() {
        let text = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n\n-a\n+b\n";
        let diff = parse_ok(text);
        let body = diff.files()[0].text().unwrap();
        assert_eq!(body.lines()[0].kind(), LineKind::Context);
        assert_eq!(texts(&diff, &diff.files()[0]), ["", "a", "b"]);
    }

    #[test]
    fn corrupted_hunk_header_is_error() {
        for header in [
            "@@ -1,2 +1,2",
            "@@ -x +1 @@",
            "@@ -1,2 +1,99999999999 @@",
            "@@ -4294967295,2 +1,2 @@",
            "@@@ -1 -1 +1 @@@",
            "@@ -1,2 +1,2 @@x",
            "@@ -0,1 +1 @@",
            "@@ -1, +1 @@",
            "@@  -1 +1 @@",
        ] {
            let text = join(&["diff --git a/f b/f", "--- a/f", "+++ b/f", header, " a"]);
            let error = parse_err(&text);
            assert_eq!(error.line, 4, "{header}");
            assert!(
                matches!(
                    error.kind,
                    ParseErrorKind::BadHunkHeader | ParseErrorKind::UnexpectedLine
                ),
                "{header}: {error:?}"
            );
        }
    }

    #[test]
    fn hunk_body_must_match_counts() {
        let truncated = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n a\n b\n";
        assert_eq!(
            parse_err(truncated),
            ParseError {
                line: 7,
                kind: ParseErrorKind::HunkTruncated
            }
        );
        let overflow = join(&[
            "diff --git a/f b/f",
            "--- a/f",
            "+++ b/f",
            "@@ -1,1 +1,2 @@",
            "-a",
            "-b",
        ]);
        assert_eq!(
            parse_err(&overflow),
            ParseError {
                line: 6,
                kind: ParseErrorKind::UnexpectedHunkLine
            }
        );
        let extra = join(&[
            "diff --git a/f b/f",
            "--- a/f",
            "+++ b/f",
            "@@ -1 +1 @@",
            " a",
            " b",
        ]);
        assert_eq!(
            parse_err(&extra),
            ParseError {
                line: 6,
                kind: ParseErrorKind::UnexpectedLine
            }
        );
    }

    #[test]
    fn structural_errors_have_lines() {
        let cases: [(&str, usize, ParseErrorKind); 11] = [
            ("garbage\n", 1, ParseErrorKind::ExpectedFileHeader),
            (
                "diff --git a/f b/f\n--- a/f\n@@ -1 +1 @@\n",
                3,
                ParseErrorKind::MissingNewFileLine,
            ),
            (
                "diff --git a/f b/f\n--- a/f\n+++ b/f\n",
                4,
                ParseErrorKind::MissingHunk,
            ),
            (
                "diff --git a/f b/f\nweird header\n",
                2,
                ParseErrorKind::UnknownHeader,
            ),
            (
                "diff --git a/f b/f\nold mode 10x644\n",
                2,
                ParseErrorKind::BadMode,
            ),
            (
                "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n\\ No newline at end of file\n",
                5,
                ParseErrorKind::StrayNoNewline,
            ),
            (
                "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -5 +5 @@\n a\n@@ -3 +3 @@\n b\n",
                6,
                ParseErrorKind::HunkOutOfOrder,
            ),
            ("diff --git a/x y b/x z\n", 2, ParseErrorKind::MissingPath),
            (
                "diff --git a/f b/f\nsimilarity index 101%\n",
                2,
                ParseErrorKind::BadSimilarity,
            ),
            (
                "diff --git a/f b/f\nindex abc\n",
                2,
                ParseErrorKind::UnknownHeader,
            ),
            (
                "diff --git a/f b/f\nindex abc..def 10x644\n",
                2,
                ParseErrorKind::BadMode,
            ),
        ];
        for (text, line, kind) in cases {
            assert_eq!(parse_err(text), ParseError { line, kind }, "{text:?}");
        }
    }

    #[test]
    fn every_prefix_and_byte_change_parses_or_errors() {
        let sample = format!("{GIT_SAMPLE}{GIT_NO_NEWLINE}");
        let bytes = sample.as_bytes();
        for end in 0..=bytes.len() {
            let _ = parse(Arc::from(&bytes[..end]), &Cancel::never());
        }
        for at in 0..bytes.len() {
            for replacement in [b'@', b'\\', b'-', b'+', b' ', b'\n', b'"', b'0', 0xff] {
                let mut changed = bytes.to_vec();
                changed[at] = replacement;
                let _ = parse(Arc::from(changed), &Cancel::never());
            }
        }
    }

    #[test]
    fn git_paths_split_equal_names_with_spaces() {
        assert_eq!(
            git_paths(b"a/x y/z b/x y/z"),
            Ok(Some(("x y/z".to_owned(), "x y/z".to_owned())))
        );
        assert_eq!(git_paths(b"a/one b/two"), Ok(None));
        assert_eq!(
            git_paths(b"a/plain \"b/t\\tab\""),
            Ok(Some(("plain".to_owned(), "t\tab".to_owned())))
        );
    }

    #[test]
    fn unquote_handles_escapes_and_rejects_damage() {
        assert_eq!(
            unquote(b"\"a\\\"b\\\\c\\303\\274\" tail"),
            Ok((b"a\"b\\c\xc3\xbc".to_vec(), 17))
        );
        for bad in [&b"\"open"[..], b"\"bad\\q\"", b"\"oct\\39\"", b"\"cut\\"] {
            assert_eq!(unquote(bad), Err(ParseErrorKind::BadPath), "{bad:?}");
        }
    }

    #[test]
    fn copy_headers_give_copied_state() {
        let text = join(&[
            "diff --git a/src.txt b/dst.txt",
            "similarity index 90%",
            "copy from src.txt",
            "copy to dst.txt",
            "index 1111111..2222222 100644",
            "--- a/src.txt",
            "+++ b/dst.txt",
            "@@ -1 +1 @@",
            "-a",
            "+b",
        ]);
        let diff = parse_ok(&text);
        assert_eq!(
            diff.files()[0].change,
            FileChange::Copied {
                similarity: Some(90)
            }
        );
        assert_eq!(diff.files()[0].old_path.as_deref(), Some("src.txt"));
    }

    #[test]
    fn git_binary_patch_is_binary() {
        let text = join(&[
            "diff --git a/b.bin b/b.bin",
            "index 1111111..2222222 100644",
            "GIT binary patch",
            "literal 4",
            "LcmZQzWMT#Y01f~L",
            "",
            "literal 4",
            "LcmZQzWMT#Y01f~L",
            "",
            "diff --git a/f b/f",
            "old mode 100644",
            "new mode 100755",
        ]);
        let diff = parse_ok(&text);
        assert_eq!(diff.files().len(), 2);
        assert_eq!(diff.files()[0].body, FileBody::Binary);
    }

    fn generated(lines: usize) -> String {
        let mut text =
            format!("diff --git a/g b/g\n--- a/g\n+++ b/g\n@@ -1,{lines} +1,{lines} @@\n");
        for i in 0..lines {
            text.push_str(&format!(" line {i}\n"));
        }
        text
    }

    #[test]
    fn cancelled_parse_stops() {
        let text = generated(3 * CANCEL_CHECK_LINES);
        let result = parse(Arc::from(text.as_bytes()), &Cancel::cancelled());
        assert_eq!(result.err(), Some(Failure::Cancelled));
        let result = parse(Arc::from(text.as_bytes()), &Cancel::never());
        assert_eq!(result.map(|diff| diff.files().len()), Ok(1));
    }
}
