//! Хост части зоны сценария `zones` (задача 15): у пробы нет адаптера раскладки, поэтому вид diff
//! ставится в часть зоны заданного размера у верхнего левого угла содержимого окна. Ниже
//! `min_size` хост показывает заглушку «Развернуть» вместо вида и не меняет рамку вида: строки не
//! рисуются, клавиши diff до вида не доходят. Код хоста — вне `view_a` и `view_b`: стоимость
//! вариантов он не меняет.

use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly, Message};
use objc2_app_kit::{NSButton, NSColor, NSResponder, NSScrollView, NSView, NSWindow};
use objc2_foundation::{NSPoint, NSRect, NSSize, ns_string};

use probe_diff_view::model::layout::{DiffLayout, Zone};
use probe_diff_view::tokens::{Theme, ThemeKey};

use crate::scene::Scene;

/// Допуск видимой области против заданной части, точки.
pub const VISIBLE_TOLERANCE: f64 = 0.5;

/// Что хост показал в части зоны.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Placed {
    Diff,
    Placeholder,
}

pub struct Host {
    root: Retained<NSView>,
    placeholder: Retained<NSButton>,
    scroll: Retained<NSScrollView>,
    /// Разница рамки вида прокрутки и его `contentSize`: полоса прокрутки.
    chrome: NSSize,
}

impl Host {
    /// Корневой вид хоста — содержимое окна, в нём вид diff и скрытая заглушка. Фон за частью —
    /// фон окна цвета `background` темы: поверхность непрозрачна.
    pub fn install(
        mtm: MainThreadMarker,
        window: &NSWindow,
        scroll: &NSScrollView,
        theme: ThemeKey,
    ) -> Host {
        let [r, g, b, a] = Theme::of(theme).background.components();
        window.setBackgroundColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
            r, g, b, a,
        )));
        let bounds = window
            .contentView()
            .map(|content| content.bounds())
            .unwrap_or_else(|| window.contentLayoutRect());
        let frame = scroll.frame();
        let content = scroll.contentSize();
        let chrome = NSSize::new(
            frame.size.width - content.width,
            frame.size.height - content.height,
        );
        let root = NSView::initWithFrame(NSView::alloc(mtm), bounds);
        window.setContentView(Some(&root));
        root.addSubview(scroll);
        let placeholder = NSButton::new(mtm);
        placeholder.setTitle(ns_string!("Развернуть"));
        placeholder.sizeToFit();
        placeholder.setHidden(true);
        root.addSubview(&placeholder);
        Host {
            root,
            placeholder,
            scroll: scroll.retain(),
            chrome,
        }
    }

    pub fn placeholder(&self) -> &NSButton {
        &self.placeholder
    }

    /// Заглушка видима в окне.
    pub fn placeholder_shown(&self) -> bool {
        !self.placeholder.isHiddenOrHasHiddenAncestor() && self.placeholder.window().is_some()
    }

    /// Вид diff видим в окне.
    pub fn diff_shown(&self) -> bool {
        !self.scroll.isHiddenOrHasHiddenAncestor() && self.scroll.window().is_some()
    }

    /// Видимая область документа.
    pub fn visible(&self) -> NSSize {
        self.scroll.documentVisibleRect().size
    }

    /// Часть зоны `zone` размером `width × height` у верхнего левого угла. Раскладку решает
    /// `adapt` сцены: ниже `min_size` — заглушка и first responder не у документа, иначе вид
    /// diff с видимой областью ровно `width × height` и first responder `document`.
    pub fn place(
        &self,
        window: &NSWindow,
        scene: &Scene,
        document: &NSResponder,
        zone: Zone,
        width: f64,
        height: f64,
    ) -> Result<Placed, String> {
        let layout = scene
            .layout_at(zone, width, height)
            .ok_or_else(|| "diff не открыт".to_owned())?;
        let top = self.root.bounds().size.height;
        match layout {
            DiffLayout::Placeholder => {
                self.scroll.setHidden(true);
                let size = self.placeholder.frame().size;
                self.placeholder.setFrameOrigin(NSPoint::new(
                    ((width - size.width) / 2.0).max(0.0).round(),
                    (top - height / 2.0 - size.height / 2.0).round(),
                ));
                self.placeholder.setHidden(false);
                // Кнопка берёт фокус только при полном доступе с клавиатуры, иначе фокус у
                // окна: клавиши diff до скрытого вида не доходят ни в одном случае.
                if !window.makeFirstResponder(Some(&self.placeholder)) {
                    window.makeFirstResponder(None);
                }
                Ok(Placed::Placeholder)
            }
            DiffLayout::Diff(_) => {
                self.placeholder.setHidden(true);
                let frame = NSRect::new(
                    NSPoint::new(0.0, top - height - self.chrome.height),
                    NSSize::new(width + self.chrome.width, height + self.chrome.height),
                );
                self.scroll.setFrame(frame);
                self.scroll.setHidden(false);
                scene.set_zone(zone);
                if !window.makeFirstResponder(Some(document)) {
                    return Err("документ не стал first responder после смены части".to_owned());
                }
                let visible = self.visible();
                if (visible.width - width).abs() > VISIBLE_TOLERANCE
                    || (visible.height - height).abs() > VISIBLE_TOLERANCE
                {
                    return Err(format!(
                        "видимая область {} × {} вместо части {width} × {height}",
                        visible.width, visible.height
                    ));
                }
                Ok(Placed::Diff)
            }
        }
    }
}
