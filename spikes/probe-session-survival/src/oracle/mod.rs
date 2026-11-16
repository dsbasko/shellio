//! Оракулы восстановления (раздел плана «Оракулы»). `model` и `suffix` — чистые модули без FFI и
//! идут в `make check`; `vt` — эталон и приёмник на `libghostty-vt` за функцией `vt` (ADR-0035).

pub mod model;
pub mod suffix;
#[cfg(feature = "vt")]
pub mod vt;
