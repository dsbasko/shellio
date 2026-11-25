//! Набор значков прототипа (`prototype/js/core/util.js`, таблица `P`): фрагменты SVG в окне
//! 16×16 и их разбор в контуры. Тест `tests/icons.rs` сверяет таблицу с прототипом по имени и
//! фрагменту в обе стороны, поэтому значки продукта не расходятся с эталоном.
//!
//! Значок рисуется обводкой толщиной [`STROKE`] со скруглёнными концами и стыками в цвете текста
//! (`S.icon` прототипа). Элемент с `fill="currentColor"` заливается, элемент с
//! `stroke="var(--line-2)"` обводится цветом линии.

/// Сторона окна просмотра значка.
pub const VIEW_BOX: f64 = 16.0;
/// Толщина обводки в единицах окна просмотра.
pub const STROKE: f64 = 1.4;
/// Значок для неизвестного имени (`P[name] || P.file` прототипа).
pub const FALLBACK: &str = "file";

/// Шаг контура в координатах окна просмотра, ось y вниз.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Op {
    Move(f64, f64),
    Line(f64, f64),
    Cubic {
        c1: (f64, f64),
        c2: (f64, f64),
        to: (f64, f64),
    },
    Close,
}

/// Чем рисуется контур.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Paint {
    /// Обводка цветом текста.
    Stroke,
    /// Обводка цветом `--line-2`: второй тон двухцветного значка.
    StrokeLine,
    /// Заливка цветом текста.
    Fill,
}

/// Один элемент значка.
#[derive(Clone, Debug, PartialEq)]
pub struct Shape {
    pub paint: Paint,
    pub ops: Vec<Op>,
}

/// Имена значков по алфавиту.
pub fn names() -> impl Iterator<Item = &'static str> {
    ICONS.iter().map(|(name, _)| *name)
}

/// Фрагмент SVG значка `name`, как в прототипе.
pub fn fragment(name: &str) -> Option<&'static str> {
    ICONS
        .binary_search_by(|(known, _)| (*known).cmp(name))
        .ok()
        .map(|index| ICONS[index].1)
}

/// Контуры значка `name`; `None` — имени в наборе нет.
pub fn shapes(name: &str) -> Option<Vec<Shape>> {
    fragment(name).and_then(parse_fragment)
}

/// Контуры значка `name`, для неизвестного имени — значка [`FALLBACK`].
pub fn shapes_or_fallback(name: &str) -> Vec<Shape> {
    shapes(name)
        .or_else(|| shapes(FALLBACK))
        .unwrap_or_default()
}

/// Разбирает фрагмент SVG из элементов `path`, `rect` и `circle`. `None` — неизвестный элемент
/// или испорченный атрибут.
fn parse_fragment(fragment: &str) -> Option<Vec<Shape>> {
    let mut shapes = Vec::new();
    let mut rest = fragment.trim();
    while !rest.is_empty() {
        let body = rest.strip_prefix('<')?;
        let end = body.find("/>")?;
        let element = &body[..end];
        rest = body[end + 2..].trim_start();
        let (tag, attributes) = match element.find(char::is_whitespace) {
            Some(split) => (&element[..split], &element[split..]),
            None => (element, ""),
        };
        let attributes = parse_attributes(attributes)?;
        let get = |name: &str| {
            attributes
                .iter()
                .find(|(known, _)| *known == name)
                .map(|(_, value)| *value)
        };
        let number = |name: &str| {
            get(name)
                .and_then(|value| value.parse::<f64>().ok())
                .filter(|value| value.is_finite())
        };
        let size = |name: &str| number(name).filter(|value| *value >= 0.0);
        let paint = match (get("fill"), get("stroke")) {
            (Some("currentColor"), Some("none")) => Paint::Fill,
            (None, None) => Paint::Stroke,
            (None, Some("var(--line-2)")) => Paint::StrokeLine,
            _ => return None,
        };
        let ops = match tag {
            "path" => parse_path(get("d")?)?,
            "rect" => {
                let radius = match get("rx") {
                    Some(_) => size("rx")?,
                    None => 0.0,
                };
                rounded_rect(
                    number("x")?,
                    number("y")?,
                    size("width")?,
                    size("height")?,
                    radius,
                )
            }
            "circle" => circle(number("cx")?, number("cy")?, size("r")?),
            _ => return None,
        };
        shapes.push(Shape { paint, ops });
    }
    Some(shapes)
}

fn parse_attributes(mut rest: &str) -> Option<Vec<(&str, &str)>> {
    let mut attributes = Vec::new();
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            return Some(attributes);
        }
        let equals = rest.find("=\"")?;
        let name = &rest[..equals];
        let value_start = equals + 2;
        let value_len = rest[value_start..].find('"')?;
        attributes.push((name, &rest[value_start..value_start + value_len]));
        rest = &rest[value_start + value_len + 1..];
    }
}

/// Доля радиуса до управляющей точки четверти окружности.
const KAPPA: f64 = 0.552_284_749_830_793_6;

fn circle(cx: f64, cy: f64, r: f64) -> Vec<Op> {
    let k = KAPPA * r;
    vec![
        Op::Move(cx + r, cy),
        Op::Cubic {
            c1: (cx + r, cy + k),
            c2: (cx + k, cy + r),
            to: (cx, cy + r),
        },
        Op::Cubic {
            c1: (cx - k, cy + r),
            c2: (cx - r, cy + k),
            to: (cx - r, cy),
        },
        Op::Cubic {
            c1: (cx - r, cy - k),
            c2: (cx - k, cy - r),
            to: (cx, cy - r),
        },
        Op::Cubic {
            c1: (cx + k, cy - r),
            c2: (cx + r, cy - k),
            to: (cx + r, cy),
        },
        Op::Close,
    ]
}

fn rounded_rect(x: f64, y: f64, width: f64, height: f64, radius: f64) -> Vec<Op> {
    let r = radius.min(width.min(height) / 2.0).max(0.0);
    let (right, bottom) = (x + width, y + height);
    if r == 0.0 {
        return vec![
            Op::Move(x, y),
            Op::Line(right, y),
            Op::Line(right, bottom),
            Op::Line(x, bottom),
            Op::Close,
        ];
    }
    let k = r * (1.0 - KAPPA);
    vec![
        Op::Move(x + r, y),
        Op::Line(right - r, y),
        Op::Cubic {
            c1: (right - k, y),
            c2: (right, y + k),
            to: (right, y + r),
        },
        Op::Line(right, bottom - r),
        Op::Cubic {
            c1: (right, bottom - k),
            c2: (right - k, bottom),
            to: (right - r, bottom),
        },
        Op::Line(x + r, bottom),
        Op::Cubic {
            c1: (x + k, bottom),
            c2: (x, bottom - k),
            to: (x, bottom - r),
        },
        Op::Line(x, y + r),
        Op::Cubic {
            c1: (x, y + k),
            c2: (x + k, y),
            to: (x + r, y),
        },
        Op::Close,
    ]
}

struct Scanner<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Scanner<'_> {
    fn skip_separators(&mut self) {
        while self
            .bytes
            .get(self.at)
            .is_some_and(|byte| byte.is_ascii_whitespace() || *byte == b',')
        {
            self.at += 1;
        }
    }

    fn command(&mut self) -> Option<u8> {
        self.skip_separators();
        let byte = *self.bytes.get(self.at)?;
        byte.is_ascii_alphabetic().then(|| {
            self.at += 1;
            byte
        })
    }

    fn done(&mut self) -> bool {
        self.skip_separators();
        self.at >= self.bytes.len()
    }

    /// Число SVG: знак, целая часть, одна дробная часть, показатель. Вторая точка начинает
    /// следующее число.
    fn number(&mut self) -> Option<f64> {
        self.skip_separators();
        let start = self.at;
        let peek = |at: usize| self.bytes.get(at).copied();
        let mut at = start;
        if matches!(peek(at), Some(b'+' | b'-')) {
            at += 1;
        }
        let digits_start = at;
        while peek(at).is_some_and(|byte| byte.is_ascii_digit()) {
            at += 1;
        }
        if peek(at) == Some(b'.') {
            at += 1;
            while peek(at).is_some_and(|byte| byte.is_ascii_digit()) {
                at += 1;
            }
        }
        if at == digits_start || (at == digits_start + 1 && peek(digits_start) == Some(b'.')) {
            return None;
        }
        if matches!(peek(at), Some(b'e' | b'E')) {
            let mut exponent = at + 1;
            if matches!(peek(exponent), Some(b'+' | b'-')) {
                exponent += 1;
            }
            if peek(exponent).is_some_and(|byte| byte.is_ascii_digit()) {
                while peek(exponent).is_some_and(|byte| byte.is_ascii_digit()) {
                    exponent += 1;
                }
                at = exponent;
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..at]).ok()?;
        let value = text.parse::<f64>().ok()?;
        self.at = at;
        Some(value)
    }

    /// Флаг дуги — одна цифра 0 или 1, в том числе слитно со следующим числом.
    fn flag(&mut self) -> Option<bool> {
        self.skip_separators();
        let flag = match self.bytes.get(self.at)? {
            b'0' => false,
            b'1' => true,
            _ => return None,
        };
        self.at += 1;
        Some(flag)
    }

    fn pair(&mut self) -> Option<(f64, f64)> {
        Some((self.number()?, self.number()?))
    }
}

/// Разбирает атрибут `d`: команды `M L H V C S Q T A Z` в обоих регистрах с неявным повтором.
/// Квадратичные кривые и дуги переводятся в кубические.
fn parse_path(d: &str) -> Option<Vec<Op>> {
    let mut scanner = Scanner {
        bytes: d.as_bytes(),
        at: 0,
    };
    let mut ops = Vec::new();
    let (mut x, mut y) = (0.0_f64, 0.0_f64);
    let (mut start_x, mut start_y) = (0.0_f64, 0.0_f64);
    // Управляющая точка прошлой кривой для гладких `S` и `T`.
    let mut cubic_control: Option<(f64, f64)> = None;
    let mut quad_control: Option<(f64, f64)> = None;
    let mut command = 0_u8;
    while !scanner.done() {
        if let Some(next) = scanner.command() {
            command = next;
        } else {
            // Неявный повтор: после `M` идут отрезки.
            command = match command {
                b'M' => b'L',
                b'm' => b'l',
                0 | b'Z' | b'z' => return None,
                other => other,
            };
        }
        let relative = command.is_ascii_lowercase();
        let base = if relative { (x, y) } else { (0.0, 0.0) };
        let mut next_cubic = None;
        let mut next_quad = None;
        match command.to_ascii_uppercase() {
            b'M' => {
                let (px, py) = scanner.pair()?;
                (x, y) = (base.0 + px, base.1 + py);
                (start_x, start_y) = (x, y);
                ops.push(Op::Move(x, y));
            }
            b'L' => {
                let (px, py) = scanner.pair()?;
                (x, y) = (base.0 + px, base.1 + py);
                ops.push(Op::Line(x, y));
            }
            b'H' => {
                x = base.0 + scanner.number()?;
                ops.push(Op::Line(x, y));
            }
            b'V' => {
                y = base.1 + scanner.number()?;
                ops.push(Op::Line(x, y));
            }
            b'C' | b'S' => {
                let c1 = if command.eq_ignore_ascii_case(&b'C') {
                    let (px, py) = scanner.pair()?;
                    (base.0 + px, base.1 + py)
                } else {
                    cubic_control.map_or((x, y), |(cx, cy)| (2.0 * x - cx, 2.0 * y - cy))
                };
                let (px, py) = scanner.pair()?;
                let c2 = (base.0 + px, base.1 + py);
                let (px, py) = scanner.pair()?;
                let to = (base.0 + px, base.1 + py);
                ops.push(Op::Cubic { c1, c2, to });
                next_cubic = Some(c2);
                (x, y) = to;
            }
            b'Q' | b'T' => {
                let control = if command.eq_ignore_ascii_case(&b'Q') {
                    let (px, py) = scanner.pair()?;
                    (base.0 + px, base.1 + py)
                } else {
                    quad_control.map_or((x, y), |(cx, cy)| (2.0 * x - cx, 2.0 * y - cy))
                };
                let (px, py) = scanner.pair()?;
                let to = (base.0 + px, base.1 + py);
                ops.push(Op::Cubic {
                    c1: (
                        x + 2.0 / 3.0 * (control.0 - x),
                        y + 2.0 / 3.0 * (control.1 - y),
                    ),
                    c2: (
                        to.0 + 2.0 / 3.0 * (control.0 - to.0),
                        to.1 + 2.0 / 3.0 * (control.1 - to.1),
                    ),
                    to,
                });
                next_quad = Some(control);
                (x, y) = to;
            }
            b'A' => {
                let (rx, ry) = scanner.pair()?;
                let rotation = scanner.number()?;
                let large = scanner.flag()?;
                let sweep = scanner.flag()?;
                let (px, py) = scanner.pair()?;
                let to = (base.0 + px, base.1 + py);
                arc(&mut ops, (x, y), (rx, ry), rotation, large, sweep, to);
                (x, y) = to;
            }
            b'Z' => {
                ops.push(Op::Close);
                (x, y) = (start_x, start_y);
            }
            _ => return None,
        }
        cubic_control = next_cubic;
        quad_control = next_quad;
    }
    Some(ops)
}

/// Дуга эллипса из конечных точек в кубические кривые (SVG 1.1, приложение F.6.5).
fn arc(
    ops: &mut Vec<Op>,
    from: (f64, f64),
    radii: (f64, f64),
    rotation_degrees: f64,
    large: bool,
    sweep: bool,
    to: (f64, f64),
) {
    let (mut rx, mut ry) = (radii.0.abs(), radii.1.abs());
    if from == to {
        return;
    }
    if rx == 0.0 || ry == 0.0 {
        ops.push(Op::Line(to.0, to.1));
        return;
    }
    let (sin, cos) = rotation_degrees.to_radians().sin_cos();
    let (dx, dy) = ((from.0 - to.0) / 2.0, (from.1 - to.1) / 2.0);
    let (x1, y1) = (cos * dx + sin * dy, -sin * dx + cos * dy);
    let lambda = (x1 * x1) / (rx * rx) + (y1 * y1) / (ry * ry);
    if lambda > 1.0 {
        let scale = lambda.sqrt();
        rx *= scale;
        ry *= scale;
    }
    let numerator = rx * rx * ry * ry - rx * rx * y1 * y1 - ry * ry * x1 * x1;
    let denominator = rx * rx * y1 * y1 + ry * ry * x1 * x1;
    let mut factor = (numerator / denominator).max(0.0).sqrt();
    if large == sweep {
        factor = -factor;
    }
    let (cxp, cyp) = (factor * rx * y1 / ry, -factor * ry * x1 / rx);
    let (cx, cy) = (
        cos * cxp - sin * cyp + (from.0 + to.0) / 2.0,
        sin * cxp + cos * cyp + (from.1 + to.1) / 2.0,
    );
    let start = ((y1 - cyp) / ry).atan2((x1 - cxp) / rx);
    let end = ((-y1 - cyp) / ry).atan2((-x1 - cxp) / rx);
    let mut delta = end - start;
    if sweep && delta < 0.0 {
        delta += std::f64::consts::TAU;
    } else if !sweep && delta > 0.0 {
        delta -= std::f64::consts::TAU;
    }
    let segments = (delta.abs() / std::f64::consts::FRAC_PI_2).ceil().max(1.0);
    let step = delta / segments;
    let handle = 4.0 / 3.0 * (step / 4.0).tan();
    let point = |u: f64, v: f64| {
        (
            cx + rx * u * cos - ry * v * sin,
            cy + rx * u * sin + ry * v * cos,
        )
    };
    let count = segments as usize;
    for index in 0..count {
        let a1 = start + step * index as f64;
        let a2 = a1 + step;
        let (s1, c1) = a1.sin_cos();
        let (s2, c2) = a2.sin_cos();
        let end_point = if index + 1 == count {
            to
        } else {
            point(c2, s2)
        };
        ops.push(Op::Cubic {
            c1: point(c1 - handle * s1, s1 + handle * c1),
            c2: point(c2 + handle * s2, s2 - handle * c2),
            to: end_point,
        });
    }
}

/// Таблица `P` прототипа по алфавиту имён. Вручную не правится: значок меняется в прототипе, затем
/// здесь, сверку делает `tests/icons.rs`.
const ICONS: &[(&str, &str)] = &[
    (
        "agent",
        r#"<path d="M8 2.2v11.6M2.2 8h11.6M3.9 3.9l8.2 8.2M12.1 3.9l-8.2 8.2"/>"#,
    ),
    (
        "book",
        r#"<path d="M2.5 3c2.2-.8 4-.5 5.5.8 1.5-1.3 3.3-1.6 5.5-.8v10c-2.2-.8-4-.5-5.5.8-1.5-1.3-3.3-1.6-5.5-.8z"/><path d="M8 3.8v10"/>"#,
    ),
    (
        "branch",
        r#"<circle cx="5" cy="3.8" r="1.5"/><circle cx="5" cy="12.2" r="1.5"/><circle cx="11" cy="5.8" r="1.5"/><path d="M5 5.3v5.4M11 7.3c0 2.2-2.4 2.6-6 3.4"/>"#,
    ),
    (
        "changes",
        r#"<path d="M8 2.5v5M5.5 5h5M5.5 11.5h5"/><rect x="2.5" y="1.5" width="11" height="13" rx="2.2"/>"#,
    ),
    ("check", r#"<path d="m3 8.5 3.4 3.4L13 4.8"/>"#),
    ("chevD", r#"<path d="M4 6l4 4 4-4"/>"#),
    ("chevL", r#"<path d="M10 3.5 5.5 8l4.5 4.5"/>"#),
    ("chevR", r#"<path d="M6 4l4 4-4 4"/>"#),
    (
        "clock",
        r#"<circle cx="8" cy="8" r="6"/><path d="M8 4.5V8l2.3 1.6"/>"#,
    ),
    ("close", r#"<path d="M4.5 4.5l7 7M11.5 4.5l-7 7"/>"#),
    ("cmd", r#"<path d="M5 4.5L8.5 8 5 11.5M9.5 11.5H13"/>"#),
    (
        "commit",
        r#"<circle cx="8" cy="8" r="2.6"/><path d="M1.5 8h3.9M10.6 8h3.9"/>"#,
    ),
    (
        "cpu",
        r#"<rect x="4" y="4" width="8" height="8" rx="1.5"/><path d="M6.5 1.8V4M9.5 1.8V4M6.5 12v2.2M9.5 12v2.2M1.8 6.5H4M1.8 9.5H4M12 6.5h2.2M12 9.5h2.2"/>"#,
    ),
    (
        "draft",
        r#"<path d="M3 13.2l.6-2.8 7.1-7.1a1.4 1.4 0 0 1 2 2l-7.1 7.1z"/><path d="M9.6 4.4l2 2"/>"#,
    ),
    (
        "drop",
        r#"<path d="M8 2v8M4.5 7 8 10.5 11.5 7"/><path d="M2.5 13.5h11"/>"#,
    ),
    (
        "errc",
        r#"<circle cx="8" cy="8" r="6.2"/><path d="M5.9 5.9l4.2 4.2M10.1 5.9l-4.2 4.2"/>"#,
    ),
    (
        "expand",
        r#"<path d="M9.5 2.5h4v4M6.5 13.5h-4v-4M13.5 2.5 9 7M2.5 13.5 7 9"/>"#,
    ),
    (
        "eye",
        r#"<path d="M1.5 8S4 3.5 8 3.5 14.5 8 14.5 8 12 12.5 8 12.5 1.5 8 1.5 8z"/><circle cx="8" cy="8" r="1.9"/>"#,
    ),
    ("file", r#"<path d="M4 2h5l3 3v9H4z"/><path d="M9 2v3h3"/>"#),
    ("filter", r#"<path d="M2 3.5h12L9.5 9v4l-3 1.2V9z"/>"#),
    ("folder", r#"<path d="M2 4.5h4.2l1.5 1.5H14v6.5H2z"/>"#),
    (
        "gear",
        r#"<circle cx="8" cy="8" r="2.2"/><path d="M8 1.8v1.6M8 12.6v1.6M1.8 8h1.6M12.6 8h1.6M3.6 3.6 4.8 4.8M11.2 11.2l1.2 1.2M3.6 12.4l1.2-1.2M11.2 4.8l1.2-1.2"/>"#,
    ),
    (
        "github",
        r#"<path d="M8 1.8a6.2 6.2 0 0 0-2 12c.3 0 .4-.1.4-.3v-1.2c-1.7.4-2-.7-2-.7-.3-.7-.7-.9-.7-.9-.6-.4 0-.4 0-.4.6 0 1 .6 1 .6.5.9 1.4.7 1.8.5 0-.4.2-.7.4-.9-1.4-.1-2.8-.7-2.8-3.1 0-.7.2-1.2.6-1.7-.1-.1-.3-.8 0-1.6 0 0 .5-.2 1.7.6a6 6 0 0 1 3.2 0c1.2-.8 1.7-.6 1.7-.6.3.8.1 1.5 0 1.6.4.5.6 1 .6 1.7 0 2.4-1.4 3-2.8 3.1.2.2.4.6.4 1.2v1.8c0 .2.1.4.4.3a6.2 6.2 0 0 0-2-12Z"/>"#,
    ),
    (
        "gitlab",
        r#"<path d="m8 14-6-5 1.8-6.5L5.8 7h4.4l2-4.5L14 9z"/>"#,
    ),
    (
        "grid",
        r#"<rect x="2.5" y="2.5" width="11" height="11" rx="2"/><path d="M2.5 7h11M7 7v6.5"/>"#,
    ),
    (
        "help",
        r#"<circle cx="8" cy="8" r="6"/><path d="M6.2 6.2A1.9 1.9 0 0 1 8 4.8c1.1 0 1.9.7 1.9 1.7 0 1.4-1.9 1.5-1.9 2.9M8 11.4h.01"/>"#,
    ),
    (
        "history",
        r#"<circle cx="5" cy="3" r="1.4"/><circle cx="5" cy="8" r="1.4"/><circle cx="5" cy="13" r="1.4"/><circle cx="11.5" cy="5.5" r="1.4"/><path d="M5 4.4v2.2M5 9.4v2.2M11.5 6.9c0 2.5-3.8 3.2-5.6 5"/>"#,
    ),
    (
        "hub",
        r#"<path d="M2 3.5c0-.6.4-1 1-1h6.5c.6 0 1 .4 1 1V7c0 .6-.4 1-1 1H6L3.8 9.8V8H3c-.6 0-1-.4-1-1z"/><path d="M12.5 6H13c.6 0 1 .4 1 1v3.5c0 .6-.4 1-1 1h-.8v1.7L10 11.5H8"/>"#,
    ),
    ("idlec", r#"<circle cx="8" cy="8" r="6.2"/>"#),
    (
        "info",
        r#"<circle cx="8" cy="8" r="6"/><path d="M8 7.2v3.6M8 5h.01"/>"#,
    ),
    (
        "keys",
        r#"<rect x="1.5" y="4" width="13" height="8" rx="1.8"/><path d="M4.5 6.8h.01M7 6.8h.01M9.5 6.8h.01M12 6.8h.01M5 9.5h6"/>"#,
    ),
    (
        "label",
        r#"<path d="M2.5 3.5h5.2l5.8 5.8-4.2 4.2-5.8-5.8z"/><path d="M5.5 6.2h.01"/>"#,
    ),
    (
        "link",
        r#"<path d="M6.8 9.2a2.6 2.6 0 0 0 3.7 0l2.2-2.2a2.6 2.6 0 0 0-3.7-3.7L8 4.3"/><path d="M9.2 6.8a2.6 2.6 0 0 0-3.7 0L3.3 9a2.6 2.6 0 0 0 3.7 3.7l1-1"/>"#,
    ),
    (
        "lock",
        r#"<rect x="3.5" y="7" width="9" height="6.5" rx="1.5"/><path d="M5.5 7V5a2.5 2.5 0 0 1 5 0v2"/>"#,
    ),
    (
        "merge",
        r#"<circle cx="4" cy="3.5" r="1.6"/><circle cx="4" cy="12.5" r="1.6"/><circle cx="12" cy="12.5" r="1.6"/><path d="M4 5v6M4 6c.5 3 4 5 6.4 6.4"/>"#,
    ),
    ("minus", r#"<path d="M4 8h8"/>"#),
    (
        "moon",
        r#"<path d="M13.2 9.6A5.4 5.4 0 0 1 6.4 2.8a5.4 5.4 0 1 0 6.8 6.8z"/>"#,
    ),
    (
        "more",
        r#"<circle cx="3.5" cy="8" r="1.2" fill="currentColor" stroke="none"/><circle cx="8" cy="8" r="1.2" fill="currentColor" stroke="none"/><circle cx="12.5" cy="8" r="1.2" fill="currentColor" stroke="none"/>"#,
    ),
    (
        "okc",
        r#"<circle cx="8" cy="8" r="6.2"/><path d="M5.3 8.2l1.8 1.8 3.6-3.8"/>"#,
    ),
    (
        "out",
        r#"<path d="M6.5 3H3.5C3 3 2.5 3.5 2.5 4v8.5c0 .5.5 1 1 1H12c.5 0 1-.5 1-1v-3"/><path d="M9.5 2.5h4v4M13.5 2.5 7.5 8.5"/>"#,
    ),
    (
        "pin",
        r#"<path d="m9.5 2 4.5 4.5-2 .7-2.7 2.7-.2 2.6L3.3 7.7l2.6-.2 2.7-2.7z"/><path d="M5.5 10.5 2.2 13.8"/>"#,
    ),
    (
        "pipeline",
        r#"<circle cx="3.5" cy="8" r="1.8"/><circle cx="12.5" cy="8" r="1.8"/><path d="M5.3 8h5.4"/>"#,
    ),
    ("play", r#"<path d="M5 3.2 12.5 8 5 12.8z"/>"#),
    ("plus", r#"<path d="M4 8h8M8 4v8"/>"#),
    (
        "refresh",
        r#"<path d="M13.5 8A5.5 5.5 0 1 1 11.8 4"/><path d="M12 1.5V4.4H9.1"/>"#,
    ),
    (
        "retry",
        r#"<path d="M12.5 8a4.5 4.5 0 1 1-1.3-3.2M12.5 3v2.6H9.9"/>"#,
    ),
    (
        "review",
        r#"<path d="M2.5 3.5c0-.6.4-1 1-1h9c.6 0 1 .4 1 1v7c0 .6-.4 1-1 1H7l-3 2.5v-2.5h-.5c-.6 0-1-.4-1-1z"/><path d="M5.5 6h5M5.5 8.5h3"/>"#,
    ),
    (
        "runc",
        r#"<circle cx="8" cy="8" r="6.2" stroke="var(--line-2)"/><path d="M8 1.8a6.2 6.2 0 0 1 6.2 6.2"/><circle cx="8" cy="8" r="1.6" fill="currentColor" stroke="none"/>"#,
    ),
    (
        "scenes",
        r#"<rect x="2.5" y="2.5" width="11" height="8.5" rx="1.5"/><path d="M5 13.5h6"/><path d="m6.5 5 3 1.7-3 1.8z"/>"#,
    ),
    (
        "search",
        r#"<circle cx="7" cy="7" r="4.3"/><path d="M10.2 10.2L13.5 13.5"/>"#,
    ),
    (
        "send",
        r#"<path d="M2 8 14 2.5 11 14l-3-4.5z"/><path d="M8 9.5 14 2.5"/>"#,
    ),
    (
        "shrink",
        r#"<path d="M9 2.8V7h4.2M7 13.2V9H2.8M13.5 2.5 9 7M2.5 13.5 7 9"/>"#,
    ),
    (
        "sideB",
        r#"<rect x="2" y="3" width="12" height="10" rx="2"/><path d="M2 9.5h12"/>"#,
    ),
    (
        "sideL",
        r#"<rect x="2" y="3" width="12" height="10" rx="2"/><path d="M6 3v10"/>"#,
    ),
    (
        "sideLP",
        r#"<rect x="2" y="3" width="12" height="10" rx="2"/><path d="M6.5 3v10M9 3v10"/>"#,
    ),
    (
        "sideR",
        r#"<rect x="2" y="3" width="12" height="10" rx="2"/><path d="M10 3v10"/>"#,
    ),
    (
        "sliders",
        r#"<path d="M2.5 5h7M12 5h1.5M2.5 11H4M7 11h6.5"/><circle cx="10.5" cy="5" r="1.5"/><circle cx="5.5" cy="11" r="1.5"/>"#,
    ),
    (
        "splitCols",
        r#"<rect x="2" y="3" width="12" height="10" rx="2"/><path d="M8 3v10"/>"#,
    ),
    (
        "splitRows",
        r#"<rect x="2" y="3" width="12" height="10" rx="2"/><path d="M2 8h12"/>"#,
    ),
    (
        "stop",
        r#"<rect x="4" y="4" width="8" height="8" rx="1.4"/>"#,
    ),
    (
        "sun",
        r#"<circle cx="8" cy="8" r="2.8"/><path d="M8 1.8v1.6M8 12.6v1.6M1.8 8h1.6M12.6 8h1.6M3.6 3.6l1.1 1.1M11.3 11.3l1.1 1.1M3.6 12.4l1.1-1.1M11.3 4.7l1.1-1.1"/>"#,
    ),
    (
        "term",
        r#"<rect x="1.8" y="2.8" width="12.4" height="10.4" rx="2"/><path d="M4.8 6.3l2 1.7-2 1.7M8.6 10h2.6"/>"#,
    ),
    (
        "undo",
        r#"<path d="M5 3 2 6l3 3"/><path d="M2 6h7.5a3.5 3.5 0 0 1 0 7H6"/>"#,
    ),
    (
        "update",
        r#"<circle cx="8" cy="8" r="6"/><path d="M8 4.8v6M5.5 8.6 8 11l2.5-2.4"/>"#,
    ),
    (
        "user",
        r#"<circle cx="8" cy="5.2" r="2.7"/><path d="M2.8 13.8c0-2.8 2.3-4.6 5.2-4.6s5.2 1.8 5.2 4.6"/>"#,
    ),
    (
        "warn",
        r#"<path d="M8 2.2 14.3 13H1.7z"/><path d="M8 6.5v3M8 11.3h.01"/>"#,
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: (f64, f64), b: (f64, f64)) -> bool {
        (a.0 - b.0).abs() < 1e-9 && (a.1 - b.1).abs() < 1e-9
    }

    fn end(ops: &[Op]) -> (f64, f64) {
        match ops.last() {
            Some(Op::Move(x, y) | Op::Line(x, y)) => (*x, *y),
            Some(Op::Cubic { to, .. }) => *to,
            _ => (f64::NAN, f64::NAN),
        }
    }

    #[test]
    fn table_is_sorted_and_every_icon_parses() {
        assert!(ICONS.windows(2).all(|pair| pair[0].0 < pair[1].0));
        for (name, fragment) in ICONS {
            let shapes = parse_fragment(fragment);
            assert!(
                shapes.as_ref().is_some_and(|shapes| !shapes.is_empty()),
                "значок {name} не разобран"
            );
            for shape in shapes.unwrap_or_default() {
                assert!(matches!(shape.ops.first(), Some(Op::Move(..))), "{name}");
                for op in &shape.ops {
                    let points: Vec<(f64, f64)> = match *op {
                        Op::Move(x, y) | Op::Line(x, y) => vec![(x, y)],
                        Op::Cubic { c1, c2, to } => vec![c1, c2, to],
                        Op::Close => vec![],
                    };
                    for (x, y) in points {
                        assert!(
                            (-1.0..=VIEW_BOX + 1.0).contains(&x)
                                && (-1.0..=VIEW_BOX + 1.0).contains(&y),
                            "{name}: точка {x},{y} вне окна"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn relative_lines_and_implicit_repeats() {
        assert_eq!(
            parse_path("M4 6l4 4 4-4"),
            Some(vec![
                Op::Move(4.0, 6.0),
                Op::Line(8.0, 10.0),
                Op::Line(12.0, 6.0)
            ])
        );
        assert_eq!(
            parse_path("M8 1.8v1.6M1.8 8h1.6"),
            Some(vec![
                Op::Move(8.0, 1.8),
                Op::Line(8.0, 3.4000000000000004),
                Op::Move(1.8, 8.0),
                Op::Line(3.4000000000000004, 8.0),
            ])
        );
        assert_eq!(
            parse_path("M4 2h5l3 3v9H4z"),
            Some(vec![
                Op::Move(4.0, 2.0),
                Op::Line(9.0, 2.0),
                Op::Line(12.0, 5.0),
                Op::Line(12.0, 14.0),
                Op::Line(4.0, 14.0),
                Op::Close,
            ])
        );
    }

    #[test]
    fn numbers_glued_by_sign_and_dot_are_separate() {
        assert_eq!(
            parse_path("M1.5.5-2-3"),
            Some(vec![Op::Move(1.5, 0.5), Op::Line(-2.0, -3.0)])
        );
        assert_eq!(parse_path("M1 2x"), None);
        assert_eq!(parse_path("1 2"), None);
    }

    #[test]
    fn arc_ends_at_its_target_and_stays_on_the_circle() {
        // Полуокружность радиуса 2 вокруг (8, 8).
        let ops = parse_path("M6 8A2 2 0 0 1 10 8").unwrap_or_default();
        assert!(close(end(&ops), (10.0, 8.0)));
        assert_eq!(ops.len(), 3, "две четверти");
        let Op::Cubic { to, .. } = ops[1] else {
            panic!("ожидалась кривая");
        };
        assert!(
            close(to, (8.0, 6.0)),
            "по часовой стрелке при y вниз: {to:?}"
        );
        // Слитные флаги и относительная цель.
        let glued = parse_path("M6 8a2 2 0 014 0").unwrap_or_default();
        assert!(close(end(&glued), (10.0, 8.0)));
    }

    #[test]
    fn moon_arc_pair_closes_on_its_start() {
        let shapes = shapes("moon").unwrap_or_default();
        assert_eq!(shapes.len(), 1);
        assert!(matches!(shapes[0].ops.last(), Some(Op::Close)));
        let before_close = &shapes[0].ops[..shapes[0].ops.len() - 1];
        assert!(close(end(before_close), (13.2, 9.6)));
    }

    #[test]
    fn circle_and_rect_are_closed_contours() {
        let search = shapes("search").unwrap_or_default();
        assert_eq!(search[0].ops.len(), 6);
        assert_eq!(search[0].ops[0], Op::Move(11.3, 7.0));
        let side = shapes("sideL").unwrap_or_default();
        assert_eq!(side[0].ops[0], Op::Move(4.0, 3.0));
        assert!(matches!(side[0].ops.last(), Some(Op::Close)));
        assert_eq!(side[1].ops, vec![Op::Move(6.0, 3.0), Op::Line(6.0, 13.0)]);
    }

    #[test]
    fn two_tone_icon_keeps_its_line_stroke() {
        let runc = shapes("runc").unwrap_or_default();
        let paints: Vec<Paint> = runc.iter().map(|shape| shape.paint).collect();
        assert_eq!(
            paints,
            vec![Paint::StrokeLine, Paint::Stroke, Paint::Fill],
            "окружность цвета линии, дуга и точка цвета текста"
        );
        let two_tone: Vec<&str> = names()
            .filter(|name| {
                shapes(name)
                    .unwrap_or_default()
                    .iter()
                    .any(|shape| shape.paint == Paint::StrokeLine)
            })
            .collect();
        assert_eq!(two_tone, vec!["runc"]);
    }

    #[test]
    fn broken_attributes_are_refused() {
        assert_eq!(
            parse_fragment(r#"<rect x="1" y="1" width="-1" height="4"/>"#),
            None
        );
        assert_eq!(
            parse_fragment(r#"<rect x="1" y="1" width="4" height="4" rx="x"/>"#),
            None
        );
        assert_eq!(parse_fragment(r#"<circle cx="1" cy="1" r="-2"/>"#), None);
        assert_eq!(
            parse_fragment(r#"<circle cx="1" cy="1" r="2" stroke="red"/>"#),
            None
        );
        assert_eq!(parse_fragment(r#"<line x1="0" y1="0"/>"#), None);
        assert!(parse_fragment(r#"<rect x="1" y="1" width="4" height="4" rx="9"/>"#).is_some());
    }

    #[test]
    fn filled_dots_and_fallback() {
        let more = shapes("more").unwrap_or_default();
        assert_eq!(more.len(), 3);
        assert!(more.iter().all(|shape| shape.paint == Paint::Fill));
        assert_eq!(shapes("нет такого"), None);
        assert_eq!(
            shapes_or_fallback("нет такого"),
            shapes_or_fallback(FALLBACK)
        );
        assert!(!shapes_or_fallback(FALLBACK).is_empty());
    }
}
