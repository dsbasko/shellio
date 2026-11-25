//! Режим сверки оракула (ADR-0037, «Режим сверки оракула»): паспорт прототипа против дерева
//! `ui.inspect` продукта.
//!
//! Сначала сверяется состав ([`composition`]), затем элементы паспорта сопоставляются с узлами
//! продукта по ключу `ck` и судятся по контракту полей (`shellio_conformance::fields`). Поля,
//! которые судит снимок, ядро не решает: оно перечисляет их в [`Report::snapshot`], и проходит их
//! стадия снимка. Кадры продукта переводятся в координаты паспорта: начало сверху слева.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use shellio_conformance::Zone;
use shellio_conformance::fields::{self, Applicability, ElementKind, Field, Judgement};
use shellio_conformance::keys::glob_matches;
use shellio_conformance::mask::{ICON_FG, Mask, PixelClip, backdrop, foreground};
use shellio_conformance::passport::{Element, Passport, Rect, Rgba, Shadow, Side};
use shellio_conformance::registry::{Exception, Registry, Surface};
use shellio_conformance::scene::{BodyMode, Composition, MenuItem, PartComposition, PartRef};

use crate::Error;
use crate::expect::Rect as ViewRect;

/// Допуск кадра в точках.
pub const FRAME_TOLERANCE: f64 = 1.0;
/// Допуск межстрочного интервала и разрядки в точках.
pub const LINE_TOLERANCE: f64 = 0.5;
/// Допуск цвета на канал sRGB.
pub const CHANNEL_TOLERANCE: u8 = 3;
/// Семейство системного шрифта: имя прототипа и имя продукта.
pub const SYSTEM_FAMILY: (&str, &str) = ("-apple-system", ".AppleSystemUIFont");
/// Каталог вложенных шрифтов в бандле продукта.
pub const BUNDLED_FONTS: &str = "/Contents/Resources/Fonts/";
/// Класс узла подписи `NSButton`: его кадр — место заголовка в ячейке, а не набор текста.
pub const BUTTON_TITLE: &str = "ButtonTitle";

/// Допуски калибровки.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Tolerances {
    /// Допуск ширины, заданной текстом, в точках.
    pub text_width: f64,
    /// Порог пересечения к объединению масок значков.
    pub mask_iou: f64,
}

/// Шрифт узла продукта.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ProductFont {
    pub family: String,
    pub face: String,
    pub weight: f64,
    pub size: f64,
    #[serde(default)]
    pub file: Option<String>,
}

/// Значок узла продукта.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ProductIcon {
    pub name: String,
    pub size: f64,
}

/// Свойства вида узла продукта (`style` в `ui.inspect`).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ProductStyle {
    #[serde(default)]
    pub font: Option<ProductFont>,
    #[serde(default)]
    pub line_height: Option<f64>,
    #[serde(default)]
    pub letter_spacing: Option<f64>,
    #[serde(default)]
    pub color: Option<Rgba>,
    #[serde(default)]
    pub fill: Option<Rgba>,
    #[serde(default)]
    pub line_color: Option<Rgba>,
    #[serde(default)]
    pub line_width: Option<f64>,
    #[serde(default)]
    pub line_sides: Vec<Side>,
    #[serde(default)]
    pub radius: Option<f64>,
    #[serde(default)]
    pub shadow: Vec<Shadow>,
    #[serde(default)]
    pub icon: Option<ProductIcon>,
}

#[derive(Debug, Clone, Deserialize)]
struct TabView {
    kind: String,
    id: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ButtonView {
    id: String,
}

#[derive(Debug, Clone, Deserialize)]
struct TabStripView {
    #[serde(default)]
    selected: Option<String>,
    tabs: Vec<TabView>,
    #[serde(default)]
    buttons: Vec<ButtonView>,
}

#[derive(Debug, Clone, Deserialize)]
struct PartView {
    #[serde(default)]
    focused: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct MenuItemView {
    #[serde(default)]
    separator: bool,
    #[serde(default)]
    title: String,
    #[serde(default)]
    enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct MenuView {
    #[serde(default)]
    open: bool,
    #[serde(default)]
    items: Vec<MenuItemView>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct StateView {
    #[serde(default)]
    tab_strip: Option<TabStripView>,
    #[serde(default)]
    part: Option<PartView>,
    #[serde(default)]
    menu: Option<MenuView>,
}

/// Узел `ui.inspect` с полями сверки.
#[derive(Debug, Clone, Deserialize)]
pub struct Node {
    pub id: String,
    #[serde(default)]
    pub class: String,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub ck: Option<String>,
    /// Кадр в точках окна, начало внизу слева.
    pub frame: ViewRect,
    pub visible: bool,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub style: Option<ProductStyle>,
    #[serde(default)]
    pub truncated: Option<bool>,
    #[serde(default)]
    state: Option<StateView>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
struct ContentSize {
    height: f64,
}

/// Дерево `ui.inspect` продукта.
#[derive(Debug, Clone, Deserialize)]
pub struct Product {
    content: ContentSize,
    pub components: Vec<Node>,
}

impl Product {
    /// Кадр `shellioctl --json` целиком или само тело ответа `ui.inspect`.
    pub fn parse(text: &str) -> Result<Product, Error> {
        let value: serde_json::Value = serde_json::from_str(text)
            .map_err(|e| Error::new(format!("ui.inspect: не JSON: {e}")))?;
        let body = value.pointer("/ok/ui/inspect").cloned().unwrap_or(value);
        serde_json::from_value(body).map_err(|e| Error::new(format!("ui.inspect: {e}")))
    }

    fn node(&self, id: &str) -> Option<&Node> {
        self.components.iter().find(|n| n.id == id)
    }

    /// Узел с ключом сверки.
    pub fn keyed(&self, ck: &str) -> Option<&Node> {
        self.components.iter().find(|n| n.ck.as_deref() == Some(ck))
    }

    /// Ключ ближайшего предка с ключом.
    fn parent_key(&self, node: &Node) -> Option<&str> {
        let mut current = node.parent.as_deref().and_then(|p| self.node(p));
        for _ in 0..self.components.len() {
            let parent = current?;
            if let Some(ck) = parent.ck.as_deref() {
                return Some(ck);
            }
            current = parent.parent.as_deref().and_then(|p| self.node(p));
        }
        None
    }

    /// Кадр узла в координатах паспорта: начало сверху слева.
    pub fn frame(&self, node: &Node) -> Rect {
        Rect {
            x: node.frame.x,
            y: self.content.height - node.frame.y - node.frame.height,
            w: node.frame.width,
            h: node.frame.height,
        }
    }
}

/// Состав продукта по `ui.inspect` в пределах поверхности состояния.
pub fn composition(product: &Product, titlebar: &[String], statusline: &[String]) -> Composition {
    let shown = |ck: &str| product.keyed(ck).is_some_and(|n| n.visible);
    let mut zones = BTreeMap::new();
    let mut focus = None;
    for zone in Zone::ALL {
        let mut parts = Vec::new();
        for index in 1.. {
            let key = format!("zone.{zone}.part.{index}");
            let Some(part) = product.keyed(&key).filter(|n| n.visible) else {
                break;
            };
            let state = part.state.clone().unwrap_or_default();
            if state.part.is_some_and(|p| p.focused) {
                focus = Some(PartRef { zone, part: index });
            }
            let strip = product
                .keyed(&format!("{key}.tabs"))
                .and_then(|n| n.state.as_ref()?.tab_strip.clone());
            let body = if shown(&format!("{key}.body.stub")) {
                BodyMode::Stub
            } else if shown(&format!("{key}.body")) || strip.is_none() {
                BodyMode::Ready
            } else {
                BodyMode::Empty
            };
            parts.push(match strip {
                Some(strip) => PartComposition {
                    active: strip.selected.as_ref().and_then(|selected| {
                        strip
                            .tabs
                            .iter()
                            .position(|t| &t.id == selected)
                            .map(|i| i + 1)
                    }),
                    tabs: strip.tabs.into_iter().map(|t| t.kind).collect(),
                    body,
                    tools: strip.buttons.into_iter().map(|b| b.id).collect(),
                },
                // Колонка навигатора рисуется без строки вкладок.
                None => PartComposition {
                    tabs: vec![shellio_conformance::scene::COLUMN_PLUGIN.to_owned()],
                    active: Some(1),
                    body,
                    tools: Vec::new(),
                },
            });
        }
        if !parts.is_empty() {
            zones.insert(zone, parts);
        }
    }
    let menu = product.components.iter().find_map(|n| {
        let menu = n.state.as_ref()?.menu.as_ref().filter(|m| m.open)?;
        Some(
            menu.items
                .iter()
                .filter(|i| !i.separator)
                .map(|i| MenuItem {
                    title: i.title.clone(),
                    enabled: i.enabled,
                })
                .collect(),
        )
    });
    let controls = |wanted: &[String]| {
        let mut found: Vec<(f64, String)> = wanted
            .iter()
            .filter_map(|ck| {
                let node = product.keyed(ck).filter(|n| n.visible)?;
                Some((node.frame.x, ck.clone()))
            })
            .collect();
        found.sort_by(|a, b| a.0.total_cmp(&b.0));
        found.into_iter().map(|(_, ck)| ck).collect()
    };
    Composition {
        zones,
        focus,
        menu,
        titlebar: controls(titlebar),
        statusline: controls(statusline),
    }
}

/// Расхождение: ключ, поле, ожидание, факт и разница.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Mismatch {
    pub ck: String,
    pub field: Field,
    pub expected: String,
    pub actual: String,
    /// Разница в единицах поля, если она числовая.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub difference: Option<f64>,
}

/// Что судит стадия снимка.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotKind {
    /// Заливка участка по кадру паспорта: псевдоэлемент, вуаль, системная кнопка окна.
    Region,
    /// Цвет значка, который несёт цвет в картинке.
    IconColor,
    /// Силуэт значка маской.
    Mask,
    /// Линия под активной вкладкой.
    Underline,
    /// Начало и ширина подписи кнопки по чернилам.
    ButtonTitleInk,
}

/// Поле, которое судит стадия снимка.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SnapshotCheck {
    pub ck: String,
    pub field: Field,
    pub kind: SnapshotKind,
}

/// Итог ядра сверки.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Report {
    pub mismatches: Vec<Mismatch>,
    pub snapshot: Vec<SnapshotCheck>,
}

/// Исключения реестра, действующие в состоянии.
pub struct Exceptions<'a> {
    rows: Vec<&'a Exception>,
    /// Ключи тел частей по области «плагин и режим тела»: ключ тела и исключение.
    bodies: Vec<(String, &'a Exception)>,
}

impl<'a> Exceptions<'a> {
    pub fn new(registry: &'a Registry, state: &str, composition: &Composition) -> Self {
        let rows: Vec<&Exception> = registry
            .exceptions
            .iter()
            .filter(|e| e.states.iter().any(|s| s == state))
            .collect();
        let mut bodies = Vec::new();
        for (zone, parts) in &composition.zones {
            for (index, part) in parts.iter().enumerate() {
                let active = part.active.and_then(|a| part.tabs.get(a.checked_sub(1)?));
                for row in &rows {
                    let plugin = match (&row.scope.plugin, active) {
                        (None, _) => row.scope.body.is_some(),
                        (Some(pattern), Some(plugin)) => match pattern.strip_suffix('*') {
                            Some(prefix) => plugin.starts_with(prefix),
                            None => pattern == plugin,
                        },
                        (Some(_), None) => false,
                    };
                    if plugin && row.scope.body.is_none_or(|body| body == part.body) {
                        bodies.push((format!("zone.{zone}.part.{}.body", index + 1), *row));
                    }
                }
            }
        }
        Self { rows, bodies }
    }

    /// Покрыто ли поле элемента исключением: само поле или `all`.
    pub fn covers(&self, ck: &str, field: Field) -> bool {
        let lists = |e: &Exception| e.fields.contains(&field) || e.fields.contains(&Field::All);
        self.rows
            .iter()
            .any(|e| lists(e) && e.scope.keys.iter().any(|p| glob_matches(p, ck)))
            || self.bodies.iter().any(|(body, e)| {
                // Содержимое тела — всё под ключом тела; само тело сверяется.
                lists(e)
                    && ck
                        .strip_prefix(body.as_str())
                        .is_some_and(|r| r.starts_with('.'))
            })
    }
}

struct Judge<'a> {
    product: &'a Product,
    passport: &'a Passport,
    exceptions: &'a Exceptions<'a>,
    tolerances: Tolerances,
    report: Report,
}

fn rgba_text(c: Rgba) -> String {
    format!("({}, {}, {}, {:.2})", c.r, c.g, c.b, c.a)
}

fn rect_text(r: Rect) -> String {
    format!("({}, {}, {}×{})", r.x, r.y, r.w, r.h)
}

/// Наибольшая разница цвета по каналам, альфа — в тех же единицах.
fn color_gap(a: Rgba, b: Rgba) -> f64 {
    [
        f64::from(a.r.abs_diff(b.r)),
        f64::from(a.g.abs_diff(b.g)),
        f64::from(a.b.abs_diff(b.b)),
        ((a.a - b.a) * 255.0).abs(),
    ]
    .into_iter()
    .fold(0.0, f64::max)
}

/// Однострочный текст: кадр ниже двух межстрочных интервалов.
fn single_line_text(element: &Element) -> bool {
    element.font.is_some()
        && element
            .line_height
            .is_some_and(|line| element.frame.h < 2.0 * line)
}

impl Judge<'_> {
    fn fail(
        &mut self,
        ck: &str,
        field: Field,
        expected: String,
        actual: String,
        diff: Option<f64>,
    ) {
        if !self.exceptions.covers(ck, field) {
            self.report.mismatches.push(Mismatch {
                ck: ck.to_owned(),
                field,
                expected,
                actual,
                difference: diff,
            });
        }
    }

    fn unread(&mut self, ck: &str, field: Field, expected: String) {
        self.fail(ck, field, expected, "свойство не читается".to_owned(), None);
    }

    fn snapshot(&mut self, ck: &str, field: Field, kind: SnapshotKind) {
        if !self.exceptions.covers(ck, field) {
            self.report.snapshot.push(SnapshotCheck {
                ck: ck.to_owned(),
                field,
                kind,
            });
        }
    }

    fn passport_element(&self, ck: &str) -> Option<&Element> {
        self.passport.elements.iter().find(|e| e.ck == ck)
    }

    /// Начало по `x` предка, от которого считается `x` ребёнка: у предка с исключением `frame_x`.
    fn x_origin(&self, element: &Element) -> Option<(f64, f64)> {
        let parent = element.parent.as_deref()?;
        if !self.exceptions.covers(parent, Field::FrameX) {
            return None;
        }
        let want = self.passport_element(parent)?.frame.x;
        let got = self.product.frame(self.product.keyed(parent)?).x;
        Some((want, got))
    }

    fn frame(&mut self, element: &Element, node: &Node) {
        let (want, got) = (element.frame, self.product.frame(node));
        let (want_x, got_x) = match self.x_origin(element) {
            Some((w, g)) => (want.x - w, got.x - g),
            None => (want.x, got.x),
        };
        let single = single_line_text(element);
        // Кадр подписи кнопки — место заголовка: начало и ширину судят чернила на снимке.
        let title = node.class == BUTTON_TITLE;
        let width = if element.kind == ElementKind::Text && element.text.is_some() {
            self.tolerances.text_width.max(FRAME_TOLERANCE)
        } else {
            FRAME_TOLERANCE
        };
        let x_gap = (want_x - got_x).abs();
        let w_gap = (want.w - got.w).abs();
        // Высоту однострочного текста движки считают по-разному: сравнивается середина строки.
        let y_gap = if single {
            ((want.y + want.h / 2.0) - (got.y + got.h / 2.0)).abs()
        } else {
            (want.y - got.y).abs().max((want.h - got.h).abs())
        };
        if title {
            self.snapshot(&element.ck, Field::Frame, SnapshotKind::ButtonTitleInk);
        }
        // Исключение `frame_x` снимает только `x`: остальные стороны судятся.
        let x_over = !title
            && x_gap > FRAME_TOLERANCE
            && !self.exceptions.covers(&element.ck, Field::FrameX);
        let w_over = !title && w_gap > width;
        if x_over || w_over || y_gap > FRAME_TOLERANCE {
            let worst = if title {
                y_gap
            } else {
                x_gap.max(w_gap).max(y_gap)
            };
            self.fail(
                &element.ck,
                Field::Frame,
                rect_text(want),
                rect_text(got),
                Some(worst),
            );
        }
    }

    fn font(&mut self, element: &Element, style: &ProductStyle) {
        let Some(want) = &element.font else { return };
        let expected = format!("{} {} {}", want.family, want.weight, want.size);
        let Some(got) = &style.font else {
            return self.unread(&element.ck, Field::Font, expected);
        };
        let family = if want.family == SYSTEM_FAMILY.0 {
            got.family == SYSTEM_FAMILY.1
        } else {
            want.family == got.family
        };
        let main = want.faces.iter().max_by_key(|f| f.glyphs);
        let bundled = got
            .file
            .as_deref()
            .is_some_and(|f| f.contains(BUNDLED_FONTS));
        let source = main.is_none_or(|f| f.custom == bundled);
        let face = main.is_none_or(|f| !f.custom || f.family == got.face);
        if !(family && source && face && want.weight == got.weight && want.size == got.size) {
            let actual = format!(
                "{} ({}) {} {}, файл {}",
                got.family,
                got.face,
                got.weight,
                got.size,
                got.file.as_deref().unwrap_or("не назван")
            );
            let expected = match main {
                Some(f) => format!(
                    "{expected}, {} ({})",
                    f.family,
                    if f.custom {
                        "вложенный"
                    } else {
                        "системный"
                    }
                ),
                None => expected,
            };
            self.fail(&element.ck, Field::Font, expected, actual, None);
        }
    }

    fn color(&mut self, ck: &str, field: Field, want: Option<Rgba>, got: Option<Rgba>) {
        let Some(want) = want else { return };
        match got {
            None => self.unread(ck, field, rgba_text(want)),
            Some(got) => {
                let gap = color_gap(want, got);
                if gap > f64::from(CHANNEL_TOLERANCE) {
                    self.fail(ck, field, rgba_text(want), rgba_text(got), Some(gap));
                }
            }
        }
    }

    fn number(&mut self, ck: &str, field: Field, want: Option<f64>, got: Option<f64>, tol: f64) {
        let Some(want) = want else { return };
        match got.or(fields::absent_value(field)) {
            None => self.unread(ck, field, want.to_string()),
            Some(got) if (want - got).abs() > tol => self.fail(
                ck,
                field,
                want.to_string(),
                got.to_string(),
                Some((want - got).abs()),
            ),
            Some(_) => {}
        }
    }

    fn element(&mut self, element: &Element, node: &Node) {
        let ck = element.ck.as_str();
        let style = node.style.clone().unwrap_or_default();
        let parent = self.product.parent_key(node);
        if element.parent.as_deref() != parent {
            self.fail(
                ck,
                Field::Composition,
                format!("родитель {:?}", element.parent),
                format!("родитель {parent:?}"),
                None,
            );
        }
        self.frame(element, node);
        if let Some(text) = &element.text {
            if node.label.as_deref() != Some(text.as_str()) {
                self.fail(
                    ck,
                    Field::Text,
                    text.clone(),
                    node.label.clone().unwrap_or_default(),
                    None,
                );
            }
            // Подпись кнопки не обрезается полем: факт обрезки у неё несёт ширина по чернилам.
            if let (Some(want), true) = (element.truncated, node.class != BUTTON_TITLE) {
                match node.truncated {
                    None => self.unread(ck, Field::Truncated, want.to_string()),
                    Some(got) if got != want => {
                        self.fail(
                            ck,
                            Field::Truncated,
                            want.to_string(),
                            got.to_string(),
                            None,
                        );
                    }
                    Some(_) => {}
                }
            }
        }
        self.font(element, &style);
        for field in [
            Field::LineHeight,
            Field::LetterSpacing,
            Field::LineWidth,
            Field::Radius,
        ] {
            if fields::rule(field, element.kind).applicability == Applicability::NotApplicable {
                continue;
            }
            let (want, got, tolerance) = match field {
                Field::LineHeight => (element.line_height, style.line_height, LINE_TOLERANCE),
                Field::LetterSpacing => {
                    (element.letter_spacing, style.letter_spacing, LINE_TOLERANCE)
                }
                Field::LineWidth => (element.line_width, style.line_width, 0.0),
                _ => (element.radius, style.radius, 0.0),
            };
            match fields::judgement(element, field) {
                Judgement::ByRule => self.number(ck, field, want, got, tolerance),
                // Интервал однострочного текста несёт середина строки в кадре.
                Judgement::ByFrames | Judgement::NotDrawn | Judgement::ByChildren => {}
                Judgement::RingAsLine | Judgement::BySnapshot => {
                    self.snapshot(ck, field, SnapshotKind::Region);
                }
            }
        }
        match fields::judgement(element, Field::Color) {
            Judgement::ByRule => self.color(ck, Field::Color, element.color, style.color),
            Judgement::BySnapshot => self.snapshot(ck, Field::Color, SnapshotKind::IconColor),
            Judgement::ByChildren
            | Judgement::ByFrames
            | Judgement::NotDrawn
            | Judgement::RingAsLine => {}
        }
        match fields::judgement(element, Field::Fill) {
            Judgement::BySnapshot => {
                if element.fill.is_some() {
                    self.snapshot(ck, Field::Fill, SnapshotKind::Region);
                }
            }
            Judgement::ByRule
            | Judgement::ByFrames
            | Judgement::ByChildren
            | Judgement::NotDrawn
            | Judgement::RingAsLine => self.color(ck, Field::Fill, element.fill, style.fill),
        }
        match fields::judgement(element, Field::Shadow) {
            Judgement::RingAsLine => {
                let ring = element.shadow[0];
                self.color(ck, Field::Shadow, Some(ring.color), style.line_color);
                self.number(ck, Field::Shadow, Some(ring.spread), style.line_width, 0.0);
                if style.line_sides.len() != 4 {
                    self.fail(
                        ck,
                        Field::Shadow,
                        "кольцо по четырём сторонам".to_owned(),
                        format!("стороны {:?}", style.line_sides),
                        None,
                    );
                }
            }
            Judgement::BySnapshot => self.snapshot(ck, Field::Shadow, SnapshotKind::Region),
            Judgement::ByRule
            | Judgement::ByFrames
            | Judgement::ByChildren
            | Judgement::NotDrawn => {
                // Наличие — точно, цвет — допуск канала.
                if element.shadow.len() != style.shadow.len() {
                    self.fail(
                        ck,
                        Field::Shadow,
                        format!("теней {}", element.shadow.len()),
                        format!("теней {}", style.shadow.len()),
                        None,
                    );
                } else {
                    for (want, got) in element.shadow.iter().zip(&style.shadow) {
                        self.color(ck, Field::Shadow, Some(want.color), Some(got.color));
                    }
                }
            }
        }
        if element.line_width.is_some_and(|w| w > 0.0) {
            self.color(ck, Field::LineColor, element.line_color, style.line_color);
            if element.line_sides != style.line_sides {
                self.fail(
                    ck,
                    Field::LineWidth,
                    format!("стороны {:?}", element.line_sides),
                    format!("стороны {:?}", style.line_sides),
                    None,
                );
            }
        }
        if let Some(want) = &element.icon {
            match &style.icon {
                None => self.unread(ck, Field::Icon, format!("{} {}", want.name, want.size)),
                Some(got) => {
                    let name = want.name == got.name || self.exceptions.covers(ck, Field::IconName);
                    if !(name && want.size == got.size) {
                        self.fail(
                            ck,
                            Field::Icon,
                            format!("{} {}", want.name, want.size),
                            format!("{} {}", got.name, got.size),
                            None,
                        );
                    }
                }
            }
            self.snapshot(ck, Field::IconMask, SnapshotKind::Mask);
        }
    }
}

/// Сверяет паспорт с продуктом: состав, пары по ключу и поля контракта. Поля снимка перечислены в
/// [`Report::snapshot`].
pub fn judge(
    passport: &Passport,
    product: &Product,
    registry: &Registry,
    state: &str,
    tolerances: Tolerances,
) -> Result<Report, Error> {
    let surface: Surface = registry
        .state(state)
        .ok_or_else(|| Error::new(format!("состояния «{state}» нет в реестре")))?
        .surface;
    let want = &passport.composition;
    let got = composition(product, &want.titlebar, &want.statusline);
    let exceptions = Exceptions::new(registry, state, want);
    // Состав сверяется в пределах поверхности состояния.
    let within = |full: &Composition| match surface {
        Surface::Titlebar => Composition {
            zones: BTreeMap::new(),
            focus: None,
            menu: None,
            titlebar: full.titlebar.clone(),
            statusline: Vec::new(),
        },
        Surface::Statusline => Composition {
            zones: BTreeMap::new(),
            focus: None,
            menu: None,
            titlebar: Vec::new(),
            statusline: full.statusline.clone(),
        },
        Surface::Layout | Surface::Menu | Surface::Picker => full.clone(),
    };
    let mut judge = Judge {
        product,
        passport,
        exceptions: &exceptions,
        tolerances,
        report: Report::default(),
    };
    for line in within(want).diff(&within(&got)) {
        judge.report.mismatches.push(Mismatch {
            ck: String::new(),
            field: Field::Composition,
            expected: "состав паспорта".to_owned(),
            actual: line,
            difference: None,
        });
    }
    let hidden = |element: &Element| {
        let mut current = Some(element);
        while let Some(e) = current {
            if !e.visible {
                return true;
            }
            current = e.parent.as_deref().and_then(|p| judge_parent(passport, p));
        }
        false
    };
    for element in &passport.elements {
        let ck = element.ck.as_str();
        if element.kind == ElementKind::Pseudo {
            if element.visible {
                let kind = if ck.contains(".tabs.tab.") && ck.ends_with("::after") {
                    SnapshotKind::Underline
                } else {
                    SnapshotKind::Region
                };
                let field = if kind == SnapshotKind::Underline {
                    Field::Underline
                } else {
                    Field::Fill
                };
                judge.snapshot(ck, field, kind);
            }
            continue;
        }
        let node = product.keyed(ck).filter(|n| n.visible);
        match (hidden(element), node) {
            // Невидимый элемент паспорта равен отсутствию узла у продукта.
            (true, None) => {}
            (true, Some(_)) => judge.fail(
                ck,
                Field::Visible,
                "не виден".to_owned(),
                "виден".to_owned(),
                None,
            ),
            (false, None) => judge.fail(
                ck,
                Field::Composition,
                "элемент паспорта".to_owned(),
                "нет узла продукта".to_owned(),
                None,
            ),
            (false, Some(node)) => judge.element(element, node),
        }
    }
    for node in &product.components {
        let Some(ck) = node
            .ck
            .as_deref()
            .filter(|ck| node.visible && surface.owns(ck))
        else {
            continue;
        };
        if judge.passport_element(ck).is_none() {
            judge.fail(
                ck,
                Field::Composition,
                "нет элемента паспорта".to_owned(),
                "узел продукта".to_owned(),
                None,
            );
        }
    }
    Ok(judge.report)
}

fn judge_parent<'a>(passport: &'a Passport, ck: &str) -> Option<&'a Element> {
    passport.elements.iter().find(|e| e.ck == ck)
}

/// Пересечение к объединению масок значка с выравниванием: кадр паспорта с дробным началом даёт
/// маску на пиксель шире маски продукта. Обе маски кладутся на общий холст наибольшего размера,
/// берётся наибольшее значение по целым сдвигам в пределах разницы размеров. Разница больше
/// одного пикселя по оси — ошибка: место значка судит кадр, а не маска.
pub fn aligned_iou(reference: &Mask, product: &Mask) -> Result<f64, String> {
    if reference.width.abs_diff(product.width) > 1 || reference.height.abs_diff(product.height) > 1
    {
        return Err(format!(
            "маски {}×{} и {}×{} расходятся больше чем на пиксель",
            reference.width, reference.height, product.width, product.height
        ));
    }
    let (width, height) = (
        reference.width.max(product.width),
        reference.height.max(product.height),
    );
    let placed = |mask: &Mask, dx: u32, dy: u32| {
        let mut on = vec![false; (width * height) as usize];
        for y in 0..mask.height {
            for x in 0..mask.width {
                on[((y + dy) * width + x + dx) as usize] = mask.on[(y * mask.width + x) as usize];
            }
        }
        Mask { width, height, on }
    };
    let mut best: f64 = 0.0;
    for (rx, ry) in shifts(width - reference.width, height - reference.height) {
        let a = placed(reference, rx, ry);
        for (px, py) in shifts(width - product.width, height - product.height) {
            best = best.max(a.iou(&placed(product, px, py))?);
        }
    }
    Ok(best)
}

fn shifts(dx: u32, dy: u32) -> impl Iterator<Item = (u32, u32)> {
    (0..=dx).flat_map(move |x| (0..=dy).map(move |y| (x, y)))
}

/// Фон под значком у продукта — то же правило, что у паспорта
/// (`shellio_conformance::mask::backdrop`): заливка ближайшего предка с ключом из `style`,
/// полупрозрачная или отсутствие заливки у всей цепочки — отказ. Узлы с ключом переводятся в
/// элементы с родителем по ключу, правило не повторяется.
pub fn product_backdrop(product: &Product, node: &Node) -> Result<Rgba, String> {
    let ck = node
        .ck
        .as_deref()
        .ok_or_else(|| format!("{}: значок без ключа", node.id))?;
    let elements: Vec<Element> = product
        .components
        .iter()
        .filter_map(|n| {
            Some(Element {
                ck: n.ck.clone()?,
                kind: ElementKind::Container,
                parent: product.parent_key(n).map(str::to_owned),
                frame: product.frame(n),
                visible: n.visible,
                text: None,
                truncated: None,
                font: None,
                line_height: None,
                letter_spacing: None,
                color: None,
                fill: n.style.as_ref().and_then(|style| style.fill),
                line_color: None,
                line_width: None,
                line_sides: Vec::new(),
                radius: None,
                padding: None,
                shadow: Vec::new(),
                icon: None,
            })
        })
        .collect();
    backdrop(&elements, ck)
}

/// Маска значка продукта по снимку окна: кадр узла на сетке пикселей при масштабе `scale`
/// (`PixelClip`), передний план — правило `ICON_FG` против фона [`product_backdrop`]. `pixel`
/// отдаёт цвет пикселя снимка в sRGB по столбцу и строке от верхнего левого угла.
pub fn product_mask(
    product: &Product,
    node: &Node,
    scale: f64,
    pixel: impl Fn(u32, u32) -> Option<[u8; 3]>,
) -> Result<Mask, String> {
    let clip = PixelClip::of(&product.frame(node), scale)?;
    let background = product_backdrop(product, node)?;
    let mut rgba = Vec::with_capacity(clip.width as usize * clip.height as usize * 4);
    for y in 0..clip.height {
        for x in 0..clip.width {
            let [r, g, b] = pixel(clip.x + x, clip.y + y)
                .ok_or_else(|| format!("значок за кадром снимка: пиксель {x}×{y} области"))?;
            rgba.extend([r, g, b, 255]);
        }
    }
    Ok(Mask {
        width: clip.width,
        height: clip.height,
        on: foreground(&rgba, background, ICON_FG),
    })
}

/// Силуэт значка: маска продукта против эталонной маски паспорта, мера — [`aligned_iou`], порог —
/// из калибровки. `None` — в допуске.
pub fn mask_mismatch(
    ck: &str,
    reference: &Mask,
    product: &Mask,
    threshold: f64,
) -> Option<Mismatch> {
    let expected = format!("пересечение к объединению не меньше {threshold}");
    match aligned_iou(reference, product) {
        Ok(iou) if iou >= threshold => None,
        Ok(iou) => Some(Mismatch {
            ck: ck.to_owned(),
            field: Field::IconMask,
            expected,
            actual: iou.to_string(),
            difference: Some(threshold - iou),
        }),
        Err(reason) => Some(Mismatch {
            ck: ck.to_owned(),
            field: Field::IconMask,
            expected,
            actual: reason,
            difference: None,
        }),
    }
}

/// Чернила текста по снимку: начало и ширина по `x` в точках окна. Столбец пикселей кадра узла
/// несёт чернила, если в нём есть пиксель переднего плана по правилу `ICON_FG` против фона
/// [`product_backdrop`]. `None` — чернил в кадре нет.
pub fn ink_span(
    product: &Product,
    node: &Node,
    scale: f64,
    pixel: impl Fn(u32, u32) -> Option<[u8; 3]>,
) -> Result<Option<(f64, f64)>, String> {
    let clip = PixelClip::of(&product.frame(node), scale)?;
    let background = product_backdrop(product, node)?;
    let mut span: Option<(u32, u32)> = None;
    for x in 0..clip.width {
        let mut ink = false;
        for y in 0..clip.height {
            let [r, g, b] = pixel(clip.x + x, clip.y + y)
                .ok_or_else(|| format!("подпись за кадром снимка: пиксель {x}×{y} области"))?;
            if foreground(&[r, g, b, 255], background, ICON_FG) == [true] {
                ink = true;
                break;
            }
        }
        if ink {
            span = Some((span.map_or(x, |(first, _)| first), x));
        }
    }
    Ok(span.map(|(first, last)| {
        (
            f64::from(clip.x + first) / scale,
            f64::from(last - first + 1) / scale,
        )
    }))
}

/// Начало и ширина подписи кнопки: чернила продукта против кадра подписи паспорта. Начало —
/// допуск кадра, ширина — допуск калибровки ширины текста. Начала передаются уже от общего
/// начала: окна или предка с исключением `frame_x`. `None` — в допуске.
pub fn title_mismatch(
    ck: &str,
    expected: (f64, f64),
    ink: Option<(f64, f64)>,
    tolerances: Tolerances,
) -> Option<Mismatch> {
    let wanted = format!("начало {}, ширина {}", expected.0, expected.1);
    let Some((x, width)) = ink else {
        return Some(Mismatch {
            ck: ck.to_owned(),
            field: Field::Frame,
            expected: wanted,
            actual: "чернил подписи на снимке нет".to_owned(),
            difference: None,
        });
    };
    let (x_gap, w_gap) = ((expected.0 - x).abs(), (expected.1 - width).abs());
    (x_gap > FRAME_TOLERANCE || w_gap > tolerances.text_width.max(FRAME_TOLERANCE)).then(|| {
        Mismatch {
            ck: ck.to_owned(),
            field: Field::Frame,
            expected: wanted,
            actual: format!("чернила: начало {x}, ширина {width}"),
            difference: Some(x_gap.max(w_gap)),
        }
    })
}

/// Медиана цвета участка снимка по каналам: пиксели, лежащие целиком внутри кадра `frame` в
/// координатах паспорта. Кадр без единого целого пикселя или за снимком — отказ.
pub fn region_median(
    frame: &Rect,
    scale: f64,
    pixel: impl Fn(u32, u32) -> Option<[u8; 3]>,
) -> Result<[u8; 3], String> {
    let edge = |v: f64| (v * scale * 1e6).round() / 1e6;
    let (x0, y0) = (edge(frame.x).ceil(), edge(frame.y).ceil());
    let (x1, y1) = (
        edge(frame.x + frame.w).floor(),
        edge(frame.y + frame.h).floor(),
    );
    if x0 < 0.0 || y0 < 0.0 || x1 <= x0 || y1 <= y0 {
        return Err(format!("участок {frame:?} не содержит ни одного пикселя"));
    }
    let mut channels: [Vec<u8>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    for y in y0 as u32..y1 as u32 {
        for x in x0 as u32..x1 as u32 {
            let rgb = pixel(x, y).ok_or_else(|| format!("участок {frame:?} за кадром снимка"))?;
            for (channel, value) in channels.iter_mut().zip(rgb) {
                channel.push(value);
            }
        }
    }
    Ok(channels.map(|mut channel| {
        channel.sort_unstable();
        channel[channel.len() / 2]
    }))
}

/// Непрозрачная заливка участка: медиана снимка против цвета паспорта, допуск канала. `None` — в
/// допуске.
pub fn fill_mismatch(ck: &str, field: Field, expected: Rgba, median: [u8; 3]) -> Option<Mismatch> {
    let [r, g, b] = median;
    let actual = Rgba { r, g, b, a: 1.0 };
    let gap = color_gap(Rgba { a: 1.0, ..expected }, actual);
    (gap > f64::from(CHANNEL_TOLERANCE)).then(|| Mismatch {
        ck: ck.to_owned(),
        field,
        expected: rgba_text(expected),
        actual: rgba_text(actual),
        difference: Some(gap),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mask(width: u32, height: u32, on: &[u8]) -> Mask {
        Mask {
            width,
            height,
            on: on.iter().map(|v| *v == 1).collect(),
        }
    }

    #[test]
    fn masks_of_one_size_compare_in_place() {
        let a = mask(2, 2, &[1, 1, 0, 0]);
        let b = mask(2, 2, &[1, 0, 0, 0]);
        assert_eq!(aligned_iou(&a, &a), Ok(1.0));
        assert_eq!(aligned_iou(&a, &b), Ok(0.5));
    }

    #[test]
    fn mask_one_pixel_wider_is_aligned_by_the_best_shift() {
        // Тот же силуэт, сдвинутый на пиксель в более широкой маске.
        let wide = mask(3, 2, &[0, 1, 1, 0, 1, 0]);
        let narrow = mask(2, 2, &[1, 1, 1, 0]);
        assert_eq!(aligned_iou(&wide, &narrow), Ok(1.0));
        // Другой силуэт сдвигом не сходится.
        let other = mask(2, 2, &[0, 0, 1, 1]);
        assert!(aligned_iou(&wide, &other).is_ok_and(|iou| iou < 0.5));
    }

    #[test]
    fn masks_differing_by_more_than_a_pixel_are_refused() {
        let a = mask(4, 2, &[0; 8]);
        let b = mask(2, 2, &[0; 4]);
        assert!(aligned_iou(&a, &b).is_err());
    }

    /// Дерево: полоса с заливкой и значок 10×10 точек в (5, 5) от верхнего левого угла.
    fn product(fill_alpha: f64) -> Product {
        let tree = serde_json::json!({
            "content": {"width": 40.0, "height": 20.0},
            "components": [
                {"id": "bar", "ck": "chrome", "frame": {"x": 0.0, "y": 0.0, "width": 40.0, "height": 20.0},
                 "visible": true, "style": {"fill": {"r": 30, "g": 29, "b": 27, "a": fill_alpha}}},
                {"id": "group", "parent": "bar", "frame": {"x": 0.0, "y": 0.0, "width": 40.0, "height": 20.0},
                 "visible": true, "style": {"fill": {"r": 200, "g": 0, "b": 0, "a": 0.5}}},
                {"id": "icon", "parent": "group", "ck": "chrome.search.icon",
                 "frame": {"x": 5.0, "y": 5.0, "width": 10.0, "height": 10.0}, "visible": true}
            ]
        });
        Product::parse(&tree.to_string()).unwrap_or_else(|e| panic!("{e}"))
    }

    /// Снимок при масштабе 2: фон полосы и диагональ значка, на границе порога и за ней.
    fn pixel(x: u32, y: u32) -> Option<[u8; 3]> {
        if x >= 80 || y >= 40 {
            return None;
        }
        let inside = (10..30).contains(&x) && (10..30).contains(&y);
        Some(match (inside, x == y, x == y + 1) {
            (true, true, _) => [30 + ICON_FG, 29, 27],
            (true, _, true) => [30 + ICON_FG - 1, 29, 27],
            _ => [30, 29, 27],
        })
    }

    #[test]
    fn product_mask_is_cut_by_the_node_frame_against_the_keyed_backdrop() {
        let product = product(1.0);
        let icon = product
            .keyed("chrome.search.icon")
            .unwrap_or_else(|| panic!("значок"));
        // Узел без ключа между значком и полосой фоном не считается.
        assert_eq!(
            product_backdrop(&product, icon),
            Ok(Rgba {
                r: 30,
                g: 29,
                b: 27,
                a: 1.0
            })
        );
        let mask = product_mask(&product, icon, 2.0, pixel).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((mask.width, mask.height), (20, 20));
        // Передний план — только пиксели с разницей не меньше порога: диагональ.
        assert_eq!(mask.on.iter().filter(|on| **on).count(), 20);
        assert!(mask.on[0] && !mask.on[1]);
    }

    #[test]
    fn translucent_backdrop_and_icon_outside_the_snapshot_are_refused() {
        let translucent = product(0.5);
        let icon = translucent
            .keyed("chrome.search.icon")
            .unwrap_or_else(|| panic!("значок"));
        assert!(product_mask(&translucent, icon, 2.0, pixel).is_err());
        let product = product(1.0);
        let icon = product
            .keyed("chrome.search.icon")
            .unwrap_or_else(|| panic!("значок"));
        assert!(product_mask(&product, icon, 2.0, |_, _| None).is_err());
    }

    #[test]
    fn mask_threshold_holds_at_the_boundary() {
        let reference = mask(2, 2, &[1, 1, 1, 1]);
        let three = mask(2, 2, &[1, 1, 1, 0]);
        assert_eq!(mask_mismatch("k", &reference, &three, 0.75), None);
        let over =
            mask_mismatch("k", &reference, &three, 0.76).unwrap_or_else(|| panic!("расхождение"));
        assert_eq!(over.field, Field::IconMask);
        assert!(over.difference.is_some_and(|d| (d - 0.01).abs() < 1e-9));
        // Пустая маска продукта против непустой эталонной — расхождение.
        let empty = mask(2, 2, &[0, 0, 0, 0]);
        assert!(mask_mismatch("k", &reference, &empty, 0.5).is_some());
        // Маски, расходящиеся больше чем на пиксель, — расхождение с причиной.
        let big = mask(4, 2, &[1; 8]);
        assert!(mask_mismatch("k", &big, &three, 0.1).is_some_and(|m| m.difference.is_none()));
    }

    /// Кнопка 30×10 точек в (5, 5) с подписью: место заголовка — весь кадр, чернила — столбцы
    /// 24..=51 пикселей снимка (12 точек от края окна, ширина 14 точек).
    fn button() -> Product {
        let tree = serde_json::json!({
            "content": {"width": 40.0, "height": 20.0},
            "components": [
                {"id": "bar", "ck": "chrome", "frame": {"x": 0.0, "y": 0.0, "width": 40.0, "height": 20.0},
                 "visible": true, "style": {"fill": {"r": 30, "g": 29, "b": 27, "a": 1.0}}},
                {"id": "title", "parent": "bar", "ck": "chrome.workspace.label", "class": BUTTON_TITLE,
                 "frame": {"x": 5.0, "y": 5.0, "width": 30.0, "height": 10.0}, "visible": true}
            ]
        });
        Product::parse(&tree.to_string()).unwrap_or_else(|e| panic!("{e}"))
    }

    fn text_pixel(x: u32, y: u32) -> Option<[u8; 3]> {
        let ink = (24..=51).contains(&x) && (14..26).contains(&y) && (x + y).is_multiple_of(2);
        Some(if ink { [217, 212, 202] } else { [30, 29, 27] })
    }

    const TEXT: Tolerances = Tolerances {
        text_width: 1.25,
        mask_iou: 0.9,
    };

    #[test]
    fn ink_span_reads_the_drawn_text_not_the_title_slot() {
        let product = button();
        let title = product
            .keyed("chrome.workspace.label")
            .unwrap_or_else(|| panic!("подпись"));
        let ink = ink_span(&product, title, 2.0, text_pixel).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(ink, Some((12.0, 14.0)));
        let blank = ink_span(&product, title, 2.0, |_, _| Some([30, 29, 27]));
        assert_eq!(blank, Ok(None));
        assert!(ink_span(&product, title, 2.0, |_, _| None).is_err());
    }

    #[test]
    fn title_ink_is_judged_by_start_and_calibrated_width() {
        let ink = Some((12.0, 14.0));
        assert_eq!(title_mismatch("k", (12.0, 14.0), ink, TEXT), None);
        // Начало — точка кадра на границе и за ней.
        assert_eq!(title_mismatch("k", (13.0, 14.0), ink, TEXT), None);
        assert!(title_mismatch("k", (13.01, 14.0), ink, TEXT).is_some());
        // Подпись с лишним отступом 2 точки.
        let padded =
            title_mismatch("k", (10.0, 14.0), ink, TEXT).unwrap_or_else(|| panic!("расхождение"));
        assert_eq!((padded.field, padded.difference), (Field::Frame, Some(2.0)));
        // Ширина — допуск калибровки на границе и за ней.
        assert_eq!(title_mismatch("k", (12.0, 15.25), ink, TEXT), None);
        assert!(title_mismatch("k", (12.0, 15.26), ink, TEXT).is_some());
        // Подпись не нарисована.
        assert!(
            title_mismatch("k", (12.0, 14.0), None, TEXT).is_some_and(|m| m.difference.is_none())
        );
    }

    #[test]
    fn region_median_takes_whole_pixels_inside_the_frame() {
        // Круглая кнопка 12×12 точек в (14, 13,5): углы кадра — фон, медиана — заливка.
        let frame = Rect {
            x: 14.0,
            y: 13.5,
            w: 12.0,
            h: 12.0,
        };
        let button = |x: u32, y: u32| {
            let (dx, dy) = (f64::from(x) - 39.5, f64::from(y) - 38.5);
            Some(if dx * dx + dy * dy <= 144.0 {
                [255, 95, 87]
            } else {
                [30, 29, 27]
            })
        };
        assert_eq!(region_median(&frame, 2.0, button), Ok([255, 95, 87]));
        // Линия в половину точки на дробной границе: целый пиксель один.
        let line = Rect {
            x: 0.0,
            y: 0.25,
            w: 4.0,
            h: 0.75,
        };
        assert_eq!(
            region_median(&line, 2.0, |_, y| Some([y as u8, 0, 0])),
            Ok([1, 0, 0])
        );
        let thin = Rect {
            x: 0.0,
            y: 0.25,
            w: 4.0,
            h: 0.5,
        };
        assert!(region_median(&thin, 2.0, |_, _| Some([0, 0, 0])).is_err());
        assert!(region_median(&frame, 2.0, |_, _| None).is_err());
    }

    #[test]
    fn opaque_fill_is_judged_within_three_units() {
        let fill = Rgba {
            r: 255,
            g: 95,
            b: 87,
            a: 1.0,
        };
        assert_eq!(fill_mismatch("k", Field::Fill, fill, [252, 98, 87]), None);
        let over = fill_mismatch("k", Field::Fill, fill, [251, 95, 87])
            .unwrap_or_else(|| panic!("расхождение"));
        assert_eq!((over.field, over.difference), (Field::Fill, Some(4.0)));
    }
}
