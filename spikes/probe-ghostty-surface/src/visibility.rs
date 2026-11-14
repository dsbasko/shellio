//! Модель видимости поверхности и фронты окклюзии без AppKit.
//!
//! Видимость складывается из признаков сцены и окна: вид в окне, без скрытого предка, окно на
//! экране (`isVisible`), не свёрнуто, видимо по `occlusionState`, приложение не скрыто. Наложение,
//! которое частично перекрывает поверхность (палитра), видимость не меняет: окно под ним остаётся
//! видимым по `occlusionState`, а поверхность продолжает показываться.
//!
//! В `ghostty_surface_set_occlusion(visible)` подаётся только фронт. Ghostty начинает с
//! `Surface.visible = true` и сам отбрасывает повтор (`src/Surface.zig:3437–3444`), поэтому модель
//! тоже начинает с «видима»: поверхность, созданная скрытой, получает явный `false`.
//!
//! Базовый вариант обходов agterm не содержит. Каждый обход включается своим флагом:
//! - `bounds` — у скрытого слоя снимается `needsDisplayOnBoundsChange`, при показе возвращается:
//!   `display` слоя ghostty рисует кадр через `drawFrame(true)` без проверки видимости и строит
//!   swap chain заново (`src/renderer/Metal.zig:129, 155–159`, `src/renderer/generic.zig:1740–1782`);
//! - `defer` — скрытие поверхности без первого кадра откладывается до первого кадра (`contents` слоя
//!   появился), показ до него отменяет отложенное скрытие;
//! - `syncdraw` — сразу после подачи `true` кадр рисуется синхронно `ghostty_surface_draw`.
//!   Пока модель считает поверхность скрытой, синхронный кадр не рисуется: он строит swap chain.

use std::cell::Cell;

use serde::Serialize;

thread_local! {
    static ENABLED: Cell<Option<Workarounds>> = const { Cell::new(None) };
    static OCCLUSION_MASKED: Cell<bool> = const { Cell::new(false) };
}

/// Стенд исключает `occlusionState` окна из признаков: окно перекрыто чужим полноэкранным окном
/// (щитом блокировки), опознанным по списку окон. Модель не меняется, маска записывается в итог.
pub fn mask_occlusion() {
    OCCLUSION_MASKED.with(|cell| cell.set(true));
}

pub fn occlusion_masked() -> bool {
    OCCLUSION_MASKED.with(Cell::get)
}

/// Признак «окно перекрыто» по биту `Visible` у `occlusionState` и маске стенда.
pub fn occluded(visible_bit: bool, masked: bool) -> bool {
    !masked && !visible_bit
}

/// Включает модель видимости у видов поверхности, которые получат поверхность после вызова.
/// Без вызова вид `set_occlusion` не подаёт: сценарии до задачи 12 идут как прежде.
pub fn enable(workarounds: Workarounds) {
    ENABLED.with(|cell| cell.set(Some(workarounds)));
}

pub fn enabled() -> Option<Workarounds> {
    ENABLED.with(Cell::get)
}

/// Признаки видимости вида, прочитанные у AppKit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Signals {
    pub in_window: bool,
    /// `isHiddenOrHasHiddenAncestor`: скрыт сам вид или предок (вкладка, зона).
    pub hidden_ancestor: bool,
    pub window_visible: bool,
    pub window_miniaturized: bool,
    /// `occlusionState` окна без `Visible`.
    pub window_occluded: bool,
    pub app_hidden: bool,
    /// Наложение частично перекрывает поверхность. Видимость не меняет.
    pub overlay_partial: bool,
    /// Стенд исключил `occlusionState` из признаков ([`mask_occlusion`]).
    pub occlusion_masked: bool,
}

impl Signals {
    pub fn visible(&self) -> bool {
        self.in_window
            && !self.hidden_ancestor
            && self.window_visible
            && !self.window_miniaturized
            && !self.window_occluded
            && !self.app_hidden
    }

    /// Признаки, из-за которых поверхность скрыта.
    pub fn reasons(&self) -> Vec<&'static str> {
        [
            (!self.in_window, "вне окна"),
            (self.hidden_ancestor, "скрыт вид или предок"),
            (!self.window_visible, "окно не на экране"),
            (self.window_miniaturized, "окно свёрнуто"),
            (self.window_occluded, "окно перекрыто"),
            (self.app_hidden, "приложение скрыто"),
        ]
        .into_iter()
        .filter_map(|(on, name)| on.then_some(name))
        .collect()
    }
}

/// Обходы agterm, включённые вариантом.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Workarounds {
    pub bounds: bool,
    pub defer_hide: bool,
    pub sync_draw: bool,
}

impl Workarounds {
    pub const VARIANTS: [&'static str; 5] = ["base", "bounds", "defer", "syncdraw", "all"];

    pub fn from_variant(variant: &str) -> Option<Workarounds> {
        let none = Workarounds::default();
        match variant {
            "base" => Some(none),
            "bounds" => Some(Workarounds {
                bounds: true,
                ..none
            }),
            "defer" => Some(Workarounds {
                defer_hide: true,
                ..none
            }),
            "syncdraw" => Some(Workarounds {
                sync_draw: true,
                ..none
            }),
            "all" => Some(Workarounds {
                bounds: true,
                defer_hide: true,
                sync_draw: true,
            }),
            _ => None,
        }
    }

    /// Имена включённых обходов; пусто — базовый вариант.
    pub fn names(&self) -> Vec<&'static str> {
        [
            (self.bounds, "bounds"),
            (self.defer_hide, "defer"),
            (self.sync_draw, "syncdraw"),
        ]
        .into_iter()
        .filter_map(|(on, name)| on.then_some(name))
        .collect()
    }
}

/// Действие с поверхностью по фронту, по порядку исполнения.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    /// `ghostty_surface_set_occlusion(visible)`.
    Feed(bool),
    /// `needsDisplayOnBoundsChange` слоя поверхности.
    BoundsRedraw(bool),
    /// `ghostty_surface_draw` после подачи `true`.
    SyncDraw,
    /// Скрытие отложено до первого кадра.
    DeferHide,
}

/// Фронты видимости одной поверхности.
#[derive(Clone, Debug)]
pub struct Edges {
    workarounds: Workarounds,
    /// Что знает ghostty: последняя подача, с начальным `true`.
    fed: bool,
    /// Скрытие отложено до первого кадра.
    deferred: bool,
    first_frame: bool,
}

impl Edges {
    pub fn new(workarounds: Workarounds) -> Edges {
        Edges {
            workarounds,
            fed: true,
            deferred: false,
            first_frame: false,
        }
    }

    pub fn fed(&self) -> bool {
        self.fed
    }

    pub fn deferred(&self) -> bool {
        self.deferred
    }

    pub fn has_first_frame(&self) -> bool {
        self.first_frame
    }

    /// Новая видимость по признакам. Повтор того же состояния ничего не подаёт.
    pub fn update(&mut self, visible: bool) -> Vec<Action> {
        if visible {
            if self.deferred {
                // Скрытие так и не подано: ghostty по-прежнему считает поверхность видимой.
                self.deferred = false;
                return Vec::new();
            }
            if self.fed {
                return Vec::new();
            }
            self.fed = true;
            let mut actions = Vec::new();
            if self.workarounds.bounds {
                actions.push(Action::BoundsRedraw(true));
            }
            actions.push(Action::Feed(true));
            if self.workarounds.sync_draw {
                actions.push(Action::SyncDraw);
            }
            actions
        } else {
            if !self.fed || self.deferred {
                return Vec::new();
            }
            if self.workarounds.defer_hide && !self.first_frame {
                self.deferred = true;
                return vec![Action::DeferHide];
            }
            self.hide()
        }
    }

    /// Первый кадр поверхности: отложенное скрытие подаётся сейчас.
    pub fn first_frame(&mut self) -> Vec<Action> {
        if self.first_frame {
            return Vec::new();
        }
        self.first_frame = true;
        if !self.deferred {
            return Vec::new();
        }
        self.deferred = false;
        self.hide()
    }

    fn hide(&mut self) -> Vec<Action> {
        self.fed = false;
        let mut actions = vec![Action::Feed(false)];
        if self.workarounds.bounds {
            actions.push(Action::BoundsRedraw(false));
        }
        actions
    }
}

/// Подача `set_occlusion`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Front {
    pub visible: bool,
    pub at_ns: u64,
    /// Откуда пришла оценка: `attach`, `move-to-window`, `hide`, `occlusion`, `first-frame`…
    pub from: &'static str,
    pub reasons: Vec<&'static str>,
    /// У слоя было `contents` в момент подачи.
    pub contents: bool,
}

/// Синхронный кадр обхода `syncdraw`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SyncDraw {
    pub at_ns: u64,
    pub ms: f64,
}

/// Журнал модели видимости одного вида: фактические подачи, а не модель.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Log {
    pub workarounds: Vec<&'static str>,
    pub evaluations: u64,
    pub fed: Vec<Front>,
    /// Отметки откладывания скрытия до первого кадра.
    pub deferred_ns: Vec<u64>,
    pub first_frame_ns: Option<u64>,
    pub sync_draws: Vec<SyncDraw>,
    pub bounds_redraw: Vec<bool>,
    pub last_signals: Option<Signals>,
}

/// Подачи `set_occlusion` чередуются: два одинаковых значения подряд — нарушение правила фронтов.
/// Первое значение не может быть `true`: ghostty начинает с видимой поверхности.
pub fn fronts_alternate(fed: &[bool]) -> bool {
    fed.first() != Some(&true) && fed.windows(2).all(|pair| pair[0] != pair[1])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn visible() -> Signals {
        Signals {
            in_window: true,
            window_visible: true,
            ..Signals::default()
        }
    }

    fn base() -> Edges {
        Edges::new(Workarounds::default())
    }

    #[test]
    fn each_signal_hides_on_its_own() {
        assert!(visible().visible());
        assert!(visible().reasons().is_empty());
        let hidden = [
            Signals {
                in_window: false,
                ..visible()
            },
            Signals {
                hidden_ancestor: true,
                ..visible()
            },
            Signals {
                window_visible: false,
                ..visible()
            },
            Signals {
                window_miniaturized: true,
                ..visible()
            },
            Signals {
                window_occluded: true,
                ..visible()
            },
            Signals {
                app_hidden: true,
                ..visible()
            },
        ];
        for signals in hidden {
            assert!(!signals.visible(), "{signals:?}");
            assert_eq!(signals.reasons().len(), 1, "{signals:?}");
        }
        let all = Signals {
            in_window: false,
            hidden_ancestor: true,
            window_visible: false,
            window_miniaturized: true,
            window_occluded: true,
            app_hidden: true,
            overlay_partial: true,
            occlusion_masked: false,
        };
        assert!(!all.visible());
        assert_eq!(all.reasons().len(), 6);
    }

    #[test]
    fn partial_overlay_does_not_hide() {
        let covered = Signals {
            overlay_partial: true,
            ..visible()
        };
        assert!(covered.visible());
        assert!(covered.reasons().is_empty());
        let mut edges = base();
        assert!(edges.update(covered.visible()).is_empty(), "фронта нет");
        assert!(edges.update(visible().visible()).is_empty());
        assert!(edges.fed());
    }

    #[test]
    fn starts_visible_and_feeds_only_fronts() {
        let mut edges = base();
        assert!(edges.update(true).is_empty(), "ghostty уже считает видимой");
        assert_eq!(edges.update(false), [Action::Feed(false)]);
        assert!(
            edges.update(false).is_empty(),
            "повторное скрытие не подаётся"
        );
        assert!(edges.update(false).is_empty());
        assert_eq!(edges.update(true), [Action::Feed(true)]);
        assert!(edges.update(true).is_empty());
        assert!(edges.fed());
    }

    #[test]
    fn surface_created_hidden_gets_explicit_false() {
        let mut edges = base();
        assert_eq!(edges.update(false), [Action::Feed(false)]);
        assert!(!edges.fed());
    }

    #[test]
    fn bounds_workaround_wraps_the_feeds() {
        let mut edges = Edges::new(Workarounds::from_variant("bounds").unwrap());
        assert_eq!(
            edges.update(false),
            [Action::Feed(false), Action::BoundsRedraw(false)]
        );
        assert_eq!(
            edges.update(true),
            [Action::BoundsRedraw(true), Action::Feed(true)],
            "перерисовка по границам возвращается до показа"
        );
    }

    #[test]
    fn sync_draw_follows_show_only() {
        let mut edges = Edges::new(Workarounds::from_variant("syncdraw").unwrap());
        assert_eq!(edges.update(false), [Action::Feed(false)]);
        assert_eq!(edges.update(true), [Action::Feed(true), Action::SyncDraw]);
        assert!(
            edges.update(true).is_empty(),
            "без фронта синхронного кадра нет"
        );
    }

    #[test]
    fn defer_postpones_hide_until_first_frame() {
        let mut edges = Edges::new(Workarounds::from_variant("defer").unwrap());
        assert_eq!(edges.update(false), [Action::DeferHide]);
        assert!(edges.deferred());
        assert!(edges.fed(), "ghostty ещё считает видимой");
        assert!(
            edges.update(false).is_empty(),
            "повтор не откладывает второй раз"
        );
        assert_eq!(edges.first_frame(), [Action::Feed(false)]);
        assert!(!edges.deferred() && !edges.fed());
        assert!(edges.first_frame().is_empty());
        assert_eq!(edges.update(true), [Action::Feed(true)]);
        assert_eq!(
            edges.update(false),
            [Action::Feed(false)],
            "после первого кадра скрытие сразу"
        );
    }

    #[test]
    fn show_before_first_frame_cancels_deferred_hide() {
        let mut edges = Edges::new(Workarounds::from_variant("defer").unwrap());
        assert_eq!(edges.update(false), [Action::DeferHide]);
        assert!(
            edges.update(true).is_empty(),
            "скрытие не подано, показывать нечего"
        );
        assert!(!edges.deferred() && edges.fed());
        assert!(edges.first_frame().is_empty());
    }

    #[test]
    fn first_frame_before_hide_means_no_deferral() {
        let mut edges = Edges::new(Workarounds::from_variant("all").unwrap());
        assert!(edges.first_frame().is_empty());
        assert_eq!(
            edges.update(false),
            [Action::Feed(false), Action::BoundsRedraw(false)]
        );
        assert_eq!(
            edges.update(true),
            [
                Action::BoundsRedraw(true),
                Action::Feed(true),
                Action::SyncDraw
            ]
        );
    }

    #[test]
    fn variants_cover_each_workaround_and_all() {
        let names: Vec<Vec<&str>> = Workarounds::VARIANTS
            .iter()
            .map(|variant| Workarounds::from_variant(variant).unwrap().names())
            .collect();
        assert_eq!(
            names,
            [
                vec![],
                vec!["bounds"],
                vec!["defer"],
                vec!["syncdraw"],
                vec!["bounds", "defer", "syncdraw"]
            ]
        );
        assert_eq!(Workarounds::from_variant("sync-draw"), None);
    }

    #[test]
    fn occlusion_mask_hides_only_the_occlusion_bit() {
        assert!(occluded(false, false), "без бита Visible окно перекрыто");
        assert!(!occluded(true, false));
        assert!(!occluded(false, true), "маска стенда снимает признак");
        assert!(!occluded(true, true));
    }

    #[test]
    fn fed_sequence_alternates() {
        assert!(fronts_alternate(&[]));
        assert!(fronts_alternate(&[false, true, false]));
        assert!(!fronts_alternate(&[false, false]));
        assert!(!fronts_alternate(&[true]), "первая подача true — повтор");
    }
}
