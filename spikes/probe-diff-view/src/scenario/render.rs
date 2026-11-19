//! Рендер вида в bitmap и чтение пикселя — пиксельная проба сценария `theme` (задача 15).
//!
//! Источник — `spikes/probe-appkit-shell/src/scenario/snapshot.rs` (P0.2), `ViewRender`,
//! `decode_samples` и `pixel_index` с их тестами. Упрощено: нет записи PNG и смешения в
//! пространстве bitmap, пиксель отдаётся сразу в sRGB вместе с сырыми сэмплами.

use std::ptr::NonNull;

use objc2::rc::Retained;
use objc2_app_kit::{NSBitmapFormat, NSBitmapImageRep, NSColor, NSColorSpace, NSView};
use objc2_foundation::{NSPoint, NSRect};
use serde::Serialize;

use probe_diff_view::tokens::Rgb;

/// Пиксель рендера: sRGB 0…255 и сырые сэмплы пространства bitmap.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Pixel {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
    /// Сэмплы как в bitmap, без предумножения, 0…255.
    pub raw: [u8; 3],
}

impl Pixel {
    /// Наибольшая разница канала sRGB с непрозрачным `rgb`, альфа — с 255.
    pub fn distance(&self, rgb: Rgb) -> u8 {
        [
            self.r.abs_diff(rgb.r),
            self.g.abs_diff(rgb.g),
            self.b.abs_diff(rgb.b),
            self.a.abs_diff(255),
        ]
        .into_iter()
        .max()
        .unwrap_or(0)
    }

    pub fn hex(&self) -> String {
        format!("#{:02X}{:02X}{:02X}", self.r, self.g, self.b)
    }

    pub fn raw_hex(&self) -> String {
        format!("#{:02X}{:02X}{:02X}", self.raw[0], self.raw[1], self.raw[2])
    }
}

fn unit_to_u8(value: f64) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// Рендер вида в bitmap на главном потоке.
pub struct ViewRender {
    rep: Retained<NSBitmapImageRep>,
    bounds: NSRect,
    flipped: bool,
}

impl ViewRender {
    pub fn of(view: &NSView) -> Result<ViewRender, String> {
        let bounds = view.bounds();
        let rep = view
            .bitmapImageRepForCachingDisplayInRect(bounds)
            .ok_or("bitmapImageRepForCachingDisplayInRect вернул nil")?;
        view.cacheDisplayInRect_toBitmapImageRep(bounds, &rep);
        Ok(ViewRender {
            rep,
            bounds,
            flipped: view.isFlipped(),
        })
    }

    pub fn color_space(&self) -> String {
        self.rep
            .colorSpace()
            .localizedName()
            .map_or_else(|| "unnamed".to_owned(), |name| name.to_string())
    }

    /// Формат сэмплов bitmap для записи в итог.
    pub fn format(&self) -> String {
        format!(
            "{} × {} px, {} бит × {} сэмпла, формат {:#x}, альфа {}",
            self.rep.pixelsWide(),
            self.rep.pixelsHigh(),
            self.rep.bitsPerSample(),
            self.rep.samplesPerPixel(),
            self.rep.bitmapFormat().0,
            self.rep.hasAlpha()
        )
    }

    /// Пиксель под точкой в координатах вида. Сэмплы читаются как есть и переводятся из профиля
    /// bitmap в sRGB: `colorAtX:y:` у bitmap с профилем дисплея искажает тёмные тона (P0.2).
    pub fn pixel(&self, point: NSPoint) -> Result<Pixel, String> {
        let (x, y) = pixel_index(
            point,
            self.bounds,
            self.flipped,
            self.rep.pixelsWide(),
            self.rep.pixelsHigh(),
        )
        .ok_or_else(|| format!("точка {},{} вне рендера", point.x, point.y))?;
        let format = self.rep.bitmapFormat();
        let bits = self.rep.bitsPerSample();
        let samples = usize::try_from(self.rep.samplesPerPixel()).unwrap_or(0);
        if format.contains(NSBitmapFormat::FloatingPointSamples)
            || !(bits == 8 || bits == 16)
            || !(3..=4).contains(&samples)
        {
            return Err(format!("формат bitmap не поддержан: {}", self.format()));
        }
        let mut raw = [0usize; 4];
        // SAFETY: буфер вмещает `samplesPerPixel` сэмплов, точка внутри bitmap.
        unsafe {
            self.rep
                .getPixel_atX_y(NonNull::from(&mut raw).cast(), x, y)
        };
        let layout = SampleLayout {
            alpha: self.rep.hasAlpha(),
            alpha_first: format.contains(NSBitmapFormat::AlphaFirst),
            premultiplied: !format.contains(NSBitmapFormat::AlphaNonpremultiplied),
            max: f64::from((1u32 << bits) - 1),
        };
        let mut components = decode_samples(&raw[..samples], layout)
            .ok_or_else(|| format!("сэмплы {raw:?} не разобраны: {}", self.format()))?;
        let raw_rgb = [
            unit_to_u8(components[0]),
            unit_to_u8(components[1]),
            unit_to_u8(components[2]),
        ];
        // SAFETY: четыре компонента — RGB пространства bitmap и альфа.
        let color = unsafe {
            NSColor::colorWithColorSpace_components_count(
                &self.rep.colorSpace(),
                NonNull::from(&mut components).cast(),
                4,
            )
        };
        let srgb = color
            .colorUsingColorSpace(&NSColorSpace::sRGBColorSpace())
            .ok_or_else(|| format!("пиксель {x},{y} без sRGB-представления"))?;
        Ok(Pixel {
            r: unit_to_u8(srgb.redComponent()),
            g: unit_to_u8(srgb.greenComponent()),
            b: unit_to_u8(srgb.blueComponent()),
            a: unit_to_u8(srgb.alphaComponent()),
            raw: raw_rgb,
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SampleLayout {
    pub alpha: bool,
    pub alpha_first: bool,
    pub premultiplied: bool,
    pub max: f64,
}

/// Сэмплы одного пикселя в RGB и альфу 0…1 без предумножения.
pub fn decode_samples(raw: &[usize], layout: SampleLayout) -> Option<[f64; 4]> {
    let unit = |value: usize| value as f64 / layout.max;
    let (rgb, alpha) = match (raw, layout.alpha, layout.alpha_first) {
        ([r, g, b], false, _) => ([*r, *g, *b], layout.max as usize),
        ([a, r, g, b], true, true) => ([*r, *g, *b], *a),
        ([r, g, b, a], true, false) => ([*r, *g, *b], *a),
        _ => return None,
    };
    let alpha = unit(alpha);
    let channel = |value: usize| {
        let value = unit(value);
        if layout.premultiplied && alpha > 0.0 {
            (value / alpha).min(1.0)
        } else {
            value
        }
    };
    Some([channel(rgb[0]), channel(rgb[1]), channel(rgb[2]), alpha])
}

/// Индекс пикселя (x, y от верхнего левого угла) под точкой в координатах вида.
pub fn pixel_index(
    point: NSPoint,
    bounds: NSRect,
    flipped: bool,
    pixels_wide: isize,
    pixels_high: isize,
) -> Option<(isize, isize)> {
    if bounds.size.width <= 0.0 || bounds.size.height <= 0.0 {
        return None;
    }
    let scale_x = pixels_wide as f64 / bounds.size.width;
    let scale_y = pixels_high as f64 / bounds.size.height;
    let from_left = point.x - bounds.origin.x;
    let from_top = if flipped {
        point.y - bounds.origin.y
    } else {
        bounds.origin.y + bounds.size.height - point.y
    };
    let x = (from_left * scale_x).floor();
    let y = (from_top * scale_y).floor();
    let inside = x >= 0.0 && y >= 0.0 && x < pixels_wide as f64 && y < pixels_high as f64;
    inside.then_some((x as isize, y as isize))
}

#[cfg(test)]
mod tests {
    use objc2_foundation::NSSize;

    use super::*;

    fn bounds() -> NSRect {
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(100.0, 50.0))
    }

    #[test]
    fn pixel_index_unflipped_counts_from_top() {
        assert_eq!(
            pixel_index(NSPoint::new(0.25, 49.75), bounds(), false, 200, 100),
            Some((0, 0))
        );
        assert_eq!(
            pixel_index(NSPoint::new(99.75, 0.25), bounds(), false, 200, 100),
            Some((199, 99))
        );
        assert_eq!(
            pixel_index(NSPoint::new(10.0, 10.0), bounds(), false, 200, 100),
            Some((20, 80))
        );
    }

    #[test]
    fn pixel_index_flipped_and_outside() {
        assert_eq!(
            pixel_index(NSPoint::new(10.0, 10.0), bounds(), true, 200, 100),
            Some((20, 20))
        );
        assert_eq!(
            pixel_index(NSPoint::new(100.0, 10.0), bounds(), true, 200, 100),
            None
        );
        assert_eq!(
            pixel_index(NSPoint::new(10.0, 0.0), bounds(), false, 200, 100),
            None
        );
        let empty = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 10.0));
        assert_eq!(
            pixel_index(NSPoint::new(0.0, 0.0), empty, false, 0, 20),
            None
        );
    }

    #[test]
    fn samples_decode_by_layout() {
        let rgba = SampleLayout {
            alpha: true,
            alpha_first: false,
            premultiplied: true,
            max: 255.0,
        };
        assert_eq!(
            decode_samples(&[255, 0, 51, 255], rgba),
            Some([1.0, 0.0, 0.2, 1.0])
        );
        let half = decode_samples(&[64, 0, 0, 128], rgba).unwrap();
        assert!((half[0] - 0.5).abs() < 0.01 && (half[3] - 128.0 / 255.0).abs() < 1e-9);
        let argb = SampleLayout {
            alpha_first: true,
            premultiplied: false,
            ..rgba
        };
        assert_eq!(
            decode_samples(&[255, 0, 255, 0], argb),
            Some([0.0, 1.0, 0.0, 1.0])
        );
        let rgb16 = SampleLayout {
            alpha: false,
            max: 65535.0,
            ..rgba
        };
        assert_eq!(
            decode_samples(&[65535, 0, 0], rgb16),
            Some([1.0, 0.0, 0.0, 1.0])
        );
        assert_eq!(decode_samples(&[1, 2, 3], rgba), None);
    }

    #[test]
    fn pixel_distance_is_the_largest_channel() {
        let pixel = Pixel {
            r: 0xE6,
            g: 0xFE,
            b: 0xEC,
            a: 255,
            raw: [0, 0, 0],
        };
        assert_eq!(pixel.distance(Rgb::hex(0xE6FFEC)), 1);
        assert_eq!(pixel.distance(Rgb::hex(0xE0FFEC)), 6);
        assert_eq!(pixel.hex(), "#E6FEEC");
        let clear = Pixel { a: 0, ..pixel };
        assert_eq!(clear.distance(Rgb::hex(0xE6FEEC)), 255);
    }
}
