//! `Menu` — системное меню на `NSMenu` (ADR-0002, таблица «Компоненты» плана P1.3): пункты с
//! устойчивыми id, заголовком, клавишей, доступностью, отметкой и подменю; намерение
//! `chosen { id }`.
//!
//! Чистая часть — пункты и их разница в операции по id ([`diff`]); её тестирует `make check`.
//! Вид держит меню и его пункты и применяет операции по идентичности своих пунктов, а не по номеру
//! в `NSMenu`: в меню «Окно» AppKit добавляет собственные пункты. Цели пунктов и делегат меню — один
//! объект, который держит владелец (инвариант 2 P0.2), доступность пунктов отвечает через
//! `validateMenuItem:`, потому что при автоматической доступности AppKit не слушает `setEnabled`.
//! Системное меню сохраняет системное оформление и токенами не красится.
//!
//! Всплывающее меню ([`MenuButton`]) открывается таймером цикла главного потока, а не на месте:
//! отслеживание меню крутит вложенный цикл, и внутри блока главной очереди он остановил бы её
//! обслуживание до закрытия меню.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::fmt;
use std::rc::Rc;

use objc2::rc::{self, Retained};
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, Message, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAccessibility, NSButton, NSCellImagePosition, NSControl, NSControlStateValueOff,
    NSControlStateValueOn, NSEventModifierFlags, NSMenu, NSMenuDelegate, NSMenuItem,
    NSMenuItemValidation, NSResponder, NSView,
};
use objc2_foundation::{
    NSArray, NSObjectNSDelayedPerforming, NSPoint, NSRect, NSRunLoopCommonModes, NSString,
};
use shellio_theme::AppTheme;

use crate::app::App;
use crate::counters::{Live, counter};
use crate::inspect::{self, InspectNode, NodeState};
use crate::layer;
use crate::own::{Callback, ComponentId, View};
use crate::piece::Piece;

counter!(MENU_OWNER, Owner, "Menu");
counter!(MENU_TARGET, Class, "ShellioMenuTarget");
counter!(MENU_BUTTON_OWNER, Owner, "MenuButton");
counter!(MENU_BUTTON, Class, "ShellioMenuButton");

/// Зазор между нижним краем якоря и меню под ним (`top: r.bottom + 6px` прототипа): системное
/// меню, открытое вплотную, закрывает нижний край значка кнопки.
const MENU_GAP: f64 = 6.0;

/// Точка открытия меню по вертикали в точках якоря высотой `height`: под его нижним краем. Меню
/// отступает от якоря на [`MENU_GAP`]; выбор, который ставит к точке первый пункт (`first`),
/// открывается вплотную.
fn open_y(flipped: bool, height: f64, first: bool) -> f64 {
    let gap = if first { 0.0 } else { MENU_GAP };
    if flipped { height + gap } else { -gap }
}

/// Клавиша пункта: знак `keyEquivalent` и модификаторы.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MenuKey {
    pub key: String,
    pub command: bool,
    pub shift: bool,
    pub option: bool,
    pub control: bool,
}

impl MenuKey {
    /// Знак с `Cmd`.
    pub fn command(key: &str) -> MenuKey {
        MenuKey {
            key: key.to_owned(),
            command: true,
            shift: false,
            option: false,
            control: false,
        }
    }

    /// Запись сочетания для чтений: `cmd+shift+k`.
    pub fn chord(&self) -> String {
        let mut parts: Vec<&str> = [
            (self.control, "ctrl"),
            (self.option, "opt"),
            (self.shift, "shift"),
            (self.command, "cmd"),
        ]
        .into_iter()
        .filter_map(|(on, name)| on.then_some(name))
        .collect();
        parts.push(&self.key);
        parts.join("+")
    }

    fn modifiers(&self) -> NSEventModifierFlags {
        let mut flags = NSEventModifierFlags::empty();
        for (on, flag) in [
            (self.command, NSEventModifierFlags::Command),
            (self.shift, NSEventModifierFlags::Shift),
            (self.option, NSEventModifierFlags::Option),
            (self.control, NSEventModifierFlags::Control),
        ] {
            if on {
                flags |= flag;
            }
        }
        flags
    }
}

/// Пункт меню.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuItem {
    pub id: String,
    pub title: String,
    pub key: Option<MenuKey>,
    pub enabled: bool,
    pub checked: bool,
    /// Пункты подменю; `None` — пункт без подменю.
    pub submenu: Option<Vec<MenuEntry>>,
}

impl MenuItem {
    /// Доступный пункт без клавиши, отметки и подменю.
    pub fn new(id: &str, title: &str) -> MenuItem {
        MenuItem {
            id: id.to_owned(),
            title: title.to_owned(),
            key: None,
            enabled: true,
            checked: false,
            submenu: None,
        }
    }
}

/// Пункт или разделитель.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuEntry {
    Item(MenuItem),
    Separator { id: String },
}

impl MenuEntry {
    pub fn id(&self) -> &str {
        match self {
            MenuEntry::Item(item) => &item.id,
            MenuEntry::Separator { id } => id,
        }
    }

    fn same_kind(&self, other: &MenuEntry) -> bool {
        matches!(
            (self, other),
            (MenuEntry::Item(_), MenuEntry::Item(_))
                | (MenuEntry::Separator { .. }, MenuEntry::Separator { .. })
        )
    }
}

/// Изменение свойства пункта.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemChange {
    Title(String),
    Key(Option<MenuKey>),
    Enabled(bool),
    Checked(bool),
    /// Подменю появилось или исчезло целиком.
    Submenu(Option<Vec<MenuEntry>>),
}

/// Операция над меню. `path` — номера пунктов с подменю от корня до уровня операции, номера —
/// в пунктах модели, а не в `NSMenu`. Операции применяются по порядку.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuOp {
    Remove {
        path: Vec<usize>,
        index: usize,
    },
    Insert {
        path: Vec<usize>,
        index: usize,
        entry: MenuEntry,
    },
    Move {
        path: Vec<usize>,
        from: usize,
        to: usize,
    },
    Update {
        path: Vec<usize>,
        index: usize,
        change: ItemChange,
    },
}

/// Намерение меню.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuIntent {
    /// Выбран пункт с этим id.
    Chosen { id: String },
}

/// Ошибка пунктов меню.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuError {
    /// Id встречается в меню больше одного раза: выбор и разница по нему неоднозначны.
    DuplicateId(String),
}

impl fmt::Display for MenuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MenuError::DuplicateId(id) => write!(f, "id пункта меню повторяется: {id}"),
        }
    }
}

impl std::error::Error for MenuError {}

/// Id всех пунктов и разделителей меню, включая подменю, уникальны.
pub fn check_ids(entries: &[MenuEntry]) -> Result<(), MenuError> {
    fn walk<'a>(entries: &'a [MenuEntry], seen: &mut HashSet<&'a str>) -> Result<(), MenuError> {
        for entry in entries {
            if !seen.insert(entry.id()) {
                return Err(MenuError::DuplicateId(entry.id().to_owned()));
            }
            if let MenuEntry::Item(MenuItem {
                submenu: Some(children),
                ..
            }) = entry
            {
                walk(children, seen)?;
            }
        }
        Ok(())
    }
    walk(entries, &mut HashSet::new())
}

/// Операции, которые переводят меню из `before` в `after`. Пункт узнаётся по id: удалённые
/// уходят, новые вставляются, переставленные переносятся тем же пунктом, у оставшихся меняются
/// только изменённые свойства. Пункт, сменивший вид (пункт и разделитель), заменяется. Id в каждом
/// наборе уникальны ([`check_ids`]).
pub fn diff(before: &[MenuEntry], after: &[MenuEntry]) -> Vec<MenuOp> {
    let mut ops = Vec::new();
    diff_level(&mut ops, &[], before, after);
    ops
}

fn diff_level(ops: &mut Vec<MenuOp>, path: &[usize], before: &[MenuEntry], after: &[MenuEntry]) {
    let kept: HashSet<&str> = after.iter().map(MenuEntry::id).collect();
    let mut current: Vec<&MenuEntry> = before.iter().collect();
    for index in (0..current.len()).rev() {
        let entry = current[index];
        let replaced = after
            .iter()
            .find(|next| next.id() == entry.id())
            .is_some_and(|next| !next.same_kind(entry));
        if !kept.contains(entry.id()) || replaced {
            ops.push(MenuOp::Remove {
                path: path.to_vec(),
                index,
            });
            current.remove(index);
        }
    }
    for (index, next) in after.iter().enumerate() {
        let found = current[index..]
            .iter()
            .position(|entry| entry.id() == next.id())
            .map(|offset| index + offset);
        match found {
            Some(from) => {
                if from != index {
                    ops.push(MenuOp::Move {
                        path: path.to_vec(),
                        from,
                        to: index,
                    });
                    let entry = current.remove(from);
                    current.insert(index, entry);
                }
                diff_entry(ops, path, index, current[index], next);
            }
            None => {
                ops.push(MenuOp::Insert {
                    path: path.to_vec(),
                    index,
                    entry: next.clone(),
                });
                current.insert(index, next);
            }
        }
    }
}

fn diff_entry(
    ops: &mut Vec<MenuOp>,
    path: &[usize],
    index: usize,
    before: &MenuEntry,
    after: &MenuEntry,
) {
    let (MenuEntry::Item(before), MenuEntry::Item(after)) = (before, after) else {
        return;
    };
    let mut update = |change| {
        ops.push(MenuOp::Update {
            path: path.to_vec(),
            index,
            change,
        });
    };
    if before.title != after.title {
        update(ItemChange::Title(after.title.clone()));
    }
    if before.key != after.key {
        update(ItemChange::Key(after.key.clone()));
    }
    if before.enabled != after.enabled {
        update(ItemChange::Enabled(after.enabled));
    }
    if before.checked != after.checked {
        update(ItemChange::Checked(after.checked));
    }
    match (&before.submenu, &after.submenu) {
        (Some(old), Some(new)) => {
            let mut inner = path.to_vec();
            inner.push(index);
            diff_level(ops, &inner, old, new);
        }
        (None, None) => {}
        (_, new) => update(ItemChange::Submenu(new.clone())),
    }
}

/// Пункт меню по id в любом подменю.
fn find<'a>(entries: &'a [MenuEntry], id: &str) -> Option<&'a MenuEntry> {
    entries.iter().find_map(|entry| {
        if entry.id() == id {
            return Some(entry);
        }
        match entry {
            MenuEntry::Item(MenuItem {
                submenu: Some(children),
                ..
            }) => find(children, id),
            _ => None,
        }
    })
}

/// Состояние меню для `ui.inspect`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuState {
    /// Корневое меню открыто: идёт отслеживание.
    pub open: bool,
    /// Подсвеченный пункт корневого меню во время отслеживания.
    pub highlighted: Option<String>,
    pub entries: Vec<MenuEntry>,
}

/// Rust-часть меню. Делегат и цель пунктов достают её через слабую ссылку.
struct MenuModel {
    entries: RefCell<Vec<MenuEntry>>,
    open: Cell<bool>,
    highlighted: RefCell<Option<String>>,
    /// Следующее открытие ставит под якорь первый пункт и подсвечивает его: `Return` выбирает его
    /// без стрелок.
    first: Cell<bool>,
    on_intent: Box<dyn Fn(MenuIntent)>,
}

impl MenuModel {
    fn enabled(&self, id: &str) -> bool {
        match find(&self.entries.borrow(), id) {
            Some(MenuEntry::Item(item)) => item.enabled,
            Some(MenuEntry::Separator { .. }) | None => false,
        }
    }
}

/// Пункт вида и его подменю.
struct Node {
    item: Retained<NSMenuItem>,
    submenu: Option<Level>,
}

/// Свои пункты одного уровня в порядке модели и `NSMenu`, в котором они лежат.
struct Level {
    menu: Retained<NSMenu>,
    nodes: Vec<Node>,
}

/// Меню на главном потоке. Освобождение отменяет отложенное открытие, закрывает открытое меню и
/// снимает делегата и цели с меню и пунктов.
pub struct Menu {
    _live: Live,
    model: Rc<MenuModel>,
    root: RefCell<Level>,
    target: Retained<MenuTarget>,
}

impl Menu {
    /// Меню с заголовком `title`. `on_intent` вызывается на главном потоке изнутри AppKit, в том
    /// числе внутри отслеживания меню, и не должен синхронно освобождать меню.
    pub fn new(
        app: &App,
        title: &str,
        entries: Vec<MenuEntry>,
        on_intent: impl Fn(MenuIntent) + 'static,
    ) -> Result<Menu, MenuError> {
        check_ids(&entries)?;
        let menu = Menu::empty(app, title, on_intent);
        menu.set_entries(entries)?;
        Ok(menu)
    }

    /// Меню без пунктов: их ставит [`Menu::set_entries`].
    pub(crate) fn empty(app: &App, title: &str, on_intent: impl Fn(MenuIntent) + 'static) -> Menu {
        let mtm = app.mtm();
        let model = Rc::new(MenuModel {
            entries: RefCell::new(Vec::new()),
            open: Cell::new(false),
            highlighted: RefCell::new(None),
            first: Cell::new(false),
            on_intent: Box::new(on_intent),
        });
        let root = new_menu(mtm, title);
        let target = MenuTarget::new(mtm, Callback::new(&model, "Menu.target"), &root);
        root.setDelegate(Some(ProtocolObject::from_ref(&*target)));
        Menu {
            _live: MENU_OWNER.live(),
            model,
            root: RefCell::new(Level {
                menu: root,
                nodes: Vec::new(),
            }),
            target,
        }
    }

    /// Новые пункты: вид получает только операции разницы.
    pub fn set_entries(&self, entries: Vec<MenuEntry>) -> Result<(), MenuError> {
        check_ids(&entries)?;
        let ops = diff(&self.model.entries.borrow(), &entries);
        // Модель меняется до AppKit: `validateMenuItem:` во время операций читает уже её.
        *self.model.entries.borrow_mut() = entries;
        let mtm = self.target.mtm();
        let mut root = self.root.borrow_mut();
        for op in &ops {
            apply(mtm, &self.target, &mut root, op);
        }
        Ok(())
    }

    pub fn entries(&self) -> Vec<MenuEntry> {
        self.model.entries.borrow().clone()
    }

    pub fn state(&self) -> MenuState {
        MenuState {
            open: self.model.open.get(),
            highlighted: self.model.highlighted.borrow().clone(),
            entries: self.entries(),
        }
    }

    /// Идёт ли отслеживание корневого меню.
    pub fn is_open(&self) -> bool {
        self.model.open.get()
    }

    /// Закрывает открытое меню; выбора при этом нет.
    pub fn close(&self) {
        if self.model.open.get() {
            let menu = self.root.borrow().menu.clone();
            menu.cancelTracking();
        }
    }

    /// `NSMenu` корня.
    pub(crate) fn ns_menu(&self) -> Retained<NSMenu> {
        self.root.borrow().menu.clone()
    }

    /// `NSMenu` подменю пункта `id`.
    pub(crate) fn submenu(&self, id: &str) -> Option<Retained<NSMenu>> {
        fn walk(level: &Level, entries: &[MenuEntry], id: &str) -> Option<Retained<NSMenu>> {
            level.nodes.iter().zip(entries).find_map(|(node, entry)| {
                let (Some(sub), MenuEntry::Item(item)) = (&node.submenu, entry) else {
                    return None;
                };
                if item.id == id {
                    return Some(sub.menu.clone());
                }
                walk(sub, item.submenu.as_deref().unwrap_or_default(), id)
            })
        }
        walk(&self.root.borrow(), &self.model.entries.borrow(), id)
    }

    /// Меню для `ui.inspect` с кадром вида `anchor`: пункты, открыто ли, подсвеченный пункт. Узел
    /// видим, только пока меню открыто: закрытое меню на экране ничего не занимает.
    pub fn inspect_at<'a>(
        &self,
        id: &str,
        parent: Option<&str>,
        anchor: impl Into<Piece<'a>>,
    ) -> InspectNode {
        let mut node = inspect::node(
            id,
            "Menu",
            parent,
            anchor.into().ns_view(),
            None,
            Some(NodeState::Menu(self.state())),
        );
        node.facts.visible &= self.is_open();
        node
    }

    /// Открывает меню выбора под видом `anchor`: первый пункт стоит под якорем и подсвечен,
    /// поэтому `Return` выбирает его, а `Esc` закрывает меню без выбора.
    pub(crate) fn pop_up_choice_below(&self, anchor: &NSView) {
        if self.model.open.get() {
            return;
        }
        self.model.first.set(true);
        self.schedule_pop_up(anchor);
    }

    /// Открывает меню под видом `anchor` на следующем обороте цикла главного потока. Открытое меню
    /// повторно не открывается.
    pub(crate) fn pop_up_below(&self, anchor: &NSView) {
        if self.model.open.get() {
            return;
        }
        self.model.first.set(false);
        self.schedule_pop_up(anchor);
    }

    fn schedule_pop_up(&self, anchor: &NSView) {
        // SAFETY: `NSRunLoopCommonModes` — неизменяемая строка Foundation, живущая весь процесс.
        let common = unsafe { NSRunLoopCommonModes };
        let modes = NSArray::from_slice(&[common]);
        let object: &NSObject = &self.target;
        let anchor: &AnyObject = anchor.as_ref();
        // SAFETY: `shellioPopUp:` реализован целью и принимает вид; запрос держит цель и вид до
        // вызова, `Drop` меню отменяет его.
        unsafe {
            object.performSelector_withObject_afterDelay_inModes(
                sel!(shellioPopUp:),
                Some(anchor),
                0.0,
                &modes,
            );
        }
    }
}

impl Drop for Menu {
    fn drop(&mut self) {
        let target: &AnyObject = self.target.as_ref();
        // SAFETY: цель — объект этого меню; отменяются только её отложенные вызовы.
        unsafe { NSObject::cancelPreviousPerformRequestsWithTarget(target) };
        let root = self.root.borrow();
        if self.model.open.get() {
            root.menu.cancelTrackingWithoutAnimation();
        }
        root.menu.setDelegate(None);
        untarget(&root.nodes);
    }
}

/// Снимает цель и действие с пунктов и их подменю.
fn untarget(nodes: &[Node]) {
    for node in nodes {
        // SAFETY: цель и действие снимаются вместе.
        unsafe {
            node.item.setTarget(None);
            node.item.setAction(None);
        }
        if let Some(sub) = &node.submenu {
            untarget(&sub.nodes);
        }
    }
}

fn new_menu(mtm: MainThreadMarker, title: &str) -> Retained<NSMenu> {
    NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str(title))
}

/// Уровень вида с пунктами модели `entries`.
fn build_level(
    mtm: MainThreadMarker,
    target: &MenuTarget,
    title: &str,
    entries: &[MenuEntry],
) -> Level {
    let mut level = Level {
        menu: new_menu(mtm, title),
        nodes: Vec::new(),
    };
    for (index, entry) in entries.iter().enumerate() {
        let node = build(mtm, target, entry);
        insert(&mut level, index, node);
    }
    level
}

/// Пункт вида по пункту модели, с подменю.
fn build(mtm: MainThreadMarker, target: &MenuTarget, entry: &MenuEntry) -> Node {
    let model = match entry {
        MenuEntry::Separator { .. } => {
            return Node {
                item: NSMenuItem::separatorItem(mtm),
                submenu: None,
            };
        }
        MenuEntry::Item(model) => model,
    };
    // SAFETY: пункт без действия; цель и действие ставятся ниже вместе.
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(&model.title),
            None,
            &NSString::from_str(""),
        )
    };
    let id = NSString::from_str(&model.id);
    let target_object: &AnyObject = target.as_ref();
    // SAFETY: представленный объект — строка id; цель реализует `shellioChoose:` и
    // `validateMenuItem:`, её держит владелец до снятия в `Drop`.
    unsafe {
        item.setRepresentedObject(Some(id.as_ref()));
        item.setTarget(Some(target_object));
        item.setAction(Some(sel!(shellioChoose:)));
    }
    set_key(&item, model.key.as_ref());
    set_checked(&item, model.checked);
    let submenu = model.submenu.as_ref().map(|children| {
        let level = build_level(mtm, target, &model.title, children);
        item.setSubmenu(Some(&level.menu));
        level
    });
    Node { item, submenu }
}

fn set_key(item: &NSMenuItem, key: Option<&MenuKey>) {
    match key {
        Some(key) => {
            item.setKeyEquivalent(&NSString::from_str(&key.key));
            item.setKeyEquivalentModifierMask(key.modifiers());
        }
        None => {
            item.setKeyEquivalent(&NSString::from_str(""));
            item.setKeyEquivalentModifierMask(NSEventModifierFlags::Command);
        }
    }
}

fn set_checked(item: &NSMenuItem, checked: bool) {
    item.setState(if checked {
        NSControlStateValueOn
    } else {
        NSControlStateValueOff
    });
}

/// Вставляет пункт на место `index` модели: в `NSMenu` перед своим пунктом, который сейчас стоит на
/// этом месте, или после последнего своего пункта уровня.
fn insert(level: &mut Level, index: usize, node: Node) {
    let at = match level.nodes.get(index) {
        Some(next) => level.menu.indexOfItem(&next.item),
        None => level
            .nodes
            .last()
            .map_or(0, |last| level.menu.indexOfItem(&last.item) + 1),
    };
    level.menu.insertItem_atIndex(&node.item, at.max(0));
    level.nodes.insert(index, node);
}

/// Уровень по пути номеров модели.
fn level_at<'a>(root: &'a mut Level, path: &[usize]) -> Option<&'a mut Level> {
    let mut level = root;
    for &index in path {
        level = level.nodes.get_mut(index)?.submenu.as_mut()?;
    }
    Some(level)
}

fn apply(mtm: MainThreadMarker, target: &MenuTarget, root: &mut Level, op: &MenuOp) {
    let (MenuOp::Remove { path, .. }
    | MenuOp::Insert { path, .. }
    | MenuOp::Move { path, .. }
    | MenuOp::Update { path, .. }) = op;
    let Some(level) = level_at(root, path) else {
        return;
    };
    match op {
        MenuOp::Remove { index, .. } => {
            if *index < level.nodes.len() {
                let node = level.nodes.remove(*index);
                level.menu.removeItem(&node.item);
                untarget(std::slice::from_ref(&node));
            }
        }
        MenuOp::Insert { index, entry, .. } => {
            let node = build(mtm, target, entry);
            let index = (*index).min(level.nodes.len());
            insert(level, index, node);
        }
        MenuOp::Move { from, to, .. } => {
            if *from < level.nodes.len() {
                let node = level.nodes.remove(*from);
                level.menu.removeItem(&node.item);
                let to = (*to).min(level.nodes.len());
                insert(level, to, node);
            }
        }
        MenuOp::Update { index, change, .. } => {
            let Some(node) = level.nodes.get_mut(*index) else {
                return;
            };
            update(mtm, target, &level.menu, node, change);
        }
    }
}

fn update(
    mtm: MainThreadMarker,
    target: &MenuTarget,
    menu: &NSMenu,
    node: &mut Node,
    change: &ItemChange,
) {
    match change {
        ItemChange::Title(title) => {
            let title = NSString::from_str(title);
            node.item.setTitle(&title);
            if let Some(sub) = &node.submenu {
                sub.menu.setTitle(&title);
            }
        }
        ItemChange::Key(key) => set_key(&node.item, key.as_ref()),
        // Доступность отвечает `validateMenuItem:` по модели, которая уже новая.
        ItemChange::Enabled(_) => menu.update(),
        ItemChange::Checked(checked) => set_checked(&node.item, *checked),
        ItemChange::Submenu(children) => {
            if let Some(old) = node.submenu.take() {
                node.item.setSubmenu(None);
                untarget(&old.nodes);
            }
            if let Some(children) = children {
                let title = node.item.title().to_string();
                let level = build_level(mtm, target, &title, children);
                node.item.setSubmenu(Some(&level.menu));
                node.submenu = Some(level);
            }
        }
    }
}

/// Id пункта из его представленного объекта.
fn item_id(item: &NSMenuItem) -> Option<String> {
    item.representedObject()
        .and_then(|object| object.downcast::<NSString>().ok())
        .map(|id| id.to_string())
}

struct TargetIvars {
    _live: Live,
    owner: Callback<MenuModel>,
    /// Корневое меню для отложенного открытия; его держит владелец.
    root: rc::Weak<NSMenu>,
}

define_class!(
    // SAFETY: NSObject не требует от подкласса ничего сверх `init`, `Drop` у класса нет.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioMenuTarget"]
    #[ivars = TargetIvars]
    struct MenuTarget;

    // SAFETY: протокол NSObject реализует суперкласс NSObject.
    unsafe impl NSObjectProtocol for MenuTarget {}

    // SAFETY: протокол требует только необязательные методы, ниже они совпадают с его объявлениями.
    unsafe impl NSMenuDelegate for MenuTarget {
        // SAFETY: селектор и аргумент — как у `menuWillOpen:` протокола.
        #[unsafe(method(menuWillOpen:))]
        fn menu_will_open(&self, _menu: &NSMenu) {
            self.ivars().owner.call(|model| model.open.set(true));
        }

        // SAFETY: селектор и аргумент — как у `menuDidClose:` протокола.
        #[unsafe(method(menuDidClose:))]
        fn menu_did_close(&self, _menu: &NSMenu) {
            self.ivars().owner.call(|model| {
                model.open.set(false);
                model.highlighted.replace(None);
            });
        }

        // SAFETY: селектор и аргументы — как у `menu:willHighlightItem:` протокола.
        #[unsafe(method(menu:willHighlightItem:))]
        fn menu_will_highlight_item(&self, _menu: &NSMenu, item: Option<&NSMenuItem>) {
            let id = item.and_then(item_id);
            self.ivars().owner.call(|model| model.highlighted.replace(id));
        }
    }

    // SAFETY: метод совпадает с объявлением протокола.
    unsafe impl NSMenuItemValidation for MenuTarget {
        // SAFETY: селектор, аргумент и результат — как у `validateMenuItem:` протокола.
        #[unsafe(method(validateMenuItem:))]
        fn validate_menu_item(&self, item: &NSMenuItem) -> bool {
            item_id(item).is_some_and(|id| {
                self.ivars()
                    .owner
                    .call(|model| model.enabled(&id))
                    .unwrap_or(false)
            })
        }
    }

    impl MenuTarget {
        // SAFETY: действие AppKit: один аргумент-отправитель, результата нет.
        #[unsafe(method(shellioChoose:))]
        fn choose(&self, sender: Option<&AnyObject>) {
            let Some(id) = sender
                .and_then(|sender| sender.downcast_ref::<NSMenuItem>())
                .and_then(item_id)
            else {
                return;
            };
            self.ivars()
                .owner
                .call(|model| (model.on_intent)(MenuIntent::Chosen { id }));
        }

        // SAFETY: отложенный вызов из `Menu::pop_up_below`: один аргумент — вид-якорь, результата
        // нет.
        #[unsafe(method(shellioPopUp:))]
        fn pop_up(&self, anchor: Option<&AnyObject>) {
            if let Some(anchor) = anchor.and_then(|anchor| anchor.downcast_ref::<NSView>()) {
                self.open_below(anchor);
            }
        }
    }
);

impl MenuTarget {
    fn new(
        mtm: MainThreadMarker,
        owner: Callback<MenuModel>,
        root: &NSMenu,
    ) -> Retained<MenuTarget> {
        let this = MenuTarget::alloc(mtm).set_ivars(TargetIvars {
            _live: MENU_TARGET.live(),
            owner,
            root: rc::Weak::new(root),
        });
        // SAFETY: `init` у NSObject без аргументов, ivars уже заданы.
        unsafe { msg_send![super(this), init] }
    }

    /// Отслеживание корневого меню под якорем. Вложенный цикл идёт вне заимствований модели:
    /// делегат, `validateMenuItem:` и выбор пункта читают её изнутри цикла.
    fn open_below(&self, anchor: &NSView) {
        let Some((closed, first)) = self
            .ivars()
            .owner
            .call(|model| (!model.open.get(), model.first.get()))
        else {
            return;
        };
        if !closed {
            return;
        }
        let Some(root) = self.ivars().root.load() else {
            return;
        };
        let y = open_y(anchor.isFlipped(), anchor.bounds().size.height, first);
        let item = if first { root.itemAtIndex(0) } else { None };
        root.popUpMenuPositioningItem_atLocation_inView(
            item.as_deref(),
            NSPoint::new(0.0, y),
            Some(anchor),
        );
    }
}

/// Rust-часть кнопки меню.
struct ButtonModel {
    id: ComponentId,
    theme: Cell<AppTheme>,
    menu: Menu,
}

/// Кнопка «…», которая открывает своё меню под собой (нажатие, `AXPress` или [`MenuButton::open`]).
pub struct MenuButton {
    view: View<NSButton, ButtonModel>,
}

impl MenuButton {
    /// `label` — имя кнопки для доступности и подсказки, `title` — заголовок меню.
    pub fn new(
        app: &App,
        theme: &AppTheme,
        label: &str,
        title: &str,
        entries: Vec<MenuEntry>,
        on_intent: impl Fn(MenuIntent) + 'static,
    ) -> Result<MenuButton, MenuError> {
        let mtm = app.mtm();
        let model = ButtonModel {
            id: ComponentId::next(),
            theme: Cell::new(*theme),
            menu: Menu::new(app, title, entries, on_intent)?,
        };
        let label = NSString::from_str(label);
        let mut view = View::new(&MENU_BUTTON_OWNER, model, "MenuButton.action", |_| {
            let button = MenuButtonView::new(mtm);
            button.setBordered(false);
            button.setTitle(&NSString::from_str(""));
            button.setImagePosition(NSCellImagePosition::ImageOnly);
            button.setImage(crate::icon::image("more", 14.0, &label).as_deref());
            button.setAccessibilityLabel(Some(&label));
            button.setToolTip(Some(&label));
            Retained::into_super(button)
        });
        let button = view.view().retain();
        let anchor = rc::Weak::new(&*button);
        let control: &NSControl = &button;
        view.bind_action(control, "MenuButton.action", move |model| {
            if let Some(anchor) = anchor.load() {
                model.menu.pop_up_below(&anchor);
            }
        });
        let menu_button = MenuButton { view };
        menu_button.apply_theme();
        Ok(menu_button)
    }

    pub fn id(&self) -> ComponentId {
        self.view.rust().id
    }

    pub fn menu(&self) -> &Menu {
        &self.view.rust().menu
    }

    /// Открывает меню под кнопкой тем же путём, что нажатие.
    pub fn open(&self) {
        self.menu().pop_up_below(self.view.view());
    }

    pub fn set_theme(&self, theme: &AppTheme) {
        let model = self.view.rust();
        if model.theme.get() == *theme {
            return;
        }
        model.theme.set(*theme);
        self.apply_theme();
    }

    /// Кнопка для `ui.inspect` с состоянием меню: пункты, открыто ли, подсвеченный пункт.
    pub fn inspect(&self, id: &str, parent: Option<&str>) -> Vec<InspectNode> {
        let button = self.view.view();
        let mut nodes = vec![inspect::node(
            id,
            "MenuButton",
            parent,
            button,
            Some(button.isEnabled()),
            Some(NodeState::Menu(self.menu().state())),
        )];
        // Значок рисует ячейка кнопки: кадр — её прямоугольник картинки.
        nodes.extend(inspect::button_image(
            &format!("{id}.icon"),
            id,
            button,
            "more",
        ));
        nodes
    }

    /// Кнопка для размещения в иерархии.
    pub(crate) fn ns_view(&self) -> &NSView {
        self.view.view()
    }

    fn apply_theme(&self) {
        let theme = self.view.rust().theme.get();
        self.view
            .view()
            .setContentTintColor(Some(&layer::ns_color(theme.colors.text_3)));
    }
}

struct ButtonIvars {
    _live: Live,
}

define_class!(
    // SAFETY: NSButton не требует от подкласса ничего сверх переопределённых ниже методов, `Drop` у
    // класса нет.
    #[unsafe(super(NSButton, NSControl, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioMenuButton"]
    #[ivars = ButtonIvars]
    struct MenuButtonView;

    impl MenuButtonView {
        // SAFETY: селектор и результат — как у `mouseDownCanMoveWindow` NSView.
        #[unsafe(method(mouseDownCanMoveWindow))]
        fn mouse_down_can_move_window(&self) -> bool {
            false
        }
    }
);

impl MenuButtonView {
    fn new(mtm: MainThreadMarker) -> Retained<MenuButtonView> {
        let this = MenuButtonView::alloc(mtm).set_ivars(ButtonIvars {
            _live: MENU_BUTTON.live(),
        });
        // SAFETY: назначенный инициализатор NSButton (через NSControl), ivars уже заданы.
        unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_opens_six_points_below_the_anchor_and_a_choice_at_its_edge() {
        assert_eq!(open_y(true, 22.0, false), 28.0, "ниже нижнего края кнопки");
        assert_eq!(
            open_y(false, 22.0, false),
            -6.0,
            "в неперевёрнутом виде низ — ноль"
        );
        assert_eq!(
            open_y(true, 22.0, true),
            22.0,
            "выбор — вплотную, как прежде"
        );
        assert_eq!(open_y(false, 22.0, true), 0.0);
    }

    fn item(id: &str) -> MenuEntry {
        MenuEntry::Item(MenuItem::new(id, &format!("Пункт {id}")))
    }

    fn item_b() -> MenuEntry {
        item("b")
    }

    fn sep(id: &str) -> MenuEntry {
        MenuEntry::Separator { id: id.to_owned() }
    }

    fn with_submenu(id: &str, children: Vec<MenuEntry>) -> MenuEntry {
        MenuEntry::Item(MenuItem {
            submenu: Some(children),
            ..MenuItem::new(id, id)
        })
    }

    fn edit(entry: &MenuEntry, change: impl FnOnce(&mut MenuItem)) -> MenuEntry {
        let MenuEntry::Item(item) = entry else {
            panic!("не пункт: {entry:?}");
        };
        let mut item = item.clone();
        change(&mut item);
        MenuEntry::Item(item)
    }

    /// Оракул: применение операции к модели так, как вид применяет её к `NSMenu`.
    fn apply_model(entries: &mut Vec<MenuEntry>, op: &MenuOp) {
        fn level<'a>(entries: &'a mut Vec<MenuEntry>, path: &[usize]) -> &'a mut Vec<MenuEntry> {
            let mut level = entries;
            for &index in path {
                let MenuEntry::Item(MenuItem {
                    submenu: Some(children),
                    ..
                }) = &mut level[index]
                else {
                    panic!("путь {path:?} не ведёт в подменю");
                };
                level = children;
            }
            level
        }
        match op {
            MenuOp::Remove { path, index } => {
                level(entries, path).remove(*index);
            }
            MenuOp::Insert { path, index, entry } => {
                level(entries, path).insert(*index, entry.clone());
            }
            MenuOp::Move { path, from, to } => {
                let level = level(entries, path);
                let entry = level.remove(*from);
                level.insert(*to, entry);
            }
            MenuOp::Update {
                path,
                index,
                change,
            } => {
                let MenuEntry::Item(item) = &mut level(entries, path)[*index] else {
                    panic!("изменение разделителя: {op:?}");
                };
                match change {
                    ItemChange::Title(title) => item.title.clone_from(title),
                    ItemChange::Key(key) => item.key.clone_from(key),
                    ItemChange::Enabled(enabled) => item.enabled = *enabled,
                    ItemChange::Checked(checked) => item.checked = *checked,
                    ItemChange::Submenu(submenu) => item.submenu.clone_from(submenu),
                }
            }
        }
    }

    fn assert_diff_reaches(before: &[MenuEntry], after: &[MenuEntry]) -> Vec<MenuOp> {
        let ops = diff(before, after);
        let mut current = before.to_vec();
        for op in &ops {
            apply_model(&mut current, op);
        }
        assert_eq!(current, after, "операции {ops:#?}");
        assert!(diff(after, after).is_empty());
        ops
    }

    #[test]
    fn same_entries_give_no_operations() {
        let entries = vec![item("a"), sep("s"), with_submenu("m", vec![item("b")])];
        assert!(diff(&entries, &entries).is_empty());
    }

    #[test]
    fn property_change_is_one_update() {
        let before = vec![item("a"), item("b")];
        let after = vec![
            item("a"),
            edit(&before[1], |item| {
                item.title = "Новое".into();
                item.checked = true;
            }),
        ];
        assert_eq!(
            assert_diff_reaches(&before, &after),
            [
                MenuOp::Update {
                    path: vec![],
                    index: 1,
                    change: ItemChange::Title("Новое".into()),
                },
                MenuOp::Update {
                    path: vec![],
                    index: 1,
                    change: ItemChange::Checked(true),
                },
            ]
        );
        let disabled = vec![
            edit(&before[0], |item| {
                item.enabled = false;
                item.key = Some(MenuKey::command("w"));
            }),
            item("b"),
        ];
        assert_eq!(
            assert_diff_reaches(&before, &disabled),
            [
                MenuOp::Update {
                    path: vec![],
                    index: 0,
                    change: ItemChange::Key(Some(MenuKey::command("w"))),
                },
                MenuOp::Update {
                    path: vec![],
                    index: 0,
                    change: ItemChange::Enabled(false),
                },
            ]
        );
    }

    #[test]
    fn insertion_in_the_middle_keeps_neighbours() {
        let before = vec![item("a"), item("c")];
        let after = vec![item("a"), item("b"), item("c")];
        assert_eq!(
            assert_diff_reaches(&before, &after),
            [MenuOp::Insert {
                path: vec![],
                index: 1,
                entry: item("b"),
            }]
        );
    }

    #[test]
    fn removal_and_reorder_move_the_same_items() {
        let before = vec![item("a"), item("b"), item("c"), item("d")];
        let after = vec![item("d"), item("a"), item("c")];
        let ops = assert_diff_reaches(&before, &after);
        assert_eq!(
            ops,
            [
                MenuOp::Remove {
                    path: vec![],
                    index: 1,
                },
                MenuOp::Move {
                    path: vec![],
                    from: 2,
                    to: 0,
                },
            ]
        );
        assert!(
            !ops.iter().any(|op| matches!(op, MenuOp::Insert { .. })),
            "оставшиеся пункты не пересоздаются"
        );
    }

    #[test]
    fn kind_change_replaces_the_entry() {
        let before = vec![item("a"), item("x")];
        let after = vec![item("a"), sep("x")];
        assert_eq!(
            assert_diff_reaches(&before, &after),
            [
                MenuOp::Remove {
                    path: vec![],
                    index: 1,
                },
                MenuOp::Insert {
                    path: vec![],
                    index: 1,
                    entry: sep("x"),
                },
            ]
        );
    }

    #[test]
    fn nested_changes_carry_the_submenu_path() {
        let before = vec![
            item("a"),
            with_submenu(
                "view",
                vec![with_submenu("look", vec![item("x"), item("y")])],
            ),
        ];
        let after = vec![
            item("a"),
            with_submenu(
                "view",
                vec![with_submenu(
                    "look",
                    vec![item("y"), edit(&item("x"), |item| item.checked = true)],
                )],
            ),
        ];
        assert_eq!(
            assert_diff_reaches(&before, &after),
            [
                MenuOp::Move {
                    path: vec![1, 0],
                    from: 1,
                    to: 0,
                },
                MenuOp::Update {
                    path: vec![1, 0],
                    index: 1,
                    change: ItemChange::Checked(true),
                },
            ]
        );
    }

    #[test]
    fn submenu_appears_and_disappears_whole() {
        let before = vec![item("a")];
        let after = vec![edit(&before[0], |item| {
            item.submenu = Some(vec![item_b()]);
        })];
        assert_eq!(
            assert_diff_reaches(&before, &after),
            [MenuOp::Update {
                path: vec![],
                index: 0,
                change: ItemChange::Submenu(Some(vec![item_b()])),
            }]
        );
        assert_eq!(
            assert_diff_reaches(&after, &before),
            [MenuOp::Update {
                path: vec![],
                index: 0,
                change: ItemChange::Submenu(None),
            }]
        );
        let renamed = vec![with_submenu("a", vec![item_b()])];
        let ops = assert_diff_reaches(&before, &renamed);
        assert!(matches!(
            ops.as_slice(),
            [
                MenuOp::Update {
                    change: ItemChange::Title(_),
                    ..
                },
                MenuOp::Update {
                    change: ItemChange::Submenu(Some(_)),
                    ..
                },
            ]
        ));
    }

    #[test]
    fn empty_menus() {
        let some = vec![item("a"), sep("s")];
        assert_eq!(assert_diff_reaches(&[], &some).len(), 2);
        assert_eq!(assert_diff_reaches(&some, &[]).len(), 2);
        assert!(diff(&[], &[]).is_empty());
    }

    #[test]
    fn duplicate_ids_are_refused_at_any_depth() {
        assert_eq!(check_ids(&[item("a"), sep("s")]), Ok(()));
        assert_eq!(
            check_ids(&[item("a"), sep("a")]),
            Err(MenuError::DuplicateId("a".into()))
        );
        assert_eq!(
            check_ids(&[item("a"), with_submenu("m", vec![item("b"), item("a")])]),
            Err(MenuError::DuplicateId("a".into()))
        );
    }

    #[test]
    fn enabled_is_read_by_id_at_any_depth() {
        let entries = vec![
            item("a"),
            with_submenu("m", vec![edit(&item("b"), |item| item.enabled = false)]),
            sep("s"),
        ];
        assert!(matches!(find(&entries, "b"), Some(MenuEntry::Item(item)) if !item.enabled));
        assert!(matches!(find(&entries, "a"), Some(MenuEntry::Item(item)) if item.enabled));
        assert!(matches!(
            find(&entries, "s"),
            Some(MenuEntry::Separator { .. })
        ));
        assert!(find(&entries, "нет").is_none());
    }

    #[test]
    fn chord_names_modifiers_in_order() {
        assert_eq!(MenuKey::command("q").chord(), "cmd+q");
        let all = MenuKey {
            key: "k".into(),
            command: true,
            shift: true,
            option: true,
            control: true,
        };
        assert_eq!(all.chord(), "ctrl+opt+shift+cmd+k");
        assert_eq!(
            all.modifiers(),
            NSEventModifierFlags::Command
                | NSEventModifierFlags::Shift
                | NSEventModifierFlags::Option
                | NSEventModifierFlags::Control
        );
    }

    /// Детерминированный генератор SplitMix64.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        fn below(&mut self, bound: usize) -> usize {
            usize::try_from(self.next() % bound as u64).unwrap_or(0)
        }

        fn chance(&mut self, percent: u64) -> bool {
            self.next() % 100 < percent
        }
    }

    /// Случайное меню: id из общего пула, поэтому соседние меню делят пункты.
    fn random_menu(rng: &mut Rng, depth: usize, used: &mut HashSet<String>) -> Vec<MenuEntry> {
        let len = rng.below(7);
        let mut entries = Vec::new();
        for _ in 0..len {
            let id = format!("i{}", rng.below(24));
            if !used.insert(id.clone()) {
                continue;
            }
            if rng.chance(15) {
                entries.push(MenuEntry::Separator { id });
                continue;
            }
            let submenu = (depth < 2 && rng.chance(30)).then(|| random_menu(rng, depth + 1, used));
            entries.push(MenuEntry::Item(MenuItem {
                id: id.clone(),
                title: format!("{id}-{}", rng.below(3)),
                key: rng
                    .chance(30)
                    .then(|| MenuKey::command(&format!("{}", rng.below(3)))),
                enabled: rng.chance(80),
                checked: rng.chance(20),
                submenu,
            }));
        }
        entries
    }

    #[test]
    fn random_menus_are_reached_by_their_diff() {
        for seed in [1_u64, 7, 42, 2026, 0xDEAD_BEEF] {
            let mut rng = Rng(seed);
            let mut before = random_menu(&mut rng, 0, &mut HashSet::new());
            for _ in 0..200 {
                let after = random_menu(&mut rng, 0, &mut HashSet::new());
                assert_eq!(check_ids(&after), Ok(()));
                assert_diff_reaches(&before, &after);
                before = after;
            }
        }
    }
}
