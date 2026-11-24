//! Проверка представления плагина по фикстурам раскладки (ADR-0008, «Проверка представления»):
//! обёртка [`shellio_layout::contract::check`], которая зовёт `adapt` модели и судит результат.

use std::collections::BTreeSet;

use shellio_layout::contract::{Fixture, Report};
use shellio_layout::{Geometry, Rect, SizeClass, Zone};

use crate::manifest::Manifest;
use crate::model::{DynModel, Presentation};

/// Вариант представления для каждого класса размера.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SizeVariants {
    pub narrow: &'static str,
    pub regular: &'static str,
    pub wide: &'static str,
    pub short_bar: &'static str,
}

impl SizeVariants {
    #[must_use]
    pub const fn get(&self, size: SizeClass) -> &'static str {
        match size {
            SizeClass::Narrow => self.narrow,
            SizeClass::Regular => self.regular,
            SizeClass::Wide => self.wide,
            SizeClass::ShortBar => self.short_bar,
        }
    }
}

/// Таблица плагина: ожидаемый вариант представления для каждой пары зоны и класса размера.
/// Плагин объявляет её при регистрации отдельно от `adapt`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VariantTable {
    pub left: SizeVariants,
    pub right: SizeVariants,
    pub center: SizeVariants,
    pub bottom: SizeVariants,
}

impl VariantTable {
    #[must_use]
    pub const fn get(&self, zone: Zone, size: SizeClass) -> &'static str {
        match zone {
            Zone::Left => self.left.get(size),
            Zone::Right => self.right.get(size),
            Zone::Center => self.center.get(size),
            Zone::Bottom => self.bottom.get(size),
        }
    }
}

/// Прогоняет `adapt` модели по фикстурам с `min_size` её манифеста и судит каждое представление
/// функцией [`judge`]. Фикстура ниже `min_size` даёт заглушку без вызова `adapt`.
pub fn check(
    model: &DynModel,
    state: &str,
    fixtures: &[Fixture],
    variants: &VariantTable,
) -> Report {
    let manifest = model.manifest();
    shellio_layout::contract::check(fixtures, manifest.min_size, |geometry| {
        let presentation = model
            .adapt(geometry, state)
            .map_err(|refusal| refusal.message)?;
        judge(
            &presentation,
            geometry,
            manifest,
            variants.get(geometry.zone, geometry.size),
        )
    })
}

/// Допуск сравнения кадров в точках: сумма долей ширины не обязана сходиться побитно.
const TOLERANCE: f64 = 1e-6;

/// Судит представление: вариант совпадает с ожидаемым; каждый регион назван, лежит внутри тела, имеет
/// положительную площадь и не перекрывает другой; `reach` называет каждое действие манифеста ровно
/// один раз и ничего сверх них.
pub fn judge(
    presentation: &Presentation,
    geometry: &Geometry,
    manifest: &Manifest,
    expected: &str,
) -> Result<(), String> {
    if presentation.variant != expected {
        return Err(format!(
            "вариант «{}», по таблице плагина — «{expected}»",
            presentation.variant
        ));
    }

    let mut names = BTreeSet::new();
    for region in &presentation.regions {
        let Rect {
            x,
            y,
            width,
            height,
        } = region.frame;
        let name = region.name;
        if name.is_empty() {
            return Err("регион без имени".to_owned());
        }
        if !names.insert(name) {
            return Err(format!("имя региона «{name}» повторяется"));
        }
        if ![x, y, width, height].into_iter().all(f64::is_finite) || width <= 0.0 || height <= 0.0 {
            return Err(format!(
                "регион «{name}» без положительной площади: {width}×{height}"
            ));
        }
        let inside = x >= -TOLERANCE
            && y >= -TOLERANCE
            && region.frame.right() <= geometry.width + TOLERANCE
            && region.frame.bottom() <= geometry.height + TOLERANCE;
        if !inside {
            return Err(format!(
                "регион «{name}» ({x}, {y}, {width}×{height}) выходит за тело {}×{}",
                geometry.width, geometry.height
            ));
        }
    }
    for (index, a) in presentation.regions.iter().enumerate() {
        for b in &presentation.regions[index + 1..] {
            if overlap(&a.frame, &b.frame) {
                return Err(format!("регионы «{}» и «{}» перекрываются", a.name, b.name));
            }
        }
    }

    let mut reached = BTreeSet::new();
    for reach in &presentation.reach {
        if !manifest.has_command(reach.action) {
            return Err(format!(
                "действия «{}» из reach нет в манифесте",
                reach.action
            ));
        }
        if !reached.insert(reach.action) {
            return Err(format!("действие «{}» в reach дважды", reach.action));
        }
    }
    if let Some(spec) = manifest
        .commands
        .iter()
        .find(|spec| !reached.contains(spec.name))
    {
        return Err(format!("действие «{}» недостижимо", spec.name));
    }
    Ok(())
}

fn overlap(a: &Rect, b: &Rect) -> bool {
    a.x < b.right() - TOLERANCE
        && b.x < a.right() - TOLERANCE
        && a.y < b.bottom() - TOLERANCE
        && b.y < a.bottom() - TOLERANCE
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use shellio_layout::MinSize;
    use shellio_layout::contract::{Outcome, fixtures};

    use super::*;
    use crate::fake::{Fake, Flaw, MANIFEST, METRICS, VARIANTS, manifest_with};
    use crate::model::{Reach, Region, Via};

    const STATE: &str = r#"{"text":"a","bumps":0}"#;

    fn admitted(fixtures: &[Fixture], min_size: MinSize) -> usize {
        fixtures
            .iter()
            .filter(|fixture| min_size.admits(fixture.geometry.width, fixture.geometry.height))
            .count()
    }

    fn run(flaw: Flaw) -> Report {
        check(
            &DynModel::new(Fake::flawed(flaw)),
            STATE,
            &fixtures(&METRICS),
            &VARIANTS,
        )
    }

    /// Каждая фикстура не ниже `min_size` отвергнута с этой причиной.
    fn every_adapted_fails(flaw: Flaw, reason: &str) {
        let all = fixtures(&METRICS);
        let report = run(flaw);
        assert_eq!(
            report.failures().count(),
            admitted(&all, MinSize::DEFAULT),
            "{flaw:?}\n{report}"
        );
        for entry in report.failures() {
            assert!(
                matches!(&entry.outcome, Outcome::Failed(text) if text.contains(reason)),
                "{flaw:?}: {:?}",
                entry.outcome
            );
        }
    }

    #[test]
    fn fake_model_passes_and_adapt_runs_once_per_admitted_fixture() {
        let all = fixtures(&METRICS);
        let fake = Fake::new();
        let calls = fake.adapt_calls();
        let report = check(&DynModel::new(fake), STATE, &all, &VARIANTS);
        assert!(report.passed(), "{report}");
        assert_eq!(report.entries.len(), all.len());
        let stubs = report
            .entries
            .iter()
            .filter(|entry| entry.outcome == Outcome::Stub)
            .count();
        assert!(stubs > 0, "есть фикстура ниже min_size");
        assert_eq!(
            calls.load(Ordering::Relaxed),
            admitted(&all, MinSize::DEFAULT)
        );
        assert_eq!(calls.load(Ordering::Relaxed), all.len() - stubs);
    }

    #[test]
    fn min_size_of_the_manifest_decides_the_stub() {
        let all = fixtures(&METRICS);
        let big = MinSize {
            width: 500.0,
            height: 300.0,
        };
        let fake = Fake::with_manifest(manifest_with(|manifest| manifest.min_size = big));
        let calls = fake.adapt_calls();
        let report = check(&DynModel::new(fake), STATE, &all, &VARIANTS);
        assert!(report.passed(), "{report}");
        let adapted = admitted(&all, big);
        assert!(adapted > 0 && adapted < admitted(&all, MinSize::DEFAULT));
        assert_eq!(calls.load(Ordering::Relaxed), adapted);
    }

    #[test]
    fn one_variant_everywhere_fails_where_the_table_differs() {
        let all = fixtures(&METRICS);
        let report = run(Flaw::OneVariant);
        assert!(!report.passed());
        for entry in &report.entries {
            let geometry = entry.geometry;
            let expected = VARIANTS.get(geometry.zone, geometry.size);
            let outcome = if !MinSize::DEFAULT.admits(geometry.width, geometry.height) {
                Outcome::Stub
            } else if expected == "drill" {
                Outcome::Adapted
            } else {
                Outcome::Failed(format!(
                    "вариант «drill», по таблице плагина — «{expected}»"
                ))
            };
            assert_eq!(entry.outcome, outcome, "{}", entry.fixture);
        }
        let failed = report.failures().count();
        assert!(failed > 0 && failed < admitted(&all, MinSize::DEFAULT));
    }

    #[test]
    fn regions_outside_overlapping_or_empty_are_rejected() {
        every_adapted_fails(Flaw::Outside, "выходит за тело");
        every_adapted_fails(Flaw::Negative, "выходит за тело");
        every_adapted_fails(Flaw::Overlap, "перекрываются");
        every_adapted_fails(Flaw::ZeroArea, "без положительной площади");
        every_adapted_fails(Flaw::SameName, "повторяется");
    }

    #[test]
    fn reach_must_equal_the_actions_of_the_manifest() {
        every_adapted_fails(Flaw::Unreachable, "действие «fake.bump» недостижимо");
        every_adapted_fails(
            Flaw::Unlisted,
            "действия «fake.hidden» из reach нет в манифесте",
        );
        every_adapted_fails(Flaw::ReachTwice, "действие «fake.fill» в reach дважды");
    }

    #[test]
    fn unreadable_state_and_panic_are_reported_per_fixture() {
        let all = fixtures(&METRICS);
        let report = check(&DynModel::new(Fake::new()), "{", &all, &VARIANTS);
        assert_eq!(report.failures().count(), admitted(&all, MinSize::DEFAULT));
        assert!(report.to_string().contains("данные панели не разобраны"));

        let report = run(Flaw::PanicInBottom);
        let failed: Vec<_> = report.failures().collect();
        assert!(!failed.is_empty());
        for entry in failed {
            assert_eq!(entry.geometry.zone, Zone::Bottom);
            assert_eq!(
                entry.outcome,
                Outcome::Failed("паника: нижняя зона не поддержана".to_owned())
            );
        }
    }

    #[test]
    fn judge_allows_touching_regions_and_a_body_without_regions() {
        let geometry = Geometry::of(Zone::Center, 600.0, 400.0);
        let reach = MANIFEST
            .commands
            .iter()
            .map(|spec| Reach {
                action: spec.name,
                via: Via::Direct,
            })
            .collect::<Vec<_>>();
        let region = |name, x, y, width, height| Region {
            name,
            frame: Rect {
                x,
                y,
                width,
                height,
            },
        };
        let touching = Presentation {
            variant: "split",
            regions: vec![
                region("list", 0.0, 0.0, 200.0, 400.0),
                region("details", 200.0, 0.0, 400.0, 200.0),
                region("footer", 200.0, 200.0, 400.0, 200.0),
            ],
            reach: reach.clone(),
        };
        assert_eq!(judge(&touching, &geometry, &MANIFEST, "split"), Ok(()));

        let bare = Presentation {
            variant: "split",
            regions: Vec::new(),
            reach: reach.clone(),
        };
        assert_eq!(judge(&bare, &geometry, &MANIFEST, "split"), Ok(()));

        for (frame, reason) in [
            (region("", 0.0, 0.0, 10.0, 10.0), "без имени"),
            (region("a", 0.0, 0.0, f64::NAN, 10.0), "площади"),
            (region("a", 0.0, 0.0, 10.0, -1.0), "площади"),
            (region("a", 0.0, 0.0, f64::INFINITY, 10.0), "площади"),
            (region("a", 591.0, 0.0, 10.0, 10.0), "выходит за тело"),
            (region("a", 0.0, -0.5, 10.0, 10.0), "выходит за тело"),
        ] {
            let presentation = Presentation {
                variant: "split",
                regions: vec![frame],
                reach: reach.clone(),
            };
            let error = judge(&presentation, &geometry, &MANIFEST, "split").unwrap_err();
            assert!(error.contains(reason), "{error}");
        }

        let nested = Presentation {
            variant: "split",
            regions: vec![
                region("outer", 0.0, 0.0, 300.0, 300.0),
                region("inner", 100.0, 100.0, 50.0, 50.0),
            ],
            reach,
        };
        assert_eq!(
            judge(&nested, &geometry, &MANIFEST, "split"),
            Err("регионы «outer» и «inner» перекрываются".to_owned())
        );
    }

    #[test]
    fn variant_table_is_indexed_by_zone_and_size() {
        assert_eq!(VARIANTS.get(Zone::Left, SizeClass::Wide), "split");
        assert_eq!(VARIANTS.get(Zone::Right, SizeClass::Narrow), "drill");
        assert_eq!(VARIANTS.get(Zone::Center, SizeClass::Regular), "split");
        assert_eq!(VARIANTS.get(Zone::Bottom, SizeClass::Regular), "bar");
        assert_eq!(VARIANTS.get(Zone::Bottom, SizeClass::ShortBar), "bar");
    }
}
