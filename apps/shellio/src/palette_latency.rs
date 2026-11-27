//! Измеритель задержки палитры в обычном окне (план `command-overlays`, правило «Бюджет
//! палитры»). Чистая часть: сырые ряды открытий и закрытий палитры с клавиши и такты
//! `CADisplayLink` серии. Время — секунды с загрузки, часы `NSEvent.timestamp`.
//!
//! Интервал судится от метки времени события клавиши до первого прохода отображения после
//! применения нового состояния. Проход даёт монитор цикла главного потока: наблюдатель засыпания
//! и выхода стоит после фиксации транзакции Core Animation, поэтому первый проход после
//! применения уже несёт окно палитры с содержимым или снятое окно палитры.

use std::collections::VecDeque;

/// Сколько последних замеров держит измеритель: серия A B B A — 440 открытий и 440 закрытий.
pub const SAMPLES: usize = 1024;
/// Сколько последних тактов держит измеритель: больше 4 минут на 120 Гц.
pub const TICKS: usize = 32_768;

/// Что сделала клавиша с палитрой.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    Open,
    Close,
}

impl Change {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Close => "close",
        }
    }
}

/// Состояние окон после применения: окно палитры на экране и key, окно владельца key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shown {
    pub palette_visible: bool,
    pub palette_key: bool,
    pub owner_key: bool,
}

impl Shown {
    /// Состояние подтверждает изменение: открытая палитра на экране и key, закрытая снята, и key
    /// у окна владельца.
    pub const fn confirms(self, change: Change) -> bool {
        match change {
            Change::Open => self.palette_visible && self.palette_key,
            Change::Close => !self.palette_visible && self.owner_key,
        }
    }
}

/// Замер одного нажатия до прохода отображения.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    pub change: Change,
    /// `NSEvent.timestamp` клавиши.
    pub event: f64,
    /// Вход в отложенный обработчик решения клавиши.
    pub entry: f64,
    /// Новое состояние палитры применено к окнам: конец своего кода.
    pub applied: f64,
    /// Поколение окна после применения.
    pub generation: u64,
    pub shown: Shown,
}

/// Строка сырого ряда: замер и первый проход отображения не раньше применения.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Row {
    pub sample: Sample,
    pub drawn: Option<f64>,
}

/// Ряды открытий и закрытий палитры текущей серии.
#[derive(Debug, Default)]
pub struct PaletteLatency {
    samples: VecDeque<Sample>,
    ticks: VecDeque<f64>,
    ticking: bool,
}

impl PaletteLatency {
    /// Новая серия: ряды очищены, такты идут, только если `ticks`.
    pub fn reset(&mut self, ticks: bool) {
        self.samples.clear();
        self.ticks.clear();
        self.ticking = ticks;
    }

    /// Серия ведёт такты `CADisplayLink`.
    pub fn ticking(&self) -> bool {
        self.ticking
    }

    pub fn record(&mut self, sample: Sample) {
        if self.samples.len() == SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
    }

    /// Такт `CADisplayLink`; вне серии с тактами отбрасывается.
    pub fn tick(&mut self, at: f64) {
        if !self.ticking {
            return;
        }
        if self.ticks.len() == TICKS {
            self.ticks.pop_front();
        }
        self.ticks.push_back(at);
    }

    pub fn ticks(&self) -> Vec<f64> {
        self.ticks.iter().copied().collect()
    }

    /// Сырые ряды. `first_pass` — первый проход отображения не раньше момента.
    pub fn rows(&self, first_pass: impl Fn(f64) -> Option<f64>) -> Vec<Row> {
        self.samples
            .iter()
            .map(|sample| Row {
                sample: *sample,
                drawn: first_pass(sample.applied),
            })
            .collect()
    }

    /// Серия пуста: ни замеров, ни тактов.
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty() && self.ticks.is_empty()
    }
}

/// Строка годна для вердикта: порядок отметок монотонен, проход наблюдался, состояние
/// подтверждает изменение. Иначе серия невалидна — ошибка исполнения, а не число.
pub fn valid(row: &Row) -> bool {
    let sample = &row.sample;
    row.drawn.is_some_and(|drawn| {
        sample.event <= sample.entry && sample.entry <= sample.applied && sample.applied <= drawn
    }) && sample.shown.confirms(sample.change)
}

/// Задержка строки в мс: от события клавиши до прохода отображения.
pub fn latency_ms(row: &Row) -> Option<f64> {
    row.drawn.map(|drawn| (drawn - row.sample.event) * 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPENED: Shown = Shown {
        palette_visible: true,
        palette_key: true,
        owner_key: false,
    };
    const CLOSED: Shown = Shown {
        palette_visible: false,
        palette_key: false,
        owner_key: true,
    };

    fn sample(change: Change, event: f64, shown: Shown) -> Sample {
        Sample {
            change,
            event,
            entry: event + 0.001,
            applied: event + 0.004,
            generation: 7,
            shown,
        }
    }

    /// Проходы отображения в 10.010, 10.020, … : первый не раньше момента.
    fn passes(at: f64) -> Option<f64> {
        let first = (at * 100.0).ceil() / 100.0;
        (first < 20.0).then_some(first)
    }

    #[test]
    fn row_takes_first_pass_after_applied() {
        let mut meter = PaletteLatency::default();
        meter.record(sample(Change::Open, 10.0, OPENED));
        meter.record(sample(Change::Close, 10.5, CLOSED));
        let rows = meter.rows(passes);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].drawn, Some(10.01));
        assert_eq!(rows[1].drawn, Some(10.51));
        assert!(rows.iter().all(valid));
        let ms = latency_ms(&rows[0]).map(|ms| (ms * 1000.0).round() / 1000.0);
        assert_eq!(ms, Some(10.0));
    }

    #[test]
    fn row_without_pass_or_confirmation_is_invalid() {
        let mut meter = PaletteLatency::default();
        meter.record(sample(Change::Open, 25.0, OPENED));
        meter.record(sample(Change::Open, 10.0, CLOSED));
        meter.record(sample(Change::Close, 10.0, OPENED));
        let rows = meter.rows(passes);
        assert_eq!(rows[0].drawn, None);
        assert!(rows.iter().all(|row| !valid(row)));
        assert_eq!(latency_ms(&rows[0]), None);
    }

    #[test]
    fn out_of_order_stamps_are_invalid() {
        let mut broken = sample(Change::Open, 10.0, OPENED);
        broken.entry = 9.0;
        let row = Row {
            sample: broken,
            drawn: Some(10.01),
        };
        assert!(!valid(&row));
    }

    #[test]
    fn ticks_follow_series_switch() {
        let mut meter = PaletteLatency::default();
        meter.tick(1.0);
        assert!(meter.ticks().is_empty());
        meter.reset(true);
        assert!(meter.ticking());
        meter.tick(1.0);
        meter.tick(1.008);
        assert_eq!(meter.ticks(), vec![1.0, 1.008]);
        meter.record(sample(Change::Open, 2.0, OPENED));
        meter.reset(false);
        assert!(meter.is_empty());
        meter.tick(3.0);
        assert!(meter.is_empty());
    }

    #[test]
    fn meter_keeps_last_samples() {
        let mut meter = PaletteLatency::default();
        for index in 0..SAMPLES + 3 {
            meter.record(sample(Change::Open, index as f64, OPENED));
        }
        let rows = meter.rows(|_| None);
        assert_eq!(rows.len(), SAMPLES);
        assert_eq!(rows[0].sample.event, 3.0);
    }
}
