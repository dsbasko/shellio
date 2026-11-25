//! Свойства вида для сверки с прототипом (ADR-0037, «Контракт полей»): шрифт с файлом, цвета,
//! линия, радиус и тень. Значения читаются у настоящих видов и слоёв, а не из темы.

use objc2::rc::Retained;
use objc2_app_kit::{
    NSColor, NSColorSpace, NSControl, NSFont, NSFontTraitsAttribute, NSFontWeightTrait,
    NSTextField, NSView,
};
use objc2_core_text::kCTFontURLAttribute;
use objc2_foundation::{NSDictionary, NSNumber, NSString, NSURL};
use objc2_quartz_core::CALayer;

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

/// Свойства слоя: заливка, линия по краю, радиус и тень.
pub(crate) fn layer_style(layer: &CALayer) -> Style {
    let fill = layer
        .backgroundColor()
        .and_then(|color| NSColor::colorWithCGColor(&color));
    let line = layer
        .borderColor()
        .and_then(|color| NSColor::colorWithCGColor(&color));
    let shadow = layer
        .shadowColor()
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
    let opacity = f64::from(layer.shadowOpacity());
    if opacity > 0.0
        && let Some(shadow) = drawn(shadow)
    {
        let offset = layer.shadowOffset();
        style.shadow.push(ShadowFacts {
            color: Rgba {
                a: shadow.a * opacity,
                ..shadow
            },
            x: offset.width,
            y: offset.height,
            blur: layer.shadowRadius(),
            spread: 0.0,
            inset: false,
        });
    }
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

/// Свойства вида: слой, а у текстового контрола ещё шрифт и цвет текста.
pub(crate) fn view_style(view: &NSView) -> Style {
    let mut style = view
        .layer()
        .map(|layer| layer_style(&layer))
        .unwrap_or_default();
    if let Some(field) = view.downcast_ref::<NSTextField>() {
        style.color = field.textColor().and_then(|color| rgba(&color));
    }
    if let Some(font) = view.downcast_ref::<NSControl>().and_then(NSControl::font) {
        style.font = Some(font_facts(&font));
    }
    style
}

/// Обрезан ли текст поля: естественная ширина его содержимого больше ширины поля.
pub(crate) fn truncated(field: &NSTextField) -> bool {
    field.intrinsicContentSize().width > field.bounds().size.width + 0.5
}

#[cfg(test)]
mod tests {
    use objc2_app_kit::{NSFontWeightBold, NSFontWeightMedium, NSFontWeightRegular};

    use super::*;

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
