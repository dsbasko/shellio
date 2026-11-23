//! `Plate` — плитка части зоны (ADR-0024, ADR-0027): [`Surface`] с заливкой панели, линией,
//! радиусом плитки и слоем фокуса, внутри [`TabStrip`] и тело. В тело ставятся виды панелей; их
//! переставляют между плитками без пересоздания. Строка вкладок и тело идут за размером плитки,
//! когда пользователь тянет разделитель сплита.

use objc2_app_kit::NSAutoresizingMaskOptions;
use shellio_theme::{AppTheme, KeyMode, Motion};

use crate::app::App;
use crate::chrome_bar::Frame;
use crate::components::menu::Menu;
use crate::components::tab_strip::TabStrip;
use crate::components::tab_strip::model::{TabError, TabIntent, TabStripState};
use crate::inspect::{self, InspectNode, Transition};
use crate::layer::without_actions;
use crate::own::ComponentId;
use crate::piece::{self, Piece};
use crate::surface::{FocusLayer, Surface, SurfaceFill, SurfaceRadius, SurfaceSpec};

/// Состояние подложки плитки: тон цвета режима у части с фокусом, вуаль у остальных (ADR-0027).
pub fn plate_spec(focused: bool, mode: KeyMode) -> SurfaceSpec {
    SurfaceSpec {
        fill: SurfaceFill::Panel,
        line: true,
        radius: SurfaceRadius::Plate,
        layer: if focused {
            FocusLayer::Tone(mode)
        } else {
            FocusLayer::Veil
        },
    }
}

/// Плитка на главном потоке. Освобождение убирает её вид и строку вкладок из иерархии; виды
/// панелей в теле владелец переставляет до этого.
pub struct Plate {
    // Порядок полей — порядок освобождения: сначала строка вкладок внутри подложки.
    strip: TabStrip,
    surface: Surface,
}

impl Plate {
    /// Плитка со строкой вкладок `state`. `on_intent` вызывается на главном потоке изнутри AppKit
    /// и не должен синхронно менять плитку.
    pub fn new(
        app: &App,
        theme: &AppTheme,
        motion: Motion,
        state: TabStripState,
        on_intent: impl Fn(TabIntent) + 'static,
    ) -> Result<Plate, TabError> {
        let surface = Surface::new(app, theme, plate_spec(state.focused, state.mode));
        let strip = TabStrip::new(app, theme, motion, state, on_intent)?;
        surface.place(&strip, Frame::new(0.0, 0.0, 0.0, 0.0));
        strip.ns_view().setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMaxYMargin,
        );
        Ok(Plate { strip, surface })
    }

    pub fn id(&self) -> ComponentId {
        self.surface.id()
    }

    pub fn strip(&self) -> &TabStrip {
        &self.strip
    }

    pub fn surface(&self) -> &Surface {
        &self.surface
    }

    /// Открывает меню выбора у кнопки «Разделить» строки вкладок.
    pub fn pop_up_choice(&self, menu: &Menu) {
        self.strip.pop_up_choice(menu);
    }

    /// Новое состояние строки вкладок и слой фокуса по нему.
    pub fn set_state(&self, state: TabStripState) -> Result<Transition, TabError> {
        self.surface.set_spec(plate_spec(state.focused, state.mode));
        self.strip.set_state(state)
    }

    /// Новая ячейка темы.
    pub fn set_theme(&self, theme: &AppTheme) -> Transition {
        self.surface.set_theme(theme);
        self.strip.set_theme(theme)
    }

    /// Прячет или показывает плитку без неявных анимаций.
    pub fn set_hidden(&self, hidden: bool) {
        let view = self.surface.ns_view();
        if view.isHidden() != hidden {
            without_actions(|| view.setHidden(hidden));
        }
    }

    pub fn set_motion(&self, motion: Motion) {
        self.strip.set_motion(motion);
    }

    /// Кадр строки вкладок в точках плитки.
    pub fn set_strip_frame(&self, frame: Frame) {
        let view = self.strip.ns_view();
        let rect = piece::ns_rect(frame);
        if view.frame() != rect {
            view.setFrame(rect);
        }
    }

    /// Ставит вид в тело с кадром `frame` в точках плитки под слоем фокуса. Вид из другого
    /// родителя переставляется без пересоздания и растягивается вместе с плиткой.
    pub fn place_body<'a>(&self, piece: impl Into<Piece<'a>>, frame: Frame) {
        let piece = piece.into();
        let view = piece.ns_view();
        let surface = self.surface.ns_view();
        // SAFETY: `superview` у живого вида только читается.
        let parent = unsafe { view.superview() };
        if parent.is_some_and(|parent| std::ptr::eq(&*parent, surface)) {
            let rect = piece::ns_rect(frame);
            if view.frame() != rect {
                view.setFrame(rect);
            }
        } else {
            self.surface.place(piece, frame);
        }
        view.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
    }

    pub(crate) fn ns_view(&self) -> &objc2_app_kit::NSView {
        self.surface.ns_view()
    }

    /// Плитка и строка вкладок для `ui.inspect`: строка вкладок — `<id>.tabs`.
    pub fn inspect(&self, id: &str, parent: Option<&str>) -> Vec<InspectNode> {
        let mut nodes = vec![inspect::node(
            id,
            "Plate",
            parent,
            self.surface.ns_view(),
            None,
            None,
        )];
        nodes.extend(self.strip.inspect(&format!("{id}.tabs"), Some(id)));
        nodes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focused_plate_takes_mode_tone_and_others_veil() {
        for mode in KeyMode::ALL {
            assert_eq!(plate_spec(true, mode).layer, FocusLayer::Tone(mode));
            assert_eq!(plate_spec(false, mode).layer, FocusLayer::Veil);
        }
        let spec = plate_spec(true, KeyMode::Navigation);
        assert_eq!(
            (spec.fill, spec.line, spec.radius),
            (SurfaceFill::Panel, true, SurfaceRadius::Plate)
        );
    }
}
