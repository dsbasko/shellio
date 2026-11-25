//! Лист калибровки допусков сверки (ADR-0037): подписи корпуса текста и примитивы листа значков.
//! Примитивы рисуются прямо `NSBezierPath` по фигурам листа, без `crate::icon` и таблицы контуров
//! продукта: калибровка меряет движок отрисовки, а не значки продукта. Ширина подписи — естественная
//! ширина набора `NSAttributedString.size` без округления.

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSAttributedStringNSStringDrawing, NSBezierPath, NSColor, NSFont, NSFontAttributeName,
    NSForegroundColorAttributeName, NSKernAttributeName, NSLineCapStyle, NSLineJoinStyle,
    NSResponder, NSView, NSViewLayerContentsRedrawPolicy,
};
use objc2_foundation::{
    NSAttributedString, NSDictionary, NSNumber, NSObject, NSPoint, NSRect, NSSize, NSString,
};
use shellio_theme::AppTheme;

use crate::app::App;
use crate::counters::{Live, counter};
use crate::inspect::{self, InspectNode, ViewFacts};
use crate::own::{Callback, ComponentId, View};
use crate::style::{self, Style};
use crate::{font, layer, runloop};

counter!(SHEET_OWNER, Owner, "CalibrationSheet");
counter!(SHEET_VIEW, Class, "ShellioCalibrationSheet");

/// Подпись корпуса: одна строка одним шрифтом, начало сверху слева в точках листа.
#[derive(Debug, Clone, PartialEq)]
pub struct CalibrationText {
    /// Хвост имени узла `ui.inspect` после имени листа.
    pub id: String,
    /// Моноширинный шрифт бандла, иначе системный.
    pub mono: bool,
    /// Начертание по шкале CSS.
    pub weight: u16,
    pub size: f64,
    /// Разрядка в долях кегля: `NSKern` — `tracking_em × size`.
    pub tracking_em: f64,
    pub text: String,
    /// Внутренний отступ с начала строки: кадр шире набора на него.
    pub pad: f64,
    pub x: f64,
    pub y: f64,
}

/// Шаг контура в координатах ячейки листа.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CalibrationOp {
    Move(f64, f64),
    Line(f64, f64),
    Cubic {
        c1: (f64, f64),
        c2: (f64, f64),
        to: (f64, f64),
    },
}

/// Фигура листа в координатах ячейки.
#[derive(Debug, Clone, PartialEq)]
pub enum CalibrationFigure {
    Line {
        from: (f64, f64),
        to: (f64, f64),
    },
    Circle {
        center: (f64, f64),
        radius: f64,
    },
    RoundRect {
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        radius: f64,
    },
    Path(Vec<CalibrationOp>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct CalibrationShape {
    pub figure: CalibrationFigure,
    /// Заливка вместо обводки.
    pub fill: bool,
    /// Второй тон двухцветного значка (`--line-2`), иначе основной (`--text-2`).
    pub secondary: bool,
}

/// Ячейка листа значков: начало сверху слева в точках листа, сторона `size`.
#[derive(Debug, Clone, PartialEq)]
pub struct CalibrationIcon {
    /// Хвост имени узла `ui.inspect` после имени листа.
    pub id: String,
    pub x: f64,
    pub y: f64,
    pub size: f64,
    /// Сторона координат ячейки.
    pub view_box: f64,
    /// Толщина обводки в координатах ячейки.
    pub stroke: f64,
    pub shapes: Vec<CalibrationShape>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CalibrationSpec {
    pub texts: Vec<CalibrationText>,
    pub icons: Vec<CalibrationIcon>,
}

/// Набранная подпись: строка с атрибутами, её шрифт и естественный размер набора.
struct Typeset {
    text: Retained<NSAttributedString>,
    font: Retained<NSFont>,
    natural: NSSize,
}

struct SheetModel {
    id: ComponentId,
    spec: CalibrationSpec,
    theme: Cell<AppTheme>,
    /// Подписи в порядке `spec.texts`.
    typeset: RefCell<Vec<Typeset>>,
}

impl SheetModel {
    fn set(&self) {
        let theme = self.theme.get();
        let color = layer::ns_color(theme.colors.text_2);
        *self.typeset.borrow_mut() = self
            .spec
            .texts
            .iter()
            .map(|text| typeset(text, &color))
            .collect();
    }
}

fn typeset(spec: &CalibrationText, color: &NSColor) -> Typeset {
    let font = if spec.mono {
        font::mono_weight(spec.size, spec.weight)
    } else {
        NSFont::systemFontOfSize_weight(spec.size, style::appkit_weight_of(f64::from(spec.weight)))
    };
    let kern = NSNumber::new_f64(spec.tracking_em * spec.size);
    // SAFETY: ключи атрибутов — неизменяемые строки AppKit, живущие весь процесс.
    let keys = unsafe {
        [
            NSForegroundColorAttributeName,
            NSFontAttributeName,
            NSKernAttributeName,
        ]
    };
    let values: [&AnyObject; 3] = [color.as_ref(), font.as_ref(), kern.as_ref()];
    // Подпись без разрядки атрибута `NSKern` не несёт, как подписи компонентов: явный ноль —
    // другой набор.
    let attributes = if spec.tracking_em == 0.0 {
        NSDictionary::from_slices(&keys[..2], &values[..2])
    } else {
        NSDictionary::from_slices(&keys, &values)
    };
    // SAFETY: значения словаря — те, что требуют ключи: `NSColor`, `NSFont` и `NSNumber`.
    let text = unsafe {
        NSAttributedString::initWithString_attributes(
            NSAttributedString::alloc(),
            &NSString::from_str(&spec.text),
            Some(&attributes),
        )
    };
    let natural = text.size();
    Typeset {
        text,
        font,
        natural,
    }
}

/// Лист калибровки на главном потоке. Фона не рисует: заливку несёт подложка под ним.
pub struct CalibrationSheet {
    view: View<SheetView, SheetModel>,
}

impl CalibrationSheet {
    pub fn new(app: &App, theme: &AppTheme, spec: CalibrationSpec) -> CalibrationSheet {
        let mtm = app.mtm();
        let model = SheetModel {
            id: ComponentId::next(),
            spec,
            theme: Cell::new(*theme),
            typeset: RefCell::new(Vec::new()),
        };
        model.set();
        let view = View::new(&SHEET_OWNER, model, "CalibrationSheet.drawRect", |owner| {
            SheetView::new(mtm, owner)
        });
        CalibrationSheet { view }
    }

    pub fn id(&self) -> ComponentId {
        self.view.rust().id
    }

    /// Новая ячейка темы: тона листа — её.
    pub fn set_theme(&self, theme: &AppTheme) {
        let model = self.view.rust();
        if model.theme.replace(*theme) != *theme {
            model.set();
            self.view.view().setNeedsDisplay(true);
        }
    }

    /// Узлы листа для `ui.inspect`: подпись — кадр естественной ширины набора с отступом, ячейка
    /// значка — её кадр. Узел вне границ листа не нарисован и невидим.
    pub fn inspect(&self, id: &str, parent: Option<&str>) -> Vec<InspectNode> {
        let view: &NSView = self.view.view();
        let model = self.view.rust();
        let whole = inspect::facts(view, None);
        let under = inspect::seen_fill(view);
        let color = style::rgba(&layer::ns_color(model.theme.get().colors.text_2));
        let part = |tail: &str, class: &'static str, rect: NSRect| InspectNode {
            id: format!("{id}.{tail}"),
            class,
            object: inspect::object_id(view),
            parent: parent.map(str::to_owned),
            ck: None,
            facts: ViewFacts {
                frame: inspect::frame_of(view.convertRect_toView(rect, None)),
                visible: whole.visible && contains(view.bounds(), rect),
                first_responder: false,
                enabled: None,
                role: None,
                label: None,
                help: None,
                selected: None,
                expanded: None,
                animation_keys: 0,
            },
            style: None,
            truncated: None,
            under,
            behind: under,
            state: None,
        };
        let typeset = model.typeset.borrow();
        let mut nodes = Vec::with_capacity(model.spec.texts.len() + model.spec.icons.len());
        for (spec, set) in model.spec.texts.iter().zip(typeset.iter()) {
            let rect = NSRect::new(
                NSPoint::new(spec.x, spec.y),
                NSSize::new(set.natural.width + spec.pad, set.natural.height),
            );
            let mut node = part(&spec.id, "CalibrationText", rect);
            node.facts.label = Some(spec.text.clone());
            node.truncated = Some(false);
            node.style = Some(Style {
                font: Some(style::font_facts(&set.font)),
                letter_spacing: Some(spec.tracking_em * spec.size),
                color,
                ..Style::default()
            });
            nodes.push(node);
        }
        for icon in &model.spec.icons {
            let rect = NSRect::new(
                NSPoint::new(icon.x, icon.y),
                NSSize::new(icon.size, icon.size),
            );
            let mut node = part(&icon.id, "CalibrationIcon", rect);
            node.style = Some(Style {
                color,
                ..Style::default()
            });
            nodes.push(node);
        }
        nodes
    }

    pub(crate) fn ns_view(&self) -> &NSView {
        self.view.view()
    }
}

fn contains(outer: NSRect, inner: NSRect) -> bool {
    inner.origin.x >= outer.origin.x
        && inner.origin.y >= outer.origin.y
        && inner.origin.x + inner.size.width <= outer.origin.x + outer.size.width
        && inner.origin.y + inner.size.height <= outer.origin.y + outer.size.height
}

fn draw_icon(icon: &CalibrationIcon, primary: &NSColor, secondary: &NSColor) {
    if icon.view_box <= 0.0 {
        return;
    }
    let scale = icon.size / icon.view_box;
    let point = |(x, y): (f64, f64)| NSPoint::new(icon.x + x * scale, icon.y + y * scale);
    for shape in &icon.shapes {
        let path = NSBezierPath::bezierPath();
        match &shape.figure {
            CalibrationFigure::Line { from, to } => {
                path.moveToPoint(point(*from));
                path.lineToPoint(point(*to));
            }
            CalibrationFigure::Circle { center, radius } => {
                let origin = point((center.0 - radius, center.1 - radius));
                let side = 2.0 * radius * scale;
                path.appendBezierPathWithOvalInRect(NSRect::new(origin, NSSize::new(side, side)));
            }
            CalibrationFigure::RoundRect {
                x,
                y,
                width,
                height,
                radius,
            } => {
                let rect = NSRect::new(point((*x, *y)), NSSize::new(width * scale, height * scale));
                path.appendBezierPathWithRoundedRect_xRadius_yRadius(
                    rect,
                    radius * scale,
                    radius * scale,
                );
            }
            CalibrationFigure::Path(ops) => {
                for op in ops {
                    match *op {
                        CalibrationOp::Move(x, y) => path.moveToPoint(point((x, y))),
                        CalibrationOp::Line(x, y) => path.lineToPoint(point((x, y))),
                        CalibrationOp::Cubic { c1, c2, to } => path
                            .curveToPoint_controlPoint1_controlPoint2(
                                point(to),
                                point(c1),
                                point(c2),
                            ),
                    }
                }
            }
        }
        if shape.secondary {
            secondary.set();
        } else {
            primary.set();
        }
        if shape.fill {
            path.fill();
        } else {
            path.setLineWidth(icon.stroke * scale);
            path.setLineCapStyle(NSLineCapStyle::Round);
            path.setLineJoinStyle(NSLineJoinStyle::Round);
            path.stroke();
        }
    }
}

struct SheetIvars {
    _live: Live,
    owner: Callback<SheetModel>,
}

define_class!(
    // SAFETY: NSView не требует от подкласса ничего сверх переопределённых ниже методов, `Drop` у
    // класса нет.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioCalibrationSheet"]
    #[ivars = SheetIvars]
    struct SheetView;

    impl SheetView {
        // SAFETY: селектор и результат — как у `isFlipped` NSView.
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        // SAFETY: селектор и аргумент — как у `drawRect:` NSView.
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            self.ivars().owner.call(|model| {
                runloop::mark_draw(model.id);
                let theme = model.theme.get();
                let primary = layer::ns_color(theme.colors.text_2);
                let secondary = layer::ns_color(theme.colors.line_2);
                for icon in &model.spec.icons {
                    draw_icon(icon, &primary, &secondary);
                }
                let typeset = model.typeset.borrow();
                for (spec, set) in model.spec.texts.iter().zip(typeset.iter()) {
                    set.text
                        .drawAtPoint(NSPoint::new(spec.x + spec.pad, spec.y));
                }
            });
        }
    }
);

impl SheetView {
    fn new(mtm: MainThreadMarker, owner: Callback<SheetModel>) -> Retained<SheetView> {
        let this = SheetView::alloc(mtm).set_ivars(SheetIvars {
            _live: SHEET_VIEW.live(),
            owner,
        });
        // SAFETY: назначенный инициализатор NSView, ivars уже заданы.
        let view: Retained<SheetView> =
            unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        view.setWantsLayer(true);
        view.setLayerContentsRedrawPolicy(NSViewLayerContentsRedrawPolicy::OnSetNeedsDisplay);
        view
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(mono: bool, weight: u16, size: f64, tracking_em: f64, value: &str) -> CalibrationText {
        CalibrationText {
            id: "text.case".to_owned(),
            mono,
            weight,
            size,
            tracking_em,
            text: value.to_owned(),
            pad: 0.0,
            x: 0.0,
            y: 0.0,
        }
    }

    fn width(spec: &CalibrationText) -> f64 {
        let color = NSColor::colorWithSRGBRed_green_blue_alpha(0.0, 0.0, 0.0, 1.0);
        typeset(spec, &color).natural.width
    }

    #[test]
    fn natural_width_follows_weight_size_and_tracking() {
        let regular = width(&text(false, 400, 13.0, 0.0, "Терминал"));
        assert!(regular > 0.0);
        assert!(width(&text(false, 700, 13.0, 0.0, "Терминал")) > regular);
        assert!(width(&text(false, 400, 20.0, 0.0, "Терминал")) > regular);
        // Прибавка от разрядки у системного шрифта не равна `NSKern` на знак: её меряет калибровка.
        assert!(width(&text(false, 400, 13.0, 0.08, "Терминал")) > regular);
        let mono = width(&text(true, 400, 12.0, 0.0, "Терминал"));
        assert!(width(&text(true, 400, 12.0, 0.08, "Терминал")) > mono);
        assert!(width(&text(false, 400, 13.0, 0.0, "")).abs() < f64::EPSILON);
    }

    #[test]
    fn typeset_font_carries_the_asked_size_and_weight() {
        let color = NSColor::colorWithSRGBRed_green_blue_alpha(0.0, 0.0, 0.0, 1.0);
        for weight in [400_u16, 500, 600, 700] {
            let set = typeset(&text(false, weight, 12.5, 0.0, "W"), &color);
            let facts = style::font_facts(&set.font);
            assert!((facts.size - 12.5).abs() < f64::EPSILON);
            assert!((facts.weight - f64::from(weight)).abs() < f64::EPSILON);
        }
        let mono = typeset(&text(true, 400, 10.5, 0.0, "⌘K"), &color);
        assert!((mono.font.pointSize() - 10.5).abs() < f64::EPSILON);
    }

    #[test]
    fn rect_outside_the_sheet_is_not_drawn() {
        let sheet = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(100.0, 50.0));
        let rect = |x, y, w, h| NSRect::new(NSPoint::new(x, y), NSSize::new(w, h));
        assert!(contains(sheet, rect(0.0, 0.0, 100.0, 50.0)));
        assert!(!contains(sheet, rect(90.0, 0.0, 10.5, 10.0)));
        assert!(!contains(sheet, rect(0.0, 45.0, 10.0, 10.0)));
        assert!(!contains(sheet, rect(-1.0, 0.0, 10.0, 10.0)));
    }
}
