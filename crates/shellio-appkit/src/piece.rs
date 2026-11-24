//! Размещение компонентов в окне и в [`Surface`] без типов objc2 снаружи, и слабое наблюдение за
//! видами освобождённого дерева компонентов (`ui.stats.gallery_trees`).

use std::collections::{BTreeMap, HashSet};

use objc2::rc;
use objc2_app_kit::{NSView, NSWindowOrderingMode};
use objc2_foundation::{NSObject, NSPoint, NSRect, NSSize};

use crate::chrome_bar::Frame;
use crate::components::menu::MenuButton;
use crate::components::tab_strip::TabStrip;
use crate::components::text_field::TextField;
use crate::components::tree_list::TreeList;
use crate::components::virtual_list::VirtualList;
use crate::marker::GenerationMarker;
use crate::panel_stub::{EmptyNote, ExpandStub, NeutralCard, PanelView};
use crate::plate::Plate;
use crate::split_view::{Pane, SplitView};
use crate::status_line::StatusLine;
use crate::surface::Surface;
use crate::window::MainWindow;

/// Компонент, который можно разместить в окне или в подложке.
#[derive(Clone, Copy)]
pub enum Piece<'a> {
    Surface(&'a Surface),
    TextField(&'a TextField),
    MenuButton(&'a MenuButton),
    TabStrip(&'a TabStrip),
    VirtualList(&'a VirtualList),
    TreeList(&'a TreeList),
    Marker(&'a GenerationMarker),
    SplitView(&'a SplitView),
    Pane(&'a Pane),
    Plate(&'a Plate),
    PanelView(&'a PanelView),
    Card(&'a NeutralCard),
    Stub(&'a ExpandStub),
    Note(&'a EmptyNote),
    Status(&'a StatusLine),
}

impl Piece<'_> {
    pub(crate) fn ns_view(&self) -> &NSView {
        match self {
            Piece::Surface(surface) => surface.ns_view(),
            Piece::TextField(field) => field.ns_view(),
            Piece::MenuButton(button) => button.ns_view(),
            Piece::TabStrip(strip) => strip.ns_view(),
            Piece::VirtualList(list) => list.ns_view(),
            Piece::TreeList(tree) => tree.ns_view(),
            Piece::Marker(marker) => marker.ns_view(),
            Piece::SplitView(split) => split.ns_view(),
            Piece::Pane(pane) => pane.ns_view(),
            Piece::Plate(plate) => plate.ns_view(),
            Piece::PanelView(panel) => panel.ns_view(),
            Piece::Card(card) => card.ns_view(),
            Piece::Stub(stub) => stub.ns_view(),
            Piece::Note(note) => note.ns_view(),
            Piece::Status(line) => line.ns_view(),
        }
    }
}

impl<'a> From<&'a StatusLine> for Piece<'a> {
    fn from(line: &'a StatusLine) -> Self {
        Piece::Status(line)
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

impl<'a> From<&'a TabStrip> for Piece<'a> {
    fn from(strip: &'a TabStrip) -> Self {
        Piece::TabStrip(strip)
    }
}

impl<'a> From<&'a VirtualList> for Piece<'a> {
    fn from(list: &'a VirtualList) -> Self {
        Piece::VirtualList(list)
    }
}

impl<'a> From<&'a TreeList> for Piece<'a> {
    fn from(tree: &'a TreeList) -> Self {
        Piece::TreeList(tree)
    }
}

impl<'a> From<&'a GenerationMarker> for Piece<'a> {
    fn from(marker: &'a GenerationMarker) -> Self {
        Piece::Marker(marker)
    }
}

impl<'a> From<&'a SplitView> for Piece<'a> {
    fn from(split: &'a SplitView) -> Self {
        Piece::SplitView(split)
    }
}

impl<'a> From<&'a Pane> for Piece<'a> {
    fn from(pane: &'a Pane) -> Self {
        Piece::Pane(pane)
    }
}

impl<'a> From<&'a Plate> for Piece<'a> {
    fn from(plate: &'a Plate) -> Self {
        Piece::Plate(plate)
    }
}

impl<'a> From<&'a PanelView> for Piece<'a> {
    fn from(panel: &'a PanelView) -> Self {
        Piece::PanelView(panel)
    }
}

impl<'a> From<&'a NeutralCard> for Piece<'a> {
    fn from(card: &'a NeutralCard) -> Self {
        Piece::Card(card)
    }
}

impl<'a> From<&'a ExpandStub> for Piece<'a> {
    fn from(stub: &'a ExpandStub) -> Self {
        Piece::Stub(stub)
    }
}

impl<'a> From<&'a EmptyNote> for Piece<'a> {
    fn from(note: &'a EmptyNote) -> Self {
        Piece::Note(note)
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
    /// Адреса объектов на момент наблюдения: повтор не добавляется.
    addresses: HashSet<usize>,
}

impl ViewWatch {
    pub fn new() -> ViewWatch {
        ViewWatch::default()
    }

    /// Вид компонента и все его подвиды на этот момент.
    pub fn watch(&mut self, piece: Piece<'_>) {
        self.watch_view(piece.ns_view());
    }

    /// Окно, его рамка со всеми видами (содержимое, системный заголовок и кнопки окна) и field
    /// editor окна на этот момент.
    pub fn watch_window(&mut self, window: &MainWindow) {
        let ns_window = window.ns_window();
        self.watch_object(ns_window.as_ref());
        if let Some(content) = ns_window.contentView() {
            // SAFETY: `superview` у вида содержимого, которое держит окно.
            let frame = unsafe { content.superview() }.unwrap_or(content);
            self.watch_view(&frame);
        }
        // SAFETY: без создания field editor только читается; объект для поиска не нужен.
        if let Some(editor) = unsafe { ns_window.fieldEditor_forObject(false, None) } {
            self.watch_view(&editor);
        }
    }

    fn watch_view(&mut self, view: &NSView) {
        self.watch_object(view.as_ref());
        for child in view.subviews().iter() {
            self.watch_view(&child);
        }
    }

    /// Объект под наблюдением один раз: field editor во время ввода лежит и в иерархии окна.
    fn watch_object(&mut self, object: &NSObject) {
        let address = std::ptr::from_ref(object).addr();
        if self.addresses.insert(address) {
            self.objects.push(rc::Weak::new(object));
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

    /// Живые по именам классов в порядке имён.
    pub fn alive_classes(&self) -> Vec<(String, u64)> {
        let mut counts = BTreeMap::<String, u64>::new();
        for object in self.objects.iter().filter_map(rc::Weak::load) {
            let name = object.class().name().to_string_lossy().into_owned();
            *counts.entry(name).or_default() += 1;
        }
        counts.into_iter().collect()
    }
}
