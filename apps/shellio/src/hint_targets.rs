//! Цели меток `f` (правило «Метки» плана наложений): видимые доступные элементы окна с действием
//! из узлов `ui.inspect`. Порядок целей — порядок документа прототипа (`overlays.js:260`): заголовок
//! окна, зоны левая, центральная, нижняя и правая, в части — строка вкладок, затем тело, в конце
//! строка статуса. Метка цели зависит от её номера, поэтому порядок — часть вида.

use std::collections::HashMap;

use shellio_control::{ComponentView, Rect};

/// Наименьшая сторона цели, как у прототипа.
const MIN_SIDE: f64 = 4.0;
/// Порядок зон в документе прототипа.
const ZONES: [&str; 4] = ["left", "center", "bottom", "right"];
/// Кнопки заголовка окна в порядке прототипа.
const CHROME: [&str; 9] = [
    "chrome.window.close",
    "chrome.window.minimize",
    "chrome.window.zoom",
    "chrome.workspace",
    "chrome.search",
    "chrome.zones.left",
    "chrome.zones.bottom",
    "chrome.zones.right",
    "chrome.appearance",
];
/// Кнопки строки вкладок после вкладок.
const TOOLS: [&str; 3] = ["split", "zoom", "menu"];
/// Корни плавающих поверхностей: цель под ними перекрыта.
const FLOATING: [&str; 4] = ["picker", "palette", "menu", "whichkey"];

/// Цель метки: ключ цели и её кадр в окне владельца.
#[derive(Clone, Debug, PartialEq)]
pub struct HintTarget {
    /// Ключ сверки узла, а у строки списка без ключа — имя узла в `ui.inspect`.
    pub key: String,
    /// Имя узла в `ui.inspect`: по нему нажимается цель.
    pub id: String,
    pub frame: Rect,
}

/// Место цели в порядке документа.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Place {
    Chrome(usize),
    /// Зона, часть, затем место внутри части.
    Part(usize, u32, InPart),
    Status,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum InPart {
    Tab(u32),
    Tool(usize),
    /// Тело: кнопка «Развернуть», затем строки сверху вниз (номер строки во входе).
    Body(usize),
}

/// Цели меток по узлам `ui.inspect`, не больше [`shellio_keymap::MAX_HINT_TARGETS`].
pub fn hint_targets(nodes: &[ComponentView]) -> Vec<HintTarget> {
    let by_id: HashMap<&str, &ComponentView> =
        nodes.iter().map(|node| (node.id.as_str(), node)).collect();
    let floating: Vec<&ComponentView> = nodes
        .iter()
        .filter(|node| node.visible && node.ck.as_deref().is_some_and(is_floating_root))
        .collect();
    let mut targets: Vec<(Place, HintTarget)> = nodes
        .iter()
        .enumerate()
        .filter(|(_, node)| node.visible && node.enabled != Some(false))
        .filter(|(_, node)| node.frame.width >= MIN_SIDE && node.frame.height >= MIN_SIDE)
        .filter_map(|(index, node)| place(node, index, &by_id).map(|place| (place, node)))
        .filter(|(_, node)| shown(node, &by_id))
        .filter(|(_, node)| !covered(node, &floating, &by_id))
        .map(|(place, node)| {
            let key = node.ck.clone().unwrap_or_else(|| node.id.clone());
            (
                place,
                HintTarget {
                    key,
                    id: node.id.clone(),
                    frame: node.frame,
                },
            )
        })
        .collect();
    targets.sort_by(|a, b| a.0.cmp(&b.0));
    targets
        .into_iter()
        .map(|(_, target)| target)
        .take(shellio_keymap::MAX_HINT_TARGETS)
        .collect()
}

fn is_floating_root(ck: &str) -> bool {
    FLOATING.contains(&ck) || (ck.split('.').nth(4) == Some("menu") && ck.split('.').count() == 5)
}

/// Место узла с действием; `None` — узел не цель.
fn place(
    node: &ComponentView,
    index: usize,
    by_id: &HashMap<&str, &ComponentView>,
) -> Option<Place> {
    if node.class == "ListRow" {
        let (zone, part) = owning_part(node, by_id)?;
        return Some(Place::Part(zone, part, InPart::Body(index)));
    }
    let ck = node.ck.as_deref()?;
    if let Some(n) = CHROME.iter().position(|key| *key == ck) {
        return Some(Place::Chrome(n));
    }
    if ck == "status.mode" {
        return Some(Place::Status);
    }
    let parts: Vec<&str> = ck.split('.').collect();
    let ["zone", zone, "part", part, rest @ ..] = parts.as_slice() else {
        return None;
    };
    let zone = ZONES.iter().position(|name| name == zone)?;
    let part: u32 = part.parse().ok()?;
    let in_part = match rest {
        ["tabs", "tab", n] => InPart::Tab(n.parse().ok()?),
        ["tabs", tool] => InPart::Tool(TOOLS.iter().position(|name| name == tool)?),
        ["body", "stub", "expand"] => InPart::Body(index),
        _ => return None,
    };
    Some(Place::Part(zone, part, in_part))
}

/// Зона и часть ближайшего предка с ключом `zone.<зона>.part.<n>`.
fn owning_part(
    node: &ComponentView,
    by_id: &HashMap<&str, &ComponentView>,
) -> Option<(usize, u32)> {
    ancestors(node, by_id).find_map(|parent| {
        let ck = parent.ck.as_deref()?;
        let parts: Vec<&str> = ck.split('.').collect();
        match parts.as_slice() {
            ["zone", zone, "part", part, ..] => Some((
                ZONES.iter().position(|name| name == zone)?,
                part.parse().ok()?,
            )),
            _ => None,
        }
    })
}

fn ancestors<'a>(
    node: &'a ComponentView,
    by_id: &'a HashMap<&str, &'a ComponentView>,
) -> impl Iterator<Item = &'a ComponentView> + 'a {
    let mut next = node.parent.as_deref();
    std::iter::from_fn(move || {
        let parent = by_id.get(next?).copied()?;
        next = parent.parent.as_deref();
        Some(parent)
    })
}

fn center(frame: &Rect) -> (f64, f64) {
    (frame.x + frame.width / 2.0, frame.y + frame.height / 2.0)
}

fn contains(frame: &Rect, (x, y): (f64, f64)) -> bool {
    x >= frame.x && x <= frame.x + frame.width && y >= frame.y && y <= frame.y + frame.height
}

/// Цель видна: все предки видимы, и середина цели внутри каждого предка с ненулевым кадром
/// (строка за краем прокрутки списка, вкладка за краем строки вкладок).
fn shown(node: &ComponentView, by_id: &HashMap<&str, &ComponentView>) -> bool {
    let middle = center(&node.frame);
    ancestors(node, by_id).all(|parent| {
        parent.visible
            && (parent.frame.width <= 0.0
                || parent.frame.height <= 0.0
                || contains(&parent.frame, middle))
    })
}

/// Середину цели закрывает плавающая поверхность, которая ей не предок.
fn covered(
    node: &ComponentView,
    floating: &[&ComponentView],
    by_id: &HashMap<&str, &ComponentView>,
) -> bool {
    let middle = center(&node.frame);
    floating.iter().any(|surface| {
        surface.id != node.id
            && contains(&surface.frame, middle)
            && !ancestors(node, by_id).any(|parent| parent.id == surface.id)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, ck: Option<&str>, parent: Option<&str>, frame: [f64; 4]) -> ComponentView {
        ComponentView {
            id: id.to_owned(),
            class: if id.contains(".row.") {
                "ListRow"
            } else {
                "View"
            }
            .to_owned(),
            object: 0,
            parent: parent.map(str::to_owned),
            ck: ck.map(str::to_owned),
            frame: Rect {
                x: frame[0],
                y: frame[1],
                width: frame[2],
                height: frame[3],
            },
            visible: true,
            first_responder: false,
            enabled: None,
            role: None,
            label: None,
            help: None,
            selected: None,
            expanded: None,
            animation_keys: 0,
            style: None,
            truncated: None,
            state: None,
        }
    }

    /// Окно p15 в сокращении: узлы идут не в порядке документа.
    fn window() -> Vec<ComponentView> {
        vec![
            node(
                "status.mode",
                Some("status.mode"),
                Some("status"),
                [0.0, 0.0, 112.0, 25.0],
            ),
            node("status", Some("status"), None, [0.0, 0.0, 1180.0, 25.0]),
            node(
                "right",
                Some("zone.right.part.1"),
                None,
                [800.0, 30.0, 380.0, 640.0],
            ),
            node(
                "right.tab",
                Some("zone.right.part.1.tabs.tab.1"),
                Some("right"),
                [807.0, 644.0, 100.0, 26.0],
            ),
            node(
                "right.body",
                Some("zone.right.part.1.body"),
                Some("right"),
                [803.0, 33.0, 370.0, 606.0],
            ),
            node(
                "right.list",
                None,
                Some("right.body"),
                [803.0, 33.0, 370.0, 606.0],
            ),
            node(
                "right.list.row.b",
                None,
                Some("right.list"),
                [803.0, 587.0, 370.0, 26.0],
            ),
            node(
                "right.list.row.a",
                None,
                Some("right.list"),
                [803.0, 613.0, 370.0, 26.0],
            ),
            node(
                "right.list.row.z",
                None,
                Some("right.list"),
                [803.0, 7.0, 370.0, 26.0],
            ),
            node(
                "center",
                Some("zone.center.part.1"),
                None,
                [240.0, 270.0, 560.0, 400.0],
            ),
            node(
                "center.menu",
                Some("zone.center.part.1.tabs.menu"),
                Some("center"),
                [767.0, 646.0, 22.0, 22.0],
            ),
            node(
                "center.split",
                Some("zone.center.part.1.tabs.split"),
                Some("center"),
                [719.0, 646.0, 22.0, 22.0],
            ),
            node(
                "center.tab2",
                Some("zone.center.part.1.tabs.tab.2"),
                Some("center"),
                [363.0, 644.0, 129.0, 26.0],
            ),
            node(
                "center.tab1",
                Some("zone.center.part.1.tabs.tab.1"),
                Some("center"),
                [241.0, 644.0, 120.0, 26.0],
            ),
            node(
                "center.close",
                Some("zone.center.part.1.tabs.tab.1.close"),
                Some("center"),
                [339.0, 648.0, 16.0, 16.0],
            ),
            node(
                "bottom",
                Some("zone.bottom.part.1"),
                None,
                [240.0, 30.0, 560.0, 240.0],
            ),
            node(
                "bottom.tab",
                Some("zone.bottom.part.1.tabs.tab.1"),
                Some("bottom"),
                [241.0, 238.0, 120.0, 26.0],
            ),
            node(
                "chrome.appearance",
                Some("chrome.appearance"),
                None,
                [1142.0, 688.0, 28.0, 24.0],
            ),
            node(
                "chrome.window.close",
                Some("chrome.window.close"),
                None,
                [9.0, 693.0, 14.0, 14.0],
            ),
            node(
                "chrome.search",
                Some("chrome.search"),
                None,
                [360.0, 687.0, 460.0, 26.0],
            ),
        ]
    }

    fn keys(nodes: &[ComponentView]) -> Vec<String> {
        hint_targets(nodes)
            .into_iter()
            .map(|target| target.key)
            .collect()
    }

    #[test]
    fn targets_follow_the_document_order_of_the_prototype() {
        assert_eq!(
            keys(&window()),
            [
                "chrome.window.close",
                "chrome.search",
                "chrome.appearance",
                "zone.center.part.1.tabs.tab.1",
                "zone.center.part.1.tabs.tab.2",
                "zone.center.part.1.tabs.split",
                "zone.center.part.1.tabs.menu",
                "zone.bottom.part.1.tabs.tab.1",
                "zone.right.part.1.tabs.tab.1",
                // Строки — по порядку узлов `ui.inspect`, ключа сверки у них нет.
                "right.list.row.b",
                "right.list.row.a",
                "status.mode",
            ]
        );
    }

    #[test]
    fn hidden_clipped_and_disabled_targets_get_no_label() {
        let mut nodes = window();
        for node in &mut nodes {
            match node.id.as_str() {
                "center.split" => node.enabled = Some(false),
                "bottom" => node.visible = false,
                _ => {}
            }
        }
        // Панель невыбранной вкладки скрыта (`visible: false`, без ключа), её строки скрыты с ней.
        let mut hidden = node(
            "right.other",
            None,
            Some("right"),
            [803.0, 33.0, 370.0, 606.0],
        );
        hidden.visible = false;
        nodes.push(hidden);
        nodes.push(node(
            "right.other.row.q",
            None,
            Some("right.other"),
            [803.0, 613.0, 370.0, 26.0],
        ));
        let keys = keys(&nodes);
        assert!(!keys.iter().any(|key| key == "right.other.row.q"));
        // Кнопка невыбранной вкладки остаётся целью, как в прототипе.
        assert!(
            keys.iter()
                .any(|key| key == "zone.center.part.1.tabs.tab.2")
        );
        assert!(!keys.iter().any(|key| key.contains("split")));
        assert!(!keys.iter().any(|key| key.starts_with("zone.bottom")));
        // Строка за нижним краем списка (y 7 при начале списка 33) меток не получает.
        assert!(!keys.iter().any(|key| key == "right.list.row.z"));
        // Крестик вкладки — не цель.
        assert!(!keys.iter().any(|key| key.ends_with("tab.1.close")));
    }

    #[test]
    fn floating_surfaces_cover_targets_under_them() {
        let mut nodes = window();
        nodes.push(node(
            "menu",
            Some("zone.center.part.1.menu"),
            None,
            [600.0, 400.0, 200.0, 260.0],
        ));
        nodes.push(node(
            "menu.item",
            Some("zone.center.part.1.menu.item.1"),
            Some("menu"),
            [600.0, 600.0, 200.0, 26.0],
        ));
        let keys = keys(&nodes);
        assert!(!keys.iter().any(|key| key == "zone.center.part.1.tabs.menu"));
        assert!(
            keys.iter()
                .any(|key| key == "zone.center.part.1.tabs.tab.1")
        );
    }

    #[test]
    fn targets_stop_at_the_label_limit() {
        let mut nodes = vec![node(
            "body",
            Some("zone.right.part.1.body"),
            None,
            [0.0, 0.0, 0.0, 0.0],
        )];
        nodes.extend((0..300).map(|n| {
            node(
                &format!("list.row.{n}"),
                None,
                Some("body"),
                [0.0, f64::from(n) * 26.0, 300.0, 26.0],
            )
        }));
        let targets = hint_targets(&nodes);
        assert_eq!(targets.len(), shellio_keymap::MAX_HINT_TARGETS);
        assert_eq!(targets[0].id, "list.row.0");
    }
}
