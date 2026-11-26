//! Метки `f` (ADR-0010, «Уточнено в P1.6 (планирование): наложения»): раскладка меток видимых
//! целей и сужение набранным префиксом. Буква `f` остаётся в алфавите, отмена — только `Esc`.

/// Алфавит меток (прототип `overlays.js:266`).
pub const HINT_ALPHABET: [char; 9] = ['a', 's', 'd', 'f', 'j', 'k', 'l', 'g', 'h'];
/// Наибольшее число целей: остальные меток не получают.
pub const MAX_HINT_TARGETS: usize = 240;

/// Метки `count` целей по порядку (не больше [`MAX_HINT_TARGETS`]): одинаковой длины 1, 2 или 3,
/// поэтому ни одна метка не начало другой (формула прототипа `overlays.js:267`–`:271`).
pub fn hint_labels(count: usize) -> Vec<String> {
    let count = count.min(MAX_HINT_TARGETS);
    let base = HINT_ALPHABET.len();
    let width: u32 = if count <= base {
        1
    } else if count <= base * base {
        2
    } else {
        3
    };
    (0..count)
        .map(|n| {
            (0..width)
                .rev()
                .map(|place| HINT_ALPHABET[n / base.pow(place) % base])
                .collect()
        })
        .collect()
}

/// Что сделал набранный знак.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HintStep {
    /// Остались метки с набранным началом.
    Narrowed,
    /// Метка набрана целиком: нажать цель с этим исходным номером.
    Activate(usize),
    /// Совпадений нет: метки закрываются без действия.
    Cancel,
}

/// Открытые метки: метка каждой цели и набранная часть.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HintLabels {
    labels: Vec<String>,
    typed: String,
}

impl HintLabels {
    pub fn new(targets: usize) -> Self {
        Self {
            labels: hint_labels(targets),
            typed: String::new(),
        }
    }

    /// Метки по исходному номеру цели.
    pub fn labels(&self) -> &[String] {
        &self.labels
    }

    pub fn typed(&self) -> &str {
        &self.typed
    }

    /// Метки с набранным началом и исходные номера их целей: номер при сужении не меняется.
    pub fn visible(&self) -> impl Iterator<Item = (usize, &str)> + '_ {
        self.labels
            .iter()
            .enumerate()
            .filter(|(_, label)| label.starts_with(&self.typed))
            .map(|(index, label)| (index, label.as_str()))
    }

    /// Набранный знак. Цель нажимается, только когда метка набрана целиком.
    pub fn press(&mut self, sign: char) -> HintStep {
        self.typed.push(sign);
        let mut hits = self.visible();
        match (hits.next(), hits.next()) {
            (None, _) => HintStep::Cancel,
            (Some((index, label)), None) if label == self.typed => HintStep::Activate(index),
            _ => HintStep::Narrowed,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn type_label(count: usize, label: &str) -> Vec<HintStep> {
        let mut hints = HintLabels::new(count);
        label.chars().map(|sign| hints.press(sign)).collect()
    }

    #[test]
    fn labels_keep_one_length_per_count() {
        assert_eq!(hint_labels(0), Vec::<String>::new());
        assert_eq!(hint_labels(1), ["a"]);
        assert_eq!(
            hint_labels(9),
            ["a", "s", "d", "f", "j", "k", "l", "g", "h"]
        );
        let ten = hint_labels(10);
        assert_eq!(ten[..3], ["aa", "as", "ad"]);
        assert_eq!(ten[9], "sa");
        let full = hint_labels(81);
        assert_eq!(full[80], "hh");
        assert!(full.iter().all(|label| label.len() == 2));
        let three = hint_labels(82);
        assert_eq!(three[0], "aaa");
        assert_eq!(three[81], "saa");
        assert!(three.iter().all(|label| label.len() == 3));
        let most = hint_labels(240);
        assert_eq!(most.len(), MAX_HINT_TARGETS);
        assert_eq!(most[239], "dhk");
        assert_eq!(hint_labels(500).len(), MAX_HINT_TARGETS);
    }

    #[test]
    fn every_label_is_unique_and_reachable_for_every_count() {
        for count in 1..=MAX_HINT_TARGETS {
            let labels = hint_labels(count);
            assert_eq!(labels.iter().collect::<HashSet<_>>().len(), count);
            for (index, label) in labels.iter().enumerate() {
                let steps = type_label(count, label);
                let (last, before) = steps.split_last().unwrap();
                assert_eq!(*last, HintStep::Activate(index), "{count}: {label}");
                assert!(before.iter().all(|step| *step == HintStep::Narrowed));
            }
        }
    }

    #[test]
    fn labels_on_f_are_reached_not_cancelled() {
        // От 4 до 9 целей у четвёртой цели метка «f».
        for count in 4..=9 {
            assert_eq!(hint_labels(count)[3], "f");
            assert_eq!(type_label(count, "f"), [HintStep::Activate(3)]);
        }
        // 29 целей: «fa» у 28-й, набор `f`, `a` нажимает именно её.
        let labels = hint_labels(29);
        assert_eq!(labels[27], "fa");
        assert_eq!(labels[28], "fs");
        assert_eq!(
            type_label(29, "fa"),
            [HintStep::Narrowed, HintStep::Activate(27)]
        );
        // Метки длиной 3 начинаются с `f` только с 244-й цели, предел — 240.
        assert!(hint_labels(240).iter().all(|label| !label.starts_with('f')));
    }

    #[test]
    fn typing_narrows_and_keeps_the_target_numbers() {
        let mut hints = HintLabels::new(29);
        assert_eq!(hints.press('s'), HintStep::Narrowed);
        assert_eq!(hints.typed(), "s");
        let visible: Vec<(usize, &str)> = hints.visible().collect();
        assert_eq!(visible.len(), 9);
        assert_eq!(visible[0], (9, "sa"));
        assert_eq!(visible[8], (17, "sh"));
        assert_eq!(hints.press('h'), HintStep::Activate(17));

        // Единственная оставшаяся метка не нажимается, пока не набрана целиком.
        let mut ten = HintLabels::new(10);
        assert_eq!(ten.press('s'), HintStep::Narrowed);
        assert_eq!(ten.visible().collect::<Vec<_>>(), [(9, "sa")]);
        assert_eq!(ten.press('a'), HintStep::Activate(9));
    }

    #[test]
    fn typing_without_a_match_cancels() {
        assert_eq!(type_label(29, "x"), [HintStep::Cancel]);
        assert_eq!(type_label(29, "sx"), [HintStep::Narrowed, HintStep::Cancel]);
        // У 29 целей первые буквы меток — только `a`, `s`, `d`, `f`.
        assert_eq!(type_label(29, "j"), [HintStep::Cancel]);
        assert_eq!(type_label(3, "F"), [HintStep::Cancel]);
        assert_eq!(type_label(0, "a"), [HintStep::Cancel]);
        assert_eq!(HintLabels::new(0).visible().count(), 0);
    }
}
