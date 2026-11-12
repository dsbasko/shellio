//! Детерминированные фикстуры сцены. Модуль не зависит от AppKit и в стоимость компонентов не
//! входит.

/// Наборы вкладок сценария `tabs`.
pub const TAB_COUNTS: [usize; 3] = [1, 8, 30];

/// Наборы строк сценария `list`.
pub const LIST_SIZES: [usize; 3] = [1_000, 10_000, 100_000];
/// Строк в списках сцены.
pub const SCENE_ROWS: usize = 1_000;

/// Строка списка. Идентификатор стабилен и не зависит от размера набора.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListRow {
    pub id: u64,
    pub title: String,
}

/// Короткие слова держат подпись в левой части строки: правая половина строки без текста.
const LIST_WORDS: [&str; 8] = [
    "build", "commit", "branch", "review", "agent", "task", "file", "log",
];

pub fn list_row(index: usize) -> ListRow {
    ListRow {
        id: index as u64 + 1,
        title: format!("{} {index:06}", LIST_WORDS[index % LIST_WORDS.len()]),
    }
}

/// Набор из `count` строк.
pub fn list(count: usize) -> Vec<ListRow> {
    (0..count).map(list_row).collect()
}

/// Строка после правки `revision`: идентификатор прежний, подпись новая.
pub fn edited(row: &ListRow, revision: u32) -> ListRow {
    ListRow {
        id: row.id,
        title: format!("{} *{revision}", row.title),
    }
}

/// Команд в палитре сцены.
pub const PALETTE_COMMANDS: usize = 2_000;
/// Команда пробы, которую вызывают пункт меню, его сочетание клавиш и палитра. Она первая в
/// наборе, поэтому при пустом запросе выбрана.
pub const MARK_COMMAND: u64 = 1;
pub const MARK_TITLE: &str = "Отметить";

/// Команда палитры. Идентификатор — номер в наборе с 1.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    pub id: u64,
    pub title: String,
    pub group: &'static str,
}

const COMMAND_VERBS: [&str; 8] = [
    "Открыть",
    "Закрыть",
    "Показать",
    "Скрыть",
    "Запустить",
    "Остановить",
    "Перейти к",
    "Найти",
];
const COMMAND_GROUPS: [&str; 5] = ["Панели", "Сессии", "Git", "Агенты", "Вид"];

/// Набор из `count` команд: первая — команда пробы, остальные — глагол, латинское слово и номер.
/// Латинские слова нужны сценарию: он набирает запрос в латинской раскладке.
pub fn commands(count: usize) -> Vec<Command> {
    (0..count)
        .map(|index| {
            if index == 0 {
                return Command {
                    id: MARK_COMMAND,
                    title: MARK_TITLE.to_owned(),
                    group: "Проба",
                };
            }
            Command {
                id: index as u64 + 1,
                title: format!(
                    "{} {} {index:04}",
                    COMMAND_VERBS[index % COMMAND_VERBS.len()],
                    LIST_WORDS[(index / COMMAND_VERBS.len()) % LIST_WORDS.len()]
                ),
                group: COMMAND_GROUPS[index % COMMAND_GROUPS.len()],
            }
        })
        .collect()
}

/// Ветвление дерева навигатора по уровням: 100 корней, у каждого 9 детей, у каждого из них 10.
pub const TREE_BRANCHING: [usize; 3] = [100, 9, 10];
/// Узлов в дереве навигатора.
pub const TREE_SIZE: usize = 10_000;

/// Узел дерева. Идентификатор — номер узла в прямом обходе с 1, он задан положением узла в
/// исходном дереве и не меняется при перестановке.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeSpec {
    pub id: u64,
    pub title: String,
    pub children: Vec<TreeSpec>,
}

/// Слова подписей по уровням; глубже последнего уровня повторяется последнее.
const TREE_WORDS: [&str; 3] = ["repo", "dir", "file"];

/// Дерево с ветвлением `branching` по уровням. Подпись — слово уровня и путь из номеров с 1.
pub fn tree(branching: &[usize]) -> Vec<TreeSpec> {
    let mut next = 1;
    tree_level(branching, &mut Vec::new(), &mut next)
}

fn tree_level(branching: &[usize], path: &mut Vec<usize>, next: &mut u64) -> Vec<TreeSpec> {
    let Some((&count, deeper)) = branching.split_first() else {
        return Vec::new();
    };
    (1..=count)
        .map(|number| {
            path.push(number);
            let id = *next;
            *next += 1;
            let word = TREE_WORDS[(path.len() - 1).min(TREE_WORDS.len() - 1)];
            let steps: Vec<String> = path
                .iter()
                .enumerate()
                .map(|(level, step)| {
                    if level == 0 {
                        format!("{step:03}")
                    } else {
                        step.to_string()
                    }
                })
                .collect();
            let title = format!("{word} {}", steps.join("."));
            let children = tree_level(deeper, path, next);
            path.pop();
            TreeSpec {
                id,
                title,
                children,
            }
        })
        .collect()
}

/// Число узлов дерева.
pub fn tree_size(nodes: &[TreeSpec]) -> usize {
    nodes.iter().map(|node| 1 + tree_size(&node.children)).sum()
}

/// Число уровней дерева; у пустого 0.
pub fn tree_depth(nodes: &[TreeSpec]) -> usize {
    nodes
        .iter()
        .map(|node| 1 + tree_depth(&node.children))
        .max()
        .unwrap_or(0)
}

/// Видимые строки дерева сверху вниз: идентификатор и уровень. Дети узла видны, если раскрыт
/// он и все его предки.
pub fn visible_rows(nodes: &[TreeSpec], expanded: &dyn Fn(u64) -> bool) -> Vec<(u64, usize)> {
    let mut rows = Vec::new();
    push_visible(nodes, 0, expanded, &mut rows);
    rows
}

fn push_visible(
    nodes: &[TreeSpec],
    level: usize,
    expanded: &dyn Fn(u64) -> bool,
    rows: &mut Vec<(u64, usize)>,
) {
    for node in nodes {
        rows.push((node.id, level));
        if expanded(node.id) {
            push_visible(&node.children, level + 1, expanded, rows);
        }
    }
}

/// Тип вкладки: от него зависит значок в центральной и нижней плитках.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabKind {
    Terminal,
    Agent,
    Editor,
    Changes,
    History,
}

/// Состояние, которое показывает точка статуса вкладки.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabStatus {
    None,
    Run,
    Idle,
    Exit,
}

/// Нагрузка активной вкладки: замер процесса или оценка данных внутри приложения.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Meter {
    Measured(String),
    Estimate(String),
}

impl Meter {
    pub fn text(&self) -> &str {
        match self {
            Meter::Measured(text) | Meter::Estimate(text) => text,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabSpec {
    pub title: String,
    pub kind: TabKind,
    pub status: TabStatus,
    pub meter: Meter,
}

const BASE: [(&str, TabKind); 10] = [
    ("zsh", TabKind::Terminal),
    ("claude", TabKind::Agent),
    ("nvim", TabKind::Editor),
    ("cargo test", TabKind::Terminal),
    ("Изменения", TabKind::Changes),
    ("codex", TabKind::Agent),
    ("make check", TabKind::Terminal),
    ("История", TabKind::History),
    ("logs · api", TabKind::Terminal),
    ("ревью 42", TabKind::Changes),
];

const STATUSES: [TabStatus; 4] = [
    TabStatus::Run,
    TabStatus::Idle,
    TabStatus::None,
    TabStatus::Exit,
];

/// Набор из `count` вкладок. Подписи повторяются через 10 вкладок с номером круга.
pub fn tabs(count: usize) -> Vec<TabSpec> {
    (0..count)
        .map(|index| {
            let (name, kind) = BASE[index % BASE.len()];
            let round = index / BASE.len();
            let title = if round == 0 {
                name.to_owned()
            } else {
                format!("{name} {}", round + 1)
            };
            let meter = match kind {
                TabKind::Terminal | TabKind::Agent => {
                    Meter::Measured(format!("{} МБ · {}%", 120 + (index * 37) % 400, index % 7))
                }
                TabKind::Editor | TabKind::Changes | TabKind::History => {
                    Meter::Estimate(format!("≈ {} МБ", 8 + (index * 5) % 60))
                }
            };
            TabSpec {
                title,
                kind,
                status: STATUSES[index % STATUSES.len()],
                meter,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn tab_sets_are_deterministic() {
        for count in TAB_COUNTS {
            assert_eq!(tabs(count), tabs(count), "набор {count}");
        }
        // Меньший набор — начало большего.
        assert_eq!(tabs(8)[..], tabs(30)[..8]);
        assert_eq!(tabs(1)[0].title, "zsh");
        assert_eq!(tabs(30)[29].title, "ревью 42 3");
        assert_eq!(tabs(30)[29].meter, Meter::Estimate("≈ 33 МБ".to_owned()));
    }

    #[test]
    fn tab_sets_have_requested_sizes_and_unique_titles() {
        for count in TAB_COUNTS {
            let set = tabs(count);
            assert_eq!(set.len(), count);
            let titles: BTreeSet<&str> = set.iter().map(|tab| tab.title.as_str()).collect();
            assert_eq!(titles.len(), count, "подписи набора {count} повторяются");
            assert!(set.iter().all(|tab| !tab.meter.text().is_empty()));
        }
        let all = tabs(30);
        for status in STATUSES {
            assert!(all.iter().any(|tab| tab.status == status), "{status:?}");
        }
        assert!(
            all.iter()
                .any(|tab| matches!(tab.meter, Meter::Measured(_)))
        );
        assert!(
            all.iter()
                .any(|tab| matches!(tab.meter, Meter::Estimate(_)))
        );
    }

    #[test]
    fn empty_tab_set() {
        assert!(tabs(0).is_empty());
    }

    #[test]
    fn list_sets_are_deterministic_prefixes() {
        for count in LIST_SIZES {
            let set = list(count);
            assert_eq!(set.len(), count);
            assert_eq!(set, list(count), "набор {count}");
        }
        assert_eq!(list(1_000)[..], list(10_000)[..1_000]);
        assert_eq!(list(SCENE_ROWS).len(), LIST_SIZES[0]);
    }

    #[test]
    fn list_ids_are_stable_and_unique() {
        let set = list(10_000);
        let ids: BTreeSet<u64> = set.iter().map(|row| row.id).collect();
        let titles: BTreeSet<&str> = set.iter().map(|row| row.title.as_str()).collect();
        assert_eq!((ids.len(), titles.len()), (10_000, 10_000));
        assert!(
            set.iter()
                .enumerate()
                .all(|(index, row)| row.id == index as u64 + 1)
        );
    }

    #[test]
    fn list_boundaries() {
        assert!(list(0).is_empty());
        assert_eq!(
            list(1),
            vec![ListRow {
                id: 1,
                title: "build 000000".to_owned()
            }]
        );
        let last = list_row(LIST_SIZES[2] - 1);
        assert_eq!(last, list(LIST_SIZES[2])[LIST_SIZES[2] - 1]);
        assert_eq!((last.id, last.title.as_str()), (100_000, "log 099999"));
        // Подпись короткая и у самой длинной строки набора.
        let longest = list(LIST_SIZES[2])
            .iter()
            .map(|row| row.title.chars().count())
            .max();
        assert_eq!(longest, Some(13));
    }

    #[test]
    fn tree_has_fixture_size_and_fixed_depth() {
        let tree = tree(&TREE_BRANCHING);
        assert_eq!(tree_size(&tree), TREE_SIZE);
        assert_eq!(tree.len(), TREE_BRANCHING[0]);
        assert_eq!(tree_depth(&tree), TREE_BRANCHING.len());
        // Глубина одинакова у каждой ветви: у каждого узла уровня ровно столько детей,
        // сколько задано для следующего уровня.
        fn check(nodes: &[TreeSpec], branching: &[usize]) {
            assert_eq!(nodes.len(), branching.first().copied().unwrap_or(0));
            for node in nodes {
                check(&node.children, &branching[1..]);
            }
        }
        check(&tree, &TREE_BRANCHING);
    }

    #[test]
    fn tree_ids_are_stable_preorder_positions() {
        let first = tree(&TREE_BRANCHING);
        assert_eq!(first, tree(&TREE_BRANCHING));
        fn preorder(nodes: &[TreeSpec], out: &mut Vec<u64>) {
            for node in nodes {
                out.push(node.id);
                preorder(&node.children, out);
            }
        }
        let mut ids = Vec::new();
        preorder(&first, &mut ids);
        assert_eq!(ids, (1..=TREE_SIZE as u64).collect::<Vec<_>>());
        assert_eq!((first[0].id, first[0].title.as_str()), (1, "repo 001"));
        let child = &first[0].children[1];
        assert_eq!((child.id, child.title.as_str()), (13, "dir 001.2"));
        let leaf = &first[99].children[8].children[9];
        assert_eq!((leaf.id, leaf.title.as_str()), (10_000, "file 100.9.10"));
        // Идентификатор узла задан его положением: у меньшего дерева та же нумерация начала.
        let small = tree(&[2, 9, 10]);
        assert_eq!(small[0], first[0]);
    }

    #[test]
    fn single_node_and_empty_trees() {
        let single = tree(&[1]);
        assert_eq!(
            single,
            vec![TreeSpec {
                id: 1,
                title: "repo 001".to_owned(),
                children: Vec::new()
            }]
        );
        assert_eq!((tree_size(&single), tree_depth(&single)), (1, 1));
        assert_eq!(visible_rows(&single, &|_| true), vec![(1, 0)]);
        let empty = tree(&[]);
        assert!(empty.is_empty());
        assert_eq!((tree_size(&empty), tree_depth(&empty)), (0, 0));
    }

    #[test]
    fn visible_rows_follow_expansion() {
        let tree = tree(&TREE_BRANCHING);
        let collapsed = visible_rows(&tree, &|_| false);
        assert_eq!(collapsed.len(), TREE_BRANCHING[0]);
        assert!(collapsed.iter().all(|(_, level)| *level == 0));
        let roots: BTreeSet<u64> = tree.iter().map(|node| node.id).collect();
        let first_level = visible_rows(&tree, &|id| roots.contains(&id));
        assert_eq!(first_level.len(), 100 + 900);
        assert_eq!(first_level[1], (2, 1));
        assert_eq!(first_level[10], (tree[1].id, 0));
        assert_eq!(visible_rows(&tree, &|_| true).len(), TREE_SIZE);
        // Раскрытый узел под свёрнутым предком детей не показывает.
        let deep = tree[0].children[0].id;
        assert_eq!(visible_rows(&tree, &|id| id == deep).len(), 100);
    }

    #[test]
    fn command_set_is_deterministic_with_mark_first() {
        let set = commands(PALETTE_COMMANDS);
        assert_eq!(set.len(), PALETTE_COMMANDS);
        assert_eq!(set, commands(PALETTE_COMMANDS));
        assert_eq!(
            (set[0].id, set[0].title.as_str()),
            (MARK_COMMAND, MARK_TITLE)
        );
        assert_eq!(set[1].title, "Закрыть build 0001");
        assert_eq!(set[1_999].title, "Найти commit 1999");
        let ids: BTreeSet<u64> = set.iter().map(|command| command.id).collect();
        let titles: BTreeSet<&str> = set.iter().map(|command| command.title.as_str()).collect();
        assert_eq!(
            (ids.len(), titles.len()),
            (PALETTE_COMMANDS, PALETTE_COMMANDS)
        );
        assert!(
            set.iter()
                .enumerate()
                .all(|(index, command)| command.id == index as u64 + 1)
        );
        assert!(commands(0).is_empty());
        assert_eq!(commands(1)[0].title, MARK_TITLE);
    }

    #[test]
    fn edited_row_keeps_id() {
        let row = list_row(44);
        let edited = edited(&row, 3);
        assert_eq!(edited.id, row.id);
        assert_eq!(edited.title, "agent 000044 *3");
        assert_ne!(edited.title, row.title);
    }
}
