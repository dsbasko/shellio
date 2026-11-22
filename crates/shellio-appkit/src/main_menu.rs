//! Главное меню владельца: «shellio → Выйти» (`app.quit`), «Окно → Закрыть» (`window.close`),
//! «Вид → Оформление → Системное / Светлое / Тёмное» (`ui.appearance.set`). Пункты вызывают
//! один обработчик действий; цели пунктов держит меню (инвариант 2 P0.2). Системное меню
//! сохраняет системное оформление и токенами не красится.

use std::rc::Rc;

use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSControlStateValueOff, NSControlStateValueOn, NSMenu, NSMenuItem,
};
use objc2_foundation::NSString;
use shellio_theme::AppearanceChoice;

use crate::app::App;
use crate::counters::{Live, counter};
use crate::own::Targets;

counter!(MAIN_MENU_OWNER, Owner, "MainMenu");

/// Действие пункта главного меню.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    Quit,
    CloseWindow,
    Appearance(AppearanceChoice),
}

/// Пункты меню «Оформление» по порядку.
pub const APPEARANCE_ITEMS: [(AppearanceChoice, &str); 3] = [
    (AppearanceChoice::System, "Системное"),
    (AppearanceChoice::Light, "Светлое"),
    (AppearanceChoice::Dark, "Тёмное"),
];

/// Главное меню приложения. Освобождение снимает его с приложения и цели с пунктов.
pub struct MainMenu {
    _live: Live,
    mtm: MainThreadMarker,
    appearance: Vec<(AppearanceChoice, Retained<NSMenuItem>)>,
    // Цели снимаются до освобождения меню: поле объявлено раньше.
    _targets: Targets,
    menu: Retained<NSMenu>,
}

impl MainMenu {
    /// Ставит главное меню приложения. `on_action` вызывается на главном потоке изнутри AppKit.
    pub fn install(app: &App, on_action: impl Fn(MenuAction) + 'static) -> MainMenu {
        let mtm = app.mtm();
        let on_action: Rc<dyn Fn(MenuAction)> = Rc::new(on_action);
        let mut targets = Targets::new(mtm);
        let mut bind = |item: &NSMenuItem, action: MenuAction| {
            let on_action = Rc::clone(&on_action);
            targets.bind(item, move || on_action(action));
        };

        let menu = new_menu(mtm, "shellio");
        let app_menu = new_menu(mtm, "shellio");
        let quit = item(mtm, "Выйти из shellio", "q");
        bind(&quit, MenuAction::Quit);
        app_menu.addItem(&quit);
        add_submenu(mtm, &menu, &app_menu);

        let window_menu = new_menu(mtm, "Окно");
        let close = item(mtm, "Закрыть", "w");
        bind(&close, MenuAction::CloseWindow);
        window_menu.addItem(&close);
        add_submenu(mtm, &menu, &window_menu);

        let view_menu = new_menu(mtm, "Вид");
        let appearance_menu = new_menu(mtm, "Оформление");
        let appearance: Vec<_> = APPEARANCE_ITEMS
            .into_iter()
            .map(|(choice, title)| {
                let entry = item(mtm, title, "");
                bind(&entry, MenuAction::Appearance(choice));
                appearance_menu.addItem(&entry);
                (choice, entry)
            })
            .collect();
        add_submenu(mtm, &view_menu, &appearance_menu);
        add_submenu(mtm, &menu, &view_menu);

        let ns_app = NSApplication::sharedApplication(mtm);
        ns_app.setMainMenu(Some(&menu));
        // Меню «Окно» получает список окон: в нём системный заголовок окна владельца.
        ns_app.setWindowsMenu(Some(&window_menu));
        let main_menu = MainMenu {
            _live: MAIN_MENU_OWNER.live(),
            mtm,
            appearance,
            _targets: targets,
            menu,
        };
        main_menu.set_appearance(AppearanceChoice::System);
        main_menu
    }

    /// Отметка у текущего выбора оформления.
    pub fn set_appearance(&self, choice: AppearanceChoice) {
        for (entry_choice, entry) in &self.appearance {
            entry.setState(if *entry_choice == choice {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            });
        }
    }
}

impl Drop for MainMenu {
    fn drop(&mut self) {
        let ns_app = NSApplication::sharedApplication(self.mtm);
        if ns_app
            .mainMenu()
            .is_some_and(|current| std::ptr::eq(&*current, &*self.menu))
        {
            ns_app.setMainMenu(None);
        }
    }
}

fn new_menu(mtm: MainThreadMarker, title: &str) -> Retained<NSMenu> {
    NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str(title))
}

fn item(mtm: MainThreadMarker, title: &str, key: &str) -> Retained<NSMenuItem> {
    // SAFETY: пункт без действия; действие и цель ставит держатель целей вместе.
    unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            None,
            &NSString::from_str(key),
        )
    }
}

/// Пункт `parent` с подменю `submenu` и его заголовком.
fn add_submenu(mtm: MainThreadMarker, parent: &NSMenu, submenu: &NSMenu) {
    let entry = item(mtm, &submenu.title().to_string(), "");
    entry.setSubmenu(Some(submenu));
    parent.addItem(&entry);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appearance_items_cover_every_choice_once() {
        let choices: Vec<_> = APPEARANCE_ITEMS.iter().map(|(choice, _)| *choice).collect();
        assert_eq!(choices.len(), AppearanceChoice::ALL.len());
        for choice in AppearanceChoice::ALL {
            assert_eq!(choices.iter().filter(|item| **item == choice).count(), 1);
        }
        assert_eq!(APPEARANCE_ITEMS[0].1, "Системное");
    }
}
