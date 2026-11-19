//! Общая подготовка сценариев вида: фикстура, вид варианта, сцена, стенд событий и учёт покрытия
//! видимых строк. Вынесено из `open_smoke.rs` (задача 10): сценарий `ax` открывает diff так же.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSResponder, NSScrollView, NSWindow};
use objc2_foundation::NSRect;
use serde::Serialize;

use probe_diff_view::fixtures::{self, FixtureName};
use probe_diff_view::model::{Mode, Side};

use super::drawmark::{self, Ledger};
use super::events;
use super::watchdog::SharedTrace;
use crate::args::Variant;
use crate::results::Report;
use crate::scene::{FontMetrics, Scene, Surface};
use crate::view_a::ViewA;
use crate::view_b::ViewB;

/// Покрытие видимых строк: поколение учёта и время отметки, которая его закрыла.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Coverage {
    pub generation: u64,
    pub at_ns: u64,
    pub rows: (u32, u32),
}

/// Вид варианта сценария: шаги общие, наблюдения вида свои.
pub enum View {
    A(Rc<ViewA>),
    B(Rc<ViewB>),
}

impl View {
    pub fn new(
        variant: Variant,
        mtm: MainThreadMarker,
        bounds: NSRect,
        theme: probe_diff_view::tokens::ThemeKey,
    ) -> View {
        match variant {
            Variant::A => View::A(ViewA::new(mtm, bounds, theme)),
            Variant::B => View::B(ViewB::new(mtm, bounds, theme)),
        }
    }

    pub fn bind(&self, scene: &Rc<Scene>) {
        match self {
            View::A(view) => view.bind(scene),
            View::B(view) => view.bind(scene),
        }
    }

    pub fn surface(&self) -> Rc<dyn Surface> {
        match self {
            View::A(view) => Rc::clone(view) as Rc<dyn Surface>,
            View::B(view) => Rc::clone(view) as Rc<dyn Surface>,
        }
    }

    pub fn font(&self) -> FontMetrics {
        match self {
            View::A(view) => view.font(),
            View::B(view) => view.font(),
        }
    }

    pub fn scroll_view(&self) -> &NSScrollView {
        match self {
            View::A(view) => view.scroll_view(),
            View::B(view) => view.scroll_view(),
        }
    }

    pub fn responder(&self) -> &NSResponder {
        match self {
            View::A(view) => view.responder(),
            View::B(view) => view.responder(),
        }
    }

    pub fn viewport_y(&self) -> f64 {
        match self {
            View::A(view) => view.viewport_y(),
            View::B(view) => view.viewport_y(),
        }
    }

    pub fn draft_shown(&self) -> Option<NSRect> {
        match self {
            View::A(view) => view.draft_shown(),
            View::B(view) => view.draft_shown(),
        }
    }

    pub fn draft_focused(&self) -> bool {
        match self {
            View::A(view) => view.draft_focused(),
            View::B(view) => view.draft_focused(),
        }
    }

    pub fn document_focused(&self) -> bool {
        match self {
            View::A(view) => view.document_focused(),
            View::B(view) => view.document_focused(),
        }
    }

    pub fn to_window(&self, area: NSRect) -> NSRect {
        match self {
            View::A(view) => view.to_window(area),
            View::B(view) => view.to_window(area),
        }
    }

    pub fn row_rect(&self, row: u32) -> Option<NSRect> {
        match self {
            View::A(view) => view.row_rect(row),
            View::B(view) => view.row_rect(row),
        }
    }

    pub fn half_rect(&self, row: u32, side: Side) -> Option<NSRect> {
        match self {
            View::A(view) => view.half_rect(row, side),
            View::B(view) => view.half_rect(row, side),
        }
    }

    pub fn scroll_to(&self, y: f64) {
        match self {
            View::A(view) => view.scroll_to(y),
            View::B(view) => view.scroll_to(y),
        }
    }

    pub fn document_frame(&self) -> NSRect {
        match self {
            View::A(view) => view.document_frame(),
            View::B(view) => view.document_frame(),
        }
    }
}
/// Открытый для сценария вид: сцена с сырыми байтами фикстуры, вид варианта в окне, учёт
/// покрытия видимых строк и журнал покрытий.
pub struct Stage {
    pub view: View,
    pub scene: Rc<Scene>,
    pub ledger: Rc<RefCell<Ledger>>,
    pub coverage: Rc<RefCell<Vec<Coverage>>>,
}

/// Фикстура `name` в памяти, вид `variant` содержимым окна и first responder, стенд событий с
/// кольцом следа, курсор мыши вне окон пробы, наблюдатели `drawmark` у учёта покрытия. Клавиша
/// открытия не нажимается.
#[allow(clippy::too_many_arguments)]
pub fn prepare(
    mtm: MainThreadMarker,
    window: &NSWindow,
    report: &mut Report,
    trace: SharedTrace,
    variant: Variant,
    mode: Mode,
    name: FixtureName,
    scenario: &str,
) -> Result<Stage, String> {
    let generated = Instant::now();
    let fixture = fixtures::generate(name);
    report.metric(
        "fixture_generate_ms",
        "ms",
        vec![generated.elapsed().as_secs_f64() * 1e3],
    );
    report.push(
        "fixture",
        format!("{}, отпечаток {:016x}", name.as_str(), fixture.digest()),
    );
    let app = NSApplication::sharedApplication(mtm);
    let theme = crate::text::current_theme(mtm);
    report.push("theme", theme.name());
    let bounds = window
        .contentView()
        .map(|content| content.bounds())
        .unwrap_or_else(|| window.contentLayoutRect());
    let surface = View::new(variant, mtm, bounds, theme);
    let font = surface.font();
    report.push(
        "font",
        format!(
            "{} {} точек: клетка {:.4} точки, строка {} точек",
            crate::text::FONT_NAME,
            crate::text::FONT_SIZE,
            font.char_width,
            font.line_height
        ),
    );
    let scene = Scene::new(fixture, mode, font)?;
    surface.bind(&scene);
    scene.attach(surface.surface());
    window.setContentView(Some(surface.scroll_view()));
    let responder: &NSResponder = surface.responder();
    if !window.makeFirstResponder(Some(responder)) || !events::is_first_responder(window, responder)
    {
        return Err(format!(
            "document view не стал first responder: {}",
            events::focus_state(&app)
        ));
    }
    events::install(app.clone(), trace);
    events::note(format!("сценарий {scenario}"));
    let cursor = match events::warp_cursor_outside(mtm, &app) {
        Ok(point) => point,
        Err(reason) => {
            events::uninstall();
            return Err(reason);
        }
    };
    report.push("cursor_warped_to", events::describe_point(cursor));
    drawmark::take();
    drawmark::take_declared();
    let ledger = Rc::new(RefCell::new(Ledger::default()));
    let coverage = Rc::new(RefCell::new(Vec::new()));
    {
        let ledger = Rc::clone(&ledger);
        drawmark::on_declare(move |declared| ledger.borrow_mut().declare(declared));
    }
    {
        let ledger = Rc::clone(&ledger);
        let coverage = Rc::clone(&coverage);
        let scene = Rc::downgrade(&scene);
        drawmark::on_mark(move |mark| {
            let mut ledger = ledger.borrow_mut();
            ledger.mark(mark);
            let Some(scene) = scene.upgrade() else {
                return;
            };
            let visible = scene.visible_rows();
            if visible.is_empty() {
                return;
            }
            let rows = visible.start as usize..visible.end as usize;
            if ledger.uncovered(rows).is_none() {
                let generation = ledger.latest();
                let mut coverage = coverage.borrow_mut();
                if coverage.last().is_none_or(|last: &Coverage| {
                    last.generation != generation || last.rows != (visible.start, visible.end)
                }) {
                    coverage.push(Coverage {
                        generation,
                        at_ns: mark.draw_end_ns,
                        rows: (visible.start, visible.end),
                    });
                }
            }
        });
    }
    Ok(Stage {
        view: surface,
        scene,
        ledger,
        coverage,
    })
}

impl Stage {
    /// Покрыты ли видимые строки сейчас.
    pub fn covered_now(&self) -> bool {
        let visible = self.scene.visible_rows();
        !visible.is_empty()
            && self
                .ledger
                .borrow()
                .uncovered(visible.start as usize..visible.end as usize)
                .is_none()
    }
}
