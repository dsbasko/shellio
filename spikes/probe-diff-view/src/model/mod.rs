//! UI-free модель diff пробы: разбор unified diff, строки с адресами, строки показа двух раскладок
//! и неизменяемый снимок с номером поколения. Модуль не зависит от AppKit.
//!
//! Текст diff хранится один раз как `Arc<[u8]>`, строки и заголовки ссылаются на него диапазонами
//! байт. Адреса строк начинаются с 1, как в git и в якоре ADR-0015, и живут внутри файла.

pub mod parse;
pub mod rows;

use std::io;
use std::num::NonZeroU32;
use std::ops::Range;
use std::sync::Arc;

use crate::worker::{Cancel, Delivery, Stopped, Worker};

pub use parse::{Failure, ParseError, ParseErrorKind};
pub use rows::{Located, Mode, Row};

/// Сторона diff.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Side {
    Old,
    New,
}

impl Side {
    pub const BOTH: [Side; 2] = [Side::Old, Side::New];

    pub fn other(self) -> Side {
        match self {
            Side::Old => Side::New,
            Side::New => Side::Old,
        }
    }
}

/// Адрес строки файла: сторона и номер строки с 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LineAddr {
    pub side: Side,
    pub line: u32,
}

impl LineAddr {
    pub fn old(line: u32) -> Self {
        LineAddr {
            side: Side::Old,
            line,
        }
    }

    pub fn new(line: u32) -> Self {
        LineAddr {
            side: Side::New,
            line,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Removed,
    Added,
}

/// Строка diff. У контекстной строки два адреса, у удалённой только старый, у добавленной
/// только новый. Текст — байты строки без префикса и без перевода строки.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiffLine {
    kind: LineKind,
    no_newline: bool,
    old: Option<NonZeroU32>,
    new: Option<NonZeroU32>,
    text_start: u32,
    text_len: u32,
}

impl DiffLine {
    pub fn kind(&self) -> LineKind {
        self.kind
    }

    /// За строкой стоит `\ No newline at end of file`: на её стороне это последняя строка
    /// файла без перевода строки.
    pub fn no_newline(&self) -> bool {
        self.no_newline
    }

    pub fn line(&self, side: Side) -> Option<u32> {
        match side {
            Side::Old => self.old,
            Side::New => self.new,
        }
        .map(NonZeroU32::get)
    }

    pub fn addr(&self, side: Side) -> Option<LineAddr> {
        self.line(side).map(|line| LineAddr { side, line })
    }

    pub fn has(&self, side: Side) -> bool {
        self.line(side).is_some()
    }

    pub fn text_range(&self) -> Range<usize> {
        let start = self.text_start as usize;
        start..start + self.text_len as usize
    }
}

/// Ханк: диапазоны заголовка `@@ -old_start,old_count +new_start,new_count @@` и строки diff
/// файла `lines`. При нулевом счётчике `start` — номер строки перед вставкой, как в git.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: u32,
    pub old_count: u32,
    pub new_start: u32,
    pub new_count: u32,
    pub lines: Range<u32>,
    section_start: u32,
    section_len: u32,
}

impl Hunk {
    /// Текст после второго `@@`: заголовок функции или раздела, если git его нашёл.
    pub fn section_range(&self) -> Range<usize> {
        let start = self.section_start as usize;
        start..start + self.section_len as usize
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileChange {
    Modified,
    Added,
    Deleted,
    Renamed { similarity: Option<u8> },
    Copied { similarity: Option<u8> },
}

/// Содержимое файла diff. Бинарный файл и файл без ханков (смена режима, переименование без
/// правок, пустой новый файл) — явные состояния, а не пустой список строк.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileBody {
    Text(TextBody),
    Binary,
    Empty,
}

/// Строки и ханки текстового файла вместе со строками показа обеих раскладок.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextBody {
    lines: Vec<DiffLine>,
    hunks: Vec<Hunk>,
    rows: rows::Rows,
}

impl TextBody {
    pub fn lines(&self) -> &[DiffLine] {
        &self.lines
    }

    pub fn hunks(&self) -> &[Hunk] {
        &self.hunks
    }

    pub fn line(&self, index: u32) -> Option<&DiffLine> {
        self.lines.get(index as usize)
    }
}

/// Файл diff. Путь `None` у стороны `/dev/null`: у нового файла нет старого пути, у удалённого
/// нет нового.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDiff {
    pub old_path: Option<String>,
    pub new_path: Option<String>,
    pub change: FileChange,
    pub old_mode: Option<u32>,
    pub new_mode: Option<u32>,
    pub body: FileBody,
}

impl FileDiff {
    /// Путь для показа и якоря треда: новый, а у удалённого файла старый.
    pub fn path(&self) -> &str {
        self.new_path
            .as_deref()
            .or(self.old_path.as_deref())
            .unwrap_or_default()
    }

    pub fn text(&self) -> Option<&TextBody> {
        match &self.body {
            FileBody::Text(text) => Some(text),
            FileBody::Binary | FileBody::Empty => None,
        }
    }
}

/// Разобранный diff из одного или нескольких файлов.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diff {
    source: Arc<[u8]>,
    files: Vec<FileDiff>,
}

impl Diff {
    pub fn source(&self) -> &Arc<[u8]> {
        &self.source
    }

    pub fn files(&self) -> &[FileDiff] {
        &self.files
    }

    pub fn file(&self, index: usize) -> Option<&FileDiff> {
        self.files.get(index)
    }

    pub fn line_text(&self, line: &DiffLine) -> &[u8] {
        self.source.get(line.text_range()).unwrap_or_default()
    }

    pub fn section(&self, hunk: &Hunk) -> &[u8] {
        self.source.get(hunk.section_range()).unwrap_or_default()
    }

    /// Следующий файл после активного. `None` у последнего: переход по кругу не идёт.
    pub fn next_file(&self, active: usize) -> Option<usize> {
        let next = active.checked_add(1)?;
        (next < self.files.len()).then_some(next)
    }

    /// Предыдущий файл. `None` у первого.
    pub fn prev_file(&self, active: usize) -> Option<usize> {
        active
            .checked_sub(1)
            .filter(|&prev| prev < self.files.len())
    }
}

/// Неизменяемый снимок разбора, который worker отдаёт главному потоку.
#[derive(Debug)]
pub struct DiffSnapshot {
    pub generation: u64,
    pub diff: Diff,
}

pub type ParseOutcome = Result<Arc<DiffSnapshot>, ParseError>;

/// Разбор diff на отдельном потоке. Новый текст заменяет прежний и отменяет его разбор.
pub struct ParseWorker {
    worker: Worker<Arc<[u8]>>,
}

impl ParseWorker {
    pub fn spawn(deliver: impl Fn(Delivery<ParseOutcome>) + Send + 'static) -> io::Result<Self> {
        let worker = Worker::spawn("probe-diff-parse", parse_job, deliver)?;
        Ok(ParseWorker { worker })
    }

    pub fn submit(&self, source: Arc<[u8]>) -> Result<u64, Stopped> {
        self.worker.submit(source)
    }

    pub fn cancel(&self) {
        self.worker.cancel();
    }

    pub fn generation(&self) -> u64 {
        self.worker.generation()
    }
}

fn parse_job(source: Arc<[u8]>, cancel: &Cancel) -> Option<ParseOutcome> {
    match parse::parse(source, cancel) {
        Ok(diff) => Some(Ok(Arc::new(DiffSnapshot {
            generation: cancel.generation(),
            diff,
        }))),
        Err(Failure::Invalid(error)) => Some(Err(error)),
        Err(Failure::Cancelled) => None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{self, Receiver};
    use std::time::Duration;

    use super::*;

    const WAIT: Duration = Duration::from_secs(10);

    fn spawn() -> (ParseWorker, Receiver<Delivery<ParseOutcome>>) {
        let (tx, rx) = mpsc::channel();
        let worker = ParseWorker::spawn(move |delivery| {
            let _ = tx.send(delivery);
        })
        .unwrap();
        (worker, rx)
    }

    fn source(text: &str) -> Arc<[u8]> {
        Arc::from(text.as_bytes())
    }

    const ONE_FILE: &str = "diff --git a/f.txt b/f.txt\n\
        index 1111111..2222222 100644\n\
        --- a/f.txt\n\
        +++ b/f.txt\n\
        @@ -1,2 +1,2 @@\n \
        a\n\
        -b\n\
        +c\n";

    #[test]
    fn worker_delivers_snapshot_with_generation() {
        let (worker, rx) = spawn();
        let generation = worker.submit(source(ONE_FILE)).unwrap();
        let delivery = rx.recv_timeout(WAIT).unwrap();
        assert_eq!(delivery.generation, generation);
        let snapshot = delivery.value.unwrap();
        assert_eq!(snapshot.generation, generation);
        assert_eq!(snapshot.diff.files().len(), 1);
        assert_eq!(snapshot.diff.files()[0].path(), "f.txt");
    }

    #[test]
    fn worker_reports_parse_error() {
        let (worker, rx) = spawn();
        worker
            .submit(source("diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1 +1\n"))
            .unwrap();
        let error = rx.recv_timeout(WAIT).unwrap().value.unwrap_err();
        assert_eq!(error.kind, ParseErrorKind::BadHunkHeader);
        assert_eq!(error.line, 4);
    }

    #[test]
    fn last_delivery_belongs_to_replacing_diff() {
        let (worker, rx) = spawn();
        let first = worker.submit(source(ONE_FILE)).unwrap();
        let second = worker.submit(source("")).unwrap();
        loop {
            let delivery = rx.recv_timeout(WAIT).unwrap();
            if delivery.generation == second {
                assert!(delivery.value.unwrap().diff.files().is_empty());
                break;
            }
            assert_eq!(delivery.generation, first);
        }
        assert_eq!(worker.generation(), second);
    }

    #[test]
    fn file_navigation_stops_at_edges() {
        let text = format!("{ONE_FILE}{}", ONE_FILE.replace("f.txt", "g.txt"));
        let diff = parse::parse(source(&text), &Cancel::never()).unwrap();
        assert_eq!(diff.files().len(), 2);
        assert_eq!(diff.next_file(0), Some(1));
        assert_eq!(diff.next_file(1), None);
        assert_eq!(diff.prev_file(1), Some(0));
        assert_eq!(diff.prev_file(0), None);
        assert_eq!(diff.prev_file(7), None);
        assert_eq!(diff.file(1).map(FileDiff::path), Some("g.txt"));
    }
}
