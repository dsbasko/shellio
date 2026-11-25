//! Значки набора прототипа (`shellio_theme::icons`) как `NSImage`: контуры рисуются обводкой со
//! скруглёнными концами. Шаблонный значок красит `contentTintColor` вида, цветной несёт свой цвет.
//! Имя вне набора — системный символ: так рисуются значки, у которых нет пары в прототипе.
//!
//! У двухцветного значка второй тон — цвет линии. Цветной значок берёт его из темы, шаблонный
//! передаёт его долей непрозрачности [`TEMPLATE_LINE`]: у шаблона один цвет.

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::Bool;
use objc2_app_kit::{NSBezierPath, NSColor, NSImage, NSLineCapStyle, NSLineJoinStyle};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
use shellio_theme::Srgb8;
use shellio_theme::icons::{self, Op, Paint, Shape};

use crate::layer;

/// Непрозрачность второго тона в шаблонном значке.
const TEMPLATE_LINE: f64 = 0.35;

/// Цвета значка: основной и второй тон.
#[derive(Clone)]
struct Inks {
    main: Retained<NSColor>,
    line: Retained<NSColor>,
}

/// Шаблонный значок `name` со стороной `size` точек. Имя из набора прототипа рисуется его
/// контурами, остальные имена — системным символом; `None` — нет и такого символа.
pub(crate) fn image(name: &str, size: f64, description: &NSString) -> Option<Retained<NSImage>> {
    match icons::shapes(name) {
        Some(shapes) => {
            let main = NSColor::blackColor();
            let line = main.colorWithAlphaComponent(TEMPLATE_LINE);
            Some(drawn(shapes, size, Inks { main, line }, true, description))
        }
        None => NSImage::imageWithSystemSymbolName_accessibilityDescription(
            &NSString::from_str(name),
            Some(description),
        ),
    }
}

/// Значок набора прототипа своих цветов: `color` — основной, `line` — второй тон двухцветного
/// значка. Вид его не перекрашивает. Неизвестное имя даёт значок по умолчанию набора.
pub(crate) fn colored(
    name: &str,
    size: f64,
    color: Srgb8,
    line: Srgb8,
    description: &NSString,
) -> Retained<NSImage> {
    let inks = Inks {
        main: layer::ns_color(color),
        line: layer::ns_color(line),
    };
    drawn(
        icons::shapes_or_fallback(name),
        size,
        inks,
        false,
        description,
    )
}

fn drawn(
    shapes: Vec<Shape>,
    size: f64,
    inks: Inks,
    template: bool,
    description: &NSString,
) -> Retained<NSImage> {
    let scale = size / icons::VIEW_BOX;
    let handler = RcBlock::new(move |_rect: NSRect| -> Bool {
        draw(&shapes, scale, &inks);
        Bool::YES
    });
    let image =
        NSImage::imageWithSize_flipped_drawingHandler(NSSize::new(size, size), true, &handler);
    image.setTemplate(template);
    image.setAccessibilityDescription(Some(description));
    image
}

fn draw(shapes: &[Shape], scale: f64, inks: &Inks) {
    let point = |(x, y): (f64, f64)| NSPoint::new(x * scale, y * scale);
    for shape in shapes {
        let path = NSBezierPath::bezierPath();
        for op in &shape.ops {
            match *op {
                Op::Move(x, y) => path.moveToPoint(point((x, y))),
                Op::Line(x, y) => path.lineToPoint(point((x, y))),
                Op::Cubic { c1, c2, to } => {
                    path.curveToPoint_controlPoint1_controlPoint2(point(to), point(c1), point(c2));
                }
                Op::Close => path.closePath(),
            }
        }
        match shape.paint {
            Paint::Fill => {
                inks.main.set();
                path.fill();
            }
            Paint::Stroke | Paint::StrokeLine => {
                if shape.paint == Paint::StrokeLine {
                    inks.line.set();
                } else {
                    inks.main.set();
                }
                path.setLineWidth(icons::STROKE * scale);
                path.setLineCapStyle(NSLineCapStyle::Round);
                path.setLineJoinStyle(NSLineJoinStyle::Round);
                path.stroke();
            }
        }
    }
}
