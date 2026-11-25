//! Контракт полей (ADR-0037, «Контракт полей»): у каждого поля паспорта — применимость по типу
//! элемента, один источник суждения у продукта и допуск. «Не применяется» задаёт только строка
//! контракта для типа элемента, а не факт прогона.

use serde::{Deserialize, Serialize};

use crate::passport::Element;

/// Тип элемента паспорта.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ElementKind {
    /// Контейнер: полоса, зона, часть, тело, строка вкладок.
    Container,
    Text,
    Control,
    Icon,
    /// Плитка части.
    Plate,
    /// Плашка: знак клавиши, режим.
    Badge,
    Tab,
    /// Псевдоэлемент `::before` или `::after`: разделитель, вуаль, линия, точка.
    Pseudo,
}

impl ElementKind {
    pub const ALL: [ElementKind; 8] = [
        ElementKind::Container,
        ElementKind::Text,
        ElementKind::Control,
        ElementKind::Icon,
        ElementKind::Plate,
        ElementKind::Badge,
        ElementKind::Tab,
        ElementKind::Pseudo,
    ];
}

/// Поле контракта. `All`, `FrameX` и `IconName` — только в списке полей исключения: все поля
/// области, одна координата кадра и имя значка без его размера.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Field {
    /// Состав, родитель и порядок среди соседей с парой.
    Composition,
    Frame,
    Visible,
    Text,
    Truncated,
    Font,
    LineHeight,
    LetterSpacing,
    Color,
    Fill,
    LineColor,
    LineWidth,
    Radius,
    Padding,
    Shadow,
    /// Имя и размер значка.
    Icon,
    /// Силуэт значка.
    IconMask,
    /// Линия под активной вкладкой.
    Underline,
    All,
    /// Только координата `x` кадра.
    FrameX,
    /// Только имя значка; размер сверяется.
    IconName,
}

impl Field {
    /// Поля контракта без `All`.
    pub const CONTRACT: [Field; 18] = [
        Field::Composition,
        Field::Frame,
        Field::Visible,
        Field::Text,
        Field::Truncated,
        Field::Font,
        Field::LineHeight,
        Field::LetterSpacing,
        Field::Color,
        Field::Fill,
        Field::LineColor,
        Field::LineWidth,
        Field::Radius,
        Field::Padding,
        Field::Shadow,
        Field::Icon,
        Field::IconMask,
        Field::Underline,
    ];
}

/// Применимость поля к типу элемента.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applicability {
    /// Поле обязано иметь значение; продукт, который его не отдаёт, — провал «свойство не читается».
    Required,
    /// Поле судится, если прототип задал значение.
    IfSet,
    /// Поле у типа не судится.
    NotApplicable,
}

/// Источник суждения у продукта.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Inspect,
    /// `ui.inspect` и AX.
    InspectAndAx,
    /// Участок снимка.
    Snapshot,
    /// Маска значка по снимку.
    Mask,
    /// Подкоманда `underline` по снимку.
    Underline,
}

/// Допуск поля.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Tolerance {
    Exact,
    /// Разница в точках не больше заданной.
    Points(f64),
    /// Кадр: 1 точка, ширина, заданная текстом, — допуск калибровки ширины текста.
    FrameOrTextWidth,
    /// Разница на канал sRGB не больше заданной.
    Channel(u8),
    /// Тень: наличие точно, цвет — 3 единицы на канал.
    ShadowPresenceAndChannel(u8),
    /// Порог пересечения к объединению из калибровки.
    MaskCalibration,
    /// Правила подкоманды `underline`.
    UnderlineRules,
}

/// Строка контракта для пары «поле, тип элемента».
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rule {
    pub applicability: Applicability,
    pub source: Source,
    pub tolerance: Tolerance,
}

/// Строка контракта. Поля исключений (`All`, `FrameX`, `IconName`) в контракте не встречаются и
/// получают «не применяется».
pub fn rule(field: Field, kind: ElementKind) -> Rule {
    use Applicability::{IfSet, NotApplicable, Required};
    use ElementKind as K;

    let applicability = match field {
        Field::Composition | Field::Frame | Field::Visible => Required,
        Field::Text | Field::Truncated => match kind {
            // Плашка всегда несёт текст: знак клавиши или имя режима.
            K::Text | K::Badge => Required,
            K::Control | K::Tab => IfSet,
            K::Container | K::Icon | K::Plate | K::Pseudo => NotApplicable,
        },
        Field::Font => match kind {
            K::Text => Required,
            K::Control | K::Tab | K::Badge => IfSet,
            K::Container | K::Icon | K::Plate | K::Pseudo => NotApplicable,
        },
        Field::LineHeight | Field::LetterSpacing => match kind {
            K::Text | K::Control | K::Tab | K::Badge => IfSet,
            K::Container | K::Icon | K::Plate | K::Pseudo => NotApplicable,
        },
        Field::Color | Field::Fill | Field::LineColor | Field::Shadow => IfSet,
        // Толщину линии несёт любой элемент с нарисованной стороной: заголовок рисует нижнюю
        // линию, строка статуса — верхнюю.
        Field::LineWidth => match kind {
            K::Control | K::Plate | K::Badge => Required,
            K::Container | K::Text | K::Icon | K::Tab | K::Pseudo => IfSet,
        },
        Field::Radius => match kind {
            K::Control | K::Plate | K::Badge => Required,
            K::Container | K::Text | K::Icon | K::Tab | K::Pseudo => NotApplicable,
        },
        Field::Padding => match kind {
            K::Control | K::Badge => Required,
            K::Container | K::Text | K::Icon | K::Plate | K::Tab | K::Pseudo => NotApplicable,
        },
        Field::Icon | Field::IconMask => match kind {
            K::Icon => Required,
            K::Container | K::Text | K::Control | K::Plate | K::Badge | K::Tab | K::Pseudo => {
                NotApplicable
            }
        },
        Field::Underline => match kind {
            K::Tab => IfSet,
            K::Container | K::Text | K::Control | K::Icon | K::Plate | K::Badge | K::Pseudo => {
                NotApplicable
            }
        },
        Field::All | Field::FrameX | Field::IconName => NotApplicable,
    };

    let source = match field {
        Field::Text => Source::InspectAndAx,
        // Цвет псевдоэлемента и вуали судит участок снимка.
        Field::Color | Field::Fill | Field::LineColor if kind == K::Pseudo => Source::Snapshot,
        Field::IconMask => Source::Mask,
        Field::Underline => Source::Underline,
        Field::Composition
        | Field::Frame
        | Field::Visible
        | Field::Truncated
        | Field::Font
        | Field::LineHeight
        | Field::LetterSpacing
        | Field::Color
        | Field::Fill
        | Field::LineColor
        | Field::LineWidth
        | Field::Radius
        | Field::Padding
        | Field::Shadow
        | Field::Icon
        | Field::All
        | Field::FrameX
        | Field::IconName => Source::Inspect,
    };

    let tolerance = match field {
        Field::Frame => Tolerance::FrameOrTextWidth,
        Field::LineHeight | Field::LetterSpacing => Tolerance::Points(0.5),
        Field::Color | Field::Fill | Field::LineColor => Tolerance::Channel(3),
        Field::Padding => Tolerance::Points(1.0),
        Field::Shadow => Tolerance::ShadowPresenceAndChannel(3),
        Field::IconMask => Tolerance::MaskCalibration,
        Field::Underline => Tolerance::UnderlineRules,
        Field::Composition
        | Field::Visible
        | Field::Text
        | Field::Truncated
        | Field::Font
        | Field::LineWidth
        | Field::Radius
        | Field::Icon
        | Field::All
        | Field::FrameX
        | Field::IconName => Tolerance::Exact,
    };

    Rule {
        applicability,
        source,
        tolerance,
    }
}

/// Есть ли у элемента паспорта значение поля. Поля, которые судит только снимок (силуэт значка,
/// линия под вкладкой), значения в элементе не имеют: маска лежит рядом с паспортом.
fn present(element: &Element, field: Field) -> Option<bool> {
    Some(match field {
        Field::Composition | Field::Frame | Field::Visible => true,
        Field::Text => element.text.is_some(),
        Field::Truncated => element.truncated.is_some(),
        Field::Font => element.font.is_some(),
        Field::LineHeight => element.line_height.is_some(),
        Field::LetterSpacing => element.letter_spacing.is_some(),
        Field::Color => element.color.is_some(),
        Field::Fill => element.fill.is_some(),
        Field::LineColor => element.line_color.is_some(),
        Field::LineWidth => element.line_width.is_some(),
        Field::Radius => element.radius.is_some(),
        Field::Padding => element.padding.is_some(),
        Field::Shadow => !element.shadow.is_empty(),
        Field::Icon => element.icon.is_some(),
        Field::IconMask | Field::Underline | Field::All | Field::FrameX | Field::IconName => {
            return None;
        }
    })
}

/// Проверяет элемент по контракту: обязательное поле имеет значение, поле «не применяется» его
/// не имеет, полный текст и факт обрезки идут вместе.
pub fn check_element(element: &Element) -> Result<(), String> {
    for field in Field::CONTRACT {
        let Some(has) = present(element, field) else {
            continue;
        };
        match rule(field, element.kind).applicability {
            Applicability::Required if !has => {
                return Err(format!(
                    "{}: обязательное поле {field:?} без значения у типа {:?}",
                    element.ck, element.kind
                ));
            }
            Applicability::NotApplicable if has => {
                return Err(format!(
                    "{}: поле {field:?} не применяется к типу {:?}",
                    element.ck, element.kind
                ));
            }
            Applicability::Required | Applicability::IfSet | Applicability::NotApplicable => {}
        }
    }
    if element.text.is_some() != element.truncated.is_some() {
        return Err(format!(
            "{}: полный текст и факт обрезки записываются вместе",
            element.ck
        ));
    }
    let pseudo_key = element.ck.ends_with("::before") || element.ck.ends_with("::after");
    if pseudo_key != (element.kind == ElementKind::Pseudo) {
        return Err(format!(
            "{}: тип pseudo и ключ «::before» или «::after» идут вместе",
            element.ck
        ));
    }
    Ok(())
}

/// Как судится поле у конкретного элемента паспорта. Строки контракта для полей, которые вид
/// AppKit свойством не несёт: применимость и сбор паспорта они не меняют.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Judgement {
    /// По источнику и допуску [`rule`].
    ByRule,
    /// Значение проявляется только в кадрах элемента и его детей с парой: судит поле «кадр».
    ByFrames,
    /// Унаследованное значение: судится у детей с парой (подпись, значок).
    ByChildren,
    /// Свойство ничего не рисует и не сверяется.
    NotDrawn,
    /// Тень-кольцо сверяется как линия по четырём сторонам: цвет тени — цвет линии, растяжение —
    /// толщина.
    RingAsLine,
    /// Вид рисует система без слоя: судит участок снимка.
    BySnapshot,
}

/// Системные кнопки окна: AppKit рисует их без слоя вида.
const SYSTEM_DRAWN: &str = "chrome.window.";

/// Значки, которые несут цвет в самой картинке, а не оттенком вида: цвет судит участок снимка.
const SELF_COLORED: [&str; 1] = ["chrome.workspace.chevron"];

/// Тень-кольцо: `box-shadow` без смещения и размытия с растяжением.
pub fn is_ring(shadow: &crate::passport::Shadow) -> bool {
    shadow.x == 0.0 && shadow.y == 0.0 && shadow.blur == 0.0 && shadow.spread > 0.0
}

/// Однострочный ли текст элемента: высота кадра меньше двух межстрочных интервалов.
fn single_line(element: &Element) -> bool {
    element
        .line_height
        .is_none_or(|line| element.frame.h < 2.0 * line)
}

/// Рисует ли элемент что-нибудь своим краем: заливку, линию или тень.
fn draws_edge(element: &Element) -> bool {
    element.fill.is_some()
        || element.line_width.is_some_and(|width| width > 0.0)
        || !element.shadow.is_empty()
}

/// Правило суждения поля у элемента паспорта.
pub fn judgement(element: &Element, field: Field) -> Judgement {
    let system = element.ck.starts_with(SYSTEM_DRAWN);
    match field {
        Field::Padding => Judgement::ByFrames,
        Field::LineHeight if single_line(element) => Judgement::ByFrames,
        Field::Color
            if matches!(element.kind, ElementKind::Control | ElementKind::Tab)
                && element.text.is_none() =>
        {
            Judgement::ByChildren
        }
        Field::Color if SELF_COLORED.contains(&element.ck.as_str()) => Judgement::BySnapshot,
        Field::Fill if system => Judgement::BySnapshot,
        Field::Shadow if system => Judgement::BySnapshot,
        Field::Shadow if !element.shadow.is_empty() && element.shadow.iter().all(is_ring) => {
            Judgement::RingAsLine
        }
        Field::Radius if !draws_edge(element) => Judgement::NotDrawn,
        _ => Judgement::ByRule,
    }
}

/// Значение, которым читается отсутствие поля у продукта: вид без линии слоя и без скругления
/// отдаёт толщину и радиус пропуском, паспорт пишет ноль.
pub fn absent_value(field: Field) -> Option<f64> {
    matches!(field, Field::LineWidth | Field::Radius).then_some(0.0)
}
