//! Перцентили замеров. Модуль не зависит от AppKit.
//!
//! Перцентиль берётся по ближайшему рангу: для доли `q` из `n` упорядоченных значений это
//! значение с рангом `⌈q·n⌉`, но не меньше первого. Интерполяции нет, поэтому каждый перцентиль
//! — одно из измеренных значений.

use std::fmt;

use serde::{Deserialize, Serialize};

pub const METHOD: &str = "nearest-rank";

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub count: usize,
    pub p50: f64,
    pub p95: f64,
    pub max: f64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StatsError {
    Empty,
    NotFinite(usize),
}

impl fmt::Display for StatsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StatsError::Empty => write!(f, "пустой набор замеров"),
            StatsError::NotFinite(index) => write!(f, "замер №{index} не конечное число"),
        }
    }
}

pub fn summarize(samples: &[f64]) -> Result<Summary, StatsError> {
    if samples.is_empty() {
        return Err(StatsError::Empty);
    }
    if let Some(index) = samples.iter().position(|value| !value.is_finite()) {
        return Err(StatsError::NotFinite(index));
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    Ok(Summary {
        count: sorted.len(),
        p50: nearest_rank(&sorted, 0.50),
        p95: nearest_rank(&sorted, 0.95),
        max: sorted[sorted.len() - 1],
    })
}

/// Повторы одного окна замера: медиана (при чётном числе — среднее двух средних), крайние
/// значения и разброс `max − min`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Repeats {
    pub count: usize,
    pub median: f64,
    pub min: f64,
    pub max: f64,
    pub spread: f64,
}

pub fn repeats(samples: &[f64]) -> Result<Repeats, StatsError> {
    if samples.is_empty() {
        return Err(StatsError::Empty);
    }
    if let Some(index) = samples.iter().position(|value| !value.is_finite()) {
        return Err(StatsError::NotFinite(index));
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let n = sorted.len();
    let median = if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    };
    let (min, max) = (sorted[0], sorted[n - 1]);
    Ok(Repeats {
        count: n,
        median,
        min,
        max,
        spread: max - min,
    })
}

/// Значение ранга `⌈q·n⌉` в непустом упорядоченном наборе.
fn nearest_rank(sorted: &[f64], q: f64) -> f64 {
    let rank = (q * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_set_is_an_error() {
        assert_eq!(summarize(&[]), Err(StatsError::Empty));
        assert_eq!(StatsError::Empty.to_string(), "пустой набор замеров");
    }

    #[test]
    fn non_finite_sample_is_an_error() {
        assert_eq!(
            summarize(&[1.0, f64::NAN, 2.0]),
            Err(StatsError::NotFinite(1))
        );
        assert_eq!(summarize(&[f64::INFINITY]), Err(StatsError::NotFinite(0)));
    }

    #[test]
    fn single_sample() {
        assert_eq!(
            summarize(&[7.5]),
            Ok(Summary {
                count: 1,
                p50: 7.5,
                p95: 7.5,
                max: 7.5
            })
        );
    }

    #[test]
    fn one_to_hundred() {
        let samples: Vec<f64> = (1..=100).rev().map(f64::from).collect();
        assert_eq!(
            summarize(&samples),
            Ok(Summary {
                count: 100,
                p50: 50.0,
                p95: 95.0,
                max: 100.0
            })
        );
    }

    #[test]
    fn small_sets_use_nearest_rank() {
        let four = summarize(&[4.0, 1.0, 3.0, 2.0]).unwrap();
        assert_eq!((four.p50, four.p95, four.max), (2.0, 4.0, 4.0));
        let twenty: Vec<f64> = (1..=20).map(f64::from).collect();
        let summary = summarize(&twenty).unwrap();
        assert_eq!((summary.p50, summary.p95, summary.max), (10.0, 19.0, 20.0));
        let two = summarize(&[10.0, 20.0]).unwrap();
        assert_eq!((two.p50, two.p95), (10.0, 20.0));
    }

    #[test]
    fn repeats_median_and_spread() {
        let five = repeats(&[0.75, 0.5, 0.625, 1.5, 0.5625]).unwrap();
        assert_eq!(
            five,
            Repeats {
                count: 5,
                median: 0.625,
                min: 0.5,
                max: 1.5,
                spread: 1.0
            }
        );
        let four = repeats(&[4.0, 1.0, 3.0, 2.0]).unwrap();
        assert_eq!((four.median, four.spread), (2.5, 3.0));
        let one = repeats(&[-2.0]).unwrap();
        assert_eq!(
            (one.median, one.min, one.max, one.spread),
            (-2.0, -2.0, -2.0, 0.0)
        );
        assert_eq!(repeats(&[]), Err(StatsError::Empty));
        assert_eq!(repeats(&[1.0, f64::NAN]), Err(StatsError::NotFinite(1)));
    }

    #[test]
    fn duplicates_and_negative_values() {
        let summary = summarize(&[-1.0, 5.0, 5.0, 5.0, 0.0]).unwrap();
        assert_eq!((summary.p50, summary.p95, summary.max), (5.0, 5.0, 5.0));
    }
}
