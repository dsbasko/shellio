//! Чистая часть сценария `idle`, бюджет 1%: 10 с неподвижного видимого diff `modified` после
//! окончания подсветки, `100 × Δ(CPU-время процесса) / Δ(монотонное время)` и число проходов
//! отображения (отметок `drawRect:`) за окно. Отчёт, раздел «CPU в простое, бюджет 1%».

use serde::Serialize;

use super::sampler::{self, Sample};
use crate::results::Verdict;

/// Окно простоя, наносекунды.
pub const WINDOW_NS: u64 = 10_000_000_000;
/// Бюджет CPU GUI в простое, проценты одного ядра.
pub const BUDGET_PERCENT: f64 = 1.0;

/// Итог окна простоя.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct IdleSummary {
    pub window_ns: u64,
    pub cpu_ns: u64,
    pub cpu_percent: f64,
    pub passes: usize,
}

/// Итог окна между выборками `start` и `end`. Ошибка исполнения: окно короче 10 с или часы назад.
pub fn summarize(start: Sample, end: Sample, passes: usize) -> Result<IdleSummary, String> {
    if end.mono_ns < start.mono_ns || end.cpu_ns < start.cpu_ns {
        return Err("выборки простоя не по времени".to_owned());
    }
    let window_ns = end.mono_ns - start.mono_ns;
    if window_ns < WINDOW_NS {
        return Err(format!("окно простоя {window_ns} нс короче {WINDOW_NS} нс"));
    }
    let cpu_ns = end.cpu_ns - start.cpu_ns;
    let cpu_percent =
        sampler::cpu_percent(cpu_ns, window_ns).ok_or_else(|| "пустое окно простоя".to_owned())?;
    Ok(IdleSummary {
        window_ns,
        cpu_ns,
        cpu_percent,
        passes,
    })
}

/// Вердикт: не больше 1%.
pub fn judge(summary: &IdleSummary) -> (Verdict, String) {
    let detail = format!(
        "CPU {:.3}% за {:.2} с, проходов отображения {}",
        summary.cpu_percent,
        summary.window_ns as f64 / 1e9,
        summary.passes
    );
    if summary.cpu_percent <= BUDGET_PERCENT {
        (Verdict::Pass, format!("{detail}, не больше 1%"))
    } else {
        (Verdict::Fail, format!("{detail}, больше 1%"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(mono_s: f64, cpu_ms: f64) -> Sample {
        Sample {
            mono_ns: (mono_s * 1e9) as u64,
            cpu_ns: (cpu_ms * 1e6) as u64,
            footprint_bytes: 0,
            lifetime_max_bytes: 0,
        }
    }

    #[test]
    fn cpu_share_of_the_window() {
        let summary = summarize(at(1.0, 10.0), at(11.0, 60.0), 0).unwrap();
        assert_eq!(summary.window_ns, 10_000_000_000);
        assert_eq!(summary.cpu_ns, 50_000_000);
        assert!((summary.cpu_percent - 0.5).abs() < 1e-9);
        assert_eq!(judge(&summary).0, Verdict::Pass);
        let busy = summarize(at(1.0, 0.0), at(11.0, 101.0), 3).unwrap();
        assert_eq!(judge(&busy).0, Verdict::Fail);
        let edge = summarize(at(0.0, 0.0), at(10.0, 100.0), 0).unwrap();
        assert_eq!(judge(&edge).0, Verdict::Pass);
    }

    #[test]
    fn short_or_backward_windows_are_execution_errors() {
        assert!(summarize(at(1.0, 0.0), at(10.5, 1.0), 0).is_err());
        assert!(summarize(at(11.0, 0.0), at(1.0, 1.0), 0).is_err());
        assert!(summarize(at(1.0, 5.0), at(12.0, 1.0), 0).is_err());
    }
}
