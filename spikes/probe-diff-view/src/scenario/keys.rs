//! Чистая часть сценария `keys`, бюджет 16 мс: интервал «`postEvent` → конец `drawRect:` нужного
//! поколения» по 100 событиям каждой клавиши после 10 прогревочных, p95 ближайшим рангом. Вердикт
//! на 120 Гц, на 60 Гц — наблюдение (`not_run`). Отчёт, раздел «Клавиши, бюджет 16 мс».

use serde::Serialize;

use crate::results::Verdict;
use crate::stats;

/// Прогревочных событий клавиши: в агрегаты не входят.
pub const WARMUP: usize = 10;
/// Измеренных событий клавиши.
pub const MEASURED: usize = 100;
/// Бюджет реакции на клавишу в `Navigation`, мс.
pub const BUDGET_MS: f64 = 16.0;

/// Клавиша сценария. `]c` и `gg` — последовательности из двух клавиш, интервал идёт от
/// `postEvent` первой из них.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum KeyName {
    J,
    K,
    CtrlD,
    NextChange,
    Bottom,
    Top,
}

impl KeyName {
    pub const ALL: [KeyName; 6] = [
        KeyName::J,
        KeyName::K,
        KeyName::CtrlD,
        KeyName::NextChange,
        KeyName::Bottom,
        KeyName::Top,
    ];

    pub fn name(self) -> &'static str {
        match self {
            KeyName::J => "j",
            KeyName::K => "k",
            KeyName::CtrlD => "ctrl-d",
            KeyName::NextChange => "]c",
            KeyName::Bottom => "G",
            KeyName::Top => "gg",
        }
    }

    /// Имя метрики в `result.json`.
    pub fn metric(self) -> &'static str {
        match self {
            KeyName::J => "keys_j_ms",
            KeyName::K => "keys_k_ms",
            KeyName::CtrlD => "keys_ctrl_d_ms",
            KeyName::NextChange => "keys_next_change_ms",
            KeyName::Bottom => "keys_bottom_ms",
            KeyName::Top => "keys_top_ms",
        }
    }
}

/// Одно событие клавиши: `postEvent` первой клавиши, конец `drawRect:` отметки, которой покрыты
/// видимые строки поколением клавиши, и, если был, конец отрисовки догоняющего снимка подсветки
/// (наблюдение без вердикта).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Press {
    pub key: KeyName,
    pub index: usize,
    pub posted_ns: u64,
    pub drawn_ns: Option<u64>,
    pub highlight_ns: Option<u64>,
}

impl Press {
    fn latency_ms(&self) -> Option<f64> {
        let drawn = self.drawn_ns?;
        (drawn >= self.posted_ns).then(|| (drawn - self.posted_ns) as f64 / 1e6)
    }
}

/// Итог клавиши: 100 измеренных интервалов и, отдельно, догоняющая подсветка.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct KeySummary {
    pub key: KeyName,
    pub latency: stats::Summary,
    /// Интервалы до догоняющей подсветки среди измеренных событий, у которых она была.
    pub highlight: Option<stats::Summary>,
    pub samples_ms: Vec<f64>,
}

/// Итог клавиши. Ошибка исполнения: не `WARMUP + MEASURED` событий, событие без отметки
/// нужного поколения или отметка раньше `postEvent`.
pub fn summarize(key: KeyName, presses: &[Press]) -> Result<KeySummary, String> {
    let mine: Vec<&Press> = presses.iter().filter(|press| press.key == key).collect();
    if mine.len() != WARMUP + MEASURED {
        return Err(format!(
            "клавиша {}: {} событий вместо {}",
            key.name(),
            mine.len(),
            WARMUP + MEASURED
        ));
    }
    if let Some(gap) = mine
        .iter()
        .enumerate()
        .find(|(position, press)| press.index != *position)
    {
        return Err(format!(
            "клавиша {}: событие №{} стоит на месте {}",
            key.name(),
            gap.1.index,
            gap.0
        ));
    }
    let mut samples = Vec::with_capacity(MEASURED);
    let mut highlight = Vec::new();
    for press in &mine[WARMUP..] {
        let latency = press.latency_ms().ok_or_else(|| {
            format!(
                "клавиша {}, событие №{}: нет отметки drawRect: нужного поколения после postEvent",
                key.name(),
                press.index
            )
        })?;
        samples.push(latency);
        if let Some(at) = press.highlight_ns.filter(|at| *at >= press.posted_ns) {
            highlight.push((at - press.posted_ns) as f64 / 1e6);
        }
    }
    let latency = stats::summarize(&samples).map_err(|err| err.to_string())?;
    Ok(KeySummary {
        key,
        latency,
        highlight: stats::summarize(&highlight).ok(),
        samples_ms: samples,
    })
}

/// Вердикт бюджета: p95 каждой клавиши не больше 16 мс. Без подтверждённых 120 Гц — `not_run`,
/// значения остаются наблюдением.
pub fn judge(summaries: &[KeySummary], rate_120: bool) -> (Verdict, String) {
    let over: Vec<String> = summaries
        .iter()
        .filter(|summary| summary.latency.p95 > BUDGET_MS)
        .map(|summary| format!("{} p95 {:.2} мс", summary.key.name(), summary.latency.p95))
        .collect();
    let worst = summaries
        .iter()
        .map(|summary| summary.latency.p95)
        .fold(0.0f64, f64::max);
    if !rate_120 {
        let observed = if over.is_empty() {
            format!("наибольший p95 {worst:.2} мс")
        } else {
            format!("выше 16 мс: {}", over.join(", "))
        };
        return (
            Verdict::NotRun,
            format!("120 Гц не подтверждены, на 60 Гц наблюдение: {observed}"),
        );
    }
    if summaries.len() != KeyName::ALL.len() {
        return (
            Verdict::Fail,
            format!(
                "итогов клавиш {} из {}",
                summaries.len(),
                KeyName::ALL.len()
            ),
        );
    }
    if over.is_empty() {
        (
            Verdict::Pass,
            format!("наибольший p95 {worst:.2} мс не больше 16 мс"),
        )
    } else {
        (Verdict::Fail, format!("выше 16 мс: {}", over.join(", ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn presses(key: KeyName, latencies_ms: &[f64]) -> Vec<Press> {
        latencies_ms
            .iter()
            .enumerate()
            .map(|(index, ms)| {
                let posted_ns = 1_000_000_000 + index as u64 * 100_000_000;
                Press {
                    key,
                    index,
                    posted_ns,
                    drawn_ns: Some(posted_ns + (ms * 1e6) as u64),
                    highlight_ns: None,
                }
            })
            .collect()
    }

    #[test]
    fn warmup_is_excluded_and_p95_is_the_nearest_rank() {
        let mut latencies = vec![500.0; WARMUP];
        latencies.extend((1..=MEASURED).map(|value| value as f64 / 10.0));
        let summary = summarize(KeyName::J, &presses(KeyName::J, &latencies)).unwrap();
        assert_eq!(summary.latency.count, MEASURED);
        assert_eq!(summary.latency.max, 10.0);
        assert_eq!(summary.latency.p95, 9.5);
        assert_eq!(summary.latency.p50, 5.0);
        assert_eq!(summary.highlight, None);
    }

    #[test]
    fn missing_mark_or_count_is_an_execution_error() {
        let latencies = vec![3.0; WARMUP + MEASURED];
        let mut all = presses(KeyName::K, &latencies);
        all[WARMUP + 5].drawn_ns = None;
        assert!(summarize(KeyName::K, &all).unwrap_err().contains("№15"));
        let short = presses(KeyName::K, &latencies[1..]);
        assert!(summarize(KeyName::K, &short).unwrap_err().contains("109"));
        let mut early = presses(KeyName::K, &latencies);
        early[WARMUP].drawn_ns = Some(early[WARMUP].posted_ns - 1);
        assert!(summarize(KeyName::K, &early).is_err());
        // Прогревочное событие без отметки не роняет итог.
        let mut warm = presses(KeyName::K, &latencies);
        warm[0].drawn_ns = None;
        assert!(summarize(KeyName::K, &warm).is_ok());
    }

    #[test]
    fn other_keys_are_ignored_and_highlight_is_separate() {
        let latencies = vec![4.0; WARMUP + MEASURED];
        let mut all = presses(KeyName::Top, &latencies);
        for press in &mut all {
            press.highlight_ns = Some(press.posted_ns + 9_000_000);
        }
        all.extend(presses(KeyName::Bottom, &latencies));
        let summary = summarize(KeyName::Top, &all).unwrap();
        assert_eq!(summary.latency.p95, 4.0);
        assert_eq!(
            summary.highlight.map(|h| (h.count, h.p95)),
            Some((MEASURED, 9.0))
        );
    }

    #[test]
    fn verdict_needs_120_hz_and_every_key_within_budget() {
        let ok: Vec<KeySummary> = KeyName::ALL
            .iter()
            .map(|&key| summarize(key, &presses(key, &[6.0; WARMUP + MEASURED])).unwrap())
            .collect();
        assert_eq!(judge(&ok, true).0, Verdict::Pass);
        let at_budget: Vec<KeySummary> = KeyName::ALL
            .iter()
            .map(|&key| summarize(key, &presses(key, &[BUDGET_MS; WARMUP + MEASURED])).unwrap())
            .collect();
        assert_eq!(judge(&at_budget, true).0, Verdict::Pass);
        let (verdict, reason) = judge(&ok, false);
        assert_eq!(verdict, Verdict::NotRun);
        assert!(reason.contains("120 Гц не подтверждены"), "{reason}");

        let mut slow = ok.clone();
        slow[3] = summarize(
            KeyName::NextChange,
            &presses(KeyName::NextChange, &[17.0; WARMUP + MEASURED]),
        )
        .unwrap();
        let (verdict, reason) = judge(&slow, true);
        assert_eq!(verdict, Verdict::Fail);
        assert!(reason.contains("]c p95 17.00"), "{reason}");
        assert_eq!(judge(&slow, false).0, Verdict::NotRun);
        assert_eq!(judge(&ok[..5], true).0, Verdict::Fail);
    }
}
