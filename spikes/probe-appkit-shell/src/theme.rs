//! Тема сцены. Ячейка выводится из `effectiveAppearance` корневого вида и системных
//! `accessibilityDisplayShouldIncreaseContrast` и `accessibilityDisplayShouldReduceMotion`;
//! переопределения `--contrast` и `--motion` действуют поверх системных значений и меняются во
//! время работы. Пересчёт идёт по смене оформления корневого вида и по уведомлению о параметрах
//! отображения доступности; новая ячейка уходит сцене одним вызовом.

use std::cell::{Cell as StdCell, RefCell};
use std::rc::{Rc, Weak};

use objc2::rc::{self, Retained};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSAppearanceNameAqua, NSAppearanceNameDarkAqua,
    NSApplication, NSView, NSWorkspace,
    NSWorkspaceAccessibilityDisplayOptionsDidChangeNotification,
};
use objc2_foundation::{NSNotification, NSObject, NSObjectProtocol};

use crate::args::{Contrast, Motion};
use crate::chrome::is_dark;
use crate::scenario::counters::{self, Live, counter};
use crate::tokens::{self, Cell, InputMode, Theme, Tokens};

counter!(THEME_OBSERVER, "ProbeThemeObserver");

/// Системные входы выбора.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Inputs {
    pub dark: bool,
    pub increase_contrast: bool,
    pub reduce_motion: bool,
}

/// Переопределения пробы; `None` — системное значение.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Overrides {
    pub contrast: Option<Contrast>,
    pub motion: Option<Motion>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub cell: Cell,
    pub motion: Motion,
}

/// Ячейка и движение по системным входам и переопределениям.
pub fn select(inputs: Inputs, overrides: Overrides) -> Selection {
    let theme = if inputs.dark {
        Theme::Dark
    } else {
        Theme::Light
    };
    let contrast = overrides.contrast.unwrap_or(if inputs.increase_contrast {
        Contrast::More
    } else {
        Contrast::Normal
    });
    let motion = overrides.motion.unwrap_or(if inputs.reduce_motion {
        Motion::Reduced
    } else {
        Motion::Normal
    });
    Selection {
        cell: Cell::new(theme, contrast),
        motion,
    }
}

/// Оформление приложения: системное или заданное явно.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    System,
    Light,
    Dark,
}

impl Choice {
    pub fn name(self) -> &'static str {
        match self {
            Choice::System => "system",
            Choice::Light => "light",
            Choice::Dark => "dark",
        }
    }

    /// `NSApp.appearance`: `nil` у системного, `aqua` или `darkAqua` у заданного.
    pub fn apply(self, app: &NSApplication) {
        // SAFETY: имена оформления — неизменяемые константы AppKit.
        let name = unsafe {
            match self {
                Choice::System => None,
                Choice::Light => Some(NSAppearanceNameAqua),
                Choice::Dark => Some(NSAppearanceNameDarkAqua),
            }
        };
        let appearance = name.and_then(NSAppearance::appearanceNamed);
        app.setAppearance(appearance.as_deref());
    }
}

pub struct ObserverIvars {
    _live: Live,
    owner: RefCell<Weak<SceneTheme>>,
}

define_class!(
    /// Подписчик на уведомление о параметрах отображения доступности.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeThemeObserver"]
    #[ivars = ObserverIvars]
    pub struct ThemeObserver;

    unsafe impl NSObjectProtocol for ThemeObserver {}

    impl ThemeObserver {
        #[unsafe(method(displayOptionsChanged:))]
        fn display_options_changed(&self, _notification: &NSNotification) {
            let owner = self.ivars().owner.borrow().upgrade();
            if let Some(owner) = counters::owner("ProbeThemeObserver.displayOptionsChanged", owner) {
                owner.display_options_changed();
            }
        }
    }
);

impl ThemeObserver {
    fn new(mtm: MainThreadMarker) -> Retained<ThemeObserver> {
        let this = ThemeObserver::alloc(mtm).set_ivars(ObserverIvars {
            _live: THEME_OBSERVER.live(),
            owner: RefCell::new(Weak::new()),
        });
        // SAFETY: `init` у NSObject без аргументов.
        unsafe { msg_send![super(this), init] }
    }
}

type Listener = Box<dyn Fn(Selection)>;

/// Текущая ячейка сцены и её источники.
pub struct SceneTheme {
    mtm: MainThreadMarker,
    overrides: StdCell<Overrides>,
    inputs: StdCell<Inputs>,
    selection: StdCell<Selection>,
    /// Вид, чьё `effectiveAppearance` выбирает тему; до привязки — оформление приложения.
    source: RefCell<Option<rc::Weak<NSView>>>,
    listener: RefCell<Option<Listener>>,
    observer: Retained<ThemeObserver>,
    changes: StdCell<usize>,
    notifications: StdCell<usize>,
}

impl SceneTheme {
    pub fn new(mtm: MainThreadMarker, overrides: Overrides) -> Rc<SceneTheme> {
        let observer = ThemeObserver::new(mtm);
        let inputs = read_inputs(mtm, None);
        let theme = Rc::new(SceneTheme {
            mtm,
            overrides: StdCell::new(overrides),
            inputs: StdCell::new(inputs),
            selection: StdCell::new(select(inputs, overrides)),
            source: RefCell::new(None),
            listener: RefCell::new(None),
            observer,
            changes: StdCell::new(0),
            notifications: StdCell::new(0),
        });
        *theme.observer.ivars().owner.borrow_mut() = Rc::downgrade(&theme);
        let center = NSWorkspace::sharedWorkspace().notificationCenter();
        // SAFETY: `displayOptionsChanged:` реализован наблюдателем и принимает уведомление;
        // подписка снимается в `Drop`. Имя уведомления — неизменяемая константа AppKit.
        unsafe {
            center.addObserver_selector_name_object(
                &theme.observer,
                sel!(displayOptionsChanged:),
                Some(NSWorkspaceAccessibilityDisplayOptionsDidChangeNotification),
                None,
            );
        }
        theme
    }

    /// Вид, по оформлению которого выбирается тема.
    pub fn attach(&self, view: &NSView) {
        *self.source.borrow_mut() = Some(rc::Weak::new(view));
    }

    /// Получатель новой ячейки. Вызывается только при её смене.
    pub fn set_listener(&self, listener: Listener) {
        *self.listener.borrow_mut() = Some(listener);
    }

    pub fn selection(&self) -> Selection {
        self.selection.get()
    }

    pub fn cell(&self) -> Cell {
        self.selection.get().cell
    }

    pub fn motion(&self) -> Motion {
        self.selection.get().motion
    }

    pub fn tokens(&self) -> Tokens {
        tokens::tokens(self.cell(), InputMode::Navigation)
    }

    pub fn inputs(&self) -> Inputs {
        self.inputs.get()
    }

    pub fn overrides(&self) -> Overrides {
        self.overrides.get()
    }

    /// Сколько раз ячейка или движение сменились после сборки.
    pub fn changes(&self) -> usize {
        self.changes.get()
    }

    /// Сколько уведомлений о параметрах отображения доступности получено.
    pub fn notifications(&self) -> usize {
        self.notifications.get()
    }

    pub fn set_contrast(&self, contrast: Option<Contrast>) {
        self.overrides.set(Overrides {
            contrast,
            ..self.overrides.get()
        });
        self.refresh();
    }

    pub fn set_motion(&self, motion: Option<Motion>) {
        self.overrides.set(Overrides {
            motion,
            ..self.overrides.get()
        });
        self.refresh();
    }

    /// Перечитывает системные входы и отдаёт новую ячейку, если она сменилась.
    pub fn refresh(&self) {
        let source = self.source.borrow().as_ref().and_then(rc::Weak::load);
        let inputs = read_inputs(self.mtm, source.as_deref());
        self.inputs.set(inputs);
        let next = select(inputs, self.overrides.get());
        if next == self.selection.get() {
            return;
        }
        self.selection.set(next);
        self.changes.set(self.changes.get() + 1);
        if let Some(listener) = self.listener.borrow().as_ref() {
            listener(next);
        }
    }

    fn display_options_changed(&self) {
        self.notifications.set(self.notifications.get() + 1);
        self.refresh();
    }
}

impl Drop for SceneTheme {
    fn drop(&mut self) {
        // SAFETY: наблюдатель — объект этой темы, снимаются все его подписки.
        unsafe {
            NSWorkspace::sharedWorkspace()
                .notificationCenter()
                .removeObserver(&self.observer)
        };
    }
}

fn read_inputs(mtm: MainThreadMarker, source: Option<&NSView>) -> Inputs {
    let appearance = match source {
        Some(view) => view.effectiveAppearance(),
        None => NSApplication::sharedApplication(mtm).effectiveAppearance(),
    };
    let workspace = NSWorkspace::sharedWorkspace();
    Inputs {
        dark: is_dark(&appearance),
        increase_contrast: workspace.accessibilityDisplayShouldIncreaseContrast(),
        reduce_motion: workspace.accessibilityDisplayShouldReduceMotion(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bools() -> [bool; 2] {
        [false, true]
    }

    #[test]
    fn system_inputs_choose_cell_and_motion() {
        for dark in bools() {
            for increase_contrast in bools() {
                for reduce_motion in bools() {
                    let inputs = Inputs {
                        dark,
                        increase_contrast,
                        reduce_motion,
                    };
                    let selection = select(inputs, Overrides::default());
                    let theme = if dark { Theme::Dark } else { Theme::Light };
                    let contrast = if increase_contrast {
                        Contrast::More
                    } else {
                        Contrast::Normal
                    };
                    assert_eq!(selection.cell, Cell::new(theme, contrast), "{inputs:?}");
                    assert_eq!(
                        selection.motion,
                        if reduce_motion {
                            Motion::Reduced
                        } else {
                            Motion::Normal
                        },
                        "{inputs:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn overrides_win_over_system_and_keep_theme() {
        let contrasts = [None, Some(Contrast::Normal), Some(Contrast::More)];
        let motions = [None, Some(Motion::Normal), Some(Motion::Reduced)];
        for dark in bools() {
            for increase_contrast in bools() {
                for reduce_motion in bools() {
                    let inputs = Inputs {
                        dark,
                        increase_contrast,
                        reduce_motion,
                    };
                    let system = select(inputs, Overrides::default());
                    for contrast in contrasts {
                        for motion in motions {
                            let overrides = Overrides { contrast, motion };
                            let selection = select(inputs, overrides);
                            assert_eq!(selection.cell.theme, system.cell.theme);
                            assert_eq!(
                                selection.cell.contrast,
                                contrast.unwrap_or(system.cell.contrast),
                                "{inputs:?} {overrides:?}"
                            );
                            assert_eq!(
                                selection.motion,
                                motion.unwrap_or(system.motion),
                                "{inputs:?} {overrides:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn named_cells() {
        let inputs = Inputs {
            dark: false,
            increase_contrast: true,
            reduce_motion: false,
        };
        assert_eq!(
            select(inputs, Overrides::default()).cell.name(),
            "light-more"
        );
        let normal = Overrides {
            contrast: Some(Contrast::Normal),
            motion: Some(Motion::Reduced),
        };
        let selection = select(inputs, normal);
        assert_eq!(selection.cell.name(), "light");
        assert_eq!(selection.motion, Motion::Reduced);
        let dark = Inputs {
            dark: true,
            ..inputs
        };
        assert_eq!(select(dark, Overrides::default()).cell.name(), "dark-more");
    }
}
