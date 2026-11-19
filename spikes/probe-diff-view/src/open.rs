//! Открытие diff, бюджет 200 мс: фикстуры сценариев `open` и `open-warm`, отметки одного
//! открытия и выбор отметки, которая идёт в вердикт. Модуль не зависит от AppKit.
//!
//! Отметки считаются от `postEvent` клавиши открытия по журналу применённых снимков подсветки и
//! журналу покрытий видимых строк. Предварительная подсветка — первый локальный снимок, который
//! покрывает все видимые строки обеих сторон, исправление — первый такой снимок полного разбора.
//! Каждая отметка — время покрытия видимых строк поколением снимка, а если снимок строк не
//! изменил, время его применения. С бюджетом сравнивается первый экран с правильной подсветкой:
//! предварительный, если он совпал с эталоном по каждой видимой строке, иначе исправление.

use serde::{Deserialize, Serialize};

use crate::fixtures::FixtureName;
use crate::highlight::{LineSet, SideStyles, Sides, Source};
use crate::model::Side;

/// Бюджет открытия: первый экран с правильной подсветкой не позже 200 мс от `postEvent`.
pub const BUDGET_MS: f64 = 200.0;
/// Свежих процессов `open` на фикстуру, вариант и режим. Первый в агрегаты не входит.
pub const RUNS: u32 = 11;
/// Открытий в одном процессе `open-warm`.
pub const WARM_OPENS: usize = 20;

/// Фикстура открытия. `real` готовит `run.sh` из git, остальные генерирует процесс.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OpenFixture {
    Added,
    Modified,
    Sparse,
    Wrap,
    Real,
}

impl OpenFixture {
    pub const ALL: [OpenFixture; 5] = [
        OpenFixture::Added,
        OpenFixture::Modified,
        OpenFixture::Sparse,
        OpenFixture::Wrap,
        OpenFixture::Real,
    ];

    pub fn name(self) -> &'static str {
        match self {
            OpenFixture::Added => "added",
            OpenFixture::Modified => "modified",
            OpenFixture::Sparse => "sparse",
            OpenFixture::Wrap => "wrap",
            OpenFixture::Real => "real",
        }
    }

    pub fn parse(name: &str) -> Option<OpenFixture> {
        OpenFixture::ALL
            .into_iter()
            .find(|fixture| fixture.name() == name)
    }

    /// Обязательная комбинация с вердиктом по бюджету. `wrap` и `real` — наблюдение.
    pub fn mandatory(self) -> bool {
        match self {
            OpenFixture::Added | OpenFixture::Modified | OpenFixture::Sparse => true,
            OpenFixture::Wrap | OpenFixture::Real => false,
        }
    }

    /// Открывается сразу на последнем ханке.
    pub fn at_last_hunk(self) -> bool {
        match self {
            OpenFixture::Sparse => true,
            OpenFixture::Added | OpenFixture::Modified | OpenFixture::Wrap | OpenFixture::Real => {
                false
            }
        }
    }

    /// Генератор синтетической фикстуры, у `real` его нет.
    pub fn synthetic(self) -> Option<FixtureName> {
        match self {
            OpenFixture::Added => Some(FixtureName::Added),
            OpenFixture::Modified => Some(FixtureName::Modified),
            OpenFixture::Sparse => Some(FixtureName::Sparse),
            OpenFixture::Wrap => Some(FixtureName::Wrap),
            OpenFixture::Real => None,
        }
    }
}

/// Какая отметка идёт в вердикт.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EndMark {
    /// Предварительная подсветка совпала с эталоном.
    Preliminary,
    /// Исправление после полного разбора.
    Correction,
}

/// Отметки одного открытия, миллисекунды от `postEvent` клавиши.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OpenMarks {
    /// Модель и индекс высот первого экрана готовы.
    pub model_ms: Option<f64>,
    /// Видимые строки покрыты поколением открытия: первый текст.
    pub first_text_ms: Option<f64>,
    /// Видимые строки покрыты после предварительной подсветки.
    pub preliminary_ms: Option<f64>,
    /// Видимых строк обеих сторон, где предварительная подсветка расходится с эталоном.
    pub preliminary_mismatched: Option<usize>,
    /// Видимые строки покрыты после исправления полным разбором.
    pub correction_ms: Option<f64>,
    /// Видимых строк обеих сторон, где исправление расходится с эталоном.
    pub correction_mismatched: Option<usize>,
    pub end: Option<EndMark>,
    /// Первый экран с правильной подсветкой — отметка вердикта.
    pub end_ms: Option<f64>,
    /// Видимых строк обеих сторон, по которым шла сверка.
    pub visible_lines: usize,
}

impl OpenMarks {
    /// Открытие закончено: исправление полным разбором применено и покрыто.
    pub fn complete(&self) -> bool {
        self.first_text_ms.is_some() && self.correction_ms.is_some()
    }
}

/// Применённый снимок подсветки: источник, поколение после применения, время и стили.
#[derive(Clone, Copy, Debug)]
pub struct Kept<'a> {
    pub source: Source,
    pub generation: u64,
    pub at_ns: u64,
    pub styles: &'a Sides<SideStyles>,
}

/// Покрытие видимых строк поколением учёта.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cover {
    pub generation: u64,
    pub at_ns: u64,
}

/// Вход расчёта отметок одного открытия.
#[derive(Clone, Copy, Debug)]
pub struct OpenInput<'a> {
    pub posted_ns: u64,
    pub model_ns: Option<u64>,
    pub open_generation: Option<u64>,
    pub kept: &'a [Kept<'a>],
    pub coverage: &'a [Cover],
    /// Видимые строки обеих сторон в барьере покрытия.
    pub visible: &'a Sides<LineSet>,
    pub reference: &'a Sides<SideStyles>,
}

/// Первое покрытие поколения не младше `generation`, не раньше `after_ns`.
pub fn covered_since(coverage: &[Cover], generation: u64, after_ns: u64) -> Option<u64> {
    coverage
        .iter()
        .find(|cover| cover.generation >= generation)
        .map(|cover| cover.at_ns.max(after_ns))
}

/// Снимок содержит стили каждой видимой строки обеих сторон.
fn covers(styles: &Sides<SideStyles>, visible: &Sides<LineSet>) -> bool {
    Side::BOTH.into_iter().all(|side| {
        visible
            .get(side)
            .lines()
            .all(|line| styles.get(side).line(line).is_some())
    })
}

/// Видимых строк обеих сторон, где стили снимка расходятся с эталоном.
fn mismatched(styles: &Sides<SideStyles>, input: &OpenInput<'_>) -> usize {
    Side::BOTH
        .into_iter()
        .map(|side| {
            input
                .visible
                .get(side)
                .lines()
                .filter(|&line| input.reference.get(side).line(line) != styles.get(side).line(line))
                .count()
        })
        .sum()
}

/// Отметки открытия. Ошибка — эталон не покрывает видимую строку: сверить экран не с чем.
pub fn marks(input: &OpenInput<'_>) -> Result<OpenMarks, String> {
    for side in Side::BOTH {
        let missing: Vec<u32> = input
            .visible
            .get(side)
            .lines()
            .filter(|&line| input.reference.get(side).line(line).is_none())
            .take(6)
            .collect();
        if !missing.is_empty() {
            return Err(format!(
                "эталон не покрывает видимые строки стороны {side:?}: {missing:?}"
            ));
        }
    }
    let ms = |at: u64| at.saturating_sub(input.posted_ns) as f64 / 1e6;
    let first = |source: Source| {
        input
            .kept
            .iter()
            .find(|kept| kept.source == source && covers(kept.styles, input.visible))
    };
    let mark = |kept: &Kept<'_>| {
        covered_since(
            input.coverage,
            kept.generation,
            kept.at_ns.max(input.posted_ns),
        )
        .map(ms)
    };
    let preliminary = first(Source::Local);
    let correction = first(Source::Full);
    let mut marks = OpenMarks {
        model_ms: input.model_ns.map(ms),
        first_text_ms: input
            .open_generation
            .and_then(|generation| covered_since(input.coverage, generation, input.posted_ns))
            .map(ms),
        preliminary_ms: preliminary.and_then(mark),
        preliminary_mismatched: preliminary.map(|kept| mismatched(kept.styles, input)),
        correction_ms: correction.and_then(mark),
        correction_mismatched: correction.map(|kept| mismatched(kept.styles, input)),
        end: None,
        end_ms: None,
        visible_lines: input.visible.old.len() + input.visible.new.len(),
    };
    let end = match (marks.preliminary_ms, marks.preliminary_mismatched) {
        (Some(at), Some(0)) => Some((EndMark::Preliminary, at)),
        _ => match (marks.correction_ms, marks.correction_mismatched) {
            (Some(at), Some(0)) => Some((EndMark::Correction, at)),
            _ => None,
        },
    };
    marks.end = end.map(|(end, _)| end);
    marks.end_ms = end.map(|(_, at)| at);
    Ok(marks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::highlight::{Run, TokenClass};

    fn styles(lines: &[(u32, TokenClass)]) -> SideStyles {
        let runs: Vec<(u32, Vec<Run>)> = lines
            .iter()
            .map(|&(line, class)| {
                (
                    line,
                    vec![Run {
                        start: 0,
                        end: 4,
                        class,
                    }],
                )
            })
            .collect();
        SideStyles::from_lines(runs.iter().map(|(line, runs)| (*line, runs.as_slice()))).unwrap()
    }

    fn sides(old: &[(u32, TokenClass)], new: &[(u32, TokenClass)]) -> Sides<SideStyles> {
        Sides {
            old: styles(old),
            new: styles(new),
        }
    }

    const MS: u64 = 1_000_000;

    fn cover(generation: u64, at_ms: u64) -> Cover {
        Cover {
            generation,
            at_ns: at_ms * MS,
        }
    }

    struct Case {
        reference: Sides<SideStyles>,
        visible: Sides<LineSet>,
        empty: Sides<SideStyles>,
        right: Sides<SideStyles>,
        wrong: Sides<SideStyles>,
    }

    fn case() -> Case {
        use TokenClass::{Comment, Keyword, String};
        Case {
            reference: sides(
                &[(1, Comment), (2, Comment), (9, Keyword)],
                &[(1, String), (2, String), (9, Keyword)],
            ),
            visible: Sides {
                old: LineSet::from_lines([1, 2]),
                new: LineSet::from_lines([1, 2]),
            },
            empty: Sides::default(),
            right: sides(&[(1, Comment), (2, Comment)], &[(1, String), (2, String)]),
            wrong: sides(&[(1, Keyword), (2, Comment)], &[(1, Keyword), (2, String)]),
        }
    }

    fn input<'a>(case: &'a Case, kept: &'a [Kept<'a>], coverage: &'a [Cover]) -> OpenInput<'a> {
        OpenInput {
            posted_ns: 100 * MS,
            model_ns: Some(110 * MS),
            open_generation: Some(5),
            kept,
            coverage,
            visible: &case.visible,
            reference: &case.reference,
        }
    }

    fn kept(source: Source, generation: u64, at_ms: u64, styles: &Sides<SideStyles>) -> Kept<'_> {
        Kept {
            source,
            generation,
            at_ns: at_ms * MS,
            styles,
        }
    }

    #[test]
    fn matching_preliminary_is_the_verdict_mark() {
        let case = case();
        // Пустой локальный снимок запроса открытия экран не покрывает и отметкой не считается.
        let kept = [
            kept(Source::Local, 4, 101, &case.empty),
            kept(Source::Local, 6, 115, &case.right),
            kept(Source::Full, 6, 480, &case.right),
        ];
        let coverage = [cover(5, 112), cover(6, 118)];
        let marks = marks(&input(&case, &kept, &coverage)).unwrap();
        assert_eq!(marks.model_ms, Some(10.0));
        assert_eq!(marks.first_text_ms, Some(12.0));
        assert_eq!(marks.preliminary_ms, Some(18.0));
        assert_eq!(marks.preliminary_mismatched, Some(0));
        // Полный разбор строк не изменил: экран верен с применения снимка.
        assert_eq!(marks.correction_ms, Some(380.0));
        assert_eq!(marks.correction_mismatched, Some(0));
        assert_eq!(marks.end, Some(EndMark::Preliminary));
        assert_eq!(marks.end_ms, Some(18.0));
        assert_eq!(marks.visible_lines, 4);
        assert!(marks.complete());
    }

    #[test]
    fn wrong_preliminary_hands_the_verdict_to_the_correction() {
        let case = case();
        let kept = [
            kept(Source::Local, 6, 115, &case.wrong),
            kept(Source::Full, 7, 280, &case.right),
        ];
        let coverage = [cover(5, 112), cover(6, 118), cover(7, 290)];
        let marks = marks(&input(&case, &kept, &coverage)).unwrap();
        assert_eq!(marks.preliminary_ms, Some(18.0));
        assert_eq!(marks.preliminary_mismatched, Some(2));
        assert_eq!(marks.correction_ms, Some(190.0));
        assert_eq!(marks.end, Some(EndMark::Correction));
        assert_eq!(marks.end_ms, Some(190.0));
    }

    #[test]
    fn correction_waits_for_its_own_coverage() {
        let case = case();
        let kept = [
            kept(Source::Local, 6, 115, &case.wrong),
            kept(Source::Full, 7, 280, &case.right),
        ];
        let coverage = [cover(5, 112), cover(6, 118)];
        let marks = marks(&input(&case, &kept, &coverage)).unwrap();
        assert_eq!(marks.correction_ms, None);
        assert_eq!(marks.end_ms, None);
        assert!(!marks.complete());
    }

    #[test]
    fn full_parse_first_is_the_correction() {
        let case = case();
        let kept = [kept(Source::Full, 6, 105, &case.right)];
        let coverage = [cover(5, 103), cover(6, 108)];
        let marks = marks(&input(&case, &kept, &coverage)).unwrap();
        assert_eq!(marks.preliminary_ms, None);
        assert_eq!(marks.preliminary_mismatched, None);
        assert_eq!(marks.end, Some(EndMark::Correction));
        assert_eq!(marks.end_ms, Some(8.0));
    }

    #[test]
    fn wrong_correction_leaves_no_verdict_mark() {
        let case = case();
        let kept = [
            kept(Source::Local, 6, 115, &case.wrong),
            kept(Source::Full, 6, 280, &case.wrong),
        ];
        let coverage = [cover(5, 112), cover(6, 118)];
        let marks = marks(&input(&case, &kept, &coverage)).unwrap();
        assert!(marks.complete());
        assert_eq!(marks.correction_mismatched, Some(2));
        assert_eq!(marks.end, None);
        assert_eq!(marks.end_ms, None);
    }

    #[test]
    fn coverage_before_the_key_does_not_count() {
        let case = case();
        let kept = [kept(Source::Full, 6, 105, &case.right)];
        // Прежнее открытие покрыло поколение 6 до клавиши: отметка не раньше `postEvent`.
        let coverage = [cover(6, 50)];
        let marks = marks(&input(&case, &kept, &coverage)).unwrap();
        assert_eq!(marks.first_text_ms, Some(0.0));
        assert_eq!(marks.correction_ms, Some(5.0));
    }

    #[test]
    fn reference_must_cover_the_screen() {
        let case = case();
        let visible = Sides {
            old: LineSet::from_lines([1, 3]),
            new: LineSet::from_lines([1]),
        };
        let input = OpenInput {
            visible: &visible,
            ..input(&case, &[], &[])
        };
        let error = marks(&input).unwrap_err();
        assert!(error.contains("[3]"), "{error}");
    }

    #[test]
    fn fixture_names_and_roles() {
        for fixture in OpenFixture::ALL {
            assert_eq!(OpenFixture::parse(fixture.name()), Some(fixture));
            assert_eq!(
                fixture.synthetic().map(FixtureName::as_str),
                (fixture != OpenFixture::Real).then_some(fixture.name())
            );
        }
        assert_eq!(OpenFixture::parse("threads"), None);
        let mandatory: Vec<&str> = OpenFixture::ALL
            .into_iter()
            .filter(|fixture| fixture.mandatory())
            .map(OpenFixture::name)
            .collect();
        assert_eq!(mandatory, ["added", "modified", "sparse"]);
        assert!(OpenFixture::Sparse.at_last_hunk());
        assert!(!OpenFixture::Added.at_last_hunk());
    }
}
