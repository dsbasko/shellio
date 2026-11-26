//! Слои Core Animation: изменения без неявных анимаций и цвета токенов в sRGB.

use objc2::rc::Retained;
use objc2_app_kit::NSColor;
use objc2_foundation::NSSize;
use objc2_quartz_core::{CALayer, CATransaction};
use shellio_theme::{Shadow, Srgb8, Tint};

/// Выполняет `change` в транзакции Core Animation с выключенными действиями: смена цвета, кадра
/// и радиуса слоёв не анимируется. Транзакция фиксируется и при панике внутри `change`.
pub(crate) fn without_actions<R>(change: impl FnOnce() -> R) -> R {
    struct Commit;
    impl Drop for Commit {
        fn drop(&mut self) {
            CATransaction::commit();
        }
    }
    CATransaction::begin();
    let _commit = Commit;
    CATransaction::setDisableActions(true);
    change()
}

/// Цвет токена в sRGB с долей непрозрачности `alpha`.
fn srgb(color: Srgb8, alpha: f64) -> Retained<NSColor> {
    let unit = color.unit();
    NSColor::colorWithSRGBRed_green_blue_alpha(unit.r, unit.g, unit.b, alpha)
}

/// Непрозрачный цвет токена в sRGB для контролов AppKit (текст, значки).
pub(crate) fn ns_color(color: Srgb8) -> Retained<NSColor> {
    srgb(color, 1.0)
}

/// Непрозрачная заливка слоя цветом токена.
pub(crate) fn set_fill(layer: &CALayer, color: Srgb8) {
    layer.setBackgroundColor(Some(&srgb(color, 1.0).CGColor()));
}

/// Полупрозрачная заливка слоя: вуаль или тон над непрозрачной поверхностью.
pub(crate) fn set_tint(layer: &CALayer, tint: Tint) {
    layer.setBackgroundColor(Some(&srgb(tint.color, tint.alpha).CGColor()));
}

/// Линия по краю слоя; `None` убирает её.
pub(crate) fn set_line(layer: &CALayer, line: Option<(Srgb8, f64)>) {
    match line {
        Some((color, width)) => {
            layer.setBorderColor(Some(&srgb(color, 1.0).CGColor()));
            layer.setBorderWidth(width);
        }
        None => {
            layer.setBorderColor(None);
            layer.setBorderWidth(0.0);
        }
    }
}

/// Радиус скругления; содержимое обрезается по нему.
pub(crate) fn set_radius(layer: &CALayer, radius: f64) {
    layer.setCornerRadius(radius);
    layer.setMasksToBounds(radius > 0.0);
}

/// Радиус скругления без обрезки: подслои и тень за кадром слоя остаются видны.
pub(crate) fn set_corner(layer: &CALayer, radius: f64) {
    layer.setCornerRadius(radius);
    layer.setMasksToBounds(false);
}

/// Доля размытия CSS, которую берёт радиус тени Core Animation: `blur` в `box-shadow` — два
/// стандартных отклонения, радиус тени слоя — одно.
pub(crate) const SHADOW_BLUR_PER_RADIUS: f64 = 2.0;

/// Тень слоя по записи токена. Доля цвета лежит в цвете тени, непрозрачность тени — единица.
/// Смещение — в координатах слоя: у слоёв перевёрнутого вида ось Y идёт вниз, как в CSS.
pub(crate) fn set_shadow(layer: &CALayer, shadow: Shadow) {
    layer.setShadowColor(Some(&srgb(shadow.tint.color, shadow.tint.alpha).CGColor()));
    layer.setShadowOpacity(1.0);
    layer.setShadowOffset(NSSize::new(shadow.x, shadow.y));
    layer.setShadowRadius(shadow.blur / SHADOW_BLUR_PER_RADIUS);
}
