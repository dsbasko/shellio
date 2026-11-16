//! Раннер пробы P0.4: процессы сценариев матрицы, `--launch` и `--aggregate`. Требует функцию
//! `vt`: сценариям рубежей нужны оракулы на `libghostty-vt` (ADR-0035).

use std::process::ExitCode;

fn main() -> ExitCode {
    probe_session_survival::main()
}
