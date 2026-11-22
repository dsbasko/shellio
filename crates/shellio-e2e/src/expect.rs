//! Входы оракула: файл ожиданий участков, ответы `ui.window` и `ui.inspect` и перевод кадров в
//! пиксели снимка.
//!
//! Ответы читаются собственными структурами по именам полей опубликованного протокола v1
//! (ADR-0007): кадр `shellioctl --json` целиком (`{"id":…,"ok":{"ui":{"window":{…}}}}`) или само
//! тело ответа. Лишние поля не мешают.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::Error;
use crate::css::{Appearance, Cell, Contrast, Layer};

/// Наименьший отступ участка от краёв компонента и от других узлов в точках («Матрица приёмки»).
pub const MIN_INSET: f64 = 4.0;

/// Погрешность сравнения координат в точках и пикселях.
const EPSILON: f64 = 1e-6;

/// Прямоугольник в точках.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub const fn new(x: f64, y: f64, width: f64, height: f64) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    fn right(&self) -> f64 {
        self.x + self.width
    }

    fn bottom(&self) -> f64 {
        self.y + self.height
    }

    fn is_finite(&self) -> bool {
        [self.x, self.y, self.width, self.height]
            .iter()
            .all(|v| v.is_finite())
    }

    /// Прямоугольник, расширенный на `by` во все стороны.
    fn grown(&self, by: f64) -> Rect {
        Rect::new(
            self.x - by,
            self.y - by,
            self.width + 2.0 * by,
            self.height + 2.0 * by,
        )
    }

    fn intersects(&self, other: &Rect) -> bool {
        self.x < other.right() - EPSILON
            && other.x < self.right() - EPSILON
            && self.y < other.bottom() - EPSILON
            && other.y < self.bottom() - EPSILON
    }
}

/// Размер в точках.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub struct Size {
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Deserialize)]
struct AxisValue<T> {
    value: T,
}

/// Нужная оракулу часть ответа `ui.window`.
#[derive(Debug, Clone, Deserialize)]
pub struct WindowView {
    pub window_number: i64,
    /// Кадр окна в точках экрана.
    pub frame: Rect,
    pub backing_scale_factor: f64,
    lightness: AxisValue<Appearance>,
    contrast: AxisValue<Contrast>,
    pub generation: u64,
}

impl WindowView {
    /// Действующая ячейка окна.
    pub fn cell(&self) -> Cell {
        Cell::new(self.lightness.value, self.contrast.value)
    }
}

/// Узел дерева `ui.inspect`.
#[derive(Debug, Clone, Deserialize)]
pub struct Component {
    pub id: String,
    pub parent: Option<String>,
    /// Кадр в точках окна, начало внизу слева.
    pub frame: Rect,
    pub visible: bool,
}

/// Нужная оракулу часть ответа `ui.inspect`.
#[derive(Debug, Clone, Deserialize)]
pub struct InspectView {
    pub window_number: i64,
    /// Размер содержимого окна: высота переводит кадры в строки снимка.
    pub content: Size,
    pub generation: u64,
    pub components: Vec<Component>,
}

impl InspectView {
    pub fn component(&self, id: &str) -> Result<&Component, Error> {
        self.components
            .iter()
            .find(|component| component.id == id)
            .ok_or_else(|| Error::new(format!("в ui.inspect нет компонента {id}")))
    }

    /// `id` — сам `component` или его предок по цепочке `parent`.
    fn is_self_or_ancestor(&self, id: &str, component: &Component) -> bool {
        let mut current = Some(component);
        for _ in 0..=self.components.len() {
            let Some(node) = current else {
                return false;
            };
            if node.id == id {
                return true;
            }
            current = node
                .parent
                .as_deref()
                .and_then(|parent| self.components.iter().find(|c| c.id == parent));
        }
        false
    }
}

/// Тело ответа `kind` из кадра `shellioctl --json` или само тело.
fn response_body<T: DeserializeOwned>(text: &str, kind: &str, what: &str) -> Result<T, Error> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| Error::new(format!("{what}: не JSON: {e}")))?;
    let body = match value.get("ok") {
        Some(ok) => ok
            .get("ui")
            .and_then(|ui| ui.get(kind))
            .ok_or_else(|| Error::new(format!("{what}: в кадре нет ok.ui.{kind}")))?
            .clone(),
        None => {
            if value.get("error").is_some() {
                return Err(Error::new(format!("{what}: кадр отказа {value}")));
            }
            value
        }
    };
    serde_json::from_value(body).map_err(|e| Error::new(format!("{what}: {e}")))
}

pub fn parse_window(text: &str) -> Result<WindowView, Error> {
    let window: WindowView = response_body(text, "window", "ui.window")?;
    let scale = window.backing_scale_factor;
    if !(scale.is_finite() && scale > 0.0) {
        return Err(Error::new(format!(
            "ui.window: неверный backing_scale_factor {scale}"
        )));
    }
    if !window.frame.is_finite() || window.frame.width <= 0.0 || window.frame.height <= 0.0 {
        return Err(Error::new(format!(
            "ui.window: неверный кадр окна {:?}",
            window.frame
        )));
    }
    Ok(window)
}

pub fn parse_inspect(text: &str) -> Result<InspectView, Error> {
    response_body(text, "inspect", "ui.inspect")
}

/// Участок ожиданий.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Region {
    /// Имя участка в отчёте.
    pub id: String,
    /// Идентификатор компонента в `ui.inspect`.
    pub component: String,
    /// Участок в точках относительно кадра компонента, начало в его верхнем левом углу, ось y
    /// вниз.
    pub rect: Rect,
    /// Слот заливки: переменная `tokens.css` без `--`, `panel` и `panel-head` — из ADR-0027.
    pub slot: String,
    /// Слой фокуса над заливкой.
    #[serde(default)]
    pub layer: Layer,
}

/// Файл ожиданий одной ячейки.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expectations {
    pub cell: Cell,
    pub regions: Vec<Region>,
}

pub fn parse_expectations(text: &str) -> Result<Expectations, Error> {
    let expectations: Expectations =
        serde_json::from_str(text).map_err(|e| Error::new(format!("ожидания: {e}")))?;
    if expectations.regions.is_empty() {
        return Err(Error::new("ожидания: нет ни одного участка"));
    }
    for (i, region) in expectations.regions.iter().enumerate() {
        if expectations.regions[..i].iter().any(|r| r.id == region.id) {
            return Err(Error::new(format!(
                "ожидания: участок {} дважды",
                region.id
            )));
        }
    }
    Ok(expectations)
}

/// Прямоугольник в пикселях снимка, начало сверху слева.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PixelRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Геометрия снимка: перевод точек окна в пиксели PNG.
#[derive(Debug, Clone, Copy)]
pub struct Geometry {
    pub scale: f64,
    /// Размер содержимого в точках.
    pub content: Size,
    /// Размер снимка в пикселях.
    pub pixels: (u32, u32),
}

impl Geometry {
    /// Сверка окна, дерева и снимка: одно окно, содержимое во весь кадр окна, размер PNG — кадр
    /// окна, умноженный на `backingScaleFactor`.
    pub fn new(
        window: &WindowView,
        inspect: &InspectView,
        png: (u32, u32),
    ) -> Result<Geometry, Error> {
        if window.window_number != inspect.window_number {
            return Err(Error::new(format!(
                "ui.window окна {}, ui.inspect окна {}",
                window.window_number, inspect.window_number
            )));
        }
        let content = inspect.content;
        let frame = window.frame;
        if (content.width - frame.width).abs() > EPSILON
            || (content.height - frame.height).abs() > EPSILON
        {
            return Err(Error::new(format!(
                "содержимое {}×{} не равно кадру окна {}×{}",
                content.width, content.height, frame.width, frame.height
            )));
        }
        let scale = window.backing_scale_factor;
        let expected = (frame.width * scale, frame.height * scale);
        let matches = |pixels: u32, points: f64| (f64::from(pixels) - points).abs() < 0.5;
        if !(matches(png.0, expected.0) && matches(png.1, expected.1)) {
            return Err(Error::new(format!(
                "размер PNG {}×{} не равен кадру окна {}×{} × масштаб {scale}",
                png.0, png.1, frame.width, frame.height
            )));
        }
        Ok(Geometry {
            scale,
            content,
            pixels: png,
        })
    }

    /// Кадр в точках окна (начало внизу слева) — в точках снимка (начало сверху слева).
    pub fn flip(&self, frame: Rect) -> Rect {
        Rect::new(
            frame.x,
            self.content.height - frame.y - frame.height,
            frame.width,
            frame.height,
        )
    }

    /// Прямоугольник в точках снимка — в пиксели внутрь: только пиксели целиком в нём.
    pub fn to_pixels(&self, rect: Rect) -> Result<PixelRect, Error> {
        let s = self.scale;
        let x0 = (rect.x * s - EPSILON).ceil();
        let y0 = (rect.y * s - EPSILON).ceil();
        let x1 = (rect.right() * s + EPSILON).floor();
        let y1 = (rect.bottom() * s + EPSILON).floor();
        let (width, height) = (f64::from(self.pixels.0), f64::from(self.pixels.1));
        if !(x0 >= 0.0 && y0 >= 0.0 && x1 <= width && y1 <= height) {
            return Err(Error::new(format!(
                "участок за кадром снимка: {rect:?} в точках, снимок {}×{} пикселей",
                self.pixels.0, self.pixels.1
            )));
        }
        if x1 <= x0 || y1 <= y0 {
            return Err(Error::new(format!(
                "участок {rect:?} не содержит ни одного пикселя"
            )));
        }
        // Границы проверены: значения целые и в пределах размеров снимка.
        Ok(PixelRect {
            x: x0 as u32,
            y: y0 as u32,
            width: (x1 - x0) as u32,
            height: (y1 - y0) as u32,
        })
    }

    /// Компонент целиком в содержимом окна.
    pub fn check_inside(&self, component: &Component) -> Result<(), Error> {
        let frame = component.frame;
        let inside = frame.is_finite()
            && frame.width > 0.0
            && frame.height > 0.0
            && frame.x >= -EPSILON
            && frame.y >= -EPSILON
            && frame.right() <= self.content.width + EPSILON
            && frame.bottom() <= self.content.height + EPSILON;
        if inside {
            Ok(())
        } else {
            Err(Error::new(format!(
                "компонент {} за кадром окна: {frame:?}",
                component.id
            )))
        }
    }
}

/// Участок ожиданий в точках снимка после проверок положения.
#[derive(Debug, Clone, Copy)]
pub struct Placed {
    /// В точках снимка, начало сверху слева.
    pub points: Rect,
    pub pixels: PixelRect,
}

/// Положение участка: внутри видимого компонента с отступом [`MIN_INSET`] от его краёв и не
/// ближе [`MIN_INSET`] к любому видимому узлу, кроме самого компонента и его предков.
pub fn place(region: &Region, inspect: &InspectView, geometry: &Geometry) -> Result<Placed, Error> {
    let component = inspect.component(&region.component)?;
    if !component.visible {
        return Err(Error::new(format!(
            "участок {}: компонент {} не видим",
            region.id, component.id
        )));
    }
    geometry.check_inside(component)?;
    let rect = region.rect;
    if !rect.is_finite() || rect.width <= 0.0 || rect.height <= 0.0 {
        return Err(Error::new(format!(
            "участок {}: неверный прямоугольник {rect:?}",
            region.id
        )));
    }
    let frame = component.frame;
    let inset_ok = rect.x >= MIN_INSET - EPSILON
        && rect.y >= MIN_INSET - EPSILON
        && rect.right() <= frame.width - MIN_INSET + EPSILON
        && rect.bottom() <= frame.height - MIN_INSET + EPSILON;
    if !inset_ok {
        return Err(Error::new(format!(
            "участок {} за кадром компонента {} {}×{} или ближе {MIN_INSET} точек к краю: {rect:?}",
            region.id, component.id, frame.width, frame.height
        )));
    }
    let top = geometry.flip(frame);
    let points = Rect::new(top.x + rect.x, top.y + rect.y, rect.width, rect.height);
    let guard = points.grown(MIN_INSET - EPSILON);
    for other in &inspect.components {
        if !other.visible || inspect.is_self_or_ancestor(&other.id, component) {
            continue;
        }
        if guard.intersects(&geometry.flip(other.frame)) {
            return Err(Error::new(format!(
                "участок {} ближе {MIN_INSET} точек к узлу {}",
                region.id, other.id
            )));
        }
    }
    let pixels = geometry.to_pixels(points)?;
    Ok(Placed { points, pixels })
}

#[cfg(test)]
mod tests {
    use super::*;

    type Result<T> = std::result::Result<T, Error>;

    fn component(id: &str, parent: Option<&str>, frame: Rect) -> Component {
        Component {
            id: id.to_owned(),
            parent: parent.map(str::to_owned),
            frame,
            visible: true,
        }
    }

    fn inspect(components: Vec<Component>) -> InspectView {
        InspectView {
            window_number: 1,
            content: Size {
                width: 100.0,
                height: 50.0,
            },
            generation: 1,
            components,
        }
    }

    fn geometry(scale: f64) -> Geometry {
        Geometry {
            scale,
            content: Size {
                width: 100.0,
                height: 50.0,
            },
            pixels: ((100.0 * scale) as u32, (50.0 * scale) as u32),
        }
    }

    fn region(component: &str, rect: Rect) -> Region {
        Region {
            id: "r".to_owned(),
            component: component.to_owned(),
            rect,
            slot: "chrome".to_owned(),
            layer: Layer::None,
        }
    }

    /// Кадр с началом внизу слева у верхнего края окна даёт верхние строки снимка.
    #[test]
    fn frames_flip_to_top_rows_and_scale_to_pixels() -> Result<()> {
        let tree = inspect(vec![component(
            "top",
            None,
            Rect::new(10.0, 40.0, 30.0, 10.0),
        )]);
        let placed = place(
            &region("top", Rect::new(4.0, 4.0, 10.0, 2.0)),
            &tree,
            &geometry(2.0),
        )?;
        assert_eq!(placed.points, Rect::new(14.0, 4.0, 10.0, 2.0));
        assert_eq!(
            placed.pixels,
            PixelRect {
                x: 28,
                y: 8,
                width: 20,
                height: 4
            }
        );
        let placed = place(
            &region("top", Rect::new(4.0, 4.0, 10.0, 2.0)),
            &tree,
            &geometry(1.0),
        )?;
        assert_eq!(
            placed.pixels,
            PixelRect {
                x: 14,
                y: 4,
                width: 10,
                height: 2
            }
        );
        Ok(())
    }

    #[test]
    fn fractional_points_round_inward() -> Result<()> {
        let pixels = geometry(1.5).to_pixels(Rect::new(1.0, 1.0, 3.0, 3.0))?;
        // 1,5…6 пикселей: целиком внутри — 2…6.
        assert_eq!(
            pixels,
            PixelRect {
                x: 2,
                y: 2,
                width: 4,
                height: 4
            }
        );
        Ok(())
    }

    #[test]
    fn region_must_keep_its_inset_and_stay_away_from_other_nodes() {
        let tree = inspect(vec![
            component("bar", None, Rect::new(0.0, 30.0, 100.0, 20.0)),
            component("bar.button", Some("bar"), Rect::new(50.0, 35.0, 10.0, 10.0)),
        ]);
        let g = geometry(2.0);
        assert!(place(&region("bar", Rect::new(4.0, 4.0, 30.0, 12.0)), &tree, &g).is_ok());
        assert!(place(&region("bar", Rect::new(3.0, 4.0, 30.0, 12.0)), &tree, &g).is_err());
        assert!(place(&region("bar", Rect::new(4.0, 4.0, 30.0, 13.0)), &tree, &g).is_err());
        // Правый край участка в 4 точках от кнопки — можно, в 3 — нельзя.
        assert!(place(&region("bar", Rect::new(4.0, 4.0, 42.0, 12.0)), &tree, &g).is_ok());
        assert!(place(&region("bar", Rect::new(4.0, 4.0, 43.0, 12.0)), &tree, &g).is_err());
        // Родитель участку не мешает.
        assert!(
            place(
                &region("bar.button", Rect::new(4.0, 4.0, 2.0, 2.0)),
                &tree,
                &g
            )
            .is_ok()
        );
        assert!(place(&region("nothing", Rect::new(4.0, 4.0, 2.0, 2.0)), &tree, &g).is_err());
    }

    #[test]
    fn invisible_or_outside_component_is_an_error() {
        let mut hidden = component("hidden", None, Rect::new(0.0, 0.0, 50.0, 50.0));
        hidden.visible = false;
        let outside = component("outside", None, Rect::new(80.0, 0.0, 50.0, 50.0));
        let tree = inspect(vec![hidden, outside]);
        let g = geometry(2.0);
        assert!(place(&region("hidden", Rect::new(4.0, 4.0, 2.0, 2.0)), &tree, &g).is_err());
        assert!(place(&region("outside", Rect::new(4.0, 4.0, 2.0, 2.0)), &tree, &g).is_err());
    }

    #[test]
    fn response_bodies_are_read_from_frames_or_bare() -> Result<()> {
        let bare = r#"{"window_number": 7, "content": {"width": 10.0, "height": 5.0},
            "generation": 3, "components": [], "materials": []}"#;
        assert_eq!(parse_inspect(bare)?.generation, 3);
        let frame = format!(r#"{{"id": 1, "ok": {{"ui": {{"inspect": {bare}}}}}}}"#);
        assert_eq!(parse_inspect(&frame)?.window_number, 7);
        assert!(parse_inspect(r#"{"id": 1, "ok": {"ui": {"window": {}}}}"#).is_err());
        assert!(parse_inspect(r#"{"id": 1, "error": {"code": "no_ui"}}"#).is_err());
        assert!(parse_inspect("не json").is_err());
        Ok(())
    }

    #[test]
    fn expectations_are_strict() {
        let good = r#"{"cell": {"appearance": "dark", "contrast": "normal"}, "regions": [
            {"id": "a", "component": "chrome", "rect": {"x": 4, "y": 4, "width": 2, "height": 2},
             "slot": "chrome"}]}"#;
        assert!(parse_expectations(good).is_ok());
        let typo = good.replace("\"slot\"", "\"slt\"");
        assert!(parse_expectations(&typo).is_err());
        let twice = r#"{"cell": {"appearance": "dark", "contrast": "normal"}, "regions": [
            {"id": "a", "component": "c", "rect": {"x": 4, "y": 4, "width": 2, "height": 2}, "slot": "chrome"},
            {"id": "a", "component": "c", "rect": {"x": 4, "y": 4, "width": 2, "height": 2}, "slot": "chrome"}]}"#;
        assert!(parse_expectations(twice).is_err());
        let empty = r#"{"cell": {"appearance": "dark", "contrast": "normal"}, "regions": []}"#;
        assert!(parse_expectations(empty).is_err());
        let layer = good.replace(
            "\"slot\": \"chrome\"",
            "\"slot\": \"panel\", \"layer\": \"glass\"",
        );
        assert!(parse_expectations(&layer).is_err());
    }
}
