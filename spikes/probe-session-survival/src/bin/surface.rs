//! GUI-процесс пробы P0.4 с поверхностью libghostty: сценарий `report-policy`, `--launch` и
//! `--aggregate`. Требует функцию `surface`; запускается из подписанного бандла пробы.

use std::process::ExitCode;

fn main() -> ExitCode {
    probe_session_survival::main()
}
