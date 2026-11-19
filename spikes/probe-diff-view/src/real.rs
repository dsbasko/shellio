//! Фикстура `real`: настоящий diff проекта между закреплёнными коммитами. Её готовит `run.sh`
//! через `git` без сети до сценариев, проба git не вызывает и читает только подготовленные файлы.
//!
//! Каталог фикстуры:
//! - `status.txt` — строки `ключ=значение`: `status=prepared` со сведениями о подготовке или
//!   `status=not_run` с `reason=` (в git-копии нет закреплённых коммитов);
//! - `diff.patch` — текст `git diff` с параметрами синтетических фикстур;
//! - `manifest.tsv` — файлы в порядке diff: blob старой стороны, blob новой (`-` — стороны нет)
//!   и путь через табуляцию;
//! - `blobs/<sha>` — содержимое blob.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::fixtures::{Fixture, Fnv};

pub const NAME: &str = "real";
pub const STATUS_FILE: &str = "status.txt";
pub const DIFF_FILE: &str = "diff.patch";
pub const MANIFEST_FILE: &str = "manifest.tsv";
pub const BLOBS_DIR: &str = "blobs";

/// Файл diff и его стороны, у отсутствующей стороны пустые байты.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RealFile {
    pub path: String,
    pub old: Arc<[u8]>,
    pub new: Arc<[u8]>,
}

/// Подготовленная фикстура: сырые байты diff и всех пар файлов.
#[derive(Clone, Debug, PartialEq)]
pub struct RealFixture {
    pub diff: Arc<[u8]>,
    pub files: Vec<RealFile>,
    /// Время чтения из git в `run.sh`, работа P3.1, без вердикта.
    pub git_read_ms: Option<f64>,
    /// Вызовов `git` при подготовке.
    pub git_calls: Option<u32>,
    /// Закреплённые коммиты старой и новой стороны.
    pub commits: (String, String),
}

/// Итог чтения каталога фикстуры.
#[derive(Clone, Debug, PartialEq)]
pub enum RealSource {
    Prepared(RealFixture),
    /// Наблюдение не проведено, причина из `status.txt`.
    NotRun(String),
}

/// Каталог фикстуры `real` в каталоге фикстур.
pub fn dir(fixtures: &Path) -> PathBuf {
    fixtures.join(NAME)
}

/// Значение ключа из строк `ключ=значение`.
fn value<'a>(status: &'a str, key: &str) -> Option<&'a str> {
    status
        .lines()
        .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
}

/// Читает фикстуру. `None` — `run.sh` её не готовил: `status.txt` нет.
pub fn load(dir: &Path) -> Result<Option<RealSource>, String> {
    let status_path = dir.join(STATUS_FILE);
    let status = match fs::read_to_string(&status_path) {
        Ok(status) => status,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("{}: {err}", status_path.display())),
    };
    match value(&status, "status") {
        Some("not_run") => {
            let reason = value(&status, "reason")
                .filter(|reason| !reason.trim().is_empty())
                .ok_or_else(|| format!("{}: not_run без причины", status_path.display()))?;
            return Ok(Some(RealSource::NotRun(reason.to_owned())));
        }
        Some("prepared") => {}
        other => {
            return Err(format!(
                "{}: неизвестное состояние {other:?}",
                status_path.display()
            ));
        }
    }
    let read = |path: PathBuf| -> Result<Arc<[u8]>, String> {
        fs::read(&path)
            .map(Arc::from)
            .map_err(|err| format!("{}: {err}", path.display()))
    };
    let diff = read(dir.join(DIFF_FILE))?;
    let manifest_path = dir.join(MANIFEST_FILE);
    let manifest = fs::read_to_string(&manifest_path)
        .map_err(|err| format!("{}: {err}", manifest_path.display()))?;
    let mut files = Vec::new();
    for (number, line) in manifest.lines().enumerate() {
        let mut fields = line.splitn(3, '\t');
        let (Some(old), Some(new), Some(path)) = (fields.next(), fields.next(), fields.next())
        else {
            return Err(format!(
                "{}: строка {} не из трёх полей",
                manifest_path.display(),
                number + 1
            ));
        };
        let blob = |sha: &str| -> Result<Arc<[u8]>, String> {
            match sha {
                "-" => Ok(Arc::from(&[][..])),
                sha if !sha.is_empty() && sha.bytes().all(|byte| byte.is_ascii_hexdigit()) => {
                    read(dir.join(BLOBS_DIR).join(sha))
                }
                sha => Err(format!(
                    "{}: строка {}: blob «{sha}»",
                    manifest_path.display(),
                    number + 1
                )),
            }
        };
        files.push(RealFile {
            path: path.to_owned(),
            old: blob(old)?,
            new: blob(new)?,
        });
    }
    if files.is_empty() {
        return Err(format!("{}: файлов нет", manifest_path.display()));
    }
    let declared = value(&status, "files").and_then(|files| files.parse::<usize>().ok());
    if declared != Some(files.len()) {
        return Err(format!(
            "{}: files={declared:?}, в манифесте {}",
            status_path.display(),
            files.len()
        ));
    }
    Ok(Some(RealSource::Prepared(RealFixture {
        diff,
        files,
        git_read_ms: value(&status, "git_read_ms").and_then(|ms| ms.parse().ok()),
        git_calls: value(&status, "git_calls").and_then(|calls| calls.parse().ok()),
        commits: (
            value(&status, "old").unwrap_or_default().to_owned(),
            value(&status, "new").unwrap_or_default().to_owned(),
        ),
    })))
}

impl RealFixture {
    /// FNV-1a diff, путей и сторон всех файлов: отпечаток, по которому сверяется эталон.
    pub fn digest(&self) -> u64 {
        let mut hash = Fnv::new();
        hash.write(&(self.diff.len() as u64).to_le_bytes());
        hash.write(&self.diff);
        for file in &self.files {
            for part in [file.path.as_bytes(), &file.old, &file.new] {
                hash.write(&(part.len() as u64).to_le_bytes());
                hash.write(part);
            }
        }
        hash.finish()
    }

    /// Фикстура вида с активным файлом `index`: стороны этого файла, тредов нет.
    pub fn fixture(&self, index: usize) -> Option<Fixture> {
        let file = self.files.get(index)?;
        Some(Fixture {
            name: NAME,
            path: file.path.clone(),
            diff: Arc::clone(&self.diff),
            old: Arc::clone(&file.old),
            new: Arc::clone(&file.new),
            threads: Vec::new(),
        })
    }

    pub fn file(&self, path: &str) -> Option<&RealFile> {
        self.files.iter().find(|file| file.path == path)
    }

    /// Байт diff и сторон всех файлов.
    pub fn bytes(&self) -> usize {
        self.diff.len()
            + self
                .files
                .iter()
                .map(|file| file.old.len() + file.new.len())
                .sum::<usize>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("probe-diff-real-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(BLOBS_DIR)).unwrap();
        dir
    }

    #[test]
    fn missing_status_means_not_prepared() {
        let dir = temp("missing");
        assert_eq!(load(&dir), Ok(None));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn not_run_carries_its_reason() {
        let dir = temp("not-run");
        fs::write(
            dir.join(STATUS_FILE),
            "status=not_run\nreason=нет закреплённых коммитов\n",
        )
        .unwrap();
        assert_eq!(
            load(&dir),
            Ok(Some(RealSource::NotRun(
                "нет закреплённых коммитов".to_owned()
            )))
        );
        fs::write(dir.join(STATUS_FILE), "status=not_run\n").unwrap();
        assert!(load(&dir).is_err());
        fs::write(dir.join(STATUS_FILE), "status=half\n").unwrap();
        assert!(load(&dir).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn prepared_fixture_reads_every_pair() {
        let dir = temp("prepared");
        fs::write(
            dir.join(STATUS_FILE),
            "status=prepared\nold=aaa\nnew=bbb\nfiles=2\ngit_read_ms=41.5\ngit_calls=5\n",
        )
        .unwrap();
        fs::write(dir.join(DIFF_FILE), "diff").unwrap();
        fs::write(dir.join(BLOBS_DIR).join("a1"), "old a").unwrap();
        fs::write(dir.join(BLOBS_DIR).join("a2"), "new a").unwrap();
        fs::write(dir.join(BLOBS_DIR).join("b2"), "new b").unwrap();
        fs::write(
            dir.join(MANIFEST_FILE),
            "a1\ta2\tsrc/a b.rs\n-\tb2\tsrc/b.rs\n",
        )
        .unwrap();
        let Ok(Some(RealSource::Prepared(real))) = load(&dir) else {
            panic!("фикстура не прочитана: {:?}", load(&dir));
        };
        assert_eq!(real.files.len(), 2);
        assert_eq!(real.files[0].path, "src/a b.rs");
        assert_eq!(&*real.files[1].old, b"");
        assert_eq!(real.git_read_ms, Some(41.5));
        assert_eq!(real.git_calls, Some(5));
        assert_eq!(real.commits, ("aaa".to_owned(), "bbb".to_owned()));
        let fixture = real.fixture(1).unwrap();
        assert_eq!(fixture.path, "src/b.rs");
        assert_eq!(&*fixture.new, b"new b");
        assert!(real.fixture(2).is_none());
        assert_eq!(real.bytes(), 4 + 5 + 5 + 5);
        let mut changed = real.clone();
        changed.files[1].new = Arc::from(&b"new c"[..]);
        assert_ne!(changed.digest(), real.digest());

        fs::write(dir.join(STATUS_FILE), "status=prepared\nfiles=3\n").unwrap();
        assert!(load(&dir).unwrap_err().contains("files="));
        fs::write(dir.join(STATUS_FILE), "status=prepared\nfiles=2\n").unwrap();
        fs::write(
            dir.join(MANIFEST_FILE),
            "zz\ta2\tsrc/a.rs\n-\tb2\tsrc/b.rs\n",
        )
        .unwrap();
        assert!(load(&dir).is_err());
        fs::write(dir.join(MANIFEST_FILE), "a1\ta2\n").unwrap();
        assert!(load(&dir).unwrap_err().contains("трёх полей"));
        fs::remove_dir_all(&dir).unwrap();
    }
}
