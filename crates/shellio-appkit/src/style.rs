//! Свойства вида для сверки с прототипом (ADR-0037, «Контракт полей»): шрифт с файлом, цвета,
//! линия, радиус и тень. Значения читаются у настоящих видов и слоёв, а не из темы.

use objc2::AnyThread;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{
    NSAttributedStringNSStringDrawing, NSColor, NSColorSpace, NSFont, NSFontAttributeName,
    NSFontTraitsAttribute, NSFontWeightTrait, NSForegroundColorAttributeName, NSKernAttributeName,
    NSLayoutManager, NSLineBreakMode, NSParagraphStyle, NSParagraphStyleAttributeName, NSTextField,
    NSView,
};
use objc2_core_text::kCTFontURLAttribute;
use objc2_foundation::{
    NSAlignmentOptions, NSAttributedString, NSDictionary, NSNumber, NSPoint, NSRect, NSSize,
    NSString, NSURL,
};
use objc2_quartz_core::CALayer;

use crate::chrome_bar::Frame;

/// Цвет sRGB с непрозрачностью 0–1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: f64,
}

impl Rgba {
    /// Цвет из долей 0–1; доли вне диапазона прижимаются к его краям.
    pub fn from_unit(r: f64, g: f64, b: f64, a: f64) -> Rgba {
        Rgba {
            r: channel(r),
            g: channel(g),
            b: channel(b),
            a: a.clamp(0.0, 1.0),
        }
    }

    pub fn opaque(&self) -> bool {
        self.a >= 1.0
    }

    /// Этот цвет поверх непрозрачного `under`.
    pub fn over(&self, under: Rgba) -> Rgba {
        let mix = |top: u8, bottom: u8| {
            channel((f64::from(top) * self.a + f64::from(bottom) * (1.0 - self.a)) / 255.0)
        };
        Rgba {
            r: mix(self.r, under.r),
            g: mix(self.g, under.g),
            b: mix(self.b, under.b),
            a: 1.0,
        }
    }
}

fn channel(unit: f64) -> u8 {
    // Значение в диапазоне 0–255 после прижатия: приведение не теряет знака и не обрезает.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let value = (unit.clamp(0.0, 1.0) * 255.0).round() as u8;
    value
}

/// Шрифт текста вида.
#[derive(Debug, Clone, PartialEq)]
pub struct FontFacts {
    /// Семейство (`familyName`).
    pub family: String,
    /// Имя PostScript начертания (`fontName`).
    pub face: String,
    /// Начертание по шкале CSS, 100–900.
    pub weight: f64,
    pub size: f64,
    /// URL файла фактически выбранного шрифта (`kCTFontURLAttribute`).
    pub file: Option<String>,
}

/// Тень слоя.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShadowFacts {
    pub color: Rgba,
    pub x: f64,
    pub y: f64,
    pub blur: f64,
    pub spread: f64,
    pub inset: bool,
}

/// Сторона вида с линией.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    Top,
    Right,
    Bottom,
    Left,
}

impl Side {
    /// Стороны по часовой стрелке сверху.
    pub const ALL: [Side; 4] = [Side::Top, Side::Right, Side::Bottom, Side::Left];
}

/// Внутренние отступы в точках.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Padding {
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
    pub left: f64,
}

/// Значок вида.
#[derive(Debug, Clone, PartialEq)]
pub struct IconFacts {
    pub name: String,
    pub size: f64,
}

/// Свойства вида. `None` и пустой список — свойства у вида нет.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Style {
    pub font: Option<FontFacts>,
    pub line_height: Option<f64>,
    pub letter_spacing: Option<f64>,
    pub color: Option<Rgba>,
    pub fill: Option<Rgba>,
    pub line_color: Option<Rgba>,
    pub line_width: Option<f64>,
    pub line_sides: Vec<Side>,
    pub radius: Option<f64>,
    pub padding: Option<Padding>,
    pub shadow: Vec<ShadowFacts>,
    pub icon: Option<IconFacts>,
}

impl Style {
    pub fn is_empty(&self) -> bool {
        *self == Style::default()
    }
}

/// Веса AppKit (`NSFontWeight*`) и их места на шкале CSS.
const WEIGHTS: [(f64, f64); 9] = [
    (-0.8, 100.0),
    (-0.6, 200.0),
    (-0.4, 300.0),
    (0.0, 400.0),
    (0.23, 500.0),
    (0.3, 600.0),
    (0.4, 700.0),
    (0.56, 800.0),
    (0.62, 900.0),
];

/// Начертание по шкале CSS для веса AppKit: ближайшая названная ступень.
pub fn css_weight(appkit: f64) -> f64 {
    WEIGHTS
        .iter()
        .min_by(|a, b| (a.0 - appkit).abs().total_cmp(&(b.0 - appkit).abs()))
        .map_or(400.0, |(_, css)| *css)
}

/// Заливка, которую видно под корнем поверхности: заливки от него к корню окна, первая
/// непрозрачная закрывает остальные, полупрозрачные над ней накладываются. `None` — непрозрачной
/// заливки в цепочке нет.
pub fn seen_fill(chain: &[Rgba]) -> Option<Rgba> {
    let opaque = chain.iter().position(Rgba::opaque)?;
    Some(
        chain[..opaque]
            .iter()
            .rev()
            .fold(chain[opaque], |under, top| top.over(under)),
    )
}

/// Цвет в sRGB; `None` — цвет в sRGB не переводится (узор, каталожный цвет без компонент).
pub(crate) fn rgba(color: &NSColor) -> Option<Rgba> {
    let srgb = color.colorUsingColorSpace(&NSColorSpace::sRGBColorSpace())?;
    Some(Rgba::from_unit(
        srgb.redComponent(),
        srgb.greenComponent(),
        srgb.blueComponent(),
        srgb.alphaComponent(),
    ))
}

/// Цвет слоя в sRGB; прозрачный цвет ничего не рисует.
fn drawn(color: Option<Retained<NSColor>>) -> Option<Rgba> {
    color
        .and_then(|color| rgba(&color))
        .filter(|color| color.a > 0.0)
}

/// Тень слоя записью `box-shadow`: доля цвета с непрозрачностью тени, размытие в мере CSS.
pub(crate) fn layer_shadow(layer: &CALayer) -> Option<ShadowFacts> {
    let color = drawn(
        layer
            .shadowColor()
            .and_then(|color| NSColor::colorWithCGColor(&color)),
    )?;
    let opacity = f64::from(layer.shadowOpacity());
    if opacity <= 0.0 {
        return None;
    }
    let offset = layer.shadowOffset();
    Some(ShadowFacts {
        color: Rgba {
            a: color.a * opacity,
            ..color
        },
        x: offset.width,
        y: offset.height,
        blur: layer.shadowRadius() * crate::layer::SHADOW_BLUR_PER_RADIUS,
        spread: 0.0,
        inset: false,
    })
}

/// Кольцо снаружи кадра вида, нарисованное линией слоя `ring`, записью `box-shadow`: тень без
/// смещения и размытия с растяжением на толщину линии.
pub(crate) fn ring_shadow(ring: &CALayer) -> Option<ShadowFacts> {
    let line = layer_style(ring);
    Some(ShadowFacts {
        color: line.line_color?,
        x: 0.0,
        y: 0.0,
        blur: 0.0,
        spread: line.line_width?,
        inset: false,
    })
}

/// Свойства слоя: заливка, линия по краю, радиус и тень.
pub(crate) fn layer_style(layer: &CALayer) -> Style {
    let fill = layer
        .backgroundColor()
        .and_then(|color| NSColor::colorWithCGColor(&color));
    let line = layer
        .borderColor()
        .and_then(|color| NSColor::colorWithCGColor(&color));
    let mut style = Style {
        fill: drawn(fill),
        ..Style::default()
    };
    let width = layer.borderWidth();
    if let Some(line) = drawn(line).filter(|_| width > 0.0) {
        style.line_color = Some(line);
        style.line_width = Some(width);
        style.line_sides = Side::ALL.to_vec();
    }
    let radius = layer.cornerRadius();
    if radius > 0.0 {
        style.radius = Some(radius);
    }
    style.shadow.extend(layer_shadow(layer));
    style
}

/// Шрифт: семейство, начертание, кегль и файл.
pub(crate) fn font_facts(font: &NSFont) -> FontFacts {
    FontFacts {
        family: font
            .familyName()
            .map(|name| name.to_string())
            .unwrap_or_default(),
        face: font.fontName().to_string(),
        weight: css_weight(appkit_weight(font)),
        size: font.pointSize(),
        file: font_file(font),
    }
}

/// Вес AppKit для начертания по шкале CSS: ближайшая названная ступень.
pub(crate) fn appkit_weight_of(css: f64) -> f64 {
    WEIGHTS
        .iter()
        .min_by(|a, b| (a.1 - css).abs().total_cmp(&(b.1 - css).abs()))
        .map_or(0.0, |(appkit, _)| *appkit)
}

/// Вес шрифта по `NSFontWeightTrait`; у шрифта без признака — обычный.
fn appkit_weight(font: &NSFont) -> f64 {
    // SAFETY: имена признаков — неизменяемые константы AppKit.
    let (traits, weight) = unsafe { (NSFontTraitsAttribute, NSFontWeightTrait) };
    font.fontDescriptor()
        .objectForKey(traits)
        .and_then(|traits| traits.downcast::<NSDictionary>().ok())
        .and_then(|traits| traits.objectForKey(weight))
        .and_then(|weight| weight.downcast::<NSNumber>().ok())
        .map_or(0.0, |weight| weight.doubleValue())
}

/// URL файла, из которого взят шрифт.
pub(crate) fn font_file(font: &NSFont) -> Option<String> {
    // SAFETY: ключ — неизменяемая константа CoreText; `CFString` и `NSString` — один объект.
    let key: &NSString = unsafe { &*std::ptr::from_ref(kCTFontURLAttribute).cast::<NSString>() };
    let url = font.fontDescriptor().objectForKey(key)?;
    let url: Retained<NSURL> = url.downcast::<NSURL>().ok()?;
    url.absoluteString().map(|url| url.to_string())
}

/// Правило высоты строки из стиля абзаца; ноль у предела и множителя — «не задан».
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct LineRule {
    pub minimum: f64,
    pub maximum: f64,
    pub multiple: f64,
    pub spacing: f64,
}

/// Межстрочный интервал: высота строки шрифта `natural`, растянутая множителем и зажатая
/// пределами правила, плюс промежуток между строками.
pub(crate) fn line_pitch(natural: f64, rule: LineRule) -> f64 {
    let mut height = natural;
    if rule.multiple > 0.0 {
        height *= rule.multiple;
    }
    if rule.minimum > 0.0 {
        height = height.max(rule.minimum);
    }
    if rule.maximum > 0.0 {
        height = height.min(rule.maximum);
    }
    height + rule.spacing.max(0.0)
}

/// Правило высоты строки у первого знака текста; `None` — стиля абзаца у текста нет.
fn line_rule(text: &NSAttributedString) -> Option<LineRule> {
    if text.length() == 0 {
        return None;
    }
    // SAFETY: имя атрибута — неизменяемая константа AppKit, знак 0 есть у непустого текста,
    // диапазон действия не запрашивается.
    let value = unsafe {
        text.attribute_atIndex_effectiveRange(
            NSParagraphStyleAttributeName,
            0,
            std::ptr::null_mut(),
        )
    }?;
    let paragraph = value.downcast::<NSParagraphStyle>().ok()?;
    Some(LineRule {
        minimum: paragraph.minimumLineHeight(),
        maximum: paragraph.maximumLineHeight(),
        multiple: paragraph.lineHeightMultiple(),
        spacing: paragraph.lineSpacing(),
    })
}

/// Межстрочный интервал текста `text`, набранного шрифтом `font`: высота строки шрифта по
/// умолчанию у `NSLayoutManager` по правилу стиля абзаца текста.
pub(crate) fn text_line_height(text: &NSAttributedString, font: &NSFont) -> f64 {
    let natural = NSLayoutManager::new().defaultLineHeightForFont(font);
    line_pitch(natural, line_rule(text).unwrap_or_default())
}

/// Межбуквенный интервал у первого знака текста; `None` — интервала нет или он нулевой.
pub(crate) fn letter_spacing(text: &NSAttributedString) -> Option<f64> {
    if text.length() == 0 {
        return None;
    }
    // SAFETY: имя атрибута — неизменяемая константа AppKit, знак 0 есть у непустого текста,
    // диапазон действия не запрашивается.
    let value = unsafe {
        text.attribute_atIndex_effectiveRange(NSKernAttributeName, 0, std::ptr::null_mut())
    }?;
    let kern = value.downcast::<NSNumber>().ok()?.doubleValue();
    (kern != 0.0).then_some(kern)
}

/// Текст шрифтом `font` цветом `color` с межбуквенным интервалом `kern` в точках.
pub(crate) fn spaced_text(
    text: &str,
    font: &NSFont,
    color: &NSColor,
    kern: f64,
) -> Retained<NSAttributedString> {
    // SAFETY: ключи атрибутов — неизменяемые строки AppKit, живущие весь процесс.
    let keys = unsafe {
        [
            NSFontAttributeName,
            NSForegroundColorAttributeName,
            NSKernAttributeName,
        ]
    };
    let kern = NSNumber::new_f64(kern);
    let values: [&AnyObject; 3] = [font.as_ref(), color.as_ref(), kern.as_ref()];
    let attributes = NSDictionary::from_slices(&keys, &values);
    // SAFETY: значения словаря — `NSFont`, `NSColor` и `NSNumber`, как требуют ключи.
    unsafe {
        NSAttributedString::initWithString_attributes(
            NSAttributedString::alloc(),
            &NSString::from_str(text),
            Some(&attributes),
        )
    }
}

/// Переносит ли поле текст на новые строки: ячейка переносит по словам или знакам и поле не
/// ограничено одной строкой.
pub(crate) fn wraps(field: &NSTextField) -> bool {
    let mode = field.lineBreakMode();
    (mode == NSLineBreakMode::ByWordWrapping || mode == NSLineBreakMode::ByCharWrapping)
        && field.maximumNumberOfLines() != 1
        && !field.usesSingleLineMode()
        && field.cell().is_some_and(|cell| cell.wraps())
}

/// Свойства вида: слой, а у поля с текстом ещё шрифт и цвет текста. Контрол без текста шрифта не
/// несёт: шрифт кнопки отдаёт узел её подписи. Межстрочный интервал отдаёт только поле с
/// переносом: у однострочного текста его несёт кадр.
pub(crate) fn view_style(view: &NSView) -> Style {
    let mut style = view
        .layer()
        .map(|layer| layer_style(&layer))
        .unwrap_or_default();
    if let Some(field) = view.downcast_ref::<NSTextField>() {
        style.color = field.textColor().and_then(|color| rgba(&color));
        if field.stringValue().length() > 0 {
            let font = field.font();
            style.font = font.as_deref().map(font_facts);
            style.letter_spacing = letter_spacing(&field.attributedStringValue());
            if wraps(field) {
                style.line_height =
                    font.map(|font| text_line_height(&field.attributedStringValue(), &font));
            }
        }
    }
    style
}

/// Поле ячейки подписи AppKit по горизонтали с каждой стороны: набор начинается на столько правее
/// кадра поля. Собственный размер однострочной подписи этих полей не содержит.
pub(crate) const LABEL_INSET: f64 = 2.0;

/// Кадр поля подписи, набор которой занимает `typeset` в точках вида `host`: поле шире набора на
/// поля ячейки с обеих сторон и стоит на пикселях экрана — начало на ближайшем, размер не меньше
/// нужного: дробный кадр размывает набор, а кадр уже набора его обрезает.
pub(crate) fn label_field_rect(host: &NSView, typeset: Frame) -> NSRect {
    let field = NSRect::new(
        NSPoint::new(typeset.x - LABEL_INSET, typeset.y),
        NSSize::new(typeset.width + 2.0 * LABEL_INSET, typeset.height),
    );
    host.backingAlignedRect_options(
        field,
        NSAlignmentOptions::AlignMinXNearest
            | NSAlignmentOptions::AlignMinYNearest
            | NSAlignmentOptions::AlignWidthOutward
            | NSAlignmentOptions::AlignHeightOutward,
    )
}

/// Высота, которой заведомо хватает набору поля с переносом при замере его высоты.
const TALL: f64 = 100_000.0;

/// Обрезан ли текст поля: естественная ширина его содержимого больше ширины поля. Поле с
/// переносом обрезано, когда набору в его ширине нужна высота больше высоты поля.
pub(crate) fn truncated(field: &NSTextField) -> bool {
    let size = field.bounds().size;
    if wraps(field) {
        return wrapped_height(field, size.width) > size.height + 0.5;
    }
    field.intrinsicContentSize().width > size.width + 0.5
}

/// Высота набора поля с переносом в кадре шириной `width`.
pub(crate) fn wrapped_height(field: &NSTextField, width: f64) -> f64 {
    let bounds = NSRect::new(NSPoint::ZERO, NSSize::new(width.max(0.0), TALL));
    field
        .cell()
        .map_or(0.0, |cell| cell.cellSizeForBounds(bounds).height)
}

/// Естественная ширина набора `text` шрифтом `font` в одну строку, без полей ячейки.
pub(crate) fn typeset_width(text: &str, font: &NSFont) -> f64 {
    if text.is_empty() {
        return 0.0;
    }
    // SAFETY: ключ атрибута — неизменяемая строка AppKit, живущая весь процесс.
    let keys = unsafe { [NSFontAttributeName] };
    let values: [&AnyObject; 1] = [font.as_ref()];
    let attributes = NSDictionary::from_slices(&keys, &values);
    // SAFETY: значение словаря — `NSFont`, как требует ключ.
    let text = unsafe {
        NSAttributedString::initWithString_attributes(
            NSAttributedString::alloc(),
            &NSString::from_str(text),
            Some(&attributes),
        )
    };
    text.size().width
}

#[cfg(test)]
mod tests {
    use objc2_app_kit::{
        NSFontWeightBold, NSFontWeightMedium, NSFontWeightRegular, NSMutableParagraphStyle,
    };
    use objc2_foundation::{NSMutableAttributedString, NSRange};

    use super::*;

    fn styled(
        text: &str,
        shape: impl Fn(&NSMutableParagraphStyle),
    ) -> Retained<NSAttributedString> {
        let text = NSMutableAttributedString::from_nsstring(&NSString::from_str(text));
        let paragraph = NSMutableParagraphStyle::new();
        shape(&paragraph);
        // SAFETY: имя атрибута — неизменяемая константа AppKit, значение — стиль абзаца, диапазон
        // лежит в тексте.
        unsafe {
            text.addAttribute_value_range(
                NSParagraphStyleAttributeName,
                &paragraph,
                NSRange::new(0, text.length()),
            );
        }
        Retained::into_super(text)
    }

    #[test]
    fn letter_spacing_reads_the_kern_of_the_text() {
        // SAFETY: вес шрифта — неизменяемая константа AppKit.
        let font = NSFont::systemFontOfSize_weight(11.0, unsafe { NSFontWeightBold });
        let spaced = spaced_text("НАВИГАЦИЯ", &font, &NSColor::whiteColor(), 0.88);
        assert_eq!(letter_spacing(&spaced), Some(0.88));
        let plain = NSAttributedString::from_nsstring(&NSString::from_str("НАВИГАЦИЯ"));
        assert_eq!(letter_spacing(&plain), None);
        let empty = NSAttributedString::from_nsstring(&NSString::from_str(""));
        assert_eq!(letter_spacing(&empty), None);
        let zero = spaced_text("ВВОД", &font, &NSColor::whiteColor(), 0.0);
        assert_eq!(
            letter_spacing(&zero),
            None,
            "нулевой интервал — не свойство"
        );
        let natural = typeset_width("НАВИГАЦИЯ", &font);
        assert!(
            (spaced.size().width - natural - 9.0 * 0.88).abs() < 0.01,
            "интервал стоит после каждого знака, как `letter-spacing`"
        );
    }

    #[test]
    fn line_pitch_follows_the_paragraph_rule() {
        let rule = LineRule::default();
        assert!((line_pitch(15.0, rule) - 15.0).abs() < f64::EPSILON);
        let fixed = LineRule {
            minimum: 18.0,
            maximum: 18.0,
            ..rule
        };
        assert!((line_pitch(15.0, fixed) - 18.0).abs() < f64::EPSILON);
        assert!((line_pitch(22.0, fixed) - 18.0).abs() < f64::EPSILON);
        let floor = LineRule {
            minimum: 18.0,
            ..rule
        };
        assert!((line_pitch(22.0, floor) - 22.0).abs() < f64::EPSILON);
        let stretched = LineRule {
            multiple: 1.5,
            spacing: 2.0,
            ..rule
        };
        assert!((line_pitch(12.0, stretched) - 20.0).abs() < f64::EPSILON);
        let negative = LineRule {
            spacing: -4.0,
            ..rule
        };
        assert!((line_pitch(15.0, negative) - 15.0).abs() < f64::EPSILON);
    }

    #[test]
    fn text_line_height_reads_the_paragraph_style_or_the_font_default() {
        // SAFETY: вес шрифта — неизменяемая константа AppKit.
        let font = NSFont::systemFontOfSize_weight(12.0, unsafe { NSFontWeightRegular });
        let natural = NSLayoutManager::new().defaultLineHeightForFont(&font);
        assert!(natural > 12.0 && natural < 18.0, "{natural}");

        let plain = NSAttributedString::from_nsstring(&NSString::from_str("Терминал: здесь тесно"));
        assert!((text_line_height(&plain, &font) - natural).abs() < f64::EPSILON);
        let empty = NSAttributedString::from_nsstring(&NSString::from_str(""));
        assert!((text_line_height(&empty, &font) - natural).abs() < f64::EPSILON);

        let fixed = styled("Терминал: здесь тесно", |paragraph| {
            paragraph.setMinimumLineHeight(18.0);
            paragraph.setMaximumLineHeight(18.0);
        });
        assert!((text_line_height(&fixed, &font) - 18.0).abs() < f64::EPSILON);
        let spaced = styled("Терминал: здесь тесно", |paragraph| {
            paragraph.setLineSpacing(3.0);
        });
        assert!((text_line_height(&spaced, &font) - (natural + 3.0)).abs() < f64::EPSILON);
    }

    #[test]
    fn typeset_width_grows_with_the_text_and_the_font() {
        // SAFETY: вес шрифта — неизменяемая константа AppKit.
        let font = NSFont::systemFontOfSize_weight(12.5, unsafe { NSFontWeightRegular });
        assert!(typeset_width("", &font).abs() < f64::EPSILON);
        let short = typeset_width("Развернуть", &font);
        assert!(short > 50.0 && short < 90.0, "{short}");
        assert!(typeset_width("Развернуть панель", &font) > short);
        // SAFETY: вес шрифта — неизменяемая константа AppKit.
        let larger = NSFont::systemFontOfSize_weight(15.0, unsafe { NSFontWeightRegular });
        assert!(typeset_width("Развернуть", &larger) > short);
    }

    const fn rgb(r: u8, g: u8, b: u8, a: f64) -> Rgba {
        Rgba { r, g, b, a }
    }

    #[test]
    fn unit_channels_round_and_clamp() {
        assert_eq!(Rgba::from_unit(0.0, 0.5, 1.0, 1.0), rgb(0, 128, 255, 1.0));
        assert_eq!(Rgba::from_unit(-1.0, 2.0, 0.1, 3.0), rgb(0, 255, 26, 1.0));
    }

    #[test]
    fn weights_map_to_the_css_scale() {
        for (appkit, css) in WEIGHTS {
            assert!((css_weight(appkit) - css).abs() < f64::EPSILON, "{appkit}");
        }
        assert!((css_weight(0.05) - 400.0).abs() < f64::EPSILON);
        assert!((css_weight(0.27) - 600.0).abs() < f64::EPSILON);
        assert!((css_weight(1.0) - 900.0).abs() < f64::EPSILON);
        assert!((css_weight(-1.0) - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn surface_root_shows_the_fill_under_it() {
        let opaque = rgb(30, 29, 27, 1.0);
        assert_eq!(seen_fill(&[opaque]), Some(opaque));
        assert_eq!(seen_fill(&[rgb(0, 0, 0, 0.0), opaque]), Some(opaque));
        assert_eq!(
            seen_fill(&[
                rgb(255, 255, 255, 0.5),
                rgb(0, 0, 0, 1.0),
                rgb(9, 9, 9, 1.0)
            ]),
            Some(rgb(128, 128, 128, 1.0)),
            "нижние заливки первая непрозрачная закрывает"
        );
        assert_eq!(seen_fill(&[rgb(0, 0, 0, 0.5)]), None);
        assert_eq!(seen_fill(&[]), None);
    }

    #[test]
    fn system_font_reports_family_weight_and_file() {
        // SAFETY: веса шрифта — неизменяемые константы AppKit.
        let (regular, medium, bold) =
            unsafe { (NSFontWeightRegular, NSFontWeightMedium, NSFontWeightBold) };
        for (weight, css) in [(regular, 400.0), (medium, 500.0), (bold, 700.0)] {
            let facts = font_facts(&NSFont::systemFontOfSize_weight(12.5, weight));
            assert!((facts.weight - css).abs() < f64::EPSILON, "{facts:?}");
            assert!((facts.size - 12.5).abs() < f64::EPSILON);
            assert!(!facts.family.is_empty() && !facts.face.is_empty());
            assert!(
                facts
                    .file
                    .as_deref()
                    .is_some_and(|url| url.starts_with("file://")),
                "{facts:?}"
            );
        }
    }

    #[test]
    fn layer_reports_fill_line_radius_and_shadow() {
        let layer = CALayer::new();
        assert!(layer_style(&layer).is_empty());
        crate::layer::set_fill(&layer, shellio_theme::Srgb8::new(30, 29, 27));
        crate::layer::set_line(&layer, Some((shellio_theme::Srgb8::new(48, 46, 43), 1.0)));
        layer.setCornerRadius(6.0);
        let style = layer_style(&layer);
        assert_eq!(style.fill, Some(rgb(30, 29, 27, 1.0)));
        assert_eq!(style.line_color, Some(rgb(48, 46, 43, 1.0)));
        assert_eq!(style.line_width, Some(1.0));
        assert_eq!(style.line_sides, Side::ALL);
        assert_eq!(style.radius, Some(6.0));
        assert!(style.shadow.is_empty());
        crate::layer::set_line(&layer, None);
        assert_eq!(layer_style(&layer).line_sides, []);
    }
}
