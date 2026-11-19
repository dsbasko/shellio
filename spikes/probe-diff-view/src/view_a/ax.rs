//! Доступность варианта A. Document view — контейнер с ролью `AXTable`, его строки — свои
//! `NSAccessibilityElement` видимых строк показа. Элемент создаётся для видимой строки и
//! переиспользуется по её адресу (номерам строк сторон): пока строка видна, клиент получает тот
//! же элемент с обновлёнными атрибутами, ушедшая за экран строка свой элемент отпускает. Элемент
//! с другим адресом не перепривязывается. Что сообщает строка, решает общий модуль
//! [`probe_diff_view::access`].

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ops::Range;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AnyThread, MainThreadMarker};
use objc2_app_kit::{
    NSAccessibility, NSAccessibilityElement, NSAccessibilityFocusedUIElementChangedNotification,
    NSAccessibilityLayoutChangedNotification, NSAccessibilityPostNotification,
    NSAccessibilityRowRole, NSAccessibilitySelectedRowsChangedNotification, NSView,
};
use objc2_foundation::{NSInteger, NSPoint, NSRect, NSSize, NSString};
use serde::Serialize;

use probe_diff_view::access::RowAx;

use crate::scene::{Announce, Model};

/// Подпись роли строки для VoiceOver.
pub const ROW_ROLE_DESCRIPTION: &str = "строка diff";
/// Подпись контейнера строк.
pub const TABLE_LABEL: &str = "diff";

/// Счётчики элементов строк и уведомлений варианта A.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct AxStats {
    /// Запросов строк от клиента (`accessibilityVisibleRows` и родственные).
    pub queries: u64,
    pub created: u64,
    /// Строк, получивших прежний элемент своего адреса.
    pub reused: u64,
    /// Элементов, отпущенных при уходе строки за экран.
    pub dropped: u64,
    /// Наибольшее число живых элементов и видимых строк в тот момент.
    pub max_live: usize,
    pub max_visible: usize,
    pub posted_focus: u64,
    pub posted_selection: u64,
    pub posted_layout: u64,
}

type Key = (Option<u32>, Option<u32>);

/// Элементы видимых строк по адресу.
#[derive(Default)]
pub struct AxRows {
    elements: RefCell<HashMap<Key, (u32, Retained<NSAccessibilityElement>)>>,
    stats: Cell<AxStats>,
    pending: Cell<Announce>,
}

/// Прямоугольник текста строки показа в пространстве родителя. AppKit читает рамку
/// `accessibilityFrameInParentSpace` в неперевёрнутых координатах родителя, хотя document view
/// перевёрнут (прогон `20261119-125459-13074`): начало строки отсчитывается от низа документа.
fn row_frame(model: &Model, row: u32, size: NSSize) -> Option<NSRect> {
    let shown = model.shown()?;
    let top = shown.heights.top(row)? as f64;
    let height = f64::from(shown.heights.text_height(row)?);
    Some(NSRect::new(
        NSPoint::new(0.0, size.height - top - height),
        NSSize::new(size.width, height),
    ))
}

impl AxRows {
    pub fn stats(&self) -> AxStats {
        self.stats.get()
    }

    fn count(&self, update: impl FnOnce(&mut AxStats)) {
        let mut stats = self.stats.get();
        update(&mut stats);
        self.stats.set(stats);
    }

    /// Элементы строк `rows` по порядку с атрибутами по модели. Элементы других строк
    /// отпускаются.
    pub fn visible(
        &self,
        mtm: MainThreadMarker,
        parent: &NSView,
        model: &Model,
        rows: Range<u32>,
    ) -> Vec<Retained<NSAccessibilityElement>> {
        let size = parent.bounds().size;
        let mut elements = self.elements.borrow_mut();
        let mut kept: HashMap<Key, (u32, Retained<NSAccessibilityElement>)> = HashMap::new();
        let mut out = Vec::with_capacity(rows.len());
        let (mut created, mut reused) = (0u64, 0u64);
        for row in rows.clone() {
            let Some(ax) = model.row_ax(row) else {
                continue;
            };
            let key = ax.key();
            let element = match elements.remove(&key) {
                Some((_, element)) => {
                    reused += 1;
                    element
                }
                None => {
                    created += 1;
                    new_element(mtm, parent)
                }
            };
            update(&element, &ax, row, row_frame(model, row, size));
            out.push(element.clone());
            kept.insert(key, (row, element));
        }
        let dropped = elements.len() as u64;
        *elements = kept;
        let live = elements.len();
        drop(elements);
        self.count(|stats| {
            stats.queries += 1;
            stats.created += created;
            stats.reused += reused;
            stats.dropped += dropped;
            if live > stats.max_live {
                stats.max_live = live;
                stats.max_visible = rows.len();
            }
        });
        out
    }

    /// Выбранные из `rows`.
    pub fn selected(&self, rows: &[Retained<NSAccessibilityElement>]) -> Vec<Retained<AnyObject>> {
        rows.iter()
            .filter(|element| element.isAccessibilitySelected())
            .map(|element| Retained::into_super(Retained::into_super(element.clone())))
            .collect()
    }

    /// Клиент уже спрашивал строки: элементы есть.
    pub fn active(&self) -> bool {
        !self.elements.borrow().is_empty()
    }

    /// Элемент строки `row`, если он уже выдан.
    pub fn element_of(&self, row: u32) -> Option<Retained<NSAccessibilityElement>> {
        self.elements
            .borrow()
            .values()
            .find(|(at, _)| *at == row)
            .map(|(_, element)| element.clone())
    }

    /// Накопить изменение. `true` — отправку надо запланировать: до этого очередь была пуста.
    pub fn defer(&self, change: Announce) -> bool {
        let before = self.pending.get();
        self.pending.set(before.merge(change));
        before.is_empty()
    }

    /// Накопленное изменение для отправки.
    pub fn take_pending(&self) -> Announce {
        self.pending.replace(Announce::default())
    }

    /// Уведомления о накопленном: фокус на элементе строки курсора или на контейнере, выбранные
    /// строки и видимые строки — на контейнере.
    pub fn post(&self, change: Announce, container: &NSView, focused: &AnyObject) {
        let target: &AnyObject = container;
        // SAFETY: элементы и имена уведомлений — объекты доступности и константы AppKit.
        unsafe {
            if change.layout {
                NSAccessibilityPostNotification(target, NSAccessibilityLayoutChangedNotification);
            }
            if change.selection {
                NSAccessibilityPostNotification(
                    target,
                    NSAccessibilitySelectedRowsChangedNotification,
                );
            }
            if change.focus {
                NSAccessibilityPostNotification(
                    focused,
                    NSAccessibilityFocusedUIElementChangedNotification,
                );
            }
        }
        self.count(|stats| {
            stats.posted_layout += u64::from(change.layout);
            stats.posted_selection += u64::from(change.selection);
            stats.posted_focus += u64::from(change.focus);
        });
    }
}

fn new_element(_mtm: MainThreadMarker, parent: &NSView) -> Retained<NSAccessibilityElement> {
    let element = NSAccessibilityElement::init(NSAccessibilityElement::alloc());
    // SAFETY: роль — константа AppKit, родитель — живой document view, он держит элемент через
    // `AxRows` и отпускает его раньше себя.
    unsafe {
        element.setAccessibilityRole(Some(NSAccessibilityRowRole));
        element.setAccessibilityParent(Some(parent));
    }
    element.setAccessibilityRoleDescription(Some(&NSString::from_str(ROW_ROLE_DESCRIPTION)));
    element
}

fn update(element: &NSAccessibilityElement, ax: &RowAx, row: u32, frame: Option<NSRect>) {
    element.setAccessibilityLabel(Some(&NSString::from_str(&ax.label())));
    let text = NSString::from_str(&ax.text);
    // SAFETY: значение строки — NSString.
    unsafe { element.setAccessibilityValue(Some(&text)) };
    element.setAccessibilitySelected(ax.selected);
    element.setAccessibilityIndex(row as NSInteger);
    if let Some(frame) = frame {
        element.setAccessibilityFrameInParentSpace(frame);
    }
}
