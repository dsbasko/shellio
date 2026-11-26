//! Главное меню владельца на компоненте [`Menu`]: «shellio → Выйти» (`app.quit`), «Окно →
//! Закрыть» (`window.close`), «Вид → Оформление → Системное / Светлое / Тёмное»
//! (`ui.appearance.set`), «Вид → Открыть панель → <название>» (`panel.open.<id>` для каждой строки
//! списка владельца). Id пунктов действий — их имена в keymap (ADR-0007). Пункты вызывают один
//! обработчик действий; цели пунктов держит меню (инвариант 2 P0.2). Системное меню сохраняет
//! системное оформление и токенами не красится.

use std::cell::Cell;
use std::rc::Rc;

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
    /// Пункт «Вид → Открыть панель»: номер строки списка [`MainMenu::install`].
    OpenPanel(usize),
}

/// Строка меню «Открыть панель»: id для пункта `panel.open.<id>` и название.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelRow {
    pub id: String,
    pub title: String,
}

/// Состояние пунктов зон меню «Вид»: `None` — недоступны; иначе цель третьего пункта (плитка левой
/// зоны) и видимость целей.
pub type ZoneMenu = Option<(bool, [bool; 3])>;

const ZONE_ITEMS: [(ZoneButton, &str); 3] = [
    (ZoneButton::Navigator, "layout.toggle.navigator"),
    (ZoneButton::Bottom, "layout.toggle.bottom"),
    (ZoneButton::Side, "layout.toggle.side"),
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
const OPEN_PANEL_MENU: &str = "menu.view.open_panel";
/// Префикс id пункта «Открыть панель»: действие `panel.open` с аргументом (ADR-0007).
const OPEN_PANEL: &str = "panel.open.";

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

/// Пункты «Открыть панель» по строкам `rows`; `enabled` — рабочая область открыта.
fn panel_entries(rows: &[PanelRow], enabled: bool) -> Vec<MenuEntry> {
    rows.iter()
        .map(|row| {
            MenuEntry::Item(MenuItem {
                enabled,
                ..MenuItem::new(&format!("{OPEN_PANEL}{}", row.id), &row.title)
            })
        })
        .collect()
}

fn entries(
    choice: AppearanceChoice,
    zones: ZoneMenu,
    rows: &[PanelRow],
    open_enabled: bool,
) -> Vec<MenuEntry> {
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
            items.push(MenuEntry::Separator {
                id: "menu.view.separator.panels".to_owned(),
            });
            items.push(submenu(
                OPEN_PANEL_MENU,
                "Открыть панель",
                panel_entries(rows, open_enabled),
            ));
            items
        }),
    ]
}

/// Действие пункта по его id; `None` — пункт без действия (подменю).
fn action_of(id: &str, rows: &[PanelRow]) -> Option<MenuAction> {
    if let Some(row) = id.strip_prefix(OPEN_PANEL) {
        return rows
            .iter()
            .position(|known| known.id == row)
            .map(MenuAction::OpenPanel);
    }
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
    rows: Rc<[PanelRow]>,
    open_enabled: Cell<bool>,
    menu: Menu,
}

impl MainMenu {
    /// Ставит главное меню приложения. `rows` — строки «Открыть панель» по порядку, до первого
    /// [`Self::set_open_enabled`] недоступные. `on_action` вызывается на главном потоке изнутри
    /// AppKit.
    pub fn install(
        app: &App,
        rows: Vec<PanelRow>,
        on_action: impl Fn(MenuAction) + 'static,
    ) -> MainMenu {
        let mtm = app.mtm();
        let rows: Rc<[PanelRow]> = rows.into();
        let known = Rc::clone(&rows);
        let menu = Menu::empty(app, "shellio", move |intent| {
            let MenuIntent::Chosen { id } = intent;
            if let Some(action) = action_of(&id, &known) {
                on_action(action);
            }
        });
        let main_menu = MainMenu {
            _live: MAIN_MENU_OWNER.live(),
            mtm,
            choice: Cell::new(AppearanceChoice::System),
            zones: Cell::new(None),
            rows,
            open_enabled: Cell::new(false),
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

    /// Доступность пунктов «Открыть панель»: рабочая область открыта.
    pub fn set_open_enabled(&self, enabled: bool) {
        if self.open_enabled.replace(enabled) != enabled {
            self.rebuild();
        }
    }

    fn rebuild(&self) {
        // Id пунктов постоянны и уникальны (тест `ids_are_unique_and_map_to_actions`), отказа здесь
        // не бывает; если он появится, меню остаётся прежним, а причина уходит в stderr.
        let entries = entries(
            self.choice.get(),
            self.zones.get(),
            &self.rows,
            self.open_enabled.get(),
        );
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
            assert_eq!(check_ids(&entries(choice, None, &[], false)), Ok(()));
        }
        let mut all = Vec::new();
        ids(
            &entries(AppearanceChoice::System, None, &[], false),
            &mut all,
        );
        let actions: Vec<MenuAction> = all.iter().filter_map(|id| action_of(id, &[])).collect();
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
        for container in [
            APP_MENU,
            WINDOW_MENU,
            VIEW_MENU,
            APPEARANCE_MENU,
            OPEN_PANEL_MENU,
        ] {
            assert!(all.iter().any(|id| id == container));
            assert_eq!(action_of(container, &[]), None, "подменю без действия");
        }
        assert_eq!(
            action_of("window.close", &[]),
            Some(MenuAction::CloseWindow)
        );
        assert_eq!(
            action_of("ui.appearance.dark", &[]),
            Some(MenuAction::Appearance(AppearanceChoice::Dark))
        );
    }

    #[test]
    fn quit_and_close_keep_their_keys() {
        let all = entries(AppearanceChoice::System, None, &[], false);
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
            &entries(AppearanceChoice::System, None, &[], false),
            &entries(AppearanceChoice::Dark, None, &[], false),
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
                &entries(AppearanceChoice::Light, None, &[], false),
                &entries(AppearanceChoice::Light, None, &[], false)
            )
            .is_empty()
        );
    }

    #[test]
    fn zone_items_share_the_chrome_labels_and_keys() {
        let view = |zones: ZoneMenu| match &entries(AppearanceChoice::System, zones, &[], false)[2]
        {
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
            action_of("layout.toggle.side", &[]),
            Some(MenuAction::Zone(ZoneButton::Side))
        );
    }

    fn rows() -> Vec<PanelRow> {
        [("terminal", "Терминал"), ("navigator", "Навигатор")]
            .into_iter()
            .map(|(id, title)| PanelRow {
                id: id.to_owned(),
                title: title.to_owned(),
            })
            .collect()
    }

    /// Пункты подменю «Открыть панель»: id, название, доступность.
    fn open_panel(entries: &[MenuEntry]) -> Vec<(String, String, bool)> {
        let MenuEntry::Item(MenuItem {
            submenu: Some(view),
            ..
        }) = &entries[2]
        else {
            panic!("нет меню «Вид»");
        };
        let Some(MenuEntry::Item(MenuItem {
            id,
            title,
            submenu: Some(children),
            ..
        })) = view.last()
        else {
            panic!("нет подменю «Открыть панель»");
        };
        assert_eq!(
            (id.as_str(), title.as_str()),
            (OPEN_PANEL_MENU, "Открыть панель")
        );
        children
            .iter()
            .map(|entry| match entry {
                MenuEntry::Item(item) => (item.id.clone(), item.title.clone(), item.enabled),
                MenuEntry::Separator { .. } => unreachable!("в подменю только пункты"),
            })
            .collect()
    }

    #[test]
    fn open_panel_lists_rows_in_order_under_keymap_ids() {
        let rows = rows();
        let off = entries(AppearanceChoice::System, None, &rows, false);
        assert_eq!(check_ids(&off), Ok(()));
        assert_eq!(
            open_panel(&off),
            [
                (
                    "panel.open.terminal".to_owned(),
                    "Терминал".to_owned(),
                    false
                ),
                (
                    "panel.open.navigator".to_owned(),
                    "Навигатор".to_owned(),
                    false
                ),
            ]
        );
        let on = entries(AppearanceChoice::System, None, &rows, true);
        assert!(
            open_panel(&on).iter().all(|item| item.2),
            "с рабочей областью доступны"
        );
        assert_eq!(
            action_of("panel.open.terminal", &rows),
            Some(MenuAction::OpenPanel(0))
        );
        assert_eq!(
            action_of("panel.open.navigator", &rows),
            Some(MenuAction::OpenPanel(1))
        );
        assert_eq!(action_of("panel.open.gone", &rows), None);
        assert_eq!(action_of("panel.open.", &rows), None);
        assert_eq!(
            action_of(OPEN_PANEL_MENU, &rows),
            None,
            "подменю без действия"
        );
    }

    #[test]
    fn opening_a_workbench_only_enables_the_open_panel_items() {
        let rows = rows();
        let ops = diff(
            &entries(AppearanceChoice::System, None, &rows, false),
            &entries(AppearanceChoice::System, None, &rows, true),
        );
        assert_eq!(
            ops,
            [0, 1].map(|index| MenuOp::Update {
                path: vec![2, 6],
                index,
                change: ItemChange::Enabled(true),
            })
        );
    }
}
