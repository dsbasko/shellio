//! Главное меню владельца на компоненте [`Menu`]: «shellio → Выйти» (`app.quit`), «Окно →
//! Закрыть» (`window.close`), «Вид → Оформление → Системное / Светлое / Тёмное»
//! (`ui.appearance.set`). Id пунктов действий — их имена в keymap (ADR-0007). Пункты вызывают один
//! обработчик действий; цели пунктов держит меню (инвариант 2 P0.2). Системное меню сохраняет
//! системное оформление и токенами не красится.

use std::cell::Cell;

use objc2::MainThreadMarker;
use objc2_app_kit::NSApplication;
use shellio_theme::AppearanceChoice;

use crate::app::App;
use crate::chrome_bar::{ZoneButton, zone_button};
use crate::components::menu::{Menu, MenuEntry, MenuIntent, MenuItem, MenuKey};
use crate::counters::{Live, counter};

counter!(MAIN_MENU_OWNER, Owner, "MainMenu");

/// Действие пункта главного меню.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    Quit,
    CloseWindow,
    Appearance(AppearanceChoice),
    /// Пункт зон меню «Вид»: тот же обработчик, что у кнопки зон заголовка.
    Zone(ZoneButton),
}

/// Состояние пунктов зон меню «Вид»: `None` — недоступны; иначе цель третьего пункта (плитка левой
/// зоны) и видимость целей.
pub type ZoneMenu = Option<(bool, [bool; 3])>;

const ZONE_ITEMS: [(ZoneButton, &str); 3] = [
    (ZoneButton::Navigator, "layout.visibility.navigator"),
    (ZoneButton::Bottom, "layout.visibility.bottom"),
    (ZoneButton::Side, "layout.visibility.side"),
];

/// Пункты меню «Оформление» по порядку: выбор, id пункта, заголовок.
pub const APPEARANCE_ITEMS: [(AppearanceChoice, &str, &str); 3] = [
    (
        AppearanceChoice::System,
        "ui.appearance.system",
        "Системное",
    ),
    (AppearanceChoice::Light, "ui.appearance.light", "Светлое"),
    (AppearanceChoice::Dark, "ui.appearance.dark", "Тёмное"),
];

const QUIT: &str = "app.quit";
const CLOSE: &str = "window.close";
const APP_MENU: &str = "menu.app";
const WINDOW_MENU: &str = "menu.window";
const VIEW_MENU: &str = "menu.view";
const APPEARANCE_MENU: &str = "menu.view.appearance";

/// Пункты главного меню с отметкой у выбора `choice`.
fn zone_entries(zones: ZoneMenu) -> Vec<MenuEntry> {
    let (side_plate, shown) = zones.unwrap_or((false, [false; 3]));
    ZONE_ITEMS
        .into_iter()
        .zip(shown)
        .map(|((button, id), shown)| {
            let key = match button {
                ZoneButton::Navigator => Some(MenuKey::command("0")),
                ZoneButton::Side => Some(MenuKey {
                    option: true,
                    ..MenuKey::command("0")
                }),
                ZoneButton::Bottom => None,
            };
            MenuEntry::Item(MenuItem {
                key,
                enabled: zones.is_some(),
                checked: shown,
                ..MenuItem::new(id, zone_button(button, side_plate).1)
            })
        })
        .collect()
}

fn entries(choice: AppearanceChoice, zones: ZoneMenu) -> Vec<MenuEntry> {
    let submenu = |id: &str, title: &str, children: Vec<MenuEntry>| {
        MenuEntry::Item(MenuItem {
            submenu: Some(children),
            ..MenuItem::new(id, title)
        })
    };
    let keyed = |id: &str, title: &str, key: &str| {
        MenuEntry::Item(MenuItem {
            key: Some(MenuKey::command(key)),
            ..MenuItem::new(id, title)
        })
    };
    let appearance = APPEARANCE_ITEMS
        .into_iter()
        .map(|(item_choice, id, title)| {
            MenuEntry::Item(MenuItem {
                checked: item_choice == choice,
                ..MenuItem::new(id, title)
            })
        })
        .collect();
    vec![
        submenu(
            APP_MENU,
            "shellio",
            vec![keyed(QUIT, "Выйти из shellio", "q")],
        ),
        submenu(WINDOW_MENU, "Окно", vec![keyed(CLOSE, "Закрыть", "w")]),
        submenu(VIEW_MENU, "Вид", {
            let mut items = zone_entries(zones);
            items.push(MenuEntry::Separator {
                id: "menu.view.separator".to_owned(),
            });
            items.push(submenu(APPEARANCE_MENU, "Оформление", appearance));
            items
        }),
    ]
}

/// Действие пункта по его id; `None` — пункт без действия (подменю).
fn action_of(id: &str) -> Option<MenuAction> {
    match id {
        QUIT => Some(MenuAction::Quit),
        CLOSE => Some(MenuAction::CloseWindow),
        _ if ZONE_ITEMS.iter().any(|(_, item)| *item == id) => ZONE_ITEMS
            .into_iter()
            .find(|(_, item)| *item == id)
            .map(|(button, _)| MenuAction::Zone(button)),
        _ => APPEARANCE_ITEMS
            .into_iter()
            .find(|(_, item_id, _)| *item_id == id)
            .map(|(choice, _, _)| MenuAction::Appearance(choice)),
    }
}

/// Главное меню приложения. Освобождение снимает его с приложения и цели с пунктов.
pub struct MainMenu {
    _live: Live,
    mtm: MainThreadMarker,
    choice: Cell<AppearanceChoice>,
    zones: Cell<ZoneMenu>,
    menu: Menu,
}

impl MainMenu {
    /// Ставит главное меню приложения. `on_action` вызывается на главном потоке изнутри AppKit.
    pub fn install(app: &App, on_action: impl Fn(MenuAction) + 'static) -> MainMenu {
        let mtm = app.mtm();
        let menu = Menu::empty(app, "shellio", move |intent| {
            let MenuIntent::Chosen { id } = intent;
            if let Some(action) = action_of(&id) {
                on_action(action);
            }
        });
        let main_menu = MainMenu {
            _live: MAIN_MENU_OWNER.live(),
            mtm,
            choice: Cell::new(AppearanceChoice::System),
            zones: Cell::new(None),
            menu,
        };
        main_menu.rebuild();
        let ns_app = NSApplication::sharedApplication(mtm);
        ns_app.setMainMenu(Some(&main_menu.menu.ns_menu()));
        // Меню «Окно» получает список окон: в нём системный заголовок окна владельца.
        if let Some(window_menu) = main_menu.menu.submenu(WINDOW_MENU) {
            ns_app.setWindowsMenu(Some(&window_menu));
        }
        main_menu
    }

    /// Отметка у текущего выбора оформления.
    pub fn set_appearance(&self, choice: AppearanceChoice) {
        if self.choice.replace(choice) != choice {
            self.rebuild();
        }
    }

    /// Пункты зон меню «Вид».
    pub fn set_zones(&self, zones: ZoneMenu) {
        if self.zones.replace(zones) != zones {
            self.rebuild();
        }
    }

    fn rebuild(&self) {
        // Id пунктов постоянны и уникальны (тест `ids_are_unique_and_map_to_actions`), отказа здесь
        // не бывает; если он появится, меню остаётся прежним, а причина уходит в stderr.
        let entries = entries(self.choice.get(), self.zones.get());
        if let Err(error) = self.menu.set_entries(entries) {
            eprintln!("shellio: главное меню не обновлено: {error}");
        }
    }
}

impl Drop for MainMenu {
    fn drop(&mut self) {
        let ns_app = NSApplication::sharedApplication(self.mtm);
        let own = self.menu.ns_menu();
        if ns_app
            .mainMenu()
            .is_some_and(|current| std::ptr::eq(&*current, &*own))
        {
            ns_app.setMainMenu(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::menu::{ItemChange, MenuOp, check_ids, diff};

    /// Все id пунктов меню с действием и без.
    fn ids(entries: &[MenuEntry], out: &mut Vec<String>) {
        for entry in entries {
            out.push(entry.id().to_owned());
            if let MenuEntry::Item(MenuItem {
                submenu: Some(children),
                ..
            }) = entry
            {
                ids(children, out);
            }
        }
    }

    #[test]
    fn appearance_items_cover_every_choice_once() {
        let choices: Vec<_> = APPEARANCE_ITEMS
            .iter()
            .map(|(choice, _, _)| *choice)
            .collect();
        assert_eq!(choices.len(), AppearanceChoice::ALL.len());
        for choice in AppearanceChoice::ALL {
            assert_eq!(choices.iter().filter(|item| **item == choice).count(), 1);
        }
        assert_eq!(APPEARANCE_ITEMS[0].2, "Системное");
    }

    #[test]
    fn ids_are_unique_and_map_to_actions() {
        for choice in AppearanceChoice::ALL {
            assert_eq!(check_ids(&entries(choice, None)), Ok(()));
        }
        let mut all = Vec::new();
        ids(&entries(AppearanceChoice::System, None), &mut all);
        let actions: Vec<MenuAction> = all.iter().filter_map(|id| action_of(id)).collect();
        let mut expected = vec![MenuAction::Quit, MenuAction::CloseWindow];
        expected.extend(AppearanceChoice::ALL.map(MenuAction::Appearance));
        expected.extend(ZoneButton::ALL.map(MenuAction::Zone));
        assert_eq!(
            actions.len(),
            expected.len(),
            "каждое действие — один пункт"
        );
        for action in expected {
            assert_eq!(actions.iter().filter(|known| **known == action).count(), 1);
        }
        for container in [APP_MENU, WINDOW_MENU, VIEW_MENU, APPEARANCE_MENU] {
            assert!(all.iter().any(|id| id == container));
            assert_eq!(action_of(container), None, "подменю без действия");
        }
        assert_eq!(action_of("window.close"), Some(MenuAction::CloseWindow));
        assert_eq!(
            action_of("ui.appearance.dark"),
            Some(MenuAction::Appearance(AppearanceChoice::Dark))
        );
    }

    #[test]
    fn quit_and_close_keep_their_keys() {
        let all = entries(AppearanceChoice::System, None);
        let key = |menu: usize| match &all[menu] {
            MenuEntry::Item(MenuItem {
                submenu: Some(children),
                ..
            }) => match &children[0] {
                MenuEntry::Item(item) => item.key.as_ref().map(MenuKey::chord),
                MenuEntry::Separator { .. } => None,
            },
            _ => None,
        };
        assert_eq!(key(0).as_deref(), Some("cmd+q"));
        assert_eq!(key(1).as_deref(), Some("cmd+w"));
    }

    #[test]
    fn changing_the_appearance_moves_only_the_mark() {
        let ops = diff(
            &entries(AppearanceChoice::System, None),
            &entries(AppearanceChoice::Dark, None),
        );
        assert_eq!(
            ops,
            [
                MenuOp::Update {
                    path: vec![2, 4],
                    index: 0,
                    change: ItemChange::Checked(false),
                },
                MenuOp::Update {
                    path: vec![2, 4],
                    index: 2,
                    change: ItemChange::Checked(true),
                },
            ]
        );
        assert!(
            diff(
                &entries(AppearanceChoice::Light, None),
                &entries(AppearanceChoice::Light, None)
            )
            .is_empty()
        );
    }

    #[test]
    fn zone_items_share_the_chrome_labels_and_keys() {
        let view = |zones: ZoneMenu| match &entries(AppearanceChoice::System, zones)[2] {
            MenuEntry::Item(MenuItem {
                submenu: Some(children),
                ..
            }) => children[..3]
                .iter()
                .map(|entry| match entry {
                    MenuEntry::Item(item) => (
                        item.title.clone(),
                        item.key.as_ref().map(MenuKey::chord),
                        item.enabled,
                        item.checked,
                    ),
                    MenuEntry::Separator { .. } => unreachable!("пункты зон идут подряд"),
                })
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        };
        let off = view(None);
        assert_eq!(
            off.iter().map(|item| item.0.as_str()).collect::<Vec<_>>(),
            ["Навигация", "Нижняя зона", "Правая зона"]
        );
        assert_eq!(
            off.iter().map(|item| item.1.as_deref()).collect::<Vec<_>>(),
            [Some("cmd+0"), None, Some("opt+cmd+0")]
        );
        assert!(
            off.iter().all(|item| !item.2 && !item.3),
            "без рабочей области недоступны"
        );
        let plate = view(Some((true, [true, false, true])));
        assert_eq!(plate[2].0, "Левая зона");
        assert_eq!(
            plate
                .iter()
                .map(|item| (item.2, item.3))
                .collect::<Vec<_>>(),
            [(true, true), (true, false), (true, true)]
        );
        assert_eq!(
            action_of("layout.visibility.side"),
            Some(MenuAction::Zone(ZoneButton::Side))
        );
    }
}
