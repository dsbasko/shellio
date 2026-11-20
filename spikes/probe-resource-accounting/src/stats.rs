//! Статистика вердикта стоимости (план P0.8, «Стоимость»): порядок состояний в блоке, ход
//! протокола точки и вердикты по чистым блокам. Ожидания — отчёт, раздел «Ожидания», `cost`.
//!
//! Состояние — пара «сэмплер, учёт участков»: `00`, `10`, `01`, `11`. Эффект в процентных пунктах
//! одного ядра оценивается двумя разностями: сэмплер — `10 − 00` и `11 − 01`, учёт участков —
//! `01 − 00` и `11 − 10`. Вердикт сначала выносится по каждой оценке во всех чистых блоках, затем
//! по эффекту.

use serde::{Deserialize, Serialize};

use crate::results::Status;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum State {
    #[serde(rename = "00")]
    S00,
    #[serde(rename = "10")]
    S10,
    #[serde(rename = "01")]
    S01,
    #[serde(rename = "11")]
    S11,
}

impl State {
    pub const ALL: [State; 4] = [State::S00, State::S10, State::S01, State::S11];

    pub fn name(self) -> &'static str {
        match self {
            State::S00 => "00",
            State::S10 => "10",
            State::S01 => "01",
            State::S11 => "11",
        }
    }

    pub fn sampler(self) -> bool {
        matches!(self, State::S10 | State::S11)
    }

    pub fn spans(self) -> bool {
        matches!(self, State::S01 | State::S11)
    }

    pub fn index(self) -> usize {
        match self {
            State::S00 => 0,
            State::S10 => 1,
            State::S01 => 2,
            State::S11 => 3,
        }
    }
}

/// Порядок состояний блока в слоте `slot`: сдвиг от слота к слоту. Повтор загрязнённого блока
/// идёт в том же слоте и с тем же порядком, поэтому чистые блоки сохраняют балансировку.
pub fn order(slot: usize) -> [State; 4] {
    let mut order = State::ALL;
    order.rotate_left(slot % State::ALL.len());
    order
}

/// Правила хода протокола точки.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rules {
    pub blocks: usize,
    pub max_repeats: usize,
    pub min_clean: usize,
    /// Новый блок не начинается, если до срока попытки осталось меньше.
    pub guard_ns: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stop {
    /// Все слоты пройдены.
    Complete,
    /// До срока попытки меньше запаса на блок и запись итога.
    Deadline,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next {
    Block {
        slot: usize,
        order: [State; 4],
        repeat: bool,
    },
    Done(Stop),
}

/// Ход протокола: слот, израсходованные повторы и итоги блоков.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    pub slot: usize,
    pub repeats: usize,
    /// Следующий блок — повтор загрязнённого в том же слоте.
    pub repeating: bool,
    pub run: usize,
    pub clean: usize,
}

impl Progress {
    pub fn next(&self, rules: &Rules, now_ns: u64, deadline_ns: u64) -> Next {
        if self.slot >= rules.blocks {
            return Next::Done(Stop::Complete);
        }
        if deadline_ns.saturating_sub(now_ns) < rules.guard_ns {
            return Next::Done(Stop::Deadline);
        }
        Next::Block {
            slot: self.slot,
            order: order(self.slot),
            repeat: self.repeating,
        }
    }

    /// Итог блока: чистый блок закрывает слот, загрязнённый повторяется, пока есть повторы.
    pub fn record(&mut self, rules: &Rules, clean: bool) {
        self.run += 1;
        if clean {
            self.clean += 1;
        }
        if !clean && self.repeats < rules.max_repeats {
            self.repeats += 1;
            self.repeating = true;
        } else {
            self.slot += 1;
            self.repeating = false;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Pass,
    Fail,
    Unconfirmed,
}

impl Verdict {
    pub fn status(self) -> Status {
        match self {
            Verdict::Pass => Status::Pass,
            Verdict::Fail => Status::Fail,
            Verdict::Unconfirmed => Status::Unconfirmed,
        }
    }
}

/// Вердикт одной оценки по всем чистым блокам: все не выше бюджета (равенство входит) — `pass`,
/// все выше — `fail`, иначе и без наблюдений — «не подтверждено».
pub fn estimate_verdict(samples: &[f64], budget: f64) -> Verdict {
    if samples.is_empty() || samples.iter().any(|v| !v.is_finite()) {
        return Verdict::Unconfirmed;
    }
    if samples.iter().all(|v| *v <= budget) {
        Verdict::Pass
    } else if samples.iter().all(|v| *v > budget) {
        Verdict::Fail
    } else {
        Verdict::Unconfirmed
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    Sampler,
    Spans,
}

/// Бюджет сэмплера — 0,5 п. п. (`docs/architecture.md`), учёта участков — 1 п. п. (ADR-0011).
pub const SAMPLER_BUDGET_PP: f64 = 0.5;
pub const SPANS_BUDGET_PP: f64 = 1.0;

impl Effect {
    pub fn name(self) -> &'static str {
        match self {
            Effect::Sampler => "сэмплер",
            Effect::Spans => "учёт участков",
        }
    }

    pub fn budget_pp(self) -> f64 {
        match self {
            Effect::Sampler => SAMPLER_BUDGET_PP,
            Effect::Spans => SPANS_BUDGET_PP,
        }
    }

    /// Две оценки: (с эффектом, без него, условие).
    pub fn pairs(self) -> [(State, State, &'static str); 2] {
        match self {
            Effect::Sampler => [
                (State::S10, State::S00, "при выключенном учёте участков"),
                (State::S11, State::S01, "при включённом учёте участков"),
            ],
            Effect::Spans => [
                (State::S01, State::S00, "при выключенном сэмплере"),
                (State::S11, State::S10, "при включённом сэмплере"),
            ],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Estimate {
    /// `10-00` и подобные.
    pub id: String,
    pub condition: String,
    /// По одной разности на чистый блок, п. п. одного ядра.
    pub samples: Vec<f64>,
    pub verdict: Verdict,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EffectVerdict {
    pub effect: Effect,
    pub budget_pp: f64,
    pub clean_blocks: usize,
    pub estimates: Vec<Estimate>,
    /// Взаимодействие: вторая оценка минус первая по блокам.
    pub interaction: Vec<f64>,
    pub verdict: Verdict,
    pub reason: String,
}

/// Значения по состоянию одного чистого блока, по [`State::index`].
pub type BlockValues = [f64; 4];

pub fn median(values: &[f64]) -> Option<f64> {
    let mut sorted: Vec<f64> = values.iter().copied().filter(|v| v.is_finite()).collect();
    sorted.sort_by(f64::total_cmp);
    match sorted.len() {
        0 => None,
        n if n % 2 == 1 => Some(sorted[n / 2]),
        n => Some((sorted[n / 2 - 1] + sorted[n / 2]) / 2.0),
    }
}

fn range(values: &[f64]) -> String {
    let min = values.iter().copied().fold(f64::INFINITY, f64::min);
    let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    match median(values) {
        Some(median) => format!("{min:.3}…{max:.3}, медиана {median:.3}"),
        None => "нет значений".to_owned(),
    }
}

/// Вердикт эффекта по чистым блокам. Хотя бы одна оценка `fail` — `fail` с условием; обе `pass`
/// — `pass`; иначе «не подтверждено». Чистых блоков меньше `min_clean` — «не подтверждено»
/// «шумно» при любых значениях.
pub fn effect_verdict(effect: Effect, blocks: &[BlockValues], min_clean: usize) -> EffectVerdict {
    let budget = effect.budget_pp();
    let estimates: Vec<Estimate> = effect
        .pairs()
        .into_iter()
        .map(|(with, without, condition)| {
            let samples: Vec<f64> = blocks
                .iter()
                .map(|b| b[with.index()] - b[without.index()])
                .collect();
            Estimate {
                id: format!("{}-{}", with.name(), without.name()),
                condition: condition.to_owned(),
                verdict: estimate_verdict(&samples, budget),
                samples,
            }
        })
        .collect();
    let interaction: Vec<f64> = estimates[1]
        .samples
        .iter()
        .zip(&estimates[0].samples)
        .map(|(b, a)| b - a)
        .collect();
    let detail = estimates
        .iter()
        .map(|e| {
            format!(
                "{} ({}): {} п. п. — {}",
                e.id,
                e.condition,
                range(&e.samples),
                match e.verdict {
                    Verdict::Pass => "не выше бюджета во всех блоках",
                    Verdict::Fail => "выше бюджета во всех блоках",
                    Verdict::Unconfirmed => "по обе стороны бюджета",
                }
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    let (verdict, head) = if blocks.len() < min_clean {
        (
            Verdict::Unconfirmed,
            format!(
                "шумно: чистых блоков {} из нужных {min_clean}",
                blocks.len()
            ),
        )
    } else {
        let failed: Vec<&str> = estimates
            .iter()
            .filter(|e| e.verdict == Verdict::Fail)
            .map(|e| e.condition.as_str())
            .collect();
        if !failed.is_empty() {
            (
                Verdict::Fail,
                format!(
                    "{} превышает бюджет {budget} п. п. {}",
                    effect.name(),
                    failed.join(" и ")
                ),
            )
        } else if estimates.iter().all(|e| e.verdict == Verdict::Pass) {
            (
                Verdict::Pass,
                format!(
                    "{} укладывается в бюджет {budget} п. п. при обоих состояниях",
                    effect.name()
                ),
            )
        } else {
            (
                Verdict::Unconfirmed,
                format!(
                    "{}: оценки по обе стороны бюджета {budget} п. п.",
                    effect.name()
                ),
            )
        }
    };
    let reason = format!(
        "{head}; чистых блоков {}; {detail}; взаимодействие {} п. п.",
        blocks.len(),
        range(&interaction)
    );
    EffectVerdict {
        effect,
        budget_pp: budget,
        clean_blocks: blocks.len(),
        estimates,
        interaction,
        verdict,
        reason,
    }
}

/// Строка таблицы стоимости для `--aggregate`: стабильная форма `raw.cost_row`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CostRow {
    pub point: String,
    pub pids: usize,
    pub panels: usize,
    /// Шаг сэмплера в секундах.
    pub step_s: f64,
    pub work: bool,
    pub blocks_run: usize,
    pub clean_blocks: usize,
    pub stop: Stop,
    pub sampler: EffectRow,
    /// `None` — точка простоя: операций нет.
    pub spans: Option<EffectRow>,
    /// Медиана CPU процесса на операцию по чистым блокам, мкс, по состояниям `00 10 01 11`.
    pub cpu_per_op_us: Option<[Option<f64>; 4]>,
    pub pass_duration_median_us: Option<f64>,
    pub wakeups_per_minute: Option<f64>,
    pub set_size: Option<usize>,
    pub partial_reads: usize,
    pub footprint_max_mib: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EffectRow {
    pub verdict: Verdict,
    pub budget_pp: f64,
    /// Медианы двух оценок по чистым блокам, п. п.
    pub medians: [Option<f64>; 2],
    pub interaction_median: Option<f64>,
}

impl EffectRow {
    pub fn of(verdict: &EffectVerdict) -> EffectRow {
        EffectRow {
            verdict: verdict.verdict,
            budget_pp: verdict.budget_pp,
            medians: [
                median(&verdict.estimates[0].samples),
                median(&verdict.estimates[1].samples),
            ],
            interaction_median: median(&verdict.interaction),
        }
    }
}

#[cfg(test)]
mod tests;
