//! Маска значка (ADR-0037, «Маски значков»): одно правило переднего плана у паспорта прототипа и у
//! снимка продукта. Пиксель — передний план, если наибольшая по каналам разница в sRGB с известным
//! цветом фона под значком не меньше [`ICON_FG`]. Фон — заливка ближайшего предка с ключом
//! ([`backdrop`]). Кадр значка привязывается к пиксельной сетке при DPR снимка ([`PixelClip`]).

use std::io::Cursor;

use png::{BitDepth, ColorType, Decoder, Encoder, Transformations};

use crate::passport::{Element, Rect, Rgba};

/// Порог переднего плана в единицах канала sRGB (`conformance/calibration/controls.json`,
/// `icon_fg`). Закреплён до любого замера.
pub const ICON_FG: u8 = 24;

/// Область снимка в пикселях устройства: кадр значка, расширенный до целых пикселей.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelClip {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl PixelClip {
    /// Кадр в точках, привязанный к сетке пикселей при плотности `dpr`: левый и верхний край вниз,
    /// правый и нижний вверх. Пустой кадр или кадр вне окна — ошибка.
    pub fn of(frame: &Rect, dpr: f64) -> Result<Self, String> {
        let edge = |v: f64| (v * dpr * 1e6).round() / 1e6;
        let (x0, y0) = (edge(frame.x).floor(), edge(frame.y).floor());
        let (x1, y1) = (
            edge(frame.x + frame.w).ceil(),
            edge(frame.y + frame.h).ceil(),
        );
        if x0 < 0.0 || y0 < 0.0 || x1 <= x0 || y1 <= y0 {
            return Err(format!(
                "кадр значка {frame:?} пуст или вне окна при DPR {dpr}"
            ));
        }
        Ok(Self {
            x: x0 as u32,
            y: y0 as u32,
            width: (x1 - x0) as u32,
            height: (y1 - y0) as u32,
        })
    }

    /// Та же область в точках CSS.
    pub fn in_points(&self, dpr: f64) -> Rect {
        Rect {
            x: f64::from(self.x) / dpr,
            y: f64::from(self.y) / dpr,
            w: f64::from(self.width) / dpr,
            h: f64::from(self.height) / dpr,
        }
    }

    /// Область для `Page.captureScreenshot` в целых точках CSS, покрывающая кадр, и место кадра в
    /// её снимке. Целая область не зависит от того, как Chrome привязывает дробное начало области
    /// снимка к пикселям; кадр вырезается из её снимка по своим пикселям ([`crop_png`]). Плотность —
    /// целое число: иначе целая точка не ложится на границу пикселя.
    pub fn capture(&self, dpr: f64) -> Result<(Rect, PixelClip), String> {
        if dpr < 1.0 || dpr.fract() != 0.0 {
            return Err(format!("область снимка: плотность {dpr} не целое число"));
        }
        let step = dpr as u32;
        let (x0, y0) = (self.x / step, self.y / step);
        let (x1, y1) = (
            (self.x + self.width).div_ceil(step),
            (self.y + self.height).div_ceil(step),
        );
        Ok((
            Rect {
                x: f64::from(x0),
                y: f64::from(y0),
                w: f64::from(x1 - x0),
                h: f64::from(y1 - y0),
            },
            PixelClip {
                x: self.x - x0 * step,
                y: self.y - y0 * step,
                width: self.width,
                height: self.height,
            },
        ))
    }
}

/// Вырезка области из PNG: RGBA 8 бит по строкам, запись детерминирована. Область за краем
/// снимка — ошибка.
pub fn crop_png(bytes: &[u8], clip: PixelClip) -> Result<Vec<u8>, String> {
    let (width, height, rgba) = decode_rgba(bytes)?;
    if clip.x + clip.width > width || clip.y + clip.height > height {
        return Err(format!("область {clip:?} за краем снимка {width}×{height}"));
    }
    let row = clip.width as usize * 4;
    let mut data = Vec::with_capacity(row * clip.height as usize);
    for y in clip.y..clip.y + clip.height {
        let start = (y as usize * width as usize + clip.x as usize) * 4;
        data.extend_from_slice(&rgba[start..start + row]);
    }
    let mut out = Vec::new();
    {
        let mut encoder = Encoder::new(&mut out, clip.width, clip.height);
        encoder.set_color(ColorType::Rgba);
        encoder.set_depth(BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
        writer.write_image_data(&data).map_err(|e| e.to_string())?;
    }
    Ok(out)
}

/// Фон под значком: заливка ближайшего предка с ключом. Непрозрачная заливка — фон; полупрозрачная
/// между значком и непрозрачной заливкой или отсутствие заливки у всей цепочки — ошибка: фон тогда
/// не известен однозначно ни у прототипа, ни у продукта.
pub fn backdrop(elements: &[Element], ck: &str) -> Result<Rgba, String> {
    let find = |key: &str| elements.iter().find(|e| e.ck == key);
    let mut current = find(ck).ok_or_else(|| format!("{ck}: значка нет среди элементов"))?;
    while let Some(parent) = &current.parent {
        current = find(parent).ok_or_else(|| format!("{ck}: родителя «{parent}» нет"))?;
        if let Some(fill) = current.fill {
            if fill.a >= 1.0 {
                return Ok(fill);
            }
            return Err(format!(
                "{ck}: полупрозрачная заливка у «{}» под значком",
                current.ck
            ));
        }
    }
    Err(format!("{ck}: ни у одного предка с ключом нет заливки"))
}

/// Передний план по правилу [`ICON_FG`]: `rgba` — пиксели по строкам, по четыре байта.
pub fn foreground(rgba: &[u8], background: Rgba, threshold: u8) -> Vec<bool> {
    rgba.as_chunks::<4>()
        .0
        .iter()
        .map(|p| {
            let d = [
                p[0].abs_diff(background.r),
                p[1].abs_diff(background.g),
                p[2].abs_diff(background.b),
            ];
            d.into_iter().max().unwrap_or(0) >= threshold
        })
        .collect()
}

/// Маска значка: ширина, высота и передний план по строкам.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mask {
    pub width: u32,
    pub height: u32,
    pub on: Vec<bool>,
}

impl Mask {
    /// Маска по снимку PNG области значка и известному фону.
    pub fn from_png(bytes: &[u8], background: Rgba) -> Result<Self, String> {
        let (width, height, rgba) = decode_rgba(bytes)?;
        Ok(Self {
            width,
            height,
            on: foreground(&rgba, background, ICON_FG),
        })
    }

    /// PNG в оттенках серого 8 бит: 255 — передний план, 0 — фон. Запись детерминирована.
    pub fn to_png(&self) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        {
            let mut encoder = Encoder::new(&mut out, self.width, self.height);
            encoder.set_color(ColorType::Grayscale);
            encoder.set_depth(BitDepth::Eight);
            let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
            let data: Vec<u8> = self.on.iter().map(|&on| if on { 255 } else { 0 }).collect();
            writer.write_image_data(&data).map_err(|e| e.to_string())?;
        }
        Ok(out)
    }

    /// Маска из PNG, записанного [`Mask::to_png`].
    pub fn read_png(bytes: &[u8]) -> Result<Self, String> {
        let (width, height, rgba) = decode_rgba(bytes)?;
        Ok(Self {
            width,
            height,
            on: rgba
                .as_chunks::<4>()
                .0
                .iter()
                .map(|p| p[0] >= 128)
                .collect(),
        })
    }

    /// Пересечение к объединению; две пустые маски совпадают полностью.
    pub fn iou(&self, other: &Mask) -> Result<f64, String> {
        if (self.width, self.height) != (other.width, other.height) {
            return Err(format!(
                "маски {}×{} и {}×{} разного размера",
                self.width, self.height, other.width, other.height
            ));
        }
        let (mut inter, mut union) = (0u64, 0u64);
        for (a, b) in self.on.iter().zip(&other.on) {
            inter += u64::from(*a && *b);
            union += u64::from(*a || *b);
        }
        Ok(if union == 0 {
            1.0
        } else {
            inter as f64 / union as f64
        })
    }
}

/// Пиксели PNG в RGBA 8 бит по строкам.
pub fn decode_rgba(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    let mut decoder = Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(Transformations::normalize_to_color8());
    let mut reader = decoder
        .read_info()
        .map_err(|e| format!("PNG не читается: {e}"))?;
    let size = reader.output_buffer_size().ok_or("PNG слишком велик")?;
    let mut buffer = vec![0; size];
    let frame = reader
        .next_frame(&mut buffer)
        .map_err(|e| format!("PNG не читается: {e}"))?;
    buffer.truncate(frame.buffer_size());
    let rgba: Vec<u8> = match frame.color_type {
        ColorType::Rgba => buffer,
        ColorType::Rgb => buffer
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|&[r, g, b]| [r, g, b, 255])
            .collect(),
        ColorType::Grayscale => buffer.iter().flat_map(|&v| [v, v, v, 255]).collect(),
        ColorType::GrayscaleAlpha => buffer
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|&[v, a]| [v, v, v, a])
            .collect(),
        ColorType::Indexed => return Err("PNG с палитрой не развернулся в RGB".to_owned()),
    };
    if rgba.len() != frame.width as usize * frame.height as usize * 4 {
        return Err(format!(
            "PNG {}×{}: неверное число байт пикселей",
            frame.width, frame.height
        ));
    }
    Ok((frame.width, frame.height, rgba))
}
