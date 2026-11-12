//! Сборка демонстрационной сцены из компонентов: окно, заголовок, сплит, плитки и навигатор.
//! Начальное окно и first responder назначает этот код, как в обычном приложении.

use std::rc::Rc;

use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_app_kit::{NSApplication, NSWindow, NSWorkspace};
use objc2_foundation::{NSPoint, NSRect};

use crate::args::{Contrast, SceneOptions};
use crate::chrome::{self, ChromeBar, ChromeTarget, SceneRoot};
use crate::split::{self, SplitScene};
use crate::tiles::{CENTER, Tiles};
use crate::tokens::{self, Cell, InputMode, Theme, Tokens};
use crate::{DisplayChoice, WINDOW_SIZE};

pub struct Scene {
    pub window: Retained<NSWindow>,
    pub cell: Cell,
    pub tokens: Tokens,
    pub root: Retained<SceneRoot>,
    pub chrome: Retained<ChromeBar>,
    pub split: SplitScene,
    pub tiles: Rc<Tiles>,
    /// Цель кнопок заголовка: кнопки её не удерживают.
    _target: Retained<ChromeTarget>,
}

impl Scene {
    pub fn build(mtm: MainThreadMarker, display: &DisplayChoice, options: SceneOptions) -> Scene {
        let cell = current_cell(mtm, options);
        let tokens = tokens::tokens(cell, InputMode::Navigation);
        let window = chrome::window(mtm, display, &tokens);
        let bounds = NSRect::new(NSPoint::ZERO, WINDOW_SIZE);
        let root = SceneRoot::new(mtm, bounds, &tokens);
        let tiles = Tiles::new(mtm, tokens, split::zone_sides(options.split, tokens.gap));
        let split = SplitScene::new(
            mtm,
            options.split,
            chrome::rect(
                0.0,
                tokens.titlebar,
                WINDOW_SIZE.width,
                WINDOW_SIZE.height - tokens.titlebar,
            ),
            &tokens,
            [&tiles.navigator, &tiles.center_host, &tiles.right_host],
        );
        let target = ChromeTarget::new(mtm, split.left.clone(), split.right.clone());
        let chrome = ChromeBar::new(mtm, WINDOW_SIZE.width, &tokens, &target);
        root.addSubview(&split.view());
        root.addSubview(&chrome);
        window.setContentView(Some(&root));
        window.makeFirstResponder(Some(&tiles.tiles[CENTER]));
        Scene {
            window,
            cell,
            tokens,
            root,
            chrome,
            split,
            tiles,
            _target: target,
        }
    }
}

/// Ячейка темы по текущему оформлению и контрасту. Полный выбор с подпиской на изменения —
/// задача 10.
fn current_cell(mtm: MainThreadMarker, options: SceneOptions) -> Cell {
    let app = NSApplication::sharedApplication(mtm);
    let theme = if chrome::is_dark(&app.effectiveAppearance()) {
        Theme::Dark
    } else {
        Theme::Light
    };
    let contrast = options.contrast.unwrap_or_else(|| {
        if NSWorkspace::sharedWorkspace().accessibilityDisplayShouldIncreaseContrast() {
            Contrast::More
        } else {
            Contrast::Normal
        }
    });
    Cell::new(theme, contrast)
}
