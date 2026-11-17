//! GUI-процесс сценария `bridge`: окно с поверхностями по командам водителя
//! (`matrix::bridge::GuiCommand`), журнал эффектов дозаписью.

use std::path::Path;

pub fn run(_control: &Path, _out: &Path, _nonce: &str) -> Result<(), String> {
    Err("GUI-процесс моста ещё не построен".to_owned())
}
