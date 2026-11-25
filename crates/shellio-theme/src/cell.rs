/// Светлота оформления.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Appearance {
    Light,
    Dark,
}

impl Appearance {
    pub const ALL: [Appearance; 2] = [Appearance::Light, Appearance::Dark];

    pub const fn name(self) -> &'static str {
        match self {
            Appearance::Light => "light",
            Appearance::Dark => "dark",
        }
    }
}

/// Контраст оформления: обычный или Increase Contrast.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Contrast {
    Normal,
    Increased,
}

impl Contrast {
    pub const ALL: [Contrast; 2] = [Contrast::Normal, Contrast::Increased];

    pub const fn name(self) -> &'static str {
        match self {
            Contrast::Normal => "normal",
            Contrast::Increased => "increased",
        }
    }
}

/// Ячейка темы: светлота и контраст.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Cell {
    pub appearance: Appearance,
    pub contrast: Contrast,
}

/// Выбор оформления `ui.appearance.set`: светлота или следование системе.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum AppearanceChoice {
    #[default]
    System,
    Light,
    Dark,
}

impl AppearanceChoice {
    pub const ALL: [AppearanceChoice; 3] = [
        AppearanceChoice::System,
        AppearanceChoice::Light,
        AppearanceChoice::Dark,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            AppearanceChoice::System => "system",
            AppearanceChoice::Light => "light",
            AppearanceChoice::Dark => "dark",
        }
    }
}

/// Переопределения поверх системных входов. Галерея задаёт оси по отдельности и только своему
/// окну; её переопределение старше выбора оформления, выбор старше системы. Служебный вход
/// `ui.cell` задаёт контраст и движение и старше остальных источников этих осей.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Overrides {
    pub choice: AppearanceChoice,
    pub gallery_appearance: Option<Appearance>,
    pub gallery_contrast: Option<Contrast>,
    pub gallery_motion: Option<Motion>,
    /// Контраст `ui.cell`.
    pub cell_contrast: Option<Contrast>,
    /// Движение `ui.cell`.
    pub cell_motion: Option<Motion>,
}

/// Откуда взято значение оси.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Source {
    System,
    /// Выбор `ui.appearance.set`.
    Choice,
    Gallery,
    /// Служебный вход `ui.cell`.
    Override,
}

impl Source {
    pub const fn name(self) -> &'static str {
        match self {
            Source::System => "system",
            Source::Choice => "ui.appearance",
            Source::Gallery => "gallery",
            Source::Override => "override",
        }
    }
}

/// Итог разбора входов: ячейка и источник каждой её оси.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ResolvedCell {
    pub cell: Cell,
    pub appearance_source: Source,
    pub contrast_source: Source,
}

impl Cell {
    pub const ALL: [Cell; 4] = [
        Cell::new(Appearance::Dark, Contrast::Normal),
        Cell::new(Appearance::Light, Contrast::Normal),
        Cell::new(Appearance::Dark, Contrast::Increased),
        Cell::new(Appearance::Light, Contrast::Increased),
    ];

    pub const fn new(appearance: Appearance, contrast: Contrast) -> Cell {
        Cell {
            appearance,
            contrast,
        }
    }

    pub const fn name(self) -> &'static str {
        match (self.appearance, self.contrast) {
            (Appearance::Dark, Contrast::Normal) => "dark",
            (Appearance::Light, Contrast::Normal) => "light",
            (Appearance::Dark, Contrast::Increased) => "dark-increased",
            (Appearance::Light, Contrast::Increased) => "light-increased",
        }
    }

    /// Ячейка из системных входов (`effectiveAppearance` окна и
    /// `accessibilityDisplayShouldIncreaseContrast`) и переопределений.
    pub fn resolve(
        system_dark: bool,
        system_increase_contrast: bool,
        overrides: Overrides,
    ) -> ResolvedCell {
        let system_appearance = if system_dark {
            Appearance::Dark
        } else {
            Appearance::Light
        };
        let choice = match overrides.choice {
            AppearanceChoice::System => None,
            AppearanceChoice::Light => Some(Appearance::Light),
            AppearanceChoice::Dark => Some(Appearance::Dark),
        };
        let (appearance, appearance_source) = match (overrides.gallery_appearance, choice) {
            (Some(appearance), _) => (appearance, Source::Gallery),
            (None, Some(appearance)) => (appearance, Source::Choice),
            (None, None) => (system_appearance, Source::System),
        };
        let (contrast, contrast_source) =
            match (overrides.cell_contrast, overrides.gallery_contrast) {
                (Some(contrast), _) => (contrast, Source::Override),
                (None, Some(contrast)) => (contrast, Source::Gallery),
                (None, None) if system_increase_contrast => (Contrast::Increased, Source::System),
                (None, None) => (Contrast::Normal, Source::System),
            };
        ResolvedCell {
            cell: Cell::new(appearance, contrast),
            appearance_source,
            contrast_source,
        }
    }
}

/// Ось движения: обычное или Reduce Motion.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Motion {
    Normal,
    Reduced,
}

impl Motion {
    pub const ALL: [Motion; 2] = [Motion::Normal, Motion::Reduced];

    pub const fn name(self) -> &'static str {
        match self {
            Motion::Normal => "normal",
            Motion::Reduced => "reduced",
        }
    }

    /// Движение из `accessibilityDisplayShouldReduceMotion` и переопределения галереи.
    pub fn resolve(system_reduce_motion: bool, overrides: Overrides) -> (Motion, Source) {
        match (overrides.cell_motion, overrides.gallery_motion) {
            (Some(motion), _) => (motion, Source::Override),
            (None, Some(motion)) => (motion, Source::Gallery),
            (None, None) if system_reduce_motion => (Motion::Reduced, Source::System),
            (None, None) => (Motion::Normal, Source::System),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options<T: Copy>(values: &[T]) -> Vec<Option<T>> {
        std::iter::once(None)
            .chain(values.iter().copied().map(Some))
            .collect()
    }

    fn all_overrides() -> Vec<Overrides> {
        let mut all = Vec::new();
        for choice in AppearanceChoice::ALL {
            for gallery_appearance in options(&Appearance::ALL) {
                for gallery_contrast in options(&Contrast::ALL) {
                    for gallery_motion in options(&Motion::ALL) {
                        all.push(Overrides {
                            choice,
                            gallery_appearance,
                            gallery_contrast,
                            gallery_motion,
                            cell_contrast: None,
                            cell_motion: None,
                        });
                    }
                }
            }
        }
        all
    }

    /// Ожидание записано отдельно от реализации: сначала галерея, затем выбор, затем система.
    fn expected(system_dark: bool, system_more: bool, o: Overrides) -> ResolvedCell {
        let (appearance, appearance_source) = if let Some(gallery) = o.gallery_appearance {
            (gallery, Source::Gallery)
        } else if o.choice == AppearanceChoice::Light {
            (Appearance::Light, Source::Choice)
        } else if o.choice == AppearanceChoice::Dark {
            (Appearance::Dark, Source::Choice)
        } else if system_dark {
            (Appearance::Dark, Source::System)
        } else {
            (Appearance::Light, Source::System)
        };
        let (contrast, contrast_source) = if let Some(gallery) = o.gallery_contrast {
            (gallery, Source::Gallery)
        } else if system_more {
            (Contrast::Increased, Source::System)
        } else {
            (Contrast::Normal, Source::System)
        };
        ResolvedCell {
            cell: Cell {
                appearance,
                contrast,
            },
            appearance_source,
            contrast_source,
        }
    }

    #[test]
    fn resolve_covers_every_combination() {
        let mut combinations = 0;
        for system_dark in [false, true] {
            for system_more in [false, true] {
                for overrides in all_overrides() {
                    if overrides.gallery_motion.is_some() {
                        continue;
                    }
                    combinations += 1;
                    assert_eq!(
                        Cell::resolve(system_dark, system_more, overrides),
                        expected(system_dark, system_more, overrides),
                        "system_dark={system_dark} system_more={system_more} {overrides:?}"
                    );
                }
            }
        }
        assert_eq!(combinations, 2 * 2 * 3 * 3 * 3);
    }

    #[test]
    fn gallery_contrast_keeps_choice_as_appearance_source() {
        let resolved = Cell::resolve(
            true,
            true,
            Overrides {
                choice: AppearanceChoice::Light,
                gallery_contrast: Some(Contrast::Normal),
                ..Overrides::default()
            },
        );
        assert_eq!(
            resolved.cell,
            Cell::new(Appearance::Light, Contrast::Normal)
        );
        assert_eq!(resolved.appearance_source.name(), "ui.appearance");
        assert_eq!(resolved.contrast_source.name(), "gallery");
    }

    #[test]
    fn motion_override_does_not_touch_cell() {
        for system_dark in [false, true] {
            for system_more in [false, true] {
                for overrides in all_overrides() {
                    let without_motion = Overrides {
                        gallery_motion: None,
                        ..overrides
                    };
                    assert_eq!(
                        Cell::resolve(system_dark, system_more, overrides),
                        Cell::resolve(system_dark, system_more, without_motion)
                    );
                }
            }
        }
    }

    #[test]
    fn motion_resolve_covers_every_combination() {
        for system_reduce in [false, true] {
            for gallery_motion in options(&Motion::ALL) {
                let overrides = Overrides {
                    gallery_motion,
                    ..Overrides::default()
                };
                let expected = match gallery_motion {
                    Some(motion) => (motion, Source::Gallery),
                    None if system_reduce => (Motion::Reduced, Source::System),
                    None => (Motion::Normal, Source::System),
                };
                assert_eq!(Motion::resolve(system_reduce, overrides), expected);
            }
        }
        for overrides in all_overrides() {
            if overrides.gallery_motion.is_none() {
                assert_eq!(Motion::resolve(true, overrides).1, Source::System);
            }
        }
    }

    /// `ui.cell` старше галереи и системы по своим осям и не трогает светлоту.
    #[test]
    fn cell_override_wins_its_axes_only() {
        for system_dark in [false, true] {
            for system_more in [false, true] {
                for base in all_overrides() {
                    let plain = Cell::resolve(system_dark, system_more, base);
                    for cell_contrast in options(&Contrast::ALL) {
                        for cell_motion in options(&Motion::ALL) {
                            let overrides = Overrides {
                                cell_contrast,
                                cell_motion,
                                ..base
                            };
                            let resolved = Cell::resolve(system_dark, system_more, overrides);
                            assert_eq!(resolved.cell.appearance, plain.cell.appearance);
                            assert_eq!(resolved.appearance_source, plain.appearance_source);
                            match cell_contrast {
                                Some(contrast) => {
                                    assert_eq!(resolved.cell.contrast, contrast);
                                    assert_eq!(resolved.contrast_source, Source::Override);
                                }
                                None => assert_eq!(resolved, plain),
                            }
                            let motion = Motion::resolve(system_more, overrides);
                            match cell_motion {
                                Some(value) => assert_eq!(motion, (value, Source::Override)),
                                None => assert_eq!(motion, Motion::resolve(system_more, base)),
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(Source::Override.name(), "override");
    }

    #[test]
    fn names_are_distinct() {
        let names: std::collections::BTreeSet<_> = Cell::ALL.iter().map(|c| c.name()).collect();
        assert_eq!(names.len(), Cell::ALL.len());
    }
}
