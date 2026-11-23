//! Контрактный тест геометрии (ADR-0009, «Контрактный тест геометрии»): фикстуры частей всех зон и
//! классов размера и обвязка проверки `adapt` с отчётом по каждой фикстуре. P1.5 прогоняет через неё
//! плагины. Фикстуры строятся настоящей [`geometry`] из раскладок, поэтому следуют за метриками темы.

use std::any::Any;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::geometry::{Geometry, LayoutMetrics, Viewport, geometry};
use crate::model::{Layout, MinSize, Part, SHARE_TOTAL, Zone};

/// Геометрия тела части, которую получает `adapt`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fixture {
    pub name: &'static str,
    pub geometry: Geometry,
}

const SMALL: Viewport = Viewport {
    width: 1180,
    height: 720,
};
const LARGE: Viewport = Viewport {
    width: 1440,
    height: 900,
};

const NAVIGATOR: u32 = 1;
const TERMINAL: u32 = 2;
/// Первая панель частей фикстуры. Части получают панели `PROBE`, `PROBE + 1`, …
const PROBE: u32 = 100;

/// Раскладка фикстуры: колонка навигатора, центр с терминалом и части зоны `zone` с панелями
/// `PROBE…` и долями `shares`.
fn layout(zone: Zone, shares: &[u16]) -> Layout<u32> {
    let mut layout = Layout::default_for(NAVIGATOR, TERMINAL);
    let parts = shares
        .iter()
        .zip(PROBE..)
        .map(|(&share, panel)| Part::single(panel, share))
        .collect();
    layout.zones.get_mut(zone).parts = parts;
    layout.focus = vec![if shares.is_empty() { TERMINAL } else { PROBE }];
    layout
}

fn without_center(mut layout: Layout<u32>) -> Layout<u32> {
    layout.zones.center.parts.clear();
    layout
}

fn halves() -> [u16; 2] {
    [SHARE_TOTAL / 2, SHARE_TOTAL / 2]
}

fn thirds() -> [u16; 3] {
    [333, 333, 334]
}

fn quarters() -> [u16; 4] {
    [250, 250, 250, 250]
}

/// Фикстуры: каждая пара зоны и класса размера, половина и треть центра, половина боковой зоны,
/// `zoom` и часть ниже `min_size` по умолчанию.
#[must_use]
pub fn fixtures(metrics: &LayoutMetrics) -> Vec<Fixture> {
    let whole = [SHARE_TOTAL];
    let mut right_open = layout(Zone::Center, &quarters());
    right_open.zones.right.parts = vec![Part::single(200, SHARE_TOTAL)];
    let mut tall_bottom = layout(Zone::Center, &whole);
    tall_bottom.zones.bottom.parts = vec![Part::single(200, SHARE_TOTAL)];
    tall_bottom.sizes.bottom = 500;
    let mut zoomed = layout(Zone::Right, &whole);
    zoomed.zones.center.parts = vec![Part::single(TERMINAL, 500), Part::single(201, 500)];
    zoomed.zoomed = Some(PROBE);

    let cases: Vec<(&'static str, Viewport, Layout<u32>)> = vec![
        ("navigator-column", SMALL, layout(Zone::Left, &[])),
        ("left-plate", SMALL, layout(Zone::Left, &whole)),
        ("left-half", SMALL, layout(Zone::Left, &halves())),
        ("left-third", SMALL, layout(Zone::Left, &thirds())),
        (
            "left-plate-free",
            SMALL,
            without_center(layout(Zone::Left, &whole)),
        ),
        (
            "left-plate-free-wide",
            LARGE,
            without_center(layout(Zone::Left, &whole)),
        ),
        ("right", SMALL, layout(Zone::Right, &whole)),
        ("right-half", SMALL, layout(Zone::Right, &halves())),
        ("right-third", SMALL, layout(Zone::Right, &thirds())),
        (
            "right-free",
            SMALL,
            without_center(layout(Zone::Right, &whole)),
        ),
        (
            "right-free-wide",
            LARGE,
            without_center(layout(Zone::Right, &whole)),
        ),
        ("center", SMALL, layout(Zone::Center, &whole)),
        ("center-wide", LARGE, layout(Zone::Center, &whole)),
        ("center-half", SMALL, layout(Zone::Center, &halves())),
        ("center-half-wide", LARGE, layout(Zone::Center, &halves())),
        ("center-third", SMALL, layout(Zone::Center, &thirds())),
        ("center-quarter-tight", SMALL, right_open),
        ("center-over-tall-bottom", SMALL, tall_bottom),
        ("bottom", SMALL, layout(Zone::Bottom, &whole)),
        ("bottom-half", SMALL, layout(Zone::Bottom, &halves())),
        (
            "bottom-free",
            SMALL,
            without_center(layout(Zone::Bottom, &whole)),
        ),
        (
            "bottom-free-half",
            SMALL,
            without_center(layout(Zone::Bottom, &halves())),
        ),
        (
            "bottom-free-wide",
            LARGE,
            without_center(layout(Zone::Bottom, &whole)),
        ),
        ("zoom", SMALL, zoomed),
    ];

    cases
        .into_iter()
        .filter_map(|(name, viewport, layout)| {
            let frames = geometry(&layout, viewport, metrics, |_| MinSize::DEFAULT);
            let geometry = match name {
                "navigator-column" => frames.navigator.map(|column| column.geometry),
                "zoom" => frames.zoomed.map(|zoomed| zoomed.geometry),
                _ => frames.part_of(&PROBE).map(|part| part.geometry),
            }?;
            Some(Fixture { name, geometry })
        })
        .collect()
}

/// Исход фикстуры.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// `adapt` принял геометрию.
    Adapted,
    /// Тело меньше `min_size`: хост показывает заглушку «Развернуть», `adapt` не вызывается.
    Stub,
    /// `adapt` отказал или запаниковал.
    Failed(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub fixture: &'static str,
    pub geometry: Geometry,
    pub outcome: Outcome,
}

/// Отчёт по каждой фикстуре.
#[derive(Clone, Debug, PartialEq)]
pub struct Report {
    pub entries: Vec<Entry>,
}

impl Report {
    #[must_use]
    pub fn passed(&self) -> bool {
        self.failures().next().is_none()
    }

    pub fn failures(&self) -> impl Iterator<Item = &Entry> {
        self.entries
            .iter()
            .filter(|entry| matches!(entry.outcome, Outcome::Failed(_)))
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for entry in &self.entries {
            let g = entry.geometry;
            let outcome = match &entry.outcome {
                Outcome::Adapted => "ok".to_owned(),
                Outcome::Stub => "заглушка «Развернуть»".to_owned(),
                Outcome::Failed(reason) => format!("FAIL: {reason}"),
            };
            writeln!(
                f,
                "{:<26} {:<7} {:<10} {:>5}×{:<5} {outcome}",
                entry.fixture,
                format!("{:?}", g.zone),
                format!("{:?}", g.size),
                g.width,
                g.height
            )?;
        }
        Ok(())
    }
}

/// Прогоняет `adapt` по фикстурам. Фикстура с телом меньше `min_size` даёт [`Outcome::Stub`] без
/// вызова `adapt`. Отказ и паника `adapt` записываются в отчёт этой фикстуры, остальные фикстуры
/// проверяются дальше.
pub fn check(
    fixtures: &[Fixture],
    min_size: MinSize,
    adapt: impl Fn(&Geometry) -> Result<(), String>,
) -> Report {
    let entries = fixtures
        .iter()
        .map(|fixture| {
            let geometry = fixture.geometry;
            let outcome = if !min_size.admits(geometry.width, geometry.height) {
                Outcome::Stub
            } else {
                match catch_unwind(AssertUnwindSafe(|| adapt(&geometry))) {
                    Ok(Ok(())) => Outcome::Adapted,
                    Ok(Err(reason)) => Outcome::Failed(reason),
                    Err(payload) => {
                        Outcome::Failed(format!("паника: {}", panic_message(&*payload)))
                    }
                }
            };
            Entry {
                fixture: fixture.name,
                geometry,
                outcome,
            }
        })
        .collect();
    Report { entries }
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|message| (*message).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "без сообщения".to_owned())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::geometry::SizeClass;

    const METRICS: LayoutMetrics = LayoutMetrics {
        titlebar: 40,
        tabbar: 34,
        gap: 6,
        line: 1,
        status_bar: 26,
        drop_edge: 24,
    };

    /// Представление демонстрационной панели.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Presentation {
        /// Список, детали вглубь.
        ListDeep,
        ListAndDetail,
        Full,
        /// Компактная полоса в ширину.
        Strip,
    }

    impl Presentation {
        fn needs(self) -> (f64, f64) {
            match self {
                Self::ListDeep | Self::Strip => (200.0, 120.0),
                Self::ListAndDetail | Self::Full => (480.0, 240.0),
            }
        }
    }

    fn adapt(geometry: &Geometry) -> Presentation {
        match geometry.zone {
            Zone::Left | Zone::Right => match geometry.size {
                SizeClass::Narrow => Presentation::ListDeep,
                SizeClass::Regular | SizeClass::Wide => Presentation::ListAndDetail,
                SizeClass::ShortBar => Presentation::Strip,
            },
            Zone::Center => match geometry.size {
                SizeClass::Narrow => Presentation::ListDeep,
                SizeClass::Regular | SizeClass::Wide => Presentation::Full,
                SizeClass::ShortBar => Presentation::Strip,
            },
            Zone::Bottom => match geometry.size {
                SizeClass::ShortBar => Presentation::Strip,
                SizeClass::Narrow => Presentation::ListDeep,
                SizeClass::Regular | SizeClass::Wide => Presentation::ListAndDetail,
            },
        }
    }

    fn fits(presentation: Presentation, geometry: &Geometry) -> Result<(), String> {
        let (width, height) = presentation.needs();
        if geometry.width >= width && geometry.height >= height {
            Ok(())
        } else {
            Err(format!("{presentation:?} требует {width}×{height}"))
        }
    }

    fn by_name(name: &str) -> Fixture {
        fixtures(&METRICS)
            .into_iter()
            .find(|fixture| fixture.name == name)
            .unwrap()
    }

    #[test]
    fn fixtures_cover_every_zone_and_size_class() {
        let all = fixtures(&METRICS);
        let pairs: BTreeSet<(Zone, u8)> = all
            .iter()
            .map(|fixture| {
                let class = match fixture.geometry.size {
                    SizeClass::Narrow => 0,
                    SizeClass::Regular => 1,
                    SizeClass::Wide => 2,
                    SizeClass::ShortBar => 3,
                };
                (fixture.geometry.zone, class)
            })
            .collect();
        assert_eq!(pairs.len(), 16, "{all:#?}");

        let names: BTreeSet<&str> = all.iter().map(|fixture| fixture.name).collect();
        assert_eq!(names.len(), all.len(), "имена фикстур уникальны");
        for name in [
            "center-half",
            "center-third",
            "left-half",
            "right-half",
            "zoom",
        ] {
            assert!(names.contains(name), "{name}");
        }
        assert!(
            all.iter()
                .any(|f| !MinSize::DEFAULT.admits(f.geometry.width, f.geometry.height)),
            "есть часть ниже min_size"
        );
    }

    #[test]
    fn fixtures_are_the_real_geometry() {
        let half = by_name("center-half").geometry;
        assert_eq!(
            half,
            Geometry {
                zone: Zone::Center,
                size: SizeClass::Narrow,
                width: 464.0,
                height: 606.0
            }
        );
        assert_eq!(by_name("center-third").geometry.width, 306.0);
        let zoom = by_name("zoom").geometry;
        assert_eq!((zoom.zone, zoom.size), (Zone::Center, SizeClass::Wide));
        assert_eq!(by_name("left-half").geometry.size, SizeClass::Narrow);
        assert_eq!(by_name("right-third").geometry.size, SizeClass::ShortBar);
        assert_eq!(by_name("bottom").geometry.size, SizeClass::ShortBar);
        assert_eq!(by_name("navigator-column").geometry.zone, Zone::Left);

        let wider = LayoutMetrics { gap: 10, ..METRICS };
        assert_ne!(
            fixtures(&wider)[0].geometry,
            fixtures(&METRICS)[0].geometry,
            "фикстуры следуют за метриками"
        );
    }

    #[test]
    fn demo_adapt_passes_every_fixture() {
        let all = fixtures(&METRICS);
        let report = check(&all, MinSize::DEFAULT, |g| fits(adapt(g), g));
        assert!(report.passed(), "{report}");
        assert_eq!(report.entries.len(), all.len());
        let stubs: Vec<&str> = report
            .entries
            .iter()
            .filter(|entry| entry.outcome == Outcome::Stub)
            .map(|entry| entry.fixture)
            .collect();
        assert_eq!(
            stubs,
            vec!["center-quarter-tight", "center-over-tall-bottom"]
        );
        let text = report.to_string();
        for fixture in &all {
            assert!(text.contains(fixture.name), "{text}");
        }
        assert!(text.contains("заглушка «Развернуть»"));
    }

    #[test]
    fn a_wrong_adapt_fails_exactly_the_fixtures_it_cannot_serve() {
        let report = check(&fixtures(&METRICS), MinSize::DEFAULT, |g| {
            fits(Presentation::Full, g)
        });
        assert!(!report.passed());
        let failed: BTreeSet<&str> = report.failures().map(|entry| entry.fixture).collect();
        assert!(failed.contains("center-half"));
        assert!(failed.contains("bottom"));
        assert!(!failed.contains("center"));
        assert!(!failed.contains("zoom"));
        assert!(report.to_string().contains("FAIL: Full требует 480×240"));
    }

    #[test]
    fn a_panicking_adapt_is_reported_and_the_rest_still_run() {
        let report = check(&fixtures(&METRICS), MinSize::DEFAULT, |g| match g.zone {
            Zone::Bottom => panic!("нижняя зона не поддержана"),
            Zone::Left | Zone::Right | Zone::Center => fits(adapt(g), g),
        });
        let failed: Vec<&Entry> = report.failures().collect();
        assert!(!failed.is_empty());
        assert!(
            failed
                .iter()
                .all(|entry| entry.geometry.zone == Zone::Bottom)
        );
        assert!(failed.iter().all(|entry| {
            entry.outcome == Outcome::Failed("паника: нижняя зона не поддержана".to_owned())
        }));
        assert!(
            report
                .entries
                .iter()
                .filter(|entry| entry.geometry.zone != Zone::Bottom)
                .all(|entry| matches!(entry.outcome, Outcome::Adapted | Outcome::Stub))
        );
        assert!(
            report
                .entries
                .iter()
                .any(|entry| entry.fixture == "zoom" && entry.outcome == Outcome::Adapted)
        );
    }

    #[test]
    fn min_size_of_the_plugin_decides_the_stub() {
        let big = MinSize {
            width: 500.0,
            height: 300.0,
        };
        let report = check(&fixtures(&METRICS), big, |g| fits(adapt(g), g));
        assert!(report.passed(), "{report}");
        let outcome = |name: &str| {
            report
                .entries
                .iter()
                .find(|entry| entry.fixture == name)
                .map(|entry| entry.outcome.clone())
        };
        assert_eq!(outcome("center-half"), Some(Outcome::Stub));
        assert_eq!(outcome("center"), Some(Outcome::Adapted));
    }
}
