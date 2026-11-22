//! `Surface` — непрозрачная подложка: заливка постоянного слота, линия по краю, радиус и слой
//! фокуса над содержимым (вуаль панели без фокуса или тон цвета режима у панели с фокусом,
//! ADR-0027). Смена фокуса меняет только слой фокуса, смена ячейки перекрашивает всё без неявных
//! анимаций. Плитки P1.4 строятся на нём.
//!
//! Слой фокуса — отдельный вид поверх содержимого, как `::after` панели прототипа: он лежит над
//! текстом и фоном внутри линии и не принимает нажатий.

use std::cell::Cell;

use objc2::rc::Retained;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSResponder, NSView, NSViewLayerContentsRedrawPolicy,
};
use objc2_foundation::{NSObject, NSPoint, NSRect};
use shellio_theme::{AppTheme, KeyMode, Srgb8, Tint};

use crate::app::App;
use crate::chrome_bar::Frame;
use crate::counters::{Live, counter};
use crate::inspect::{self, InspectNode};
use crate::layer::{self, without_actions};
use crate::own::{Callback, ComponentId, View};
use crate::piece::{self, Piece};
use crate::runloop;

counter!(SURFACE_OWNER, Owner, "Surface");
counter!(SURFACE_VIEW, Class, "ShellioSurface");
counter!(SURFACE_OVERLAY, Class, "ShellioSurfaceOverlay");

/// Толщина линии по краю в точках (`border: 1px solid var(--line)` прототипа).
pub const LINE_WIDTH: f64 = 1.0;

/// Постоянный слот заливки.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SurfaceFill {
    Chrome,
    Surface,
    Head,
    /// Тело плитки (ADR-0027).
    Panel,
    /// Заголовок плитки (ADR-0027).
    PanelHead,
}

/// Радиус из токенов темы.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SurfaceRadius {
    None,
    Row,
    Control,
    Plate,
    Float,
}

/// Слой фокуса над содержимым.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FocusLayer {
    /// Без слоя.
    Plain,
    /// Вуаль панели без фокуса.
    Veil,
    /// Тон цвета режима у панели с фокусом.
    Tone(KeyMode),
}

/// Состояние `Surface`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SurfaceSpec {
    pub fill: SurfaceFill,
    pub line: bool,
    pub radius: SurfaceRadius,
    pub layer: FocusLayer,
}

/// Цвета и геометрия `Surface` в ячейке темы: чистая часть, которую применяет вид.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SurfacePaint {
    pub fill: Srgb8,
    pub line: Option<Srgb8>,
    pub line_width: f64,
    pub radius: f64,
    pub overlay: Option<Tint>,
}

impl SurfacePaint {
    pub fn resolve(theme: &AppTheme, spec: SurfaceSpec) -> SurfacePaint {
        let colors = &theme.colors;
        let radii = &theme.metrics.radius;
        SurfacePaint {
            fill: match spec.fill {
                SurfaceFill::Chrome => colors.chrome,
                SurfaceFill::Surface => colors.surface,
                SurfaceFill::Head => colors.head,
                SurfaceFill::Panel => colors.panel,
                SurfaceFill::PanelHead => colors.panel_head,
            },
            line: spec.line.then_some(colors.line),
            line_width: if spec.line { LINE_WIDTH } else { 0.0 },
            radius: match spec.radius {
                SurfaceRadius::None => 0.0,
                SurfaceRadius::Row => radii.row,
                SurfaceRadius::Control => radii.control,
                SurfaceRadius::Plate => radii.plate,
                SurfaceRadius::Float => radii.float,
            },
            overlay: match spec.layer {
                FocusLayer::Plain => None,
                FocusLayer::Veil => Some(theme.focus.veil),
                FocusLayer::Tone(mode) => Some(theme.tone(mode)),
            },
        }
    }
}

/// Rust-часть `Surface`. Виды читают её через слабую ссылку.
struct SurfaceModel {
    id: ComponentId,
    spec: Cell<SurfaceSpec>,
    theme: Cell<AppTheme>,
}

impl SurfaceModel {
    fn paint(&self) -> SurfacePaint {
        SurfacePaint::resolve(&self.theme.get(), self.spec.get())
    }
}

/// Подложка на главном потоке. Освобождение убирает её вид из иерархии.
pub struct Surface {
    view: View<SurfaceView, SurfaceModel>,
}

impl Surface {
    pub fn new(app: &App, theme: &AppTheme, spec: SurfaceSpec) -> Surface {
        let mtm = app.mtm();
        let model = SurfaceModel {
            id: ComponentId::next(),
            spec: Cell::new(spec),
            theme: Cell::new(*theme),
        };
        let view = View::new(&SURFACE_OWNER, model, "Surface.updateLayer", |owner| {
            let overlay = SurfaceOverlay::new(mtm, owner.clone());
            SurfaceView::new(mtm, owner, overlay)
        });
        let surface = Surface { view };
        surface.show_overlay(surface.paint().overlay.is_some());
        surface
    }

    pub fn id(&self) -> ComponentId {
        self.view.rust().id
    }

    pub fn spec(&self) -> SurfaceSpec {
        self.view.rust().spec.get()
    }

    pub fn paint(&self) -> SurfacePaint {
        self.view.rust().paint()
    }

    /// Новое состояние; виды перерисовываются, только если изменились их цвета или геометрия.
    pub fn set_spec(&self, spec: SurfaceSpec) {
        self.change(|model| model.spec.set(spec));
    }

    /// Новая ячейка темы.
    pub fn set_theme(&self, theme: &AppTheme) {
        self.change(|model| model.theme.set(*theme));
    }

    /// Вид подложки для размещения в иерархии.
    pub(crate) fn ns_view(&self) -> &NSView {
        self.view.view()
    }

    /// Ставит компонент в подложку под слой фокуса с кадром `frame` в точках подложки, начало
    /// сверху слева: слой фокуса остаётся верхним.
    pub fn place<'a>(&self, piece: impl Into<Piece<'a>>, frame: Frame) {
        let surface = self.view.view();
        let overlay: &NSView = &surface.ivars().overlay;
        piece::place(surface, piece.into().ns_view(), frame, Some(overlay));
    }

    /// Подложка для `ui.inspect`.
    pub fn inspect(&self, id: &str, parent: Option<&str>) -> Vec<InspectNode> {
        vec![inspect::node(
            id,
            "Surface",
            parent,
            self.view.view(),
            None,
            None,
        )]
    }

    fn change(&self, update: impl FnOnce(&SurfaceModel)) {
        let model = self.view.rust();
        let before = model.paint();
        update(model);
        let after = model.paint();
        if before == after {
            return;
        }
        let surface = self.view.view();
        if (before.fill, before.line, before.line_width, before.radius)
            != (after.fill, after.line, after.line_width, after.radius)
        {
            surface.setNeedsDisplay(true);
        }
        if (before.overlay, before.radius) != (after.overlay, after.radius) {
            self.show_overlay(after.overlay.is_some());
            surface.ivars().overlay.setNeedsDisplay(true);
        }
    }

    fn show_overlay(&self, visible: bool) {
        let overlay = &self.view.view().ivars().overlay;
        without_actions(|| overlay.setHidden(!visible));
    }
}

struct SurfaceIvars {
    _live: Live,
    owner: Callback<SurfaceModel>,
    overlay: Retained<SurfaceOverlay>,
}

define_class!(
    // SAFETY: NSView не требует от подкласса ничего сверх переопределённых ниже методов, `Drop` у
    // класса нет.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioSurface"]
    #[ivars = SurfaceIvars]
    struct SurfaceView;

    impl SurfaceView {
        // SAFETY: селектор и результат — как у `isFlipped` NSView.
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        // SAFETY: селектор и результат — как у `wantsUpdateLayer` NSView.
        #[unsafe(method(wantsUpdateLayer))]
        fn wants_update_layer(&self) -> bool {
            true
        }

        // SAFETY: селектор и результат — как у `updateLayer` NSView.
        #[unsafe(method(updateLayer))]
        fn update_layer(&self) {
            let Some(paint) = self.ivars().owner.call(|model| {
                runloop::mark_draw(model.id);
                model.paint()
            }) else {
                return;
            };
            if let Some(layer) = self.layer() {
                without_actions(|| {
                    layer::set_fill(&layer, paint.fill);
                    layer::set_line(&layer, paint.line.map(|line| (line, paint.line_width)));
                    layer::set_radius(&layer, paint.radius);
                });
            }
        }
    }
);

impl SurfaceView {
    fn new(
        mtm: MainThreadMarker,
        owner: Callback<SurfaceModel>,
        overlay: Retained<SurfaceOverlay>,
    ) -> Retained<SurfaceView> {
        let this = SurfaceView::alloc(mtm).set_ivars(SurfaceIvars {
            _live: SURFACE_VIEW.live(),
            owner,
            overlay,
        });
        // SAFETY: назначенный инициализатор NSView, ivars уже заданы.
        let view: Retained<SurfaceView> =
            unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        layered(&view);
        let overlay = &view.ivars().overlay;
        overlay.setFrame(view.bounds());
        overlay.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        view.addSubview(overlay);
        view
    }
}

struct OverlayIvars {
    _live: Live,
    owner: Callback<SurfaceModel>,
}

define_class!(
    // SAFETY: NSView не требует от подкласса ничего сверх переопределённых ниже методов, `Drop` у
    // класса нет.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioSurfaceOverlay"]
    #[ivars = OverlayIvars]
    struct SurfaceOverlay;

    impl SurfaceOverlay {
        // SAFETY: селектор и результат — как у `wantsUpdateLayer` NSView.
        #[unsafe(method(wantsUpdateLayer))]
        fn wants_update_layer(&self) -> bool {
            true
        }

        // SAFETY: селектор, аргумент и результат — как у `hitTest:` NSView. Слой фокуса нажатий не
        // принимает: `nil` передаёт их видам под ним.
        #[unsafe(method(hitTest:))]
        fn hit_test(&self, _point: NSPoint) -> *mut NSView {
            std::ptr::null_mut()
        }

        // SAFETY: селектор и результат — как у `updateLayer` NSView.
        #[unsafe(method(updateLayer))]
        fn update_layer(&self) {
            let Some(paint) = self.ivars().owner.call(|model| {
                runloop::mark_draw(model.id);
                model.paint()
            }) else {
                return;
            };
            if let (Some(layer), Some(tint)) = (self.layer(), paint.overlay) {
                without_actions(|| {
                    layer::set_tint(&layer, tint);
                    layer::set_radius(&layer, paint.radius);
                });
            }
        }
    }
);

impl SurfaceOverlay {
    fn new(mtm: MainThreadMarker, owner: Callback<SurfaceModel>) -> Retained<SurfaceOverlay> {
        let this = SurfaceOverlay::alloc(mtm).set_ivars(OverlayIvars {
            _live: SURFACE_OVERLAY.live(),
            owner,
        });
        // SAFETY: назначенный инициализатор NSView, ivars уже заданы.
        let view: Retained<SurfaceOverlay> =
            unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        layered(&view);
        view
    }
}

/// Вид со своим слоем, который перерисовывается только по `setNeedsDisplay`.
fn layered(view: &NSView) {
    view.setWantsLayer(true);
    view.setLayerContentsRedrawPolicy(NSViewLayerContentsRedrawPolicy::OnSetNeedsDisplay);
}

#[cfg(test)]
mod tests {
    use shellio_theme::{Appearance, Cell as ThemeCell, Contrast};

    use super::*;

    fn spec(layer: FocusLayer) -> SurfaceSpec {
        SurfaceSpec {
            fill: SurfaceFill::Panel,
            line: true,
            radius: SurfaceRadius::Plate,
            layer,
        }
    }

    #[test]
    fn paint_takes_slots_radius_and_line_from_theme() {
        for cell in ThemeCell::ALL {
            let theme = AppTheme::for_cell(cell);
            let paint = SurfacePaint::resolve(&theme, spec(FocusLayer::Plain));
            assert_eq!(paint.fill, theme.colors.panel, "{}", cell.name());
            assert_eq!(paint.line, Some(theme.colors.line));
            assert_eq!(paint.line_width, LINE_WIDTH);
            assert_eq!(paint.radius, theme.metrics.radius.plate);
            assert_eq!(paint.overlay, None);
        }
    }

    #[test]
    fn every_fill_and_radius_maps_to_its_token() {
        let theme = AppTheme::for_cell(ThemeCell::new(Appearance::Light, Contrast::Increased));
        let fills = [
            (SurfaceFill::Chrome, theme.colors.chrome),
            (SurfaceFill::Surface, theme.colors.surface),
            (SurfaceFill::Head, theme.colors.head),
            (SurfaceFill::Panel, theme.colors.panel),
            (SurfaceFill::PanelHead, theme.colors.panel_head),
        ];
        for (fill, expected) in fills {
            let paint = SurfacePaint::resolve(
                &theme,
                SurfaceSpec {
                    fill,
                    ..spec(FocusLayer::Plain)
                },
            );
            assert_eq!(paint.fill, expected, "{fill:?}");
        }
        let radii = theme.metrics.radius;
        let cases = [
            (SurfaceRadius::None, 0.0),
            (SurfaceRadius::Row, radii.row),
            (SurfaceRadius::Control, radii.control),
            (SurfaceRadius::Plate, radii.plate),
            (SurfaceRadius::Float, radii.float),
        ];
        for (radius, expected) in cases {
            let paint = SurfacePaint::resolve(
                &theme,
                SurfaceSpec {
                    radius,
                    ..spec(FocusLayer::Plain)
                },
            );
            assert_eq!(paint.radius, expected, "{radius:?}");
        }
    }

    #[test]
    fn without_line_there_is_no_border() {
        let theme = AppTheme::for_cell(ThemeCell::ALL[0]);
        let paint = SurfacePaint::resolve(
            &theme,
            SurfaceSpec {
                line: false,
                ..spec(FocusLayer::Plain)
            },
        );
        assert_eq!((paint.line, paint.line_width), (None, 0.0));
    }

    #[test]
    fn focus_layer_is_veil_or_mode_tone_of_the_cell() {
        for cell in ThemeCell::ALL {
            let theme = AppTheme::for_cell(cell);
            let veil = SurfacePaint::resolve(&theme, spec(FocusLayer::Veil));
            assert_eq!(veil.overlay, Some(theme.focus.veil), "{}", cell.name());
            for mode in KeyMode::ALL {
                let tone = SurfacePaint::resolve(&theme, spec(FocusLayer::Tone(mode)));
                assert_eq!(tone.overlay, Some(theme.tone(mode)), "{}", cell.name());
                assert_eq!(tone.fill, veil.fill, "фокус меняет только слой над панелью");
            }
        }
    }

    #[test]
    fn cell_change_repaints_every_part() {
        let dark = AppTheme::for_cell(ThemeCell::new(Appearance::Dark, Contrast::Normal));
        let light = AppTheme::for_cell(ThemeCell::new(Appearance::Light, Contrast::Normal));
        let before = SurfacePaint::resolve(&dark, spec(FocusLayer::Veil));
        let after = SurfacePaint::resolve(&light, spec(FocusLayer::Veil));
        assert_ne!(before.fill, after.fill);
        assert_ne!(before.line, after.line);
        assert_ne!(before.overlay, after.overlay);
    }
}
