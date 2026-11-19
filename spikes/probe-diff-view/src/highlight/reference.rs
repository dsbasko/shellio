//! Эталон подсветки фикстуры: стили полного разбора tree-sitter запросом без диапазона, по
//! строкам, которые открывают сценарии. Считается командой `--reference` вне измеряемого
//! процесса и лежит в `<каталог фикстур>/<имя>/reference.json`.
//!
//! Строки эталона — строки обеих сторон у первых и последних [`EDGE_DIFF_LINES`] строк diff
//! файла. Первый экран окна сценариев и экран у предела прокрутки в обоих режимах вмещают меньше
//! строк diff, поэтому строка экрана без эталона — ошибка исполнения, а не совпадение.

use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::{LineIndex, LineSet, Run, SideStyles, Sides, Styler, TokenClass, language, parse_full};
use crate::fixtures::{self, FixtureName, Fnv};
use crate::model::{Side, TextBody, parse};
use crate::real::{self, RealFixture, RealSource};
use crate::worker::Cancel;

pub const SCHEMA: u32 = 1;
pub const FILE: &str = "reference.json";
/// Строк diff с начала и с конца файла, которые покрывает эталон.
pub const EDGE_DIFF_LINES: usize = 200;
/// Синтетические фикстуры, которые открывают сценарии. Эталон `real` пишется, если `run.sh`
/// подготовил её каталог.
pub const FIXTURES: [FixtureName; 4] = [
    FixtureName::Added,
    FixtureName::Modified,
    FixtureName::Sparse,
    FixtureName::Wrap,
];

/// Строки сторон у строк diff с индексами из `range`: у контекстной строки обе стороны.
pub fn diff_lines(body: &TextBody, range: Range<usize>) -> Sides<LineSet> {
    let lines = body.lines().get(range).unwrap_or_default();
    let side = |side: Side| LineSet::from_lines(lines.iter().filter_map(|line| line.line(side)));
    Sides {
        old: side(Side::Old),
        new: side(Side::New),
    }
}

/// Строки сторон у первых и последних `count` строк diff.
pub fn edge_lines(body: &TextBody, count: usize) -> Sides<LineSet> {
    let total = body.lines().len();
    let head = diff_lines(body, 0..count.min(total));
    let tail = diff_lines(body, total.saturating_sub(count)..total);
    head.map(|side, lines| {
        LineSet::from_ranges(
            lines
                .ranges()
                .iter()
                .chain(tail.get(side).ranges())
                .cloned(),
        )
    })
}

/// Отпечаток грамматики, запроса и правил раскраски: эталон другого отпечатка устарел.
pub fn grammar_fingerprint() -> String {
    /// Растёт при изменении правил раскраски в `Styler::emit`.
    const STYLE_RULES: u32 = 1;
    let language = language();
    let mut hash = Fnv::new();
    hash.write(tree_sitter_rust::NODE_TYPES.as_bytes());
    hash.write(tree_sitter_rust::HIGHLIGHTS_QUERY.as_bytes());
    format!(
        "abi {}, node types and query fnv {:016x}, rules {STYLE_RULES}",
        language.abi_version(),
        hash.finish()
    )
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineRuns {
    pub line: u32,
    /// `[начало, конец, класс]` в байтах от начала строки.
    pub runs: Vec<(u32, u32, TokenClass)>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileReference {
    pub path: String,
    pub old: Vec<LineRuns>,
    pub new: Vec<LineRuns>,
}

impl FileReference {
    pub fn styles(&self, side: Side) -> Result<SideStyles, String> {
        let lines = match side {
            Side::Old => &self.old,
            Side::New => &self.new,
        };
        let runs: Vec<(u32, Vec<Run>)> = lines
            .iter()
            .map(|line| {
                let runs = line
                    .runs
                    .iter()
                    .map(|&(start, end, class)| Run { start, end, class })
                    .collect();
                (line.line, runs)
            })
            .collect();
        SideStyles::from_lines(runs.iter().map(|(line, runs)| (*line, runs.as_slice())))
            .map_err(|err| format!("эталон {}: {err}", self.path))
    }

    pub fn sides(&self) -> Result<Sides<SideStyles>, String> {
        Ok(Sides {
            old: self.styles(Side::Old)?,
            new: self.styles(Side::New)?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reference {
    pub schema: u32,
    pub fixture: String,
    /// `Fixture::digest` шестнадцатеричной строкой.
    pub digest: String,
    pub grammar: String,
    pub files: Vec<FileReference>,
}

impl Reference {
    /// Эталон той же фикстуры, грамматики и схемы.
    pub fn check(&self, fixture: &str, digest: u64) -> Result<(), String> {
        let want = digest_string(digest);
        let grammar = grammar_fingerprint();
        if self.schema != SCHEMA {
            return Err(format!("схема эталона {} вместо {SCHEMA}", self.schema));
        }
        if self.fixture != fixture {
            return Err(format!("эталон фикстуры {} вместо {fixture}", self.fixture));
        }
        if self.digest != want {
            return Err(format!(
                "эталон устарел: отпечаток {} вместо {want}",
                self.digest
            ));
        }
        if self.grammar != grammar {
            return Err(format!(
                "эталон устарел: грамматика «{}» вместо «{grammar}»",
                self.grammar
            ));
        }
        Ok(())
    }

    pub fn file(&self, path: &str) -> Option<&FileReference> {
        self.files.iter().find(|file| file.path == path)
    }
}

pub fn digest_string(digest: u64) -> String {
    format!("{digest:016x}")
}

fn side_lines(styles: &SideStyles) -> Vec<LineRuns> {
    styles
        .lines()
        .map(|(line, runs)| LineRuns {
            line,
            runs: runs
                .iter()
                .map(|run| (run.start, run.end, run.class))
                .collect(),
        })
        .collect()
}

/// Эталон файла: полный разбор каждой стороны и запрос без диапазона.
pub fn build_file(
    styler: &mut Styler,
    path: &str,
    texts: &Sides<Arc<[u8]>>,
    lines: &Sides<LineSet>,
) -> Result<FileReference, String> {
    let mut styles = Sides::<SideStyles>::default();
    for side in Side::BOTH {
        let text = texts.get(side);
        let tree = parse_full(text, &|| false)
            .map_err(|stop| format!("{path}: полный разбор не дал дерева: {stop:?}"))?;
        let index = LineIndex::new(text);
        *styles.get_mut(side) = styler.unranged(&tree, text, &index, lines.get(side));
    }
    Ok(FileReference {
        path: path.to_owned(),
        old: side_lines(&styles.old),
        new: side_lines(&styles.new),
    })
}

/// Эталон синтетической фикстуры.
pub fn synthetic(styler: &mut Styler, name: FixtureName) -> Result<Reference, String> {
    let fixture = fixtures::generate(name);
    let diff = parse::parse(fixture.diff.clone(), &Cancel::never())
        .map_err(|err| format!("{}: diff не разобран: {err:?}", name.as_str()))?;
    let body = diff
        .files()
        .first()
        .and_then(|file| file.text())
        .ok_or_else(|| format!("{}: нет текстового файла", name.as_str()))?;
    let texts = Sides {
        old: Arc::clone(&fixture.old),
        new: Arc::clone(&fixture.new),
    };
    let file = build_file(
        styler,
        &fixture.path,
        &texts,
        &edge_lines(body, EDGE_DIFF_LINES),
    )?;
    Ok(Reference {
        schema: SCHEMA,
        fixture: name.as_str().to_owned(),
        digest: digest_string(fixture.digest()),
        grammar: grammar_fingerprint(),
        files: vec![file],
    })
}

/// Эталон фикстуры `real`: каждый текстовый файл diff по строкам у его первых и последних
/// [`EDGE_DIFF_LINES`] строк diff. Файл diff без пары сторон в манифесте — ошибка.
pub fn real(styler: &mut Styler, fixture: &RealFixture) -> Result<Reference, String> {
    let diff = parse::parse(fixture.diff.clone(), &Cancel::never())
        .map_err(|err| format!("real: diff не разобран: {err:?}"))?;
    let mut files = Vec::new();
    for file in diff.files() {
        let Some(body) = file.text() else {
            continue;
        };
        let pair = fixture
            .file(file.path())
            .ok_or_else(|| format!("real: в манифесте нет файла {}", file.path()))?;
        let texts = Sides {
            old: Arc::clone(&pair.old),
            new: Arc::clone(&pair.new),
        };
        files.push(build_file(
            styler,
            file.path(),
            &texts,
            &edge_lines(body, EDGE_DIFF_LINES),
        )?);
    }
    if files.is_empty() {
        return Err("real: нет текстовых файлов".to_owned());
    }
    Ok(Reference {
        schema: SCHEMA,
        fixture: real::NAME.to_owned(),
        digest: digest_string(fixture.digest()),
        grammar: grammar_fingerprint(),
        files,
    })
}

pub fn path(dir: &Path, fixture: &str) -> PathBuf {
    dir.join(fixture).join(FILE)
}

/// Пишет через временный файл и переименование: оборванная запись не оставит полупустой JSON.
pub fn write(dir: &Path, reference: &Reference) -> Result<PathBuf, String> {
    let target = path(dir, &reference.fixture);
    let parent = target
        .parent()
        .ok_or_else(|| format!("{}: нет каталога", target.display()))?;
    fs::create_dir_all(parent).map_err(|err| format!("каталог {}: {err}", parent.display()))?;
    let mut bytes =
        serde_json::to_vec(reference).map_err(|err| format!("сериализация эталона: {err}"))?;
    bytes.push(b'\n');
    let partial = parent.join(format!(".{FILE}.partial"));
    fs::write(&partial, &bytes).map_err(|err| format!("запись {}: {err}", partial.display()))?;
    fs::rename(&partial, &target)
        .map_err(|err| format!("переименование в {}: {err}", target.display()))?;
    Ok(target)
}

pub fn read(dir: &Path, fixture: &str) -> Result<Reference, String> {
    let path = path(dir, fixture);
    let bytes = fs::read(&path).map_err(|err| format!("эталон {}: {err}", path.display()))?;
    serde_json::from_slice(&bytes)
        .map_err(|err| format!("эталон {} повреждён: {err}", path.display()))
}

/// Эталоны всех фикстур [`FIXTURES`] в `dir` и фикстуры `real`, если её каталог подготовлен.
pub fn write_all(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut styler = Styler::new()?;
    let mut written = FIXTURES
        .into_iter()
        .map(|name| write(dir, &synthetic(&mut styler, name)?))
        .collect::<Result<Vec<_>, String>>()?;
    if let Some(RealSource::Prepared(fixture)) = real::load(&real::dir(dir))? {
        written.push(write(dir, &real(&mut styler, &fixture)?)?);
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_DIFF: &str = "\
diff --git a/src/a.rs b/src/a.rs
index 1111111..2222222 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,4 +1,4 @@
 fn a() {
-    let x = 1;
+    let x = 2;
 }
 // end
";
    const SAMPLE_OLD: &str = "fn a() {\n    let x = 1;\n}\n// end\n";
    const SAMPLE_NEW: &str = "fn a() {\n    let x = 2;\n}\n// end\n";

    fn sample_body() -> crate::model::Diff {
        parse::parse(Arc::from(SAMPLE_DIFF.as_bytes()), &Cancel::never()).unwrap()
    }

    fn texts() -> Sides<Arc<[u8]>> {
        Sides {
            old: Arc::from(SAMPLE_OLD.as_bytes()),
            new: Arc::from(SAMPLE_NEW.as_bytes()),
        }
    }

    #[test]
    fn diff_and_edge_lines_take_both_sides_of_context() {
        let diff = sample_body();
        let body = diff.files()[0].text().unwrap();
        let first = diff_lines(body, 0..2);
        assert_eq!(first.old, LineSet::from_lines([1, 2]));
        assert_eq!(first.new, LineSet::from_lines([1]));
        let edges = edge_lines(body, 1);
        assert_eq!(edges.old, LineSet::from_lines([1, 4]));
        assert_eq!(edges.new, LineSet::from_lines([1, 4]));
        let all = edge_lines(body, 100);
        assert_eq!(all.old, LineSet::from_lines(1..=4));
        assert_eq!(all.new, LineSet::from_lines(1..=4));
    }

    #[test]
    fn reference_round_trips_through_json() {
        let diff = sample_body();
        let body = diff.files()[0].text().unwrap();
        let mut styler = Styler::new().unwrap();
        let file = build_file(&mut styler, "src/a.rs", &texts(), &edge_lines(body, 100)).unwrap();
        assert_eq!(file.old.len(), 4);
        assert_eq!(file.old[3].runs, vec![(0, 6, TokenClass::Comment)]);
        let reference = Reference {
            schema: SCHEMA,
            fixture: "sample".to_owned(),
            digest: digest_string(7),
            grammar: grammar_fingerprint(),
            files: vec![file],
        };
        let dir = std::env::temp_dir().join(format!("probe-diff-reference-{}", std::process::id()));
        let written = write(&dir, &reference).unwrap();
        assert_eq!(written, dir.join("sample").join(FILE));
        let back = read(&dir, "sample").unwrap();
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(back, reference);
        assert_eq!(back.check("sample", 7), Ok(()));
        let styles = back.file("src/a.rs").unwrap().sides().unwrap();
        assert_eq!(
            styles.new.line(2).unwrap().first().unwrap().class,
            TokenClass::Keyword
        );
    }

    #[test]
    fn stale_reference_is_rejected() {
        let reference = Reference {
            schema: SCHEMA,
            fixture: "sample".to_owned(),
            digest: digest_string(7),
            grammar: grammar_fingerprint(),
            files: Vec::new(),
        };
        assert!(
            reference
                .check("sample", 8)
                .unwrap_err()
                .contains("устарел")
        );
        assert!(reference.check("other", 7).is_err());
        let old_grammar = Reference {
            grammar: "abi 0".to_owned(),
            ..reference.clone()
        };
        assert!(
            old_grammar
                .check("sample", 7)
                .unwrap_err()
                .contains("грамматика")
        );
        let old_schema = Reference {
            schema: SCHEMA + 1,
            ..reference
        };
        assert!(old_schema.check("sample", 7).is_err());
    }

    #[test]
    fn reference_lines_must_increase() {
        let file = FileReference {
            path: "x".to_owned(),
            old: vec![
                LineRuns {
                    line: 2,
                    runs: Vec::new(),
                },
                LineRuns {
                    line: 2,
                    runs: Vec::new(),
                },
            ],
            new: Vec::new(),
        };
        assert!(file.styles(Side::Old).is_err());
        assert_eq!(file.styles(Side::New), Ok(SideStyles::default()));
    }
}
