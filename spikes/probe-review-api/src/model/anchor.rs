//! Общий якорь черновика и его перевод в якорь GitHub и позицию GitLab.
//!
//! [`Anchor`] не содержит SHA: пути и строки diff, сторона, необязательное начало диапазона.
//! SHA подставляет только [`super::ReviewSession`] из своей ревизии целиком, поэтому якорь
//! провайдера и позицию GitLab нельзя собрать из свежих refs и ранее полученного `head_sha`.

use serde::Serialize;

use super::ModelError;
use super::diff::{DiffLine, FileDiff, FileMatch, LineKind};
use super::revision::{AnchorFate, Revision, Sha, StaleReason};

/// Сторона diff: старая (GitHub `LEFT`) или новая (GitHub `RIGHT`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    Old,
    New,
}

impl Side {
    pub fn github(self) -> &'static str {
        match self {
            Side::Old => "LEFT",
            Side::New => "RIGHT",
        }
    }
}

/// Строка diff с выбранной стороной. Добавленная строка есть только на новой стороне, удалённая —
/// только на старой, неизменённая — на обеих.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LinePoint {
    side: Side,
    line: DiffLine,
}

impl LinePoint {
    pub fn new(side: Side, line: DiffLine) -> Result<LinePoint, ModelError> {
        let fits = match (line.kind, side) {
            (LineKind::Added, Side::New) | (LineKind::Removed, Side::Old) => true,
            (LineKind::Context, _) => true,
            (LineKind::Added, Side::Old) | (LineKind::Removed, Side::New) => false,
        };
        if fits {
            Ok(LinePoint { side, line })
        } else {
            Err(ModelError::Side {
                side,
                kind: line.kind,
            })
        }
    }

    /// Строка файла с номером `line` на стороне `side`.
    pub fn at(file: &FileDiff, side: Side, line: u32) -> Result<LinePoint, ModelError> {
        let found =
            file.line_at(side == Side::Old, line)
                .ok_or_else(|| ModelError::AnchorMissing {
                    path: file.new_path().to_owned(),
                    side,
                    line,
                })?;
        LinePoint::new(side, found)
    }

    pub fn side(self) -> Side {
        self.side
    }

    pub fn line(self) -> DiffLine {
        self.line
    }

    /// Номер строки на выбранной стороне (GitHub `line` и `start_line`).
    pub fn side_line(self) -> u32 {
        match self.side {
            Side::Old => self.line.old,
            Side::New => self.line.new,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Anchor {
    old_path: String,
    new_path: String,
    start: Option<LinePoint>,
    end: LinePoint,
}

impl Anchor {
    /// Якорь на одну строку данного diff.
    pub fn line(file: &FileDiff, end: LinePoint) -> Result<Anchor, ModelError> {
        Anchor::build(file, None, end)
    }

    /// Диапазон строк одного блока данного diff, начало строго раньше конца.
    pub fn range(file: &FileDiff, start: LinePoint, end: LinePoint) -> Result<Anchor, ModelError> {
        Anchor::build(file, Some(start), end)
    }

    fn build(
        file: &FileDiff,
        start: Option<LinePoint>,
        end: LinePoint,
    ) -> Result<Anchor, ModelError> {
        let anchor = Anchor {
            old_path: file.old_path().to_owned(),
            new_path: file.new_path().to_owned(),
            start,
            end,
        };
        anchor.span(file)?;
        Ok(anchor)
    }

    /// Якорь из журнала. Блоков diff в журнале нет, поэтому проверяется только порядок: счётчики
    /// строк diff не убывают, и у более поздней строки хотя бы один больше.
    pub(crate) fn restore(
        old_path: String,
        new_path: String,
        start: Option<LinePoint>,
        end: LinePoint,
    ) -> Result<Anchor, ModelError> {
        if let Some(start) = start {
            let (a, b) = (start.line, end.line);
            if a.old > b.old || a.new > b.new || (a.old, a.new) == (b.old, b.new) {
                return Err(ModelError::RangeOrder);
            }
        }
        Ok(Anchor {
            old_path,
            new_path,
            start,
            end,
        })
    }

    pub fn old_path(&self) -> &str {
        &self.old_path
    }

    pub fn new_path(&self) -> &str {
        &self.new_path
    }

    pub fn start(&self) -> Option<LinePoint> {
        self.start
    }

    pub fn end(&self) -> LinePoint {
        self.end
    }

    /// Блок и индексы начала и конца якоря в файле.
    fn span(&self, file: &FileDiff) -> Result<(usize, usize, usize), ModelError> {
        let missing = |point: LinePoint| ModelError::AnchorMissing {
            path: self.new_path.clone(),
            side: point.side,
            line: point.side_line(),
        };
        let (hunk, end) = file
            .locate(self.end.line)
            .ok_or_else(|| missing(self.end))?;
        let Some(start) = self.start else {
            return Ok((hunk, end, end));
        };
        let (start_hunk, begin) = file.locate(start.line).ok_or_else(|| missing(start))?;
        if start_hunk != hunk {
            return Err(ModelError::RangeHunk);
        }
        if begin >= end {
            return Err(ModelError::RangeOrder);
        }
        Ok((hunk, begin, end))
    }

    /// Якорь стоит на строках одного из файлов diff.
    pub fn validate(&self, diffs: &[FileDiff]) -> Result<(), ModelError> {
        match FileDiff::find(diffs, &self.old_path, &self.new_path) {
            FileMatch::Same(file) => self.span(file).map(|_| ()),
            FileMatch::PathChanged | FileMatch::Missing => Err(ModelError::AnchorMissing {
                path: self.new_path.clone(),
                side: self.end.side,
                line: self.end.side_line(),
            }),
        }
    }

    /// Судьба якоря при смене ревизии. Якорь сохраняется, только если все его строки от начала до
    /// конца есть в свежем diff с тем же видом, теми же счётчиками и тем же текстом в одном блоке.
    /// Сдвинутая строка — устаревший якорь: номер не пересчитывается.
    pub fn fate(&self, held: &[FileDiff], fresh: &[FileDiff]) -> AnchorFate {
        let FileMatch::Same(held_file) = FileDiff::find(held, &self.old_path, &self.new_path)
        else {
            return AnchorFate::Stale(StaleReason::NotInOwnDiff);
        };
        let Ok((hunk, begin, end)) = self.span(held_file) else {
            return AnchorFate::Stale(StaleReason::NotInOwnDiff);
        };
        let fresh_file = match FileDiff::find(fresh, &self.old_path, &self.new_path) {
            FileMatch::Same(file) => file,
            FileMatch::PathChanged => return AnchorFate::Stale(StaleReason::PathChanged),
            FileMatch::Missing => return AnchorFate::Stale(StaleReason::FileMissing),
        };
        let Ok((fresh_hunk, fresh_begin, fresh_end)) = self.span(fresh_file) else {
            return AnchorFate::Stale(StaleReason::LinesChanged);
        };
        let held_lines = &held_file.hunks()[hunk][begin..=end];
        let fresh_lines = &fresh_file.hunks()[fresh_hunk][fresh_begin..=fresh_end];
        if held_lines == fresh_lines {
            AnchorFate::Kept
        } else {
            AnchorFate::Stale(StaleReason::LinesChanged)
        }
    }

    pub(super) fn github(&self, commit_id: &Sha) -> GitHubAnchor {
        GitHubAnchor {
            commit_id: commit_id.clone(),
            path: self.new_path.clone(),
            line: self.end.side_line(),
            side: self.end.side.github(),
            start_line: self.start.map(LinePoint::side_line),
            start_side: self.start.map(|s| s.side.github()),
        }
    }

    pub(super) fn gitlab(&self, revision: &Revision) -> GitLabPosition {
        GitLabPosition {
            base_sha: revision.base_sha().clone(),
            start_sha: revision.start_sha().clone(),
            head_sha: revision.head_sha().clone(),
            position_type: "text",
            old_path: self.old_path.clone(),
            new_path: self.new_path.clone(),
            old_line: self.end.line.old_line(),
            new_line: self.end.line.new_line(),
            line_range: self.start.map(|start| LineRange {
                start: RangeEnd::new(&self.new_path, start.line),
                end: RangeEnd::new(&self.new_path, self.end.line),
            }),
        }
    }
}

/// Якорь комментария GitHub (REST `pulls/comments`, `comments[]` Create review). Путь — новый:
/// GitHub адресует файл PR его текущим путём.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GitHubAnchor {
    commit_id: Sha,
    path: String,
    line: u32,
    side: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_side: Option<&'static str>,
}

impl GitHubAnchor {
    pub fn commit_id(&self) -> &Sha {
        &self.commit_id
    }
}

/// Позиция GitLab (`position` в Discussions и Draft Notes API). У добавленной строки только
/// `new_line`, у удалённой только `old_line`, у неизменённой оба.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GitLabPosition {
    base_sha: Sha,
    start_sha: Sha,
    head_sha: Sha,
    position_type: &'static str,
    old_path: String,
    new_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    old_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    new_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    line_range: Option<LineRange>,
}

impl GitLabPosition {
    /// Тройка SHA позиции целиком — для сверки исходящего запроса с ревизией сессии.
    pub fn revision(&self) -> Revision {
        Revision::new(
            self.base_sha.clone(),
            self.start_sha.clone(),
            self.head_sha.clone(),
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LineRange {
    start: RangeEnd,
    end: RangeEnd,
}

/// Конец диапазона GitLab: `type` — `new` у добавленной строки, `old` у удалённой, у неизменённой
/// не передаётся; `line_code` — `<SHA-1 пути>_<старый счётчик>_<новый счётчик>`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RangeEnd {
    line_code: String,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    kind: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    old_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    new_line: Option<u32>,
}

impl RangeEnd {
    fn new(path: &str, line: DiffLine) -> RangeEnd {
        RangeEnd {
            line_code: line_code(path, line),
            kind: match line.kind {
                LineKind::Added => Some("new"),
                LineKind::Removed => Some("old"),
                LineKind::Context => None,
            },
            old_line: line.old_line(),
            new_line: line.new_line(),
        }
    }
}

/// `line_code` GitLab (`Gitlab::Git.diff_line_code`): SHA-1 пути файла и оба счётчика строки.
pub fn line_code(path: &str, line: DiffLine) -> String {
    let digest = sha1_smol::Sha1::from(path).digest();
    format!("{digest}_{}_{}", line.old, line.new)
}
