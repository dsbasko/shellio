//! Наблюдатель ячейки темы: системные входы (`effectiveAppearance` вида-источника,
//! `accessibilityDisplayShouldIncreaseContrast`, `accessibilityDisplayShouldReduceMotion`) и
//! переопределения дают ячейку и движение через [`Cell::resolve`] и [`Motion::resolve`].
//!
//! Входы перечитываются по уведомлению `NSWorkspaceAccessibilityDisplayOptionsDidChangeNotification`
//! центра `NSWorkspace` и по [`CellObserver::refresh`], который вызывает вид-источник при смене
//! своего оформления. Новая ячейка уходит получателю только при её смене.

use std::cell::{Cell as StdCell, RefCell};
use std::rc::Rc;

use objc2::MainThreadMarker;
use objc2::rc::{self, Retained};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSAppearanceNameAqua, NSAppearanceNameDarkAqua,
    NSApplication, NSView, NSWorkspace,
    NSWorkspaceAccessibilityDisplayOptionsDidChangeNotification,
};
use objc2_foundation::NSArray;
use shellio_theme::{
    AppTheme, Appearance, AppearanceChoice, Cell, Motion, Overrides, ResolvedCell, Source,
};

use crate::app::App;
use crate::notify::Subscription;
use crate::own::Callback;

/// Системные входы выбора ячейки.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SystemInputs {
    /// `effectiveAppearance` вида-источника ближе к тёмному оформлению.
    pub dark: bool,
    pub increase_contrast: bool,
    pub reduce_motion: bool,
}

/// Ячейка и движение с источниками осей.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ThemeSelection {
    pub cell: ResolvedCell,
    pub motion: Motion,
    pub motion_source: Source,
}

impl ThemeSelection {
    /// Выбор по системным входам и переопределениям.
    pub fn resolve(inputs: SystemInputs, overrides: Overrides) -> ThemeSelection {
        let (motion, motion_source) = Motion::resolve(inputs.reduce_motion, overrides);
        ThemeSelection {
            cell: Cell::resolve(inputs.dark, inputs.increase_contrast, overrides),
            motion,
            motion_source,
        }
    }

    pub fn theme(&self) -> AppTheme {
        AppTheme::for_cell(self.cell.cell)
    }
}

/// Текущий выбор и его входы. Возвращает новый выбор только при его смене.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Tracker {
    inputs: SystemInputs,
    overrides: Overrides,
    selection: ThemeSelection,
}

impl Tracker {
    pub(crate) fn new(inputs: SystemInputs, overrides: Overrides) -> Tracker {
        Tracker {
            inputs,
            overrides,
            selection: ThemeSelection::resolve(inputs, overrides),
        }
    }

    pub(crate) fn selection(&self) -> ThemeSelection {
        self.selection
    }

    pub(crate) fn inputs(&self) -> SystemInputs {
        self.inputs
    }

    pub(crate) fn overrides(&self) -> Overrides {
        self.overrides
    }

    pub(crate) fn set_inputs(&mut self, inputs: SystemInputs) -> Option<ThemeSelection> {
        self.inputs = inputs;
        self.reselect()
    }

    pub(crate) fn set_overrides(&mut self, overrides: Overrides) -> Option<ThemeSelection> {
        self.overrides = overrides;
        self.reselect()
    }

    fn reselect(&mut self) -> Option<ThemeSelection> {
        let next = ThemeSelection::resolve(self.inputs, self.overrides);
        (next != self.selection).then(|| {
            self.selection = next;
            next
        })
    }
}

/// Наблюдатель ячейки на главном потоке. Подписка на уведомление снимается в `Drop`.
pub struct CellObserver {
    // Порядок полей — порядок освобождения: сначала подписка, затем состояние.
    _subscription: Subscription,
    inner: Rc<ObserverInner>,
}

struct ObserverInner {
    mtm: MainThreadMarker,
    tracker: RefCell<Tracker>,
    source: RefCell<Option<rc::Weak<NSView>>>,
    on_change: Box<dyn Fn(ThemeSelection)>,
    notifications: StdCell<u64>,
}

impl CellObserver {
    /// Читает входы и подписывается на уведомление. До привязки вида-источника светлота берётся
    /// из `effectiveAppearance` приложения. `on_change` получает новый выбор при каждой смене.
    pub fn new(
        app: &App,
        overrides: Overrides,
        on_change: impl Fn(ThemeSelection) + 'static,
    ) -> CellObserver {
        let mtm = app.mtm();
        let inner = Rc::new(ObserverInner {
            mtm,
            tracker: RefCell::new(Tracker::new(read_inputs(mtm, None), overrides)),
            source: RefCell::new(None),
            on_change: Box::new(on_change),
            notifications: StdCell::new(0),
        });
        let workspace = NSWorkspace::sharedWorkspace();
        // SAFETY: имя уведомления — неизменяемая константа AppKit.
        let name = unsafe { NSWorkspaceAccessibilityDisplayOptionsDidChangeNotification };
        let subscription = Subscription::notification(
            mtm,
            &workspace.notificationCenter(),
            name,
            Callback::new(&inner, "CellObserver.displayOptionsChanged"),
            |inner: &ObserverInner| {
                inner.notifications.set(inner.notifications.get() + 1);
                inner.refresh();
            },
        );
        CellObserver {
            _subscription: subscription,
            inner,
        }
    }

    pub fn selection(&self) -> ThemeSelection {
        self.inner.tracker.borrow().selection()
    }

    pub fn inputs(&self) -> SystemInputs {
        self.inner.tracker.borrow().inputs()
    }

    pub fn overrides(&self) -> Overrides {
        self.inner.tracker.borrow().overrides()
    }

    /// Уведомлений о параметрах отображения доступности получено.
    pub fn notifications(&self) -> u64 {
        self.inner.notifications.get()
    }

    pub fn set_overrides(&self, overrides: Overrides) {
        let changed = self.inner.tracker.borrow_mut().set_overrides(overrides);
        self.inner.emit(changed);
    }

    /// Перечитывает системные входы.
    pub fn refresh(&self) {
        self.inner.refresh();
    }

    /// Вид, чьё `effectiveAppearance` даёт светлоту: содержимое окна. Входы перечитываются сразу.
    pub(crate) fn attach(&self, view: &NSView) {
        *self.inner.source.borrow_mut() = Some(rc::Weak::new(view));
        self.inner.refresh();
    }
}

impl ObserverInner {
    fn refresh(&self) {
        let source = self.source.borrow().as_ref().and_then(rc::Weak::load);
        let inputs = read_inputs(self.mtm, source.as_deref());
        let changed = self.tracker.borrow_mut().set_inputs(inputs);
        self.emit(changed);
    }

    /// Получатель вызывается без заимствования состояния: он может читать наблюдатель.
    fn emit(&self, changed: Option<ThemeSelection>) {
        if let Some(selection) = changed {
            (self.on_change)(selection);
        }
    }
}

/// Светлота, которую переопределения задают окну поверх системы; `None` — окно следует системе.
/// Окно с этим оформлением рисует системные части (полосы прокрутки, кольцо фокуса) в той же
/// светлоте, что токены. Пока переопределения нет, оформление окна системное, и вид-источник
/// читает настоящую светлоту системы.
pub(crate) fn forced_appearance(overrides: Overrides) -> Option<Appearance> {
    overrides.gallery_appearance.or(match overrides.choice {
        AppearanceChoice::System => None,
        AppearanceChoice::Light => Some(Appearance::Light),
        AppearanceChoice::Dark => Some(Appearance::Dark),
    })
}

/// Именованное оформление AppKit светлоты `appearance`.
pub(crate) fn ns_appearance(appearance: Appearance) -> Option<Retained<NSAppearance>> {
    // SAFETY: имена оформлений — неизменяемые константы AppKit.
    let name = unsafe {
        match appearance {
            Appearance::Light => NSAppearanceNameAqua,
            Appearance::Dark => NSAppearanceNameDarkAqua,
        }
    };
    NSAppearance::appearanceNamed(name)
}

fn read_inputs(mtm: MainThreadMarker, source: Option<&NSView>) -> SystemInputs {
    let appearance = match source {
        Some(view) => view.effectiveAppearance(),
        None => NSApplication::sharedApplication(mtm).effectiveAppearance(),
    };
    let workspace = NSWorkspace::sharedWorkspace();
    SystemInputs {
        dark: is_dark(&appearance),
        increase_contrast: workspace.accessibilityDisplayShouldIncreaseContrast(),
        reduce_motion: workspace.accessibilityDisplayShouldReduceMotion(),
    }
}

/// Ближе ли оформление к тёмному. Сравнение через `bestMatchFromAppearancesWithNames`, а не по
/// имени: у оформлений высокого контраста и vibrant свои имена.
fn is_dark(appearance: &NSAppearance) -> bool {
    // SAFETY: имена оформлений — неизменяемые константы AppKit.
    let (aqua, dark) = unsafe { (NSAppearanceNameAqua, NSAppearanceNameDarkAqua) };
    let names: Retained<NSArray<_>> = NSArray::from_slice(&[aqua, dark]);
    appearance
        .bestMatchFromAppearancesWithNames(&names)
        .is_some_and(|best| best.isEqualToString(dark))
}

#[cfg(test)]
mod tests {
    use shellio_theme::Contrast;

    use super::*;

    fn inputs(dark: bool, increase_contrast: bool, reduce_motion: bool) -> SystemInputs {
        SystemInputs {
            dark,
            increase_contrast,
            reduce_motion,
        }
    }

    #[test]
    fn selection_combines_cell_and_motion_resolution() {
        for dark in [false, true] {
            for more in [false, true] {
                for reduce in [false, true] {
                    let overrides = Overrides {
                        choice: AppearanceChoice::Light,
                        gallery_motion: Some(Motion::Normal),
                        ..Overrides::default()
                    };
                    let selection = ThemeSelection::resolve(inputs(dark, more, reduce), overrides);
                    assert_eq!(selection.cell, Cell::resolve(dark, more, overrides));
                    assert_eq!(
                        (selection.motion, selection.motion_source),
                        Motion::resolve(reduce, overrides)
                    );
                }
            }
        }
    }

    #[test]
    fn theme_follows_resolved_cell() {
        let selection = ThemeSelection::resolve(inputs(true, true, false), Overrides::default());
        assert_eq!(
            selection.theme().cell,
            Cell::new(Appearance::Dark, Contrast::Increased)
        );
    }

    #[test]
    fn tracker_reports_only_changes() {
        let mut tracker = Tracker::new(inputs(false, false, false), Overrides::default());
        assert_eq!(tracker.set_inputs(inputs(false, false, false)), None);
        let dark = tracker.set_inputs(inputs(true, false, false)).unwrap();
        assert_eq!(dark.cell.cell.appearance, Appearance::Dark);
        assert_eq!(dark.cell.appearance_source, Source::System);
        assert_eq!(tracker.selection(), dark);
        let reduced = tracker.set_inputs(inputs(true, false, true)).unwrap();
        assert_eq!(reduced.motion, Motion::Reduced);
        assert_eq!(tracker.inputs(), inputs(true, false, true));
    }

    #[test]
    fn overridden_axes_hide_system_changes() {
        let overrides = Overrides {
            gallery_appearance: Some(Appearance::Light),
            gallery_contrast: Some(Contrast::Normal),
            gallery_motion: Some(Motion::Normal),
            ..Overrides::default()
        };
        let mut tracker = Tracker::new(inputs(false, false, false), overrides);
        assert_eq!(tracker.set_inputs(inputs(true, true, true)), None);
        let system = tracker.set_overrides(Overrides::default()).unwrap();
        assert_eq!(
            system.cell.cell,
            Cell::new(Appearance::Dark, Contrast::Increased)
        );
        assert_eq!(system.motion, Motion::Reduced);
        assert_eq!(tracker.overrides(), Overrides::default());
    }

    #[test]
    fn choice_switches_lightness_and_back_to_system() {
        let mut tracker = Tracker::new(inputs(true, false, false), Overrides::default());
        let light = tracker
            .set_overrides(Overrides {
                choice: AppearanceChoice::Light,
                ..Overrides::default()
            })
            .unwrap();
        assert_eq!(light.cell.cell.appearance, Appearance::Light);
        assert_eq!(light.cell.appearance_source, Source::Choice);
        let system = tracker.set_overrides(Overrides::default()).unwrap();
        assert_eq!(system.cell.cell.appearance, Appearance::Dark);
        assert_eq!(system.cell.appearance_source, Source::System);
        assert_eq!(tracker.set_overrides(Overrides::default()), None);
    }

    #[test]
    fn window_follows_system_until_lightness_is_overridden() {
        assert_eq!(forced_appearance(Overrides::default()), None);
        for (choice, expected) in [
            (AppearanceChoice::System, None),
            (AppearanceChoice::Light, Some(Appearance::Light)),
            (AppearanceChoice::Dark, Some(Appearance::Dark)),
        ] {
            let overrides = Overrides {
                choice,
                ..Overrides::default()
            };
            assert_eq!(forced_appearance(overrides), expected, "{choice:?}");
            let gallery = Overrides {
                gallery_appearance: Some(Appearance::Dark),
                ..overrides
            };
            assert_eq!(
                forced_appearance(gallery),
                Some(Appearance::Dark),
                "галерея старше выбора"
            );
        }
        let contrast_only = Overrides {
            gallery_contrast: Some(Contrast::Increased),
            ..Overrides::default()
        };
        assert_eq!(
            forced_appearance(contrast_only),
            None,
            "контраст светлоту не задаёт"
        );
    }
}
