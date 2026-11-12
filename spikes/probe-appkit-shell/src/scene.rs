//! Сборка демонстрационной сцены из компонентов: окно, заголовок, сплит, плитки и навигатор.
//! Начальное окно и first responder назначает этот код, как в обычном приложении. Смену ячейки
//! темы сцена раздаёт всем компонентам одним проходом.

use std::rc::Rc;

use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_app_kit::{NSApplication, NSMenu, NSWindow};
use objc2_foundation::{NSPoint, NSRect};
use objc2_quartz_core::CATransaction;

use crate::args::SceneOptions;
use crate::chrome::{self, ChromeBar, ChromeTarget, SceneRoot};
use crate::fixtures::{self, MARK_COMMAND, MARK_TITLE, PALETTE_COMMANDS};
use crate::menu::{self, CommandTarget, Commands, MARK_KEY};
use crate::palette::Palette;
use crate::split::{self, SplitScene};
use crate::theme::{Overrides, SceneTheme, Selection};
use crate::tiles::{CENTER, Tiles};
use crate::tokens::{self, Cell, InputMode, Tokens};
use crate::{DisplayChoice, WINDOW_SIZE};

pub struct Scene {
    pub window: Retained<NSWindow>,
    pub theme: Rc<SceneTheme>,
    pub root: Retained<SceneRoot>,
    pub chrome: Retained<ChromeBar>,
    pub split: SplitScene,
    pub tiles: Rc<Tiles>,
    pub palette: Rc<Palette>,
    pub commands: Rc<Commands>,
    /// Меню «Правка» в главном меню.
    pub edit_menu: Option<Retained<NSMenu>>,
    /// Цель кнопок заголовка: кнопки её не удерживают.
    _target: Retained<ChromeTarget>,
    /// Цель пунктов команд: пункты её не удерживают.
    _commands_target: Retained<CommandTarget>,
}

impl Scene {
    pub fn build(
        mtm: MainThreadMarker,
        display: &DisplayChoice,
        options: SceneOptions,
    ) -> Rc<Scene> {
        let theme = SceneTheme::new(
            mtm,
            Overrides {
                contrast: options.contrast,
                motion: options.motion,
            },
        );
        let tokens = theme.tokens();
        let window = chrome::window(mtm, display, &tokens);
        let bounds = NSRect::new(NSPoint::ZERO, WINDOW_SIZE);
        let root = SceneRoot::new(mtm, bounds, &tokens);
        let tiles = Tiles::new(
            mtm,
            tokens,
            theme.motion(),
            split::zone_sides(options.split, tokens.gap),
        );
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

        let commands = Rc::new(Commands::default());
        let palette = Palette::new(
            mtm,
            &window,
            &tokens,
            Rc::clone(&commands),
            fixtures::commands(PALETTE_COMMANDS),
        );
        palette.set_fallback(&tiles.tiles[CENTER]);
        let commands_target =
            CommandTarget::new(mtm, Rc::clone(&commands), Rc::downgrade(&palette));
        let edit_menu = NSApplication::sharedApplication(mtm)
            .mainMenu()
            .map(|bar| menu::install(mtm, &bar, &commands_target, (MARK_COMMAND, MARK_TITLE)));
        for tile in &tiles.tiles {
            let item = commands_target.command_item(mtm, MARK_COMMAND, MARK_TITLE, MARK_KEY);
            tile.strip().add_menu_items(&[item]);
        }
        let scene = Rc::new(Scene {
            window,
            theme,
            root,
            chrome,
            split,
            tiles,
            palette,
            commands,
            edit_menu,
            _target: target,
            _commands_target: commands_target,
        });
        let owner = Rc::downgrade(&scene);
        scene.theme.set_listener(Box::new(move |selection| {
            if let Some(scene) = owner.upgrade() {
                scene.apply(selection);
            }
        }));
        scene.theme.attach(&scene.root);
        let theme = Rc::downgrade(&scene.theme);
        scene.root.set_on_appearance(Box::new(move || {
            if let Some(theme) = theme.upgrade() {
                theme.refresh();
            }
        }));
        // Оформление могло смениться, пока сцена собиралась без подписки.
        scene.theme.refresh();
        scene
    }

    pub fn cell(&self) -> Cell {
        self.theme.cell()
    }

    pub fn tokens(&self) -> Tokens {
        self.theme.tokens()
    }

    /// Новая ячейка и движение для всех компонентов сцены. Отдельные слои (дуга индикатора,
    /// тени палитры) без этого меняли бы цвет неявной анимацией, и при Reduce Motion тоже.
    fn apply(&self, selection: Selection) {
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        let tokens = tokens::tokens(selection.cell, InputMode::Navigation);
        chrome::paint_window(&self.window, &tokens);
        self.root.set_tokens(&tokens);
        self.chrome.set_tokens(&tokens);
        self.split.set_tokens(&tokens);
        self.tiles.set_tokens(tokens);
        self.tiles.set_motion(selection.motion);
        self.palette.set_tokens(&tokens);
        CATransaction::commit();
    }
}
