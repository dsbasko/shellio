//! Разбор PNG снимка в RGBA по 8 бит и проверка, что пиксели в sRGB.
//!
//! Сценарий переводит снимок в sRGB один раз (`sips --matchTo` профиля sRGB), оракул только
//! проверяет результат. Приоритет сведений о цвете — по PNG третьей редакции: `cICP`, затем
//! `iCCP`, затем `sRGB`, затем пара `gAMA` и `cHRM`. sRGB признаётся `cICP` с первичными
//! BT.709 и переносом sRGB, чанк `sRGB` и пара `gAMA` 45455 с `cHRM` sRGB — запасные значения
//! чанка `sRGB` из стандарта, их пишет `sips` для файла, уже бывшего в sRGB. Любой профиль ICC и
//! файл без сведений о цвете отвергаются: оракул профили не толкует.

use std::io::Cursor;

use ::png::{ColorType, Decoder, Transformations};
use serde::Serialize;

use crate::Error;
use crate::css::Rgb;

/// Как PNG объявляет sRGB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SrgbTag {
    /// Чанк `cICP` с первичными BT.709 и переносом sRGB.
    Cicp,
    /// Чанк `sRGB`.
    SrgbChunk,
    /// Чанки `gAMA` и `cHRM` с запасными значениями sRGB.
    GammaChromaticities,
}

/// Снимок в RGBA по 8 бит, строки сверху вниз.
#[derive(Debug, Clone)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub tag: SrgbTag,
    rgba: Vec<u8>,
}

/// Пиксель снимка.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pixel {
    pub rgb: Rgb,
    pub alpha: u8,
}

impl Image {
    /// Пиксель в столбце `x` и строке `y` от верхнего левого угла.
    pub fn pixel(&self, x: u32, y: u32) -> Option<Pixel> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let index = (y as usize * self.width as usize + x as usize) * 4;
        let p = self.rgba.get(index..index + 4)?;
        Some(Pixel {
            rgb: Rgb::new(p[0], p[1], p[2]),
            alpha: p[3],
        })
    }
}

/// `gAMA` sRGB в стотысячных: 1/2,2.
const SRGB_GAMMA: u32 = 45_455;
/// `cHRM` sRGB в стотысячных: белая точка, красный, зелёный, синий.
const SRGB_CHROMATICITIES: [u32; 8] = [
    31_270, 32_900, 64_000, 33_000, 30_000, 60_000, 15_000, 6_000,
];

/// Разбор PNG. Снимок не в sRGB — ошибка с тем, что найдено вместо sRGB.
pub fn decode(bytes: &[u8]) -> Result<Image, Error> {
    let mut decoder = Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(Transformations::normalize_to_color8());
    let mut reader = decoder
        .read_info()
        .map_err(|e| Error::new(format!("PNG не читается: {e}")))?;
    let tag = srgb_tag(reader.info())?;
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| Error::new("PNG слишком велик"))?;
    let mut buffer = vec![0; size];
    let frame = reader
        .next_frame(&mut buffer)
        .map_err(|e| Error::new(format!("PNG не читается: {e}")))?;
    buffer.truncate(frame.buffer_size());
    let rgba = match frame.color_type {
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
        ColorType::Indexed => {
            return Err(Error::new("PNG с палитрой не развернулся в RGB"));
        }
    };
    let expected = frame.width as usize * frame.height as usize * 4;
    if rgba.len() != expected {
        return Err(Error::new(format!(
            "PNG {}×{}: {} байт пикселей вместо {expected}",
            frame.width,
            frame.height,
            rgba.len()
        )));
    }
    Ok(Image {
        width: frame.width,
        height: frame.height,
        tag,
        rgba,
    })
}

fn srgb_tag(info: &::png::Info<'_>) -> Result<SrgbTag, Error> {
    if let Some(cicp) = info.coding_independent_code_points {
        let srgb = cicp.color_primaries == 1
            && cicp.transfer_function == 13
            && cicp.matrix_coefficients == 0
            && cicp.is_video_full_range_image;
        return if srgb {
            Ok(SrgbTag::Cicp)
        } else {
            Err(Error::new(format!(
                "PNG не в sRGB: cICP с первичными {}, переносом {}, матрицей {}, полный диапазон — {}",
                cicp.color_primaries,
                cicp.transfer_function,
                cicp.matrix_coefficients,
                cicp.is_video_full_range_image
            )))
        };
    }
    if let Some(profile) = &info.icc_profile {
        let name = icc_description(profile).unwrap_or_else(|| "без имени".to_owned());
        return Err(Error::new(format!(
            "PNG не в sRGB: профиль ICC «{name}», снимок не переведён в sRGB"
        )));
    }
    if info.srgb.is_some() {
        return Ok(SrgbTag::SrgbChunk);
    }
    match (info.gama_chunk, info.chrm_chunk) {
        (Some(gamma), Some(chromaticities)) => {
            let found = [
                chromaticities.white.0,
                chromaticities.white.1,
                chromaticities.red.0,
                chromaticities.red.1,
                chromaticities.green.0,
                chromaticities.green.1,
                chromaticities.blue.0,
                chromaticities.blue.1,
            ]
            .map(|value| value.into_scaled());
            if gamma.into_scaled() == SRGB_GAMMA && found == SRGB_CHROMATICITIES {
                Ok(SrgbTag::GammaChromaticities)
            } else {
                Err(Error::new(format!(
                    "PNG не в sRGB: gAMA {}, cHRM {found:?}",
                    gamma.into_scaled()
                )))
            }
        }
        (Some(gamma), None) => Err(Error::new(format!(
            "PNG не в sRGB: gAMA {} без cHRM",
            gamma.into_scaled()
        ))),
        (None, Some(_)) => Err(Error::new("PNG не в sRGB: cHRM без gAMA")),
        (None, None) => Err(Error::new(
            "PNG не в sRGB: нет сведений о цвете (cICP, iCCP, sRGB, gAMA и cHRM)",
        )),
    }
}

fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let slice = bytes.get(at..at.checked_add(4)?)?;
    Some(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

/// Описание профиля ICC (тег `desc`) для сообщения об ошибке: `desc` версии 2 или `mluc`
/// версии 4. Профиль не толкуется, имя только называет его в отказе.
fn icc_description(profile: &[u8]) -> Option<String> {
    let count = read_u32(profile, 128)? as usize;
    let tag = (0..count.min(256)).find_map(|i| {
        let entry = 132 + i * 12;
        (profile.get(entry..entry + 4)? == b"desc").then_some(entry)
    })?;
    let offset = read_u32(profile, tag + 4)? as usize;
    let body = profile.get(offset..)?;
    match body.get(0..4)? {
        b"desc" => {
            let length = read_u32(body, 8)? as usize;
            let text = body.get(12..12 + length)?;
            let text: String = text
                .iter()
                .take_while(|&&b| b != 0)
                .map(|&b| char::from(b))
                .collect();
            Some(text)
        }
        b"mluc" => {
            let records = read_u32(body, 8)?;
            if records == 0 {
                return None;
            }
            let length = read_u32(body, 20)? as usize;
            let start = read_u32(body, 24)? as usize;
            let units: Vec<u16> = body
                .get(start..start + length)?
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&pair| u16::from_be_bytes(pair))
                .collect();
            String::from_utf16(&units).ok()
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mluc(text: &str) -> Vec<u8> {
        let units: Vec<u8> = text.encode_utf16().flat_map(u16::to_be_bytes).collect();
        let mut tag = b"mluc".to_vec();
        tag.extend([0; 4]);
        tag.extend(1_u32.to_be_bytes());
        tag.extend(12_u32.to_be_bytes());
        tag.extend(*b"enUS");
        tag.extend((units.len() as u32).to_be_bytes());
        tag.extend(28_u32.to_be_bytes());
        tag.extend(units);
        tag
    }

    fn profile_with_desc(desc: &[u8]) -> Vec<u8> {
        let mut profile = vec![0; 128];
        profile.extend(1_u32.to_be_bytes());
        profile.extend(*b"desc");
        profile.extend(144_u32.to_be_bytes());
        profile.extend((desc.len() as u32).to_be_bytes());
        profile.extend(desc);
        profile
    }

    #[test]
    fn icc_description_reads_v4_and_v2_tags() {
        let v4 = profile_with_desc(&mluc("Display P3"));
        assert_eq!(icc_description(&v4).as_deref(), Some("Display P3"));

        let mut v2 = b"desc".to_vec();
        v2.extend([0; 4]);
        v2.extend(11_u32.to_be_bytes());
        v2.extend(b"Color LCD\0\0");
        let v2 = profile_with_desc(&v2);
        assert_eq!(icc_description(&v2).as_deref(), Some("Color LCD"));

        assert_eq!(icc_description(&[0; 64]), None);
        assert_eq!(icc_description(&profile_with_desc(b"XYZ \0\0\0\0")), None);
    }
}
