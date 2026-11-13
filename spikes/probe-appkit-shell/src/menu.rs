//! `Menu`: меню сцены и локальный обработчик команд. Пункт меню, его сочетание клавиш и команда
//! палитры вызывают один обработчик `Commands::perform`. Меню «Правка» — стандартные действия
//! без цели: их получает first responder, в поле палитры это field editor.

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{NSEventModifierFlags, NSMenu, NSMenuItem};
use objc2_foundation::{NSObject, NSObjectProtocol, NSString};

use crate::palette::Palette;
use crate::scenario::counters::{self, Live, counter};

counter!(COMMAND_TARGET, "ProbeCommandTarget");

pub const EDIT_MENU: &str = "Правка";
pub const COMMANDS_MENU: &str = "Команды";
pub const PALETTE_ITEM: &str = "Палитра команд";
/// Сочетание палитры: Cmd-K.
pub const PALETTE_KEY: &str = "k";
/// Сочетание команды пробы: Cmd-J.
pub const MARK_KEY: &str = "j";

/// Стандартные пункты «Правки»: подпись, действие, эквивалент. Заглавная буква эквивалента
/// добавляет Shift.
fn edit_items() -> [(&'static str, Sel, &'static str); 6] {
    [
        ("Отменить", sel!(undo:), "z"),
        ("Повторить", sel!(redo:), "Z"),
        ("Вырезать", sel!(cut:), "x"),
        ("Скопировать", sel!(copy:), "c"),
        ("Вставить", sel!(paste:), "v"),
        ("Выделить всё", sel!(selectAll:), "a"),
    ]
}

/// Путь, которым вызвана команда.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Via {
    /// Пункт меню с этим заголовком: выбор в меню или его сочетание клавиш.
    Menu(String),
    Palette,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invocation {
    pub id: u64,
    pub via: Via,
}

/// Локальный обработчик команд сцены: записывает каждый вызов.
#[derive(Default)]
pub struct Commands {
    log: RefCell<Vec<Invocation>>,
}

impl Commands {
    pub fn perform(&self, id: u64, via: Via) {
        self.log.borrow_mut().push(Invocation { id, via });
    }

    /// Сколько раз вызвана команда `id`.
    pub fn count(&self, id: u64) -> usize {
        self.log
            .borrow()
            .iter()
            .filter(|invocation| invocation.id == id)
            .count()
    }

    pub fn log(&self) -> Vec<Invocation> {
        self.log.borrow().clone()
    }
}

pub struct TargetIvars {
    _live: Live,
    commands: Rc<Commands>,
    palette: Weak<Palette>,
}

define_class!(
    /// Цель пунктов меню команд: команда по тегу пункта и открытие палитры.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeCommandTarget"]
    #[ivars = TargetIvars]
    pub struct CommandTarget;

    unsafe impl NSObjectProtocol for CommandTarget {}

    impl CommandTarget {
        #[unsafe(method(runCommand:))]
        fn run_command(&self, sender: &NSMenuItem) {
            let Ok(id) = u64::try_from(sender.tag()) else {
                return;
            };
            // SAFETY: `menu` у пункта, который AppKit передал отправителем действия.
            let menu = unsafe { sender.menu() }
                .map_or_else(String::new, |menu| menu.title().to_string());
            self.ivars().commands.perform(id, Via::Menu(menu));
        }

        #[unsafe(method(showPalette:))]
        fn show_palette(&self, _sender: Option<&AnyObject>) {
            if let Some(palette) =
                counters::owner("ProbeCommandTarget.showPalette", self.ivars().palette.upgrade())
            {
                palette.open();
            }
        }
    }
);

impl CommandTarget {
    pub fn new(
        mtm: MainThreadMarker,
        commands: Rc<Commands>,
        palette: Weak<Palette>,
    ) -> Retained<CommandTarget> {
        let this = CommandTarget::alloc(mtm).set_ivars(TargetIvars {
            _live: COMMAND_TARGET.live(),
            commands,
            palette,
        });
        // SAFETY: `init` у NSObject без аргументов.
        unsafe { msg_send![super(this), init] }
    }

    /// Пункт команды `id` с сочетанием Cmd-`key`; вызывает `Commands::perform`.
    pub fn command_item(
        &self,
        mtm: MainThreadMarker,
        id: u64,
        title: &str,
        key: &str,
    ) -> Retained<NSMenuItem> {
        let item = self.item(mtm, title, sel!(runCommand:), key);
        item.setTag(isize::try_from(id).unwrap_or(isize::MAX));
        item
    }

    fn item(
        &self,
        mtm: MainThreadMarker,
        title: &str,
        action: Sel,
        key: &str,
    ) -> Retained<NSMenuItem> {
        let item = menu_item(mtm, title, action, key);
        let target: &AnyObject = self;
        // SAFETY: действия `runCommand:` и `showPalette:` реализованы этим классом; пункт держит
        // цель слабо, сцена держит её, пока её меню стоят в главном меню и в полосах вкладок.
        unsafe { item.setTarget(Some(target)) };
        item
    }
}

/// Пункт без цели: действие уходит first responder.
fn menu_item(mtm: MainThreadMarker, title: &str, action: Sel, key: &str) -> Retained<NSMenuItem> {
    // SAFETY: действие либо реализует цель, заданная после создания, либо ищется по цепочке
    // ответчиков; пункт без обработчика AppKit отключает.
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            Some(action),
            &NSString::from_str(key),
        )
    };
    if !key.is_empty() {
        item.setKeyEquivalentModifierMask(NSEventModifierFlags::Command);
    }
    item
}

fn submenu(
    mtm: MainThreadMarker,
    bar: &NSMenu,
    title: &str,
) -> (Retained<NSMenu>, Retained<NSMenuItem>) {
    let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str(title));
    let item = NSMenuItem::new(mtm);
    item.setTitle(&NSString::from_str(title));
    item.setSubmenu(Some(&menu));
    bar.addItem(&item);
    (menu, item)
}

/// Меню сцены в главном меню: «Правка» и пункты строки меню, которые снимает `uninstall`.
pub struct Installed {
    pub edit: Retained<NSMenu>,
    items: Vec<Retained<NSMenuItem>>,
}

impl Installed {
    /// Убирает пункты сцены из строки меню `bar`.
    pub fn uninstall(&self, bar: &NSMenu) {
        for item in &self.items {
            if bar.indexOfItem(item) >= 0 {
                bar.removeItem(item);
            }
        }
    }
}

/// Добавляет в главное меню «Правку» и «Команды» с палитрой и командой пробы `mark`.
pub fn install(
    mtm: MainThreadMarker,
    bar: &NSMenu,
    target: &CommandTarget,
    mark: (u64, &str),
) -> Installed {
    let (edit, edit_item) = submenu(mtm, bar, EDIT_MENU);
    for (index, (title, action, key)) in edit_items().into_iter().enumerate() {
        if index == 2 {
            edit.addItem(&NSMenuItem::separatorItem(mtm));
        }
        edit.addItem(&menu_item(mtm, title, action, key));
    }
    let (commands, commands_item) = submenu(mtm, bar, COMMANDS_MENU);
    commands.addItem(&target.item(mtm, PALETTE_ITEM, sel!(showPalette:), PALETTE_KEY));
    commands.addItem(&NSMenuItem::separatorItem(mtm));
    commands.addItem(&target.command_item(mtm, mark.0, mark.1, MARK_KEY));
    Installed {
        edit,
        items: vec![edit_item, commands_item],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_count_each_invocation() {
        let commands = Commands::default();
        commands.perform(1, Via::Palette);
        commands.perform(1, Via::Menu(COMMANDS_MENU.to_owned()));
        commands.perform(7, Via::Palette);
        assert_eq!(
            (commands.count(1), commands.count(7), commands.count(2)),
            (2, 1, 0)
        );
        assert_eq!(
            commands.log()[1],
            Invocation {
                id: 1,
                via: Via::Menu(COMMANDS_MENU.to_owned())
            }
        );
    }

    #[test]
    fn edit_items_are_standard_actions() {
        let actions: Vec<String> = edit_items()
            .iter()
            .map(|(_, action, _)| action.name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            actions,
            ["undo:", "redo:", "cut:", "copy:", "paste:", "selectAll:"]
        );
    }
}
