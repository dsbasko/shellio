//! Чистая часть `TreeList`: узлы с устойчивыми id, раскрытые узлы и выбор, разница состояний в
//! операции над `NSOutlineView`, свёртка с правилом выбора и клавиатурные намерения.
//!
//! Раскрытие и выбор держатся за id узла, а не за номер строки (P0.2, инвариант 7 и находка 5):
//! вид держит один объект Objective-C на id, поэтому перезагрузка детей не теряет ни раскрытия, ни
//! выбора, а разница всё равно повторяет раскрытие перезагруженных поддеревьев и выбор по id.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::rc::Rc;

/// Узел дерева.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeNode {
    /// Устойчивый id: раскрытие, выбор и разница идут по нему. Уникален во всём дереве.
    pub id: String,
    pub text: String,
    pub children: Vec<TreeNode>,
}

impl TreeNode {
    pub fn leaf(id: &str, text: &str) -> TreeNode {
        TreeNode::branch(id, text, Vec::new())
    }

    pub fn branch(id: &str, text: &str, children: Vec<TreeNode>) -> TreeNode {
        TreeNode {
            id: id.to_owned(),
            text: text.to_owned(),
            children,
        }
    }

    /// Раскрыть можно только узел с детьми.
    pub fn is_expandable(&self) -> bool {
        !self.children.is_empty()
    }
}

/// Состояние дерева.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeListState {
    /// AX-имя дерева.
    pub label: String,
    /// Корни общие для состояний: смена раскрытия или выбора не копирует узлы, а разница
    /// пропускает те же узлы без сравнения.
    pub roots: Rc<[TreeNode]>,
    /// Раскрытые узлы. Все их предки тоже раскрыты.
    pub expanded: BTreeSet<String>,
    /// Выбранный узел; все его предки раскрыты.
    pub selected: Option<String>,
}

impl Default for TreeListState {
    fn default() -> Self {
        TreeListState {
            label: String::new(),
            roots: Rc::from(Vec::new()),
            expanded: BTreeSet::new(),
            selected: None,
        }
    }
}

/// Видимая строка дерева.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeRow {
    pub id: String,
    pub text: String,
    /// Уровень: у корня 0.
    pub level: usize,
    pub expandable: bool,
    pub expanded: bool,
}

impl TreeListState {
    pub fn new<'a>(
        label: &str,
        roots: impl Into<Rc<[TreeNode]>>,
        expanded: impl IntoIterator<Item = &'a str>,
        selected: Option<&str>,
    ) -> TreeListState {
        TreeListState {
            label: label.to_owned(),
            roots: roots.into(),
            expanded: expanded.into_iter().map(str::to_owned).collect(),
            selected: selected.map(str::to_owned),
        }
    }

    /// Видимые строки в прямом порядке: дети раскрытого узла идут сразу за ним.
    pub fn rows(&self) -> Vec<TreeRow> {
        let mut rows = Vec::new();
        let mut stack: Vec<(&TreeNode, usize)> =
            self.roots.iter().rev().map(|node| (node, 0)).collect();
        while let Some((node, level)) = stack.pop() {
            let expandable = node.is_expandable();
            let expanded = expandable && self.expanded.contains(&node.id);
            rows.push(TreeRow {
                id: node.id.clone(),
                text: node.text.clone(),
                level,
                expandable,
                expanded,
            });
            if expanded {
                stack.extend(node.children.iter().rev().map(|child| (child, level + 1)));
            }
        }
        rows
    }

    /// Число узлов во всём дереве.
    pub fn node_count(&self) -> usize {
        let mut count = 0;
        preorder(&self.roots, &mut |_, _| {
            count += 1;
            true
        });
        count
    }
}

/// Ошибка состояния дерева.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeListError {
    DuplicateId(String),
    UnknownSelection(String),
    /// Выбранный узел под свёрнутым предком.
    HiddenSelection(String),
    UnknownExpanded(String),
    /// Раскрыт узел без детей.
    LeafExpanded(String),
    /// Раскрытый узел под свёрнутым предком.
    HiddenExpanded(String),
}

impl fmt::Display for TreeListError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TreeListError::DuplicateId(id) => write!(f, "узел {id} повторяется"),
            TreeListError::UnknownSelection(id) => write!(f, "выбран узел {id}, которого нет"),
            TreeListError::HiddenSelection(id) => {
                write!(f, "выбранный узел {id} лежит под свёрнутым предком")
            }
            TreeListError::UnknownExpanded(id) => write!(f, "раскрыт узел {id}, которого нет"),
            TreeListError::LeafExpanded(id) => write!(f, "раскрыт узел {id} без детей"),
            TreeListError::HiddenExpanded(id) => {
                write!(f, "раскрытый узел {id} лежит под свёрнутым предком")
            }
        }
    }
}

impl std::error::Error for TreeListError {}

/// Узел и его родитель.
struct Entry<'a> {
    node: &'a TreeNode,
    parent: Option<&'a str>,
}

/// Узлы дерева по id.
struct Index<'a>(HashMap<&'a str, Entry<'a>>);

impl<'a> Index<'a> {
    fn build(roots: &'a [TreeNode]) -> Result<Index<'a>, TreeListError> {
        let mut map = HashMap::new();
        let mut duplicate = None;
        preorder(roots, &mut |node, parent| {
            if map
                .insert(node.id.as_str(), Entry { node, parent })
                .is_some()
            {
                duplicate = Some(node.id.clone());
                return false;
            }
            true
        });
        match duplicate {
            Some(id) => Err(TreeListError::DuplicateId(id)),
            None => Ok(Index(map)),
        }
    }

    fn get(&self, id: &str) -> Option<&Entry<'a>> {
        self.0.get(id)
    }

    /// `id` лежит под `ancestor` (сам узел — не потомок себя).
    fn is_descendant(&self, id: &str, ancestor: &str) -> bool {
        let mut cursor = self.get(id).and_then(|entry| entry.parent);
        while let Some(parent) = cursor {
            if parent == ancestor {
                return true;
            }
            cursor = self.get(parent).and_then(|entry| entry.parent);
        }
        false
    }

    /// Все предки `id` раскрыты.
    fn is_visible(&self, expanded: &BTreeSet<String>, id: &str) -> bool {
        let mut cursor = self.get(id).and_then(|entry| entry.parent);
        while let Some(parent) = cursor {
            if !expanded.contains(parent) {
                return false;
            }
            cursor = self.get(parent).and_then(|entry| entry.parent);
        }
        true
    }

    fn ancestors(&self, id: &str) -> Vec<&'a str> {
        let mut chain = Vec::new();
        let mut cursor = self.get(id).and_then(|entry| entry.parent);
        while let Some(parent) = cursor {
            chain.push(parent);
            cursor = self.get(parent).and_then(|entry| entry.parent);
        }
        chain
    }
}

/// Прямой обход всех узлов. `visit` получает узел и id родителя и отвечает, заходить ли в детей.
fn preorder<'a>(
    roots: &'a [TreeNode],
    visit: &mut impl FnMut(&'a TreeNode, Option<&'a str>) -> bool,
) {
    let mut stack: Vec<(&TreeNode, Option<&str>)> =
        roots.iter().rev().map(|node| (node, None)).collect();
    while let Some((node, parent)) = stack.pop() {
        if visit(node, parent) {
            stack.extend(
                node.children
                    .iter()
                    .rev()
                    .map(|child| (child, Some(node.id.as_str()))),
            );
        }
    }
}

/// Прямой обход видимых узлов: в детей заходит только у раскрытого узла, и если `visit` разрешил.
fn visible_preorder<'a>(
    roots: &'a [TreeNode],
    expanded: &BTreeSet<String>,
    visit: &mut impl FnMut(&'a TreeNode) -> bool,
) {
    preorder(roots, &mut |node, _| {
        visit(node) && expanded.contains(&node.id)
    });
}

/// Id уникальны во всём дереве, раскрытые узлы существуют, имеют детей и видимы, выбранный узел
/// существует и видим.
pub fn check(state: &TreeListState) -> Result<(), TreeListError> {
    let index = Index::build(&state.roots)?;
    for id in &state.expanded {
        let Some(entry) = index.get(id) else {
            return Err(TreeListError::UnknownExpanded(id.clone()));
        };
        if !entry.node.is_expandable() {
            return Err(TreeListError::LeafExpanded(id.clone()));
        }
        if !index.is_visible(&state.expanded, id) {
            return Err(TreeListError::HiddenExpanded(id.clone()));
        }
    }
    if let Some(id) = &state.selected {
        if index.get(id).is_none() {
            return Err(TreeListError::UnknownSelection(id.clone()));
        }
        if !index.is_visible(&state.expanded, id) {
            return Err(TreeListError::HiddenSelection(id.clone()));
        }
    }
    Ok(())
}

/// Раскрыть узел `id` вместе со свёрнутыми предками. Узел без детей и неизвестный id состояния не
/// меняют.
pub fn expand(state: &TreeListState, id: &str) -> TreeListState {
    let mut next = state.clone();
    let Ok(index) = Index::build(&state.roots) else {
        return next;
    };
    if index
        .get(id)
        .is_some_and(|entry| entry.node.is_expandable())
    {
        next.expanded.insert(id.to_owned());
        next.expanded
            .extend(index.ancestors(id).into_iter().map(str::to_owned));
    }
    next
}

/// Свернуть узлы `ids` одним шагом модели: узел и все его потомки выходят из раскрытых. Выбор
/// под свёрнутым узлом переходит на этот узел (верхний из свёрнутых предков выбора). Неизвестные
/// id пропускаются.
pub fn collapse(state: &TreeListState, ids: &[&str]) -> TreeListState {
    let mut next = state.clone();
    let Ok(index) = Index::build(&state.roots) else {
        return next;
    };
    let ids: Vec<&str> = ids
        .iter()
        .copied()
        .filter(|id| index.get(id).is_some())
        .collect();
    next.expanded.retain(|open| {
        !ids.iter()
            .any(|id| open == id || index.is_descendant(open, id))
    });
    if let Some(selected) = state.selected.as_deref() {
        // Верхний свёрнутый предок: выбор встаёт на видимую строку.
        let top = index
            .ancestors(selected)
            .into_iter()
            .rev()
            .find(|ancestor| ids.contains(ancestor));
        if let Some(top) = top {
            next.selected = Some(top.to_owned());
        }
    }
    next
}

/// Раскрыть или свернуть один узел.
pub fn set_expanded(state: &TreeListState, id: &str, expanded: bool) -> TreeListState {
    if expanded {
        expand(state, id)
    } else {
        collapse(state, &[id])
    }
}

/// Операция над деревом. Порядок в разнице: подпись, свёртка (по прежним узлам), перезагрузка
/// детей, текст, раскрытие и выбор (по новым узлам).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeOp {
    Label(String),
    /// Первое наполнение: полный `reloadData`.
    Fill,
    /// Свернуть узлы вместе с потомками. Один шаг модели — одна операция, сколько бы узлов он ни
    /// сворачивал; в списке только верхние узлы, в прямом порядке прежнего дерева.
    Collapse(Vec<String>),
    /// Перезагрузить узлы вместе с поддеревом: `None` — все корни. Только верхние узлы, чьи дети
    /// изменились, в прямом порядке нового дерева.
    Reload(Vec<Option<String>>),
    /// Обновить текст узлов вне перезагруженных поддеревьев.
    Text(Vec<String>),
    /// Раскрыть узлы в прямом порядке: родитель раньше ребёнка. Уже раскрытые вид пропускает.
    Expand(Vec<String>),
    /// Выбор по id; `reveal` — прокрутить выбранный узел в видимую часть.
    Select {
        id: Option<String>,
        reveal: bool,
    },
}

/// Дети каждого родителя: `None` — корни.
fn children_map(roots: &[TreeNode]) -> HashMap<Option<&str>, Vec<&str>> {
    let mut map = HashMap::new();
    map.insert(None, roots.iter().map(|node| node.id.as_str()).collect());
    preorder(roots, &mut |node, _| {
        map.insert(
            Some(node.id.as_str()),
            node.children
                .iter()
                .map(|child| child.id.as_str())
                .collect(),
        );
        true
    });
    map
}

/// Операции, которые переводят дерево из `before` в `after`; `before` равно `None` до первого
/// наполнения. Оба состояния прошли [`check`].
pub fn diff(before: Option<&TreeListState>, after: &TreeListState) -> Vec<TreeOp> {
    let mut ops = Vec::new();
    let Some(before) = before else {
        ops.push(TreeOp::Label(after.label.clone()));
        ops.push(TreeOp::Fill);
        let mut expand = Vec::new();
        visible_preorder(&after.roots, &after.expanded, &mut |node| {
            if after.expanded.contains(&node.id) {
                expand.push(node.id.clone());
            }
            true
        });
        if !expand.is_empty() {
            ops.push(TreeOp::Expand(expand));
        }
        ops.push(TreeOp::Select {
            id: after.selected.clone(),
            reveal: true,
        });
        return ops;
    };
    if before.label != after.label {
        ops.push(TreeOp::Label(after.label.clone()));
    }

    let mut collapse = Vec::new();
    visible_preorder(&before.roots, &before.expanded, &mut |node| {
        if before.expanded.contains(&node.id) && !after.expanded.contains(&node.id) {
            collapse.push(node.id.clone());
            return false;
        }
        true
    });
    if !collapse.is_empty() {
        ops.push(TreeOp::Collapse(collapse));
    }

    let structure = !Rc::ptr_eq(&before.roots, &after.roots);
    // Узлы, раскрытие которых вид мог забыть: перезагруженные узлы и их поддеревья.
    let mut reloaded: HashSet<&str> = HashSet::new();
    let mut all_reloaded = false;
    if structure {
        let old = children_map(&before.roots);
        let new = children_map(&after.roots);
        let mut reload = Vec::new();
        if old.get(&None) != new.get(&None) {
            reload.push(None);
            all_reloaded = true;
        } else {
            preorder(&after.roots, &mut |node, _| {
                let key = Some(node.id.as_str());
                match (old.get(&key), new.get(&key)) {
                    (Some(was), Some(now)) if was != now => {
                        reload.push(Some(node.id.clone()));
                        false
                    }
                    _ => true,
                }
            });
            for key in reload.iter().flatten() {
                if let Some(node) = find(&after.roots, key) {
                    preorder(std::slice::from_ref(node), &mut |inner, _| {
                        reloaded.insert(inner.id.as_str());
                        true
                    });
                }
            }
        }
        if !reload.is_empty() {
            ops.push(TreeOp::Reload(reload));
        }
        if !all_reloaded {
            let old_text: HashMap<&str, &str> = {
                let mut map = HashMap::new();
                preorder(&before.roots, &mut |node, _| {
                    map.insert(node.id.as_str(), node.text.as_str());
                    true
                });
                map
            };
            let mut text = Vec::new();
            preorder(&after.roots, &mut |node, _| {
                if reloaded.contains(node.id.as_str()) {
                    return false;
                }
                if old_text
                    .get(node.id.as_str())
                    .is_some_and(|was| *was != node.text)
                {
                    text.push(node.id.clone());
                }
                true
            });
            if !text.is_empty() {
                ops.push(TreeOp::Text(text));
            }
        }
    }

    let mut expand = Vec::new();
    visible_preorder(&after.roots, &after.expanded, &mut |node| {
        let forgotten = all_reloaded || reloaded.contains(node.id.as_str());
        if after.expanded.contains(&node.id) && (forgotten || !before.expanded.contains(&node.id)) {
            expand.push(node.id.clone());
        }
        true
    });
    let expanded = !expand.is_empty();
    if expanded {
        ops.push(TreeOp::Expand(expand));
    }

    let selection_changed = before.selected != after.selected;
    if structure
        || selection_changed
        || expanded
        || ops.iter().any(|op| matches!(op, TreeOp::Collapse(_)))
    {
        ops.push(TreeOp::Select {
            id: after.selected.clone(),
            reveal: structure || selection_changed,
        });
    }
    ops
}

/// Узел по id прямым обходом.
fn find<'a>(roots: &'a [TreeNode], id: &str) -> Option<&'a TreeNode> {
    let mut found = None;
    preorder(roots, &mut |node, _| {
        if found.is_some() {
            return false;
        }
        if node.id == id {
            found = Some(node);
            return false;
        }
        true
    });
    found
}

/// Намерение дерева.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeListIntent {
    /// Выбрать узел: щелчок, Up и Down, Left к родителю, Right к первому ребёнку.
    Select { id: String },
    /// Раскрыть или свернуть узел: треугольник, Left и Right.
    Toggle { id: String, expand: bool },
    /// Открыть узел: `Enter`.
    Activate { id: String },
}

/// Клавиша, которую дерево обрабатывает само при фокусе в нём.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeListKey {
    Up,
    Down,
    Left,
    Right,
    Enter,
}

/// Намерение клавиши по правилам `NSOutlineView`: Up и Down выбирают соседнюю видимую строку без
/// перехода через край (без выбора Down выбирает первую, Up — последнюю). Right раскрывает
/// свёрнутый узел, а у раскрытого выбирает первого ребёнка. Left сворачивает раскрытый узел, а у
/// свёрнутого или листа выбирает родителя. `Enter` открывает выбранный узел. `None` — клавиша
/// ничего не делает.
pub fn key_intent(state: &TreeListState, key: TreeListKey) -> Option<TreeListIntent> {
    let rows = state.rows();
    let last = rows.len().checked_sub(1)?;
    let index = state
        .selected
        .as_deref()
        .and_then(|id| rows.iter().position(|row| row.id == id));
    let select = |index: usize| {
        Some(TreeListIntent::Select {
            id: rows[index].id.clone(),
        })
    };
    match (key, index) {
        (TreeListKey::Down, None) => select(0),
        (TreeListKey::Up, None) => select(last),
        (TreeListKey::Left | TreeListKey::Right | TreeListKey::Enter, None) => None,
        (TreeListKey::Down, Some(index)) => (index < last).then(|| index + 1).and_then(select),
        (TreeListKey::Up, Some(index)) => index.checked_sub(1).and_then(select),
        (TreeListKey::Enter, Some(index)) => Some(TreeListIntent::Activate {
            id: rows[index].id.clone(),
        }),
        (TreeListKey::Right, Some(index)) => {
            let row = &rows[index];
            match (row.expandable, row.expanded) {
                (true, false) => Some(TreeListIntent::Toggle {
                    id: row.id.clone(),
                    expand: true,
                }),
                (true, true) => select(index + 1),
                (false, _) => None,
            }
        }
        (TreeListKey::Left, Some(index)) => {
            let row = &rows[index];
            if row.expanded {
                return Some(TreeListIntent::Toggle {
                    id: row.id.clone(),
                    expand: false,
                });
            }
            let level = row.level.checked_sub(1)?;
            let parent = rows[..index]
                .iter()
                .rposition(|above| above.level == level)?;
            select(parent)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Дерево `roots` × `branches` × `leaves` глубины 3, как в галерее: `n000`, `n000.0`,
    /// `n000.0.0`.
    fn gallery_like(roots: usize, branches: usize, leaves: usize) -> Vec<TreeNode> {
        (0..roots)
            .map(|root| {
                let root_id = format!("n{root:03}");
                let children = (0..branches)
                    .map(|branch| {
                        let branch_id = format!("{root_id}.{branch}");
                        let leaves = (0..leaves)
                            .map(|leaf| {
                                let id = format!("{branch_id}.{leaf}");
                                TreeNode::leaf(&id, &format!("Лист {id}"))
                            })
                            .collect();
                        TreeNode::branch(&branch_id, &format!("Ветка {branch_id}"), leaves)
                    })
                    .collect();
                TreeNode::branch(&root_id, &format!("Корень {root_id}"), children)
            })
            .collect()
    }

    fn gallery_state() -> TreeListState {
        TreeListState::new(
            "Дерево",
            gallery_like(100, 9, 10),
            ["n000", "n000.0"],
            Some("n000.0.0"),
        )
    }

    /// Дерево вида так, как его ведёт `NSOutlineView` с узлами-объектами: свёртка забывает
    /// раскрытие поддерева, перезагрузка (пессимистично) забывает раскрытие перезагруженных
    /// поддеревьев, а раскрыть можно только видимый узел с детьми. Строка показывает текст,
    /// который узел имел при последней загрузке строки: наполнение, перезагрузка поддерева или
    /// `Text`.
    struct Outline {
        roots: Rc<[TreeNode]>,
        expanded: BTreeSet<String>,
        selected: Option<String>,
        reloaded_all: bool,
        shown_text: HashMap<String, String>,
    }

    /// Тексты узлов `roots` по id.
    fn texts(roots: &[TreeNode]) -> HashMap<String, String> {
        let mut map = HashMap::new();
        preorder(roots, &mut |node, _| {
            map.insert(node.id.clone(), node.text.clone());
            true
        });
        map
    }

    impl Outline {
        fn new(state: Option<&TreeListState>) -> Outline {
            Outline {
                roots: state.map_or_else(|| Rc::from(Vec::new()), |s| Rc::clone(&s.roots)),
                expanded: state.map(|s| s.expanded.clone()).unwrap_or_default(),
                selected: state.and_then(|s| s.selected.clone()),
                reloaded_all: false,
                shown_text: state.map(|s| texts(&s.roots)).unwrap_or_default(),
            }
        }

        fn swap(&mut self, after: &TreeListState) {
            if Rc::ptr_eq(&self.roots, &after.roots) {
                return;
            }
            self.roots = Rc::clone(&after.roots);
            let index = Index::build(&self.roots).expect("новое дерево проверено");
            self.expanded.retain(|id| index.get(id).is_some());
        }

        fn apply(&mut self, ops: &[TreeOp], after: &TreeListState) {
            let mut swapped = false;
            for op in ops {
                if !matches!(op, TreeOp::Label(_) | TreeOp::Collapse(_)) && !swapped {
                    self.swap(after);
                    swapped = true;
                }
                match op {
                    TreeOp::Label(_) => {}
                    TreeOp::Fill => {
                        self.expanded.clear();
                        self.reloaded_all = true;
                        self.shown_text = texts(&self.roots);
                    }
                    TreeOp::Collapse(ids) => {
                        assert!(!swapped, "свёртка идёт по прежним узлам");
                        let index = Index::build(&self.roots).expect("прежнее дерево");
                        for id in ids {
                            assert!(self.expanded.contains(id), "свёртка свёрнутого {id}");
                            assert!(
                                index.is_visible(&self.expanded, id),
                                "свёртка скрытого {id}"
                            );
                            self.expanded
                                .retain(|open| open != id && !index.is_descendant(open, id));
                        }
                    }
                    TreeOp::Reload(keys) => {
                        let index = Index::build(&self.roots).expect("новое дерево");
                        for key in keys {
                            match key {
                                None => {
                                    self.expanded.clear();
                                    self.shown_text = texts(&self.roots);
                                }
                                Some(key) => {
                                    let entry = index.get(key).expect("перезагрузка известного");
                                    self.shown_text
                                        .extend(texts(std::slice::from_ref(entry.node)));
                                    self.expanded.retain(|open| {
                                        open != key && !index.is_descendant(open, key)
                                    });
                                }
                            }
                        }
                    }
                    TreeOp::Text(ids) => {
                        let index = Index::build(&self.roots).expect("новое дерево");
                        for id in ids {
                            let entry = index.get(id).expect("текст известного узла");
                            self.shown_text.insert(id.clone(), entry.node.text.clone());
                        }
                    }
                    TreeOp::Expand(ids) => {
                        let index = Index::build(&self.roots).expect("новое дерево");
                        for id in ids {
                            let entry = index.get(id).expect("раскрытие известного узла");
                            assert!(entry.node.is_expandable(), "раскрытие листа {id}");
                            assert!(
                                index.is_visible(&self.expanded, id),
                                "раскрытие скрытого {id}: родитель раньше ребёнка"
                            );
                            self.expanded.insert(id.clone());
                        }
                    }
                    TreeOp::Select { id, .. } => self.selected = id.clone(),
                }
            }
            if !swapped {
                self.swap(after);
            }
        }

        fn rows(&self) -> Vec<TreeRow> {
            TreeListState {
                label: String::new(),
                roots: Rc::clone(&self.roots),
                expanded: self.expanded.clone(),
                selected: None,
            }
            .rows()
        }
    }

    fn assert_applies(before: Option<&TreeListState>, after: &TreeListState) -> Vec<TreeOp> {
        assert_eq!(check(after), Ok(()), "после");
        let ops = diff(before, after);
        let mut outline = Outline::new(before);
        outline.apply(&ops, after);
        assert_eq!(outline.expanded, after.expanded, "раскрытие по id: {ops:?}");
        assert_eq!(outline.rows(), after.rows(), "видимые строки");
        for row in after.rows() {
            assert_eq!(
                outline.shown_text.get(&row.id),
                Some(&row.text),
                "показанный текст {}: {ops:?}",
                row.id
            );
        }
        assert_eq!(outline.selected, after.selected, "выбор по id");
        ops
    }

    fn row_of(state: &TreeListState, id: &str) -> Option<usize> {
        state.rows().iter().position(|row| row.id == id)
    }

    #[test]
    fn first_fill_reloads_then_expands_parents_first_and_selects() {
        let state = gallery_state();
        let ops = assert_applies(None, &state);
        assert_eq!(
            ops,
            vec![
                TreeOp::Label("Дерево".to_owned()),
                TreeOp::Fill,
                TreeOp::Expand(vec!["n000".to_owned(), "n000.0".to_owned()]),
                TreeOp::Select {
                    id: Some("n000.0.0".to_owned()),
                    reveal: true,
                },
            ]
        );
        assert!(diff(Some(&state), &state).is_empty());
        assert!(diff(Some(&state), &state.clone()).is_empty(), "те же корни");
    }

    #[test]
    fn depth_three_leaves_are_rows_with_levels() {
        let state = gallery_state();
        let rows = state.rows();
        assert_eq!(rows.len(), 100 + 9 + 10);
        assert_eq!(
            (rows[0].id.as_str(), rows[0].level, rows[0].expanded),
            ("n000", 0, true)
        );
        assert_eq!(
            (rows[1].id.as_str(), rows[1].level, rows[1].expanded),
            ("n000.0", 1, true)
        );
        assert_eq!(
            (rows[2].id.as_str(), rows[2].level, rows[2].expandable),
            ("n000.0.0", 2, false)
        );
        assert_eq!(
            rows[12].id, "n000.1",
            "после десяти листьев — следующая ветка"
        );
        assert!(!rows[12].expanded && rows[12].expandable);
        assert_eq!(rows[20].id, "n001");
        assert_eq!(state.node_count(), 100 + 900 + 9_000);
        assert_eq!(check(&state), Ok(()));
    }

    #[test]
    fn insert_keeps_expansion_and_selection_by_id() {
        let before = gallery_state();
        let mut roots = before.roots.to_vec();
        roots.insert(0, TreeNode::leaf("x1", "Новый корень 1"));
        roots[1]
            .children
            .insert(0, TreeNode::leaf("n000.x1", "Новая ветка 1"));
        let after = TreeListState {
            roots: roots.into(),
            ..before.clone()
        };
        let ops = assert_applies(Some(&before), &after);
        assert_eq!(ops[0], TreeOp::Reload(vec![None]), "корни изменились");
        assert_eq!(
            ops.last(),
            Some(&TreeOp::Select {
                id: Some("n000.0.0".to_owned()),
                reveal: true,
            })
        );
        assert_eq!(row_of(&before, "n000.0.0"), Some(2));
        assert_eq!(row_of(&after, "n000.0.0"), Some(4), "номер строки сдвинут");
        assert_eq!(row_of(&after, "n000.0"), Some(3));
    }

    #[test]
    fn nested_insert_reloads_only_the_changed_parent() {
        let before = gallery_state();
        let mut roots = before.roots.to_vec();
        roots[0]
            .children
            .insert(0, TreeNode::leaf("n000.x1", "Новая ветка 1"));
        roots[0].children[1]
            .children
            .push(TreeNode::leaf("n000.0.x", "Новый лист"));
        let after = TreeListState {
            roots: roots.into(),
            ..before.clone()
        };
        let ops = assert_applies(Some(&before), &after);
        assert_eq!(ops[0], TreeOp::Reload(vec![Some("n000".to_owned())]));
        assert!(ops.contains(&TreeOp::Expand(vec![
            "n000".to_owned(),
            "n000.0".to_owned()
        ])));
    }

    #[test]
    fn reorder_keeps_expansion_and_selection_by_id() {
        let before = gallery_state();
        let mut roots = before.roots.to_vec();
        roots.swap(0, 1);
        let after = TreeListState {
            roots: roots.into(),
            ..before.clone()
        };
        assert_applies(Some(&before), &after);
        assert_eq!(row_of(&after, "n000"), Some(1));
        assert_eq!(row_of(&after, "n000.0.0"), Some(3));
        assert_eq!(after.rows()[0].id, "n001");

        let mut roots = before.roots.to_vec();
        roots[0].children.reverse();
        let reversed = TreeListState {
            roots: roots.into(),
            ..before.clone()
        };
        let ops = assert_applies(Some(&before), &reversed);
        assert_eq!(ops[0], TreeOp::Reload(vec![Some("n000".to_owned())]));
        assert_eq!(row_of(&reversed, "n000.0"), Some(9));
    }

    #[test]
    fn collapsing_the_parent_of_the_selection_selects_the_parent() {
        let before = gallery_state();
        let after = set_expanded(&before, "n000.0", false);
        assert_eq!(after.selected.as_deref(), Some("n000.0"));
        assert_eq!(after.expanded, BTreeSet::from(["n000".to_owned()]));
        let ops = assert_applies(Some(&before), &after);
        assert_eq!(
            ops,
            vec![
                TreeOp::Collapse(vec!["n000.0".to_owned()]),
                TreeOp::Select {
                    id: Some("n000.0".to_owned()),
                    reveal: true,
                },
            ]
        );
        let root = collapse(&before, &["n000"]);
        assert_eq!(root.selected.as_deref(), Some("n000"));
        assert!(
            root.expanded.is_empty(),
            "потомки свёрнутого не помнят раскрытия"
        );
        assert_applies(Some(&before), &root);
        let other = collapse(&before, &["n000.0", "n001"]);
        assert_eq!(other.selected.as_deref(), Some("n000.0"));
        assert_eq!(
            collapse(&before, &["n000", "n000.0"]).selected.as_deref(),
            Some("n000")
        );
        assert_eq!(
            collapse(&before, &["n001"]).selected.as_deref(),
            Some("n000.0.0"),
            "свёртка не предка выбора не меняет"
        );
    }

    #[test]
    fn collapsing_a_hundred_roots_is_one_operation() {
        let roots = gallery_like(100, 9, 10);
        let ids: Vec<String> = roots.iter().map(|root| root.id.clone()).collect();
        let before = TreeListState::new(
            "Дерево",
            roots,
            ids.iter().map(String::as_str).chain(["n050.3"]),
            Some("n050.3.2"),
        );
        assert_eq!(check(&before), Ok(()));
        let names: Vec<&str> = ids.iter().map(String::as_str).collect();
        let after = collapse(&before, &names);
        assert!(after.expanded.is_empty());
        assert_eq!(after.selected.as_deref(), Some("n050"));
        let ops = assert_applies(Some(&before), &after);
        let collapses: Vec<&TreeOp> = ops
            .iter()
            .filter(|op| matches!(op, TreeOp::Collapse(_)))
            .collect();
        assert_eq!(collapses.len(), 1, "{ops:?}");
        assert_eq!(collapses[0], &TreeOp::Collapse(ids.clone()), "только корни");
        assert_eq!(ops.len(), 2, "свёртка и выбор: {ops:?}");
        assert_eq!(after.rows().len(), 100);
    }

    #[test]
    fn expand_opens_hidden_ancestors_and_ignores_leaves() {
        let before = TreeListState::new("", gallery_like(3, 2, 2), [], None);
        let after = expand(&before, "n001.1");
        assert_eq!(
            after.expanded,
            BTreeSet::from(["n001".to_owned(), "n001.1".to_owned()])
        );
        let ops = assert_applies(Some(&before), &after);
        assert_eq!(
            ops[0],
            TreeOp::Expand(vec!["n001".to_owned(), "n001.1".to_owned()])
        );
        assert_eq!(expand(&before, "n001.1.0"), before, "лист");
        assert_eq!(expand(&before, "нет"), before);
        assert_eq!(collapse(&before, &["нет"]), before);
    }

    #[test]
    fn changed_text_outside_reloads_is_a_text_operation() {
        let before = gallery_state();
        let mut roots = before.roots.to_vec();
        roots[3].text = "Другой корень".to_owned();
        let after = TreeListState {
            roots: roots.into(),
            ..before.clone()
        };
        let ops = assert_applies(Some(&before), &after);
        assert_eq!(ops[0], TreeOp::Text(vec!["n003".to_owned()]));
        assert!(!ops.iter().any(|op| matches!(op, TreeOp::Reload(_))));
    }

    #[test]
    fn label_and_selection_alone() {
        let before = gallery_state();
        let after = TreeListState {
            label: "Другое".to_owned(),
            selected: Some("n000.0.5".to_owned()),
            ..before.clone()
        };
        assert_eq!(
            assert_applies(Some(&before), &after),
            vec![
                TreeOp::Label("Другое".to_owned()),
                TreeOp::Select {
                    id: Some("n000.0.5".to_owned()),
                    reveal: true,
                },
            ]
        );
        let toggled = set_expanded(&before, "n002", true);
        assert_eq!(
            assert_applies(Some(&before), &toggled),
            vec![
                TreeOp::Expand(vec!["n002".to_owned()]),
                TreeOp::Select {
                    id: Some("n000.0.0".to_owned()),
                    reveal: false,
                },
            ],
            "раскрытие не прокручивает к выбору"
        );
    }

    #[test]
    fn check_rejects_broken_states() {
        let roots = gallery_like(2, 2, 2);
        let state = |expanded: &[&str], selected: Option<&str>| {
            TreeListState::new("", roots.clone(), expanded.iter().copied(), selected)
        };
        assert_eq!(check(&state(&["n000"], Some("n000.1"))), Ok(()));
        assert_eq!(
            check(&state(&[], Some("z"))),
            Err(TreeListError::UnknownSelection("z".to_owned()))
        );
        assert_eq!(
            check(&state(&[], Some("n000.1"))),
            Err(TreeListError::HiddenSelection("n000.1".to_owned()))
        );
        assert_eq!(
            check(&state(&["z"], None)),
            Err(TreeListError::UnknownExpanded("z".to_owned()))
        );
        assert_eq!(
            check(&state(&["n000", "n000.1", "n000.1.0"], None)),
            Err(TreeListError::LeafExpanded("n000.1.0".to_owned()))
        );
        assert_eq!(
            check(&state(&["n000.1"], None)),
            Err(TreeListError::HiddenExpanded("n000.1".to_owned()))
        );
        let mut duplicate = roots.clone();
        duplicate[1].children[0].children[0].id = "n000".to_owned();
        assert_eq!(
            check(&TreeListState::new("", duplicate, [], None)),
            Err(TreeListError::DuplicateId("n000".to_owned()))
        );
    }

    #[test]
    fn keys_follow_outline_rules() {
        let state = gallery_state();
        let select = |id: &str| Some(TreeListIntent::Select { id: id.to_owned() });
        let at = |id: &str| TreeListState {
            selected: Some(id.to_owned()),
            ..state.clone()
        };
        assert_eq!(
            key_intent(&at("n000.0.0"), TreeListKey::Down),
            select("n000.0.1")
        );
        assert_eq!(
            key_intent(&at("n000.0.0"), TreeListKey::Up),
            select("n000.0")
        );
        assert_eq!(
            key_intent(&at("n000"), TreeListKey::Up),
            None,
            "без перехода через край"
        );
        assert_eq!(key_intent(&at("n099"), TreeListKey::Down), None);
        assert_eq!(
            key_intent(&at("n000.0.0"), TreeListKey::Left),
            select("n000.0"),
            "лист — к родителю"
        );
        assert_eq!(
            key_intent(&at("n000.0"), TreeListKey::Left),
            Some(TreeListIntent::Toggle {
                id: "n000.0".to_owned(),
                expand: false,
            })
        );
        assert_eq!(
            key_intent(&at("n001"), TreeListKey::Left),
            None,
            "свёрнутый корень"
        );
        assert_eq!(
            key_intent(&at("n000.1"), TreeListKey::Left),
            select("n000"),
            "свёрнутая ветка — к родителю"
        );
        assert_eq!(
            key_intent(&at("n000.1"), TreeListKey::Right),
            Some(TreeListIntent::Toggle {
                id: "n000.1".to_owned(),
                expand: true,
            })
        );
        assert_eq!(
            key_intent(&at("n000"), TreeListKey::Right),
            select("n000.0"),
            "раскрытый — к первому ребёнку"
        );
        assert_eq!(
            key_intent(&at("n000.0.3"), TreeListKey::Right),
            None,
            "лист"
        );
        assert_eq!(
            key_intent(&at("n000.0.3"), TreeListKey::Enter),
            Some(TreeListIntent::Activate {
                id: "n000.0.3".to_owned(),
            })
        );
        let none = TreeListState {
            selected: None,
            ..state.clone()
        };
        assert_eq!(key_intent(&none, TreeListKey::Down), select("n000"));
        assert_eq!(key_intent(&none, TreeListKey::Up), select("n099"));
        for key in [TreeListKey::Left, TreeListKey::Right, TreeListKey::Enter] {
            assert_eq!(key_intent(&none, key), None);
        }
        let empty = TreeListState::default();
        for key in [
            TreeListKey::Up,
            TreeListKey::Down,
            TreeListKey::Left,
            TreeListKey::Right,
            TreeListKey::Enter,
        ] {
            assert_eq!(key_intent(&empty, key), None);
        }
    }

    /// SplitMix64: воспроизводимые деревья без внешних зависимостей.
    struct Random(u64);

    impl Random {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        fn below(&mut self, bound: usize) -> usize {
            (self.next() % bound.max(1) as u64) as usize
        }
    }

    /// Случайное дерево из узлов пула: каждый узел пула попадает в дерево не больше одного раза.
    fn random_tree(random: &mut Random, pool: &mut Vec<usize>, depth: usize) -> Vec<TreeNode> {
        let count = random.below(4);
        let mut nodes = Vec::new();
        for _ in 0..count {
            let Some(id) = pool.pop() else {
                break;
            };
            let children = if depth < 3 && random.below(3) > 0 {
                random_tree(random, pool, depth + 1)
            } else {
                Vec::new()
            };
            let text = if random.below(5) == 0 { "!" } else { "." };
            nodes.push(TreeNode::branch(
                &format!("u{id}"),
                &format!("u{id}{text}"),
                children,
            ));
        }
        nodes
    }

    /// Те же узлы с частью текстов, изменённых случайно.
    fn retext(random: &mut Random, nodes: &[TreeNode]) -> Vec<TreeNode> {
        nodes
            .iter()
            .map(|node| {
                let text = if random.below(3) == 0 {
                    format!("{}~", node.text)
                } else {
                    node.text.clone()
                };
                TreeNode::branch(&node.id, &text, retext(random, &node.children))
            })
            .collect()
    }

    fn random_state(random: &mut Random) -> TreeListState {
        let mut pool: Vec<usize> = (0..40).collect();
        for index in (1..pool.len()).rev() {
            pool.swap(index, random.below(index + 1));
        }
        let roots = random_tree(random, &mut pool, 0);
        let mut state = TreeListState::new("", roots, [], None);
        let mut ids = Vec::new();
        preorder(&state.roots, &mut |node, _| {
            ids.push(node.id.clone());
            true
        });
        for id in &ids {
            if random.below(2) == 0 {
                state = expand(&state, id);
            }
        }
        for id in &ids {
            if random.below(4) == 0 {
                state = collapse(&state, &[id]);
            }
        }
        let rows = state.rows();
        if !rows.is_empty() && random.below(4) > 0 {
            state.selected = Some(rows[random.below(rows.len())].id.clone());
        }
        state
    }

    #[test]
    fn random_trees_apply_exactly() {
        let mut random = Random(0x7EE5);
        let mut texts_changed = 0;
        for _ in 0..500 {
            let before = random_state(&mut random);
            let after = random_state(&mut random);
            assert_eq!(check(&before), Ok(()));
            assert_applies(None, &before);
            assert_applies(Some(&before), &after);
            // Тот же набор узлов с другим раскрытием и выбором — без перезагрузки.
            let mut same = random_state(&mut random);
            same.roots = Rc::clone(&before.roots);
            same.expanded.retain(|_| false);
            same.selected = None;
            for id in before.expanded.iter().rev() {
                if random.below(2) == 0 {
                    same = expand(&same, id);
                }
            }
            let ops = assert_applies(Some(&before), &same);
            assert!(
                !ops.iter()
                    .any(|op| matches!(op, TreeOp::Reload(_) | TreeOp::Text(_)))
            );
            // Новые корни той же формы с другими текстами — только `Text`, без перезагрузки.
            let mut retexted = before.clone();
            retexted.roots = Rc::from(retext(&mut random, &before.roots));
            let ops = assert_applies(Some(&before), &retexted);
            assert!(!ops.iter().any(|op| matches!(op, TreeOp::Reload(_))));
            texts_changed += ops
                .iter()
                .map(|op| match op {
                    TreeOp::Text(ids) => ids.len(),
                    _ => 0,
                })
                .sum::<usize>();
        }
        assert!(
            texts_changed > 500,
            "случаи `Text` наблюдаются: {texts_changed}"
        );
    }
}
