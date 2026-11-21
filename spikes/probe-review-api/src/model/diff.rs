//! Diff файла с позициями строк по разбору GitLab (`lib/gitlab/diff/parser.rb`): у каждой строки
//! два счётчика. Добавленная строка увеличивает только новый, удалённая — только старый,
//! неизменённая — оба. Поэтому у добавленной строки старый счётчик указывает на следующую старую
//! строку, у удалённой новый — на следующую новую, и пара счётчиков однозначна в пределах файла.
//! Оба счётчика входят в `line_code` GitLab; GitHub берёт номер строки своей стороны.

use super::ModelError;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LineKind {
    Added,
    Removed,
    Context,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DiffLine {
    pub kind: LineKind,
    pub old: u32,
    pub new: u32,
}

impl DiffLine {
    /// Номер строки в старом файле, если строка в нём есть.
    pub fn old_line(self) -> Option<u32> {
        (self.kind != LineKind::Added).then_some(self.old)
    }

    /// Номер строки в новом файле, если строка в нём есть.
    pub fn new_line(self) -> Option<u32> {
        (self.kind != LineKind::Removed).then_some(self.new)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HunkLine {
    pub line: DiffLine,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDiff {
    old_path: String,
    new_path: String,
    hunks: Vec<Vec<HunkLine>>,
}

/// Поиск файла в diff ревизии по паре путей.
#[derive(Debug, PartialEq, Eq)]
pub enum FileMatch<'a> {
    Same(&'a FileDiff),
    /// Файл с тем же новым путём есть, старый путь другой.
    PathChanged,
    Missing,
}

impl FileDiff {
    /// Разбирает unified diff одного файла (поле `patch` GitHub, `diff` GitLab). Строки до первого
    /// заголовка блока пропускаются, `\ No newline at end of file` счётчиков не меняет.
    pub fn parse(old_path: &str, new_path: &str, patch: &str) -> Result<FileDiff, ModelError> {
        let mut hunks: Vec<Vec<HunkLine>> = Vec::new();
        let (mut old, mut new) = (0u32, 0u32);
        for (index, raw) in patch.lines().enumerate() {
            let number = index + 1;
            if raw.starts_with("@@") {
                (old, new) = parse_header(raw).ok_or_else(|| ModelError::Patch {
                    line: number,
                    reason: "неверный заголовок блока".to_owned(),
                })?;
                hunks.push(Vec::new());
                continue;
            }
            let Some(hunk) = hunks.last_mut() else {
                continue;
            };
            let (kind, text) = match raw.chars().next() {
                Some('+') => (LineKind::Added, &raw[1..]),
                Some('-') => (LineKind::Removed, &raw[1..]),
                Some(' ') => (LineKind::Context, &raw[1..]),
                None => (LineKind::Context, ""),
                Some('\\') => continue,
                Some(other) => {
                    return Err(ModelError::Patch {
                        line: number,
                        reason: format!("неизвестный префикс строки «{other}»"),
                    });
                }
            };
            hunk.push(HunkLine {
                line: DiffLine { kind, old, new },
                text: text.to_owned(),
            });
            match kind {
                LineKind::Added => new += 1,
                LineKind::Removed => old += 1,
                LineKind::Context => {
                    old += 1;
                    new += 1;
                }
            }
        }
        Ok(FileDiff {
            old_path: old_path.to_owned(),
            new_path: new_path.to_owned(),
            hunks,
        })
    }

    pub fn old_path(&self) -> &str {
        &self.old_path
    }

    pub fn new_path(&self) -> &str {
        &self.new_path
    }

    pub fn hunks(&self) -> &[Vec<HunkLine>] {
        &self.hunks
    }

    /// Строка diff с номером `line` на стороне `old` (иначе новой), если она есть на этой стороне.
    pub fn line_at(&self, old: bool, line: u32) -> Option<DiffLine> {
        self.hunks.iter().flatten().map(|l| l.line).find(|l| {
            if old {
                l.old_line() == Some(line)
            } else {
                l.new_line() == Some(line)
            }
        })
    }

    /// Блок и индекс строки в нём; строка сравнивается видом и обоими счётчиками.
    pub fn locate(&self, line: DiffLine) -> Option<(usize, usize)> {
        self.hunks.iter().enumerate().find_map(|(hunk, lines)| {
            lines
                .iter()
                .position(|l| l.line == line)
                .map(|index| (hunk, index))
        })
    }

    pub fn find<'a>(diffs: &'a [FileDiff], old_path: &str, new_path: &str) -> FileMatch<'a> {
        if let Some(file) = diffs
            .iter()
            .find(|f| f.old_path == old_path && f.new_path == new_path)
        {
            return FileMatch::Same(file);
        }
        if diffs.iter().any(|f| f.new_path == new_path) {
            FileMatch::PathChanged
        } else {
            FileMatch::Missing
        }
    }
}

/// `@@ -a[,b] +c[,d] @@ …` → (a, c). Как в GitLab, `-0,0` нового файла даёт старый счётчик 0.
fn parse_header(raw: &str) -> Option<(u32, u32)> {
    let mut parts = raw.split_whitespace();
    if parts.next() != Some("@@") {
        return None;
    }
    let start = |token: Option<&str>, sign: char| -> Option<u32> {
        let rest = token?.strip_prefix(sign)?;
        rest.split(',').next()?.parse().ok()
    };
    let old = start(parts.next(), '-')?;
    let new = start(parts.next(), '+')?;
    Some((old, new))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATCH: &str = "\
@@ -1,4 +1,5 @@
 one
-two
+TWO
+two and a half
 three
 four
@@ -10,3 +11,2 @@ fn tail()
 ten
-eleven
 twelve
\\ No newline at end of file";

    fn lines(file: &FileDiff) -> Vec<(LineKind, u32, u32)> {
        file.hunks()
            .iter()
            .flatten()
            .map(|l| (l.line.kind, l.line.old, l.line.new))
            .collect()
    }

    #[test]
    fn counters_follow_gitlab_parser() -> Result<(), ModelError> {
        let file = FileDiff::parse("a.txt", "a.txt", PATCH)?;
        use LineKind::*;
        assert_eq!(
            lines(&file),
            vec![
                (Context, 1, 1),
                (Removed, 2, 2),
                (Added, 3, 2),
                (Added, 3, 3),
                (Context, 3, 4),
                (Context, 4, 5),
                (Context, 10, 11),
                (Removed, 11, 12),
                (Context, 12, 12),
            ]
        );
        assert_eq!(file.hunks().len(), 2);
        Ok(())
    }

    #[test]
    fn new_and_deleted_files_start_from_zero() -> Result<(), ModelError> {
        let added = FileDiff::parse("n.txt", "n.txt", "@@ -0,0 +1,2 @@\n+a\n+b\n")?;
        assert_eq!(
            lines(&added),
            vec![(LineKind::Added, 0, 1), (LineKind::Added, 0, 2)]
        );
        let deleted = FileDiff::parse("d.txt", "d.txt", "@@ -1,2 +0,0 @@\n-a\n-b\n")?;
        assert_eq!(
            lines(&deleted),
            vec![(LineKind::Removed, 1, 0), (LineKind::Removed, 2, 0)]
        );
        Ok(())
    }

    #[test]
    fn header_lines_before_first_hunk_are_skipped() -> Result<(), ModelError> {
        let file = FileDiff::parse(
            "a.txt",
            "a.txt",
            "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -3 +3 @@\n-x\n+y\n",
        )?;
        assert_eq!(
            lines(&file),
            vec![(LineKind::Removed, 3, 3), (LineKind::Added, 4, 3)]
        );
        Ok(())
    }

    #[test]
    fn rejects_bad_header_and_prefix() {
        assert!(matches!(
            FileDiff::parse("a", "a", "@@ -x +1 @@\n"),
            Err(ModelError::Patch { line: 1, .. })
        ));
        assert!(matches!(
            FileDiff::parse("a", "a", "@@ -1 +1 @@\n*bad\n"),
            Err(ModelError::Patch { line: 2, .. })
        ));
    }

    #[test]
    fn line_lookup_by_side() -> Result<(), ModelError> {
        let file = FileDiff::parse("a.txt", "a.txt", PATCH)?;
        let removed = file.line_at(true, 2).ok_or(ModelError::NotAnchored)?;
        assert_eq!(removed.kind, LineKind::Removed);
        let added = file.line_at(false, 3).ok_or(ModelError::NotAnchored)?;
        assert_eq!((added.kind, added.old, added.new), (LineKind::Added, 3, 3));
        assert_eq!(file.line_at(false, 99), None);
        assert_eq!(file.locate(added), Some((0, 3)));
        Ok(())
    }

    #[test]
    fn find_distinguishes_renamed_and_missing() -> Result<(), ModelError> {
        let diffs = vec![FileDiff::parse("old.rs", "new.rs", "@@ -1 +1 @@\n x\n")?];
        assert!(matches!(
            FileDiff::find(&diffs, "old.rs", "new.rs"),
            FileMatch::Same(_)
        ));
        assert_eq!(
            FileDiff::find(&diffs, "new.rs", "new.rs"),
            FileMatch::PathChanged
        );
        assert_eq!(FileDiff::find(&diffs, "x.rs", "x.rs"), FileMatch::Missing);
        Ok(())
    }
}
