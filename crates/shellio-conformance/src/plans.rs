//! Раздел «Сверка с прототипом» в незавершённых планах (ADR-0037, «Тест раздела планов»).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Заголовок раздела, обязательный в каждом незавершённом плане.
pub const SECTION: &str = "## Сверка с прототипом";

/// Планы `<dir>/*.md` без раздела. Подкаталоги (`completed/`) не просматриваются.
pub fn missing_section(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if !path.is_file() || path.extension().is_none_or(|e| e != "md") {
            continue;
        }
        let text = fs::read_to_string(&path)?;
        if !text.lines().any(|line| line.trim_end() == SECTION) {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}
