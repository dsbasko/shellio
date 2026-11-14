//! Поиск системного материала во всех окнах приложения. Обход идёт от `contentView.superview`
//! по видам и по дереву слоёв в порядке отрисовки. Находка — вид или слой, в цепочке классов
//! которого есть `NSVisualEffectView`, `NSGlassEffectView`, `NSGlassEffectContainerView`,
//! `NSScrollPocket`, `CABackdropLayer` или класс с `Backdrop` в имени. Для каждой находки
//! записываются класс, кадр, скрытость, alpha и перекрыта ли она непрозрачным видом сцены,
//! который рисуется позже. Само присутствие класса ничего не доказывает (Context плана), поэтому
//! итог варианта строится как разница с пустым окном той же конфигурации.

use std::collections::{HashMap, HashSet};

use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2_app_kit::{NSApplication, NSView, NSWindow};
use objc2_quartz_core::CALayer;
use serde::Serialize;

use super::readback;
use crate::results::Rect;

const MATERIAL_CLASSES: [&str; 5] = [
    "NSVisualEffectView",
    "NSGlassEffectView",
    "NSGlassEffectContainerView",
    "NSScrollPocket",
    "CABackdropLayer",
];
/// Префикс имён классов сцены пробы.
const SCENE_PREFIX: &str = "Probe";
const EDGE_SLACK: f64 = 0.5;

/// Класс из цепочки, по которому узел считается материалом.
pub fn material_class(chain: &[String]) -> Option<&str> {
    chain
        .iter()
        .find(|class| MATERIAL_CLASSES.contains(&class.as_str()) || class.contains("Backdrop"))
        .map(String::as_str)
}

/// Узел обхода в порядке отрисовки: каждый следующий рисуется поверх предыдущих.
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    /// Класс и его суперклассы, начиная с самого класса.
    pub chain: Vec<String>,
    /// Кадр в координатах окна, в точках. У слоя без своего вида — кадр вида-владельца.
    pub frame: Rect,
    /// Скрыт сам узел или его предок.
    pub hidden: bool,
    /// Итоговая непрозрачность с учётом предков.
    pub alpha: f64,
    pub layer: bool,
    /// Непрозрачный видимый вид сцены пробы: перекрывает то, что под ним.
    pub cover: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    Visible,
    /// Перекрыт непрозрачным видом сцены.
    Covered,
    Hidden,
    /// Пересечение с окном пусто.
    ZeroSize,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Found {
    pub window: isize,
    pub window_class: String,
    pub class: String,
    pub matched: String,
    pub layer: bool,
    pub frame: Rect,
    pub hidden: bool,
    pub alpha: f64,
    pub covered_by: Option<String>,
    pub state: State,
}

impl Found {
    /// Ключ сравнения с пустым окном: класс, кадр с точностью до половины точки и вид узла.
    fn key(&self) -> (String, String, bool, [i64; 4]) {
        let half = |value: f64| (value * 2.0).round() as i64;
        (
            self.matched.clone(),
            self.class.clone(),
            self.layer,
            [
                half(self.frame.x),
                half(self.frame.y),
                half(self.frame.width),
                half(self.frame.height),
            ],
        )
    }
}

fn intersect(a: Rect, b: Rect) -> Option<Rect> {
    let x0 = a.x.max(b.x);
    let y0 = a.y.max(b.y);
    let x1 = (a.x + a.width).min(b.x + b.width);
    let y1 = (a.y + a.height).min(b.y + b.height);
    (x1 - x0 > EDGE_SLACK && y1 - y0 > EDGE_SLACK).then_some(Rect {
        x: x0,
        y: y0,
        width: x1 - x0,
        height: y1 - y0,
    })
}

fn encloses(outer: Rect, inner: Rect) -> bool {
    outer.x <= inner.x + EDGE_SLACK
        && outer.y <= inner.y + EDGE_SLACK
        && outer.x + outer.width >= inner.x + inner.width - EDGE_SLACK
        && outer.y + outer.height >= inner.y + inner.height - EDGE_SLACK
}

/// Находки окна: узлы-материалы и их состояние в окне с границами `bounds`.
pub fn classify(window: isize, window_class: &str, bounds: Rect, nodes: &[Node]) -> Vec<Found> {
    nodes
        .iter()
        .enumerate()
        .filter_map(|(index, node)| {
            let matched = material_class(&node.chain)?.to_owned();
            let visible_part = intersect(node.frame, bounds);
            let covered_by = visible_part.and_then(|part| {
                nodes[index + 1..]
                    .iter()
                    .find(|later| later.cover && encloses(later.frame, part))
                    .map(|later| later.chain.first().cloned().unwrap_or_default())
            });
            let state = if node.hidden || node.alpha <= 0.001 {
                State::Hidden
            } else if visible_part.is_none() {
                State::ZeroSize
            } else if covered_by.is_some() {
                State::Covered
            } else {
                State::Visible
            };
            Some(Found {
                window,
                window_class: window_class.to_owned(),
                class: node.chain.first().cloned().unwrap_or_default(),
                matched,
                layer: node.layer,
                frame: node.frame,
                hidden: node.hidden,
                alpha: node.alpha,
                covered_by,
                state,
            })
        })
        .collect()
}

/// Итог варианта по находкам, которых нет в пустом окне.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    VisibleMaterial,
    HiddenOnly,
    None,
}

impl Outcome {
    pub fn describe(self) -> &'static str {
        match self {
            Outcome::VisibleMaterial => "видимый материал",
            Outcome::HiddenOnly => "только скрытый",
            Outcome::None => "нет",
        }
    }

    pub fn worst(self, other: Outcome) -> Outcome {
        let rank = |outcome: Outcome| match outcome {
            Outcome::None => 0,
            Outcome::HiddenOnly => 1,
            Outcome::VisibleMaterial => 2,
        };
        if rank(other) > rank(self) {
            other
        } else {
            self
        }
    }
}

pub fn outcome<'a>(found: impl IntoIterator<Item = &'a Found>) -> Outcome {
    found
        .into_iter()
        .map(|found| match found.state {
            State::Visible => Outcome::VisibleMaterial,
            State::Covered | State::Hidden | State::ZeroSize => Outcome::HiddenOnly,
        })
        .fold(Outcome::None, Outcome::worst)
}

/// Находки сцены, которых нет среди находок пустого окна. Видимую находку сцены вычитает только
/// видимая находка пустого окна: материал, скрытый или перекрытый в пустом окне и видимый в сцене,
/// остаётся в разнице.
pub fn difference<'a>(scene: &'a [Found], baseline: &[Found]) -> Vec<&'a Found> {
    let mut known: HashMap<_, bool> = HashMap::new();
    for found in baseline {
        *known.entry(found.key()).or_default() |= found.state == State::Visible;
    }
    scene
        .iter()
        .filter(|found| match known.get(&found.key()) {
            None => true,
            Some(&baseline_visible) => found.state == State::Visible && !baseline_visible,
        })
        .collect()
}

/// Обход одного окна.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct WindowScan {
    pub number: isize,
    pub class: String,
    pub frame: Rect,
    pub visible: bool,
    pub nodes: usize,
    pub found: Vec<Found>,
}

pub fn scan_app(app: &NSApplication) -> Vec<WindowScan> {
    app.windows()
        .iter()
        .map(|window| scan_window(&window))
        .collect()
}

pub fn scan_window(window: &NSWindow) -> WindowScan {
    let object: &AnyObject = window;
    let class = readback::class_name(object);
    let content = window.contentView();
    // SAFETY: `superview` у вида, который держит окно.
    let root = content
        .as_ref()
        .and_then(|content| unsafe { content.superview() })
        .or(content);
    let mut nodes = Vec::new();
    let mut bounds = Rect {
        x: 0.0,
        y: 0.0,
        width: 0.0,
        height: 0.0,
    };
    if let Some(root) = &root {
        bounds = readback::frame_in_window(root);
        let mut view_layers = HashSet::new();
        collect_view_layers(root, &mut view_layers);
        walk(root, false, 1.0, &view_layers, &mut nodes);
    }
    WindowScan {
        number: window.windowNumber(),
        found: classify(window.windowNumber(), &class, bounds, &nodes),
        class,
        frame: readback::rect(window.frame()),
        visible: window.isVisible(),
        nodes: nodes.len(),
    }
}

/// Класс объекта по `-class` и его суперклассы.
fn chain(object: &AnyObject) -> Vec<String> {
    // SAFETY: `-class` есть у каждого NSObject и возвращает класс.
    let class: &'static AnyClass = unsafe { msg_send![object, class] };
    let mut chain = Vec::new();
    let mut current = Some(class);
    while let Some(class) = current {
        chain.push(class.name().to_string_lossy().into_owned());
        current = class.superclass();
    }
    chain
}

fn collect_view_layers(view: &NSView, layers: &mut HashSet<*const CALayer>) {
    if let Some(layer) = view.layer() {
        layers.insert(Retained::as_ptr(&layer));
    }
    for subview in view.subviews().iter() {
        collect_view_layers(&subview, layers);
    }
}

fn walk(
    view: &NSView,
    parent_hidden: bool,
    parent_alpha: f64,
    view_layers: &HashSet<*const CALayer>,
    nodes: &mut Vec<Node>,
) {
    let frame = readback::frame_in_window(view);
    let hidden = parent_hidden || view.isHidden();
    let layer = view.layer();
    let opacity = layer
        .as_ref()
        .map_or(1.0, |layer| f64::from(layer.opacity()));
    let alpha = parent_alpha * view.alphaValue() * opacity;
    let view_chain = chain(view);
    let opaque = layer
        .as_ref()
        .and_then(|layer| layer.backgroundColor())
        .and_then(|color| readback::cg_srgb(&color))
        .is_some_and(|color| color.a == 255);
    let scene_view = view_chain
        .first()
        .is_some_and(|class| class.starts_with(SCENE_PREFIX));
    nodes.push(Node {
        chain: view_chain,
        frame,
        hidden,
        alpha,
        layer: false,
        cover: scene_view && opaque && !hidden && alpha >= 0.999,
    });
    if let Some(layer) = &layer {
        let own = chain(layer);
        if material_class(&own).is_some() {
            nodes.push(Node {
                chain: own,
                frame,
                hidden: hidden || layer.isHidden(),
                alpha,
                layer: true,
                cover: false,
            });
        }
        walk_sublayers(layer, frame, hidden, alpha, view_layers, nodes);
    }
    for subview in view.subviews().iter() {
        walk(&subview, hidden, alpha, view_layers, nodes);
    }
}

/// Подслои без своего вида; их кадр приближается кадром вида-владельца.
fn walk_sublayers(
    layer: &CALayer,
    frame: Rect,
    hidden: bool,
    alpha: f64,
    view_layers: &HashSet<*const CALayer>,
    nodes: &mut Vec<Node>,
) {
    // SAFETY: массив подслоёв читается на главном потоке, где живёт дерево слоёв.
    let Some(sublayers) = (unsafe { layer.sublayers() }) else {
        return;
    };
    for sublayer in sublayers.iter() {
        if view_layers.contains(&Retained::as_ptr(&sublayer)) {
            continue;
        }
        let hidden = hidden || sublayer.isHidden();
        let alpha = alpha * f64::from(sublayer.opacity());
        nodes.push(Node {
            chain: chain(&sublayer),
            frame,
            hidden,
            alpha,
            layer: true,
            cover: false,
        });
        walk_sublayers(&sublayer, frame, hidden, alpha, view_layers, nodes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, width: f64, height: f64) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    fn node(chain: &[&str], frame: Rect) -> Node {
        Node {
            chain: chain.iter().map(|class| (*class).to_owned()).collect(),
            frame,
            hidden: false,
            alpha: 1.0,
            layer: false,
            cover: false,
        }
    }

    fn cover(frame: Rect) -> Node {
        Node {
            cover: true,
            ..node(&["ProbeTile", "NSView"], frame)
        }
    }

    const BOUNDS: Rect = Rect {
        x: 0.0,
        y: 0.0,
        width: 100.0,
        height: 100.0,
    };

    #[test]
    fn subclasses_and_backdrop_names_match() {
        let private = ["NSPrivateSidebarView", "NSVisualEffectView", "NSView"].map(String::from);
        assert_eq!(material_class(&private), Some("NSVisualEffectView"));
        let backdrop = ["_NSFooBackdropView", "NSView"].map(String::from);
        assert_eq!(material_class(&backdrop), Some("_NSFooBackdropView"));
        let plain = ["NSView", "NSResponder", "NSObject"].map(String::from);
        assert_eq!(material_class(&plain), None);
    }

    #[test]
    fn states_follow_visibility_size_and_later_cover() {
        let material = || {
            node(
                &["NSVisualEffectView", "NSView"],
                rect(0.0, 0.0, 50.0, 100.0),
            )
        };
        let visible = classify(1, "NSWindow", BOUNDS, &[material()]);
        assert_eq!(visible[0].state, State::Visible);

        let hidden = Node {
            hidden: true,
            ..material()
        };
        assert_eq!(classify(1, "w", BOUNDS, &[hidden])[0].state, State::Hidden);
        let transparent = Node {
            alpha: 0.0,
            ..material()
        };
        assert_eq!(
            classify(1, "w", BOUNDS, &[transparent])[0].state,
            State::Hidden
        );
        let zero = node(&["NSVisualEffectView"], rect(10.0, 10.0, 0.0, 0.0));
        assert_eq!(classify(1, "w", BOUNDS, &[zero])[0].state, State::ZeroSize);
        let outside = node(&["NSVisualEffectView"], rect(200.0, 0.0, 10.0, 10.0));
        assert_eq!(
            classify(1, "w", BOUNDS, &[outside])[0].state,
            State::ZeroSize
        );

        let covered = classify(
            1,
            "w",
            BOUNDS,
            &[material(), cover(rect(0.0, 0.0, 60.0, 100.0))],
        );
        assert_eq!(covered[0].state, State::Covered);
        assert_eq!(covered[0].covered_by.as_deref(), Some("ProbeTile"));
        // Вид, нарисованный раньше, материал не перекрывает.
        let below = classify(1, "w", BOUNDS, &[cover(BOUNDS), material()]);
        assert_eq!(below[0].state, State::Visible);
        // Частичное перекрытие оставляет материал видимым.
        let partial = classify(
            1,
            "w",
            BOUNDS,
            &[material(), cover(rect(0.0, 0.0, 30.0, 100.0))],
        );
        assert_eq!(partial[0].state, State::Visible);
        // Часть за пределами окна не мешает перекрытию видимой части.
        let wide = node(&["NSVisualEffectView"], rect(-20.0, 0.0, 70.0, 100.0));
        let clipped = classify(1, "w", BOUNDS, &[wide, cover(rect(0.0, 0.0, 50.0, 100.0))]);
        assert_eq!(clipped[0].state, State::Covered);
    }

    #[test]
    fn outcome_and_difference_with_baseline() {
        let found = |class: &str, x: f64, state: State| Found {
            window: 1,
            window_class: "NSWindow".to_owned(),
            class: class.to_owned(),
            matched: "NSVisualEffectView".to_owned(),
            layer: false,
            frame: rect(x, 0.0, 10.0, 10.0),
            hidden: state == State::Hidden,
            alpha: 1.0,
            covered_by: None,
            state,
        };
        let baseline = vec![found("NSVisualEffectView", 0.0, State::Hidden)];
        let scene = vec![
            found("NSVisualEffectView", 0.0, State::Hidden),
            found("NSVisualEffectView", 40.0, State::Covered),
        ];
        let diff = difference(&scene, &baseline);
        assert_eq!(diff.len(), 1);
        assert_eq!(diff[0].frame.x, 40.0);
        assert_eq!(outcome(diff), Outcome::HiddenOnly);
        assert_eq!(outcome(difference(&baseline, &baseline)), Outcome::None);
        let visible = vec![found("NSVisualEffectView", 40.0, State::Visible)];
        assert_eq!(
            outcome(difference(&visible, &baseline)),
            Outcome::VisibleMaterial
        );
        // Скрытый или перекрытый в пустом окне материал, видимый в сцене, остаётся в разнице.
        for hidden in [State::Hidden, State::Covered, State::ZeroSize] {
            let baseline = vec![found("NSVisualEffectView", 0.0, hidden)];
            let shown = vec![found("NSVisualEffectView", 0.0, State::Visible)];
            let diff = difference(&shown, &baseline);
            assert_eq!(diff.len(), 1);
            assert_eq!(outcome(diff), Outcome::VisibleMaterial);
        }
        // Видимый в пустом окне материал вычитается в любом состоянии сцены.
        let shown = vec![found("NSVisualEffectView", 0.0, State::Visible)];
        for state in [State::Visible, State::Hidden, State::Covered] {
            let scene = vec![found("NSVisualEffectView", 0.0, state)];
            assert_eq!(outcome(difference(&scene, &shown)), Outcome::None);
        }
        // Тот же ключ в пустом окне и скрытым, и видимым: видимая находка сцены вычитается.
        let mixed = vec![
            found("NSVisualEffectView", 0.0, State::Hidden),
            found("NSVisualEffectView", 0.0, State::Visible),
        ];
        assert_eq!(outcome(difference(&shown, &mixed)), Outcome::None);
        assert_eq!(
            Outcome::HiddenOnly.worst(Outcome::None),
            Outcome::HiddenOnly
        );
        assert_eq!(
            Outcome::HiddenOnly.worst(Outcome::VisibleMaterial),
            Outcome::VisibleMaterial
        );
    }
}
