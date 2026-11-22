//! Размещение компонентов в окне и в [`Surface`] без типов objc2 снаружи, и слабое наблюдение за
//! видами освобождённого дерева компонентов (`ui.stats.gallery_trees`).

use objc2::rc;
use objc2_app_kit::{NSView, NSWindowOrderingMode};
use objc2_foundation::{NSObject, NSPoint, NSRect, NSSize};

use crate::chrome_bar::Frame;
use crate::components::menu::MenuButton;
use crate::components::text_field::TextField;
use crate::marker::GenerationMarker;
use crate::surface::Surface;
use crate::window::MainWindow;

/// Компонент, который можно разместить в окне или в подложке.
#[derive(Clone, Copy)]
pub enum Piece<'a> {
    Surface(&'a Surface),
    TextField(&'a TextField),
    MenuButton(&'a MenuButton),
    Marker(&'a GenerationMarker),
}

impl Piece<'_> {
    pub(crate) fn ns_view(&self) -> &NSView {
        match self {
            Piece::Surface(surface) => surface.ns_view(),
            Piece::TextField(field) => field.ns_view(),
            Piece::MenuButton(button) => button.ns_view(),
            Piece::Marker(marker) => marker.ns_view(),
        }
    }
}

impl<'a> From<&'a Surface> for Piece<'a> {
    fn from(surface: &'a Surface) -> Self {
        Piece::Surface(surface)
    }
}

impl<'a> From<&'a TextField> for Piece<'a> {
    fn from(field: &'a TextField) -> Self {
        Piece::TextField(field)
    }
}

impl<'a> From<&'a MenuButton> for Piece<'a> {
    fn from(button: &'a MenuButton) -> Self {
        Piece::MenuButton(button)
    }
}

impl<'a> From<&'a GenerationMarker> for Piece<'a> {
    fn from(marker: &'a GenerationMarker) -> Self {
        Piece::Marker(marker)
    }
}

pub(crate) fn ns_rect(frame: Frame) -> NSRect {
    NSRect::new(
        NSPoint::new(frame.x, frame.y),
        NSSize::new(frame.width, frame.height),
    )
}

/// Ставит вид в `parent` с кадром `frame` в точках родителя; `below` — вид, под которым он
/// ляжет. Освобождение компонента убирает его вид из иерархии.
pub(crate) fn place(parent: &NSView, view: &NSView, frame: Frame, below: Option<&NSView>) {
    view.setFrame(ns_rect(frame));
    match below {
        Some(top) => {
            parent.addSubview_positioned_relativeTo(view, NSWindowOrderingMode::Below, Some(top));
        }
        None => parent.addSubview(view),
    }
}

/// Слабые ссылки на объекты дерева компонентов: после освобождения дерева живых не остаётся.
#[derive(Default)]
pub struct ViewWatch {
    objects: Vec<rc::Weak<NSObject>>,
}

impl ViewWatch {
    pub fn new() -> ViewWatch {
        ViewWatch::default()
    }

    /// Вид компонента и все его подвиды на этот момент.
    pub fn watch(&mut self, piece: Piece<'_>) {
        self.watch_view(piece.ns_view());
    }

    /// Окно, его содержимое и все виды содержимого на этот момент.
    pub fn watch_window(&mut self, window: &MainWindow) {
        let ns_window = window.ns_window();
        let object: &NSObject = ns_window.as_ref();
        self.objects.push(rc::Weak::new(object));
        if let Some(content) = ns_window.contentView() {
            self.watch_view(&content);
        }
    }

    fn watch_view(&mut self, view: &NSView) {
        let object: &NSObject = view.as_ref();
        self.objects.push(rc::Weak::new(object));
        for child in view.subviews().iter() {
            self.watch_view(&child);
        }
    }

    /// Объектов под наблюдением.
    pub fn len(&self) -> u64 {
        u64::try_from(self.objects.len()).unwrap_or(u64::MAX)
    }

    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }

    /// Из них ещё живы.
    pub fn alive(&self) -> u64 {
        let alive = self
            .objects
            .iter()
            .filter(|object| object.load().is_some())
            .count();
        u64::try_from(alive).unwrap_or(u64::MAX)
    }
}
