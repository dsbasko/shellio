//! Оракул снимков `make e2e` (план P1.3, «Матрица приёмки»; ADR-0021, «Уточнено в P1.3»).
//!
//! Ожидаемые цвета участков оракул вычисляет сам из `prototype/css/tokens.css` и таблицы
//! ADR-0027 ([`css`]), без кода темы приложения. Снимок окна в sRGB ([`png`]) сравнивается по
//! участкам из файла ожиданий ([`expect`]): медиана пикселей участка против ожидаемого цвета с
//! допуском [`TOLERANCE`] на канал. Участки сравниваются только на снимке, чей маркер поколения
//! равен ожидаемому поколению.

#![forbid(unsafe_code)]

pub mod css;
pub mod expect;
pub mod png;

use std::fmt;

use serde::Serialize;

use crate::css::{Cell, Layer, Palette, Rgb};
use crate::expect::{Expectations, Geometry, InspectView, PixelRect, Rect, WindowView};
use crate::png::{Image, SrgbTag};

/// Допуск на канал в sRGB («Матрица приёмки»): фиксирован до первого прогона.
pub const TOLERANCE: u8 = 3;

/// Идентификатор маркера поколения в `ui.inspect`.
pub const MARKER_COMPONENT: &str = "gallery.marker";
/// Ячеек в стороне сетки маркера и сторона ячейки в точках.
pub const MARKER_CELLS: u32 = 8;
pub const MARKER_CELL: f64 = 3.0;

/// Ошибка исполнения оракула: вход не годится для сравнения.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    message: String,
}

impl Error {
    pub fn new(message: impl Into<String>) -> Error {
        Error {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

/// Почему маркер поколения не прочитан.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unreadable {
    pub row: u32,
    pub col: u32,
    pub rgb: Rgb,
    pub alpha: u8,
}

impl fmt::Display for Unreadable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "ячейка маркера {}×{} не чёрная и не белая: {}, альфа {}",
            self.row, self.col, self.rgb, self.alpha
        )
    }
}

/// Поколение из маркера снимка: `Err` внутри — маркер не читается (кадр ещё не отрисован или
/// закрыт), внешний `Err` — вход не годится.
pub fn read_marker(
    image: &Image,
    inspect: &InspectView,
    geometry: &Geometry,
) -> Result<Result<u64, Unreadable>, Error> {
    let marker = inspect.component(MARKER_COMPONENT)?;
    if !marker.visible {
        return Err(Error::new("маркер поколения не видим"));
    }
    geometry.check_inside(marker)?;
    let side = MARKER_CELL * f64::from(MARKER_CELLS);
    let frame = marker.frame;
    if (frame.width - side).abs() > 1e-6 || (frame.height - side).abs() > 1e-6 {
        return Err(Error::new(format!(
            "маркер поколения {}×{} точек вместо {side}×{side}",
            frame.width, frame.height
        )));
    }
    let top = geometry.flip(frame);
    let mut value = 0_u64;
    for row in 0..MARKER_CELLS {
        for col in 0..MARKER_CELLS {
            let x = (top.x + (f64::from(col) + 0.5) * MARKER_CELL) * geometry.scale;
            let y = (top.y + (f64::from(row) + 0.5) * MARKER_CELL) * geometry.scale;
            // Центр ячейки внутри снимка: маркер проверен на положение в содержимом.
            let pixel = image
                .pixel(x.floor() as u32, y.floor() as u32)
                .ok_or_else(|| Error::new("центр ячейки маркера за кадром снимка"))?;
            let bits = pixel.rgb.channels().map(|channel| channel >= 128);
            let bit = match bits {
                [true, true, true] if pixel.alpha == 255 => true,
                [false, false, false] if pixel.alpha == 255 => false,
                _ => {
                    return Ok(Err(Unreadable {
                        row,
                        col,
                        rgb: pixel.rgb,
                        alpha: pixel.alpha,
                    }));
                }
            };
            value = (value << 1) | u64::from(bit);
        }
    }
    Ok(Ok(value))
}

/// Итог сравнения.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Маркер равен ожидаемому поколению, все участки в допуске.
    Pass,
    /// Маркер равен ожидаемому поколению, хотя бы один участок вне допуска.
    Mismatch,
    /// Отрисовка ожидаемого поколения не наблюдалась: маркер не читается или называет другое
    /// поколение, либо `ui.inspect` снят на другом поколении. Участки не сравнивались.
    Generation,
    /// Ошибка исполнения: вход не годится для сравнения.
    Error,
}

/// Поколения сравнения.
#[derive(Debug, Clone, Serialize)]
pub struct Generations {
    pub expected: u64,
    pub inspect: u64,
    /// Поколение из маркера снимка; `None` — маркер не читается.
    pub png: Option<u64>,
}

/// Итог участка.
#[derive(Debug, Clone, Serialize)]
pub struct RegionReport {
    pub id: String,
    pub component: String,
    pub slot: String,
    pub layer: Layer,
    /// Участок в точках снимка, начало сверху слева.
    pub points: Rect,
    pub pixels: PixelRect,
    pub expected: Rgb,
    /// Медиана каждого канала по пикселям участка.
    pub median: Rgb,
    pub max_delta: u8,
    /// Пиксели участка с альфой меньше 255: окно непрозрачно, их быть не должно.
    pub transparent: u64,
    pub pass: bool,
}

/// Отчёт сравнения: JSON в stdout оракула.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub verdict: Verdict,
    /// Причина вердикта, кроме `pass`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cell: Option<Cell>,
    pub tolerance: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scale: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub png: Option<PngReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<Generations>,
    pub regions: Vec<RegionReport>,
}

/// Снимок в отчёте.
#[derive(Debug, Clone, Serialize)]
pub struct PngReport {
    pub width: u32,
    pub height: u32,
    pub srgb: SrgbTag,
}

impl Report {
    /// Отчёт об ошибке исполнения.
    pub fn error(error: &Error) -> Report {
        Report {
            verdict: Verdict::Error,
            reason: Some(error.message().to_owned()),
            cell: None,
            tolerance: TOLERANCE,
            scale: None,
            png: None,
            generation: None,
            regions: Vec::new(),
        }
    }
}

/// Входы сравнения после разбора.
pub struct Inputs<'a> {
    pub palette: &'a Palette,
    pub expectations: &'a Expectations,
    pub window: &'a WindowView,
    pub inspect: &'a InspectView,
    pub image: &'a Image,
    /// Ожидаемое поколение: поколение ответа команды.
    pub generation: u64,
}

/// Медиана по каналам участка и число непрозрачных пикселей.
fn sample(image: &Image, rect: PixelRect) -> Result<(Rgb, u64), Error> {
    let mut histograms = [[0_u64; 256]; 3];
    let mut transparent = 0;
    for y in rect.y..rect.y + rect.height {
        for x in rect.x..rect.x + rect.width {
            let pixel = image
                .pixel(x, y)
                .ok_or_else(|| Error::new(format!("пиксель {x}×{y} за кадром снимка")))?;
            for (histogram, channel) in histograms.iter_mut().zip(pixel.rgb.channels()) {
                histogram[usize::from(channel)] += 1;
            }
            if pixel.alpha < 255 {
                transparent += 1;
            }
        }
    }
    let total = u64::from(rect.width) * u64::from(rect.height);
    // Нижняя медиана: значение, на котором накопленное число переходит середину.
    let median = |histogram: &[u64; 256]| {
        let half = total.div_ceil(2);
        let mut seen = 0;
        for (value, count) in histogram.iter().enumerate() {
            seen += count;
            if seen >= half {
                return value as u8;
            }
        }
        255
    };
    Ok((
        Rgb::new(
            median(&histograms[0]),
            median(&histograms[1]),
            median(&histograms[2]),
        ),
        transparent,
    ))
}

/// Сравнение снимка с ожиданиями. Ошибки исполнения — внешний `Err`.
pub fn compare(inputs: &Inputs<'_>) -> Result<Report, Error> {
    let Inputs {
        palette,
        expectations,
        window,
        inspect,
        image,
        generation,
    } = *inputs;
    let geometry = Geometry::new(window, inspect, (image.width, image.height))?;
    let cell = window.cell();
    if cell != expectations.cell {
        return Err(Error::new(format!(
            "ячейка окна «{cell}», ожидания для «{}»",
            expectations.cell
        )));
    }
    let mut report = Report {
        verdict: Verdict::Pass,
        reason: None,
        cell: Some(cell),
        tolerance: TOLERANCE,
        scale: Some(geometry.scale),
        png: Some(PngReport {
            width: image.width,
            height: image.height,
            srgb: image.tag,
        }),
        generation: Some(Generations {
            expected: generation,
            inspect: inspect.generation,
            png: None,
        }),
        regions: Vec::new(),
    };
    let placed = expectations
        .regions
        .iter()
        .map(|region| {
            let expected = palette.expected(cell, &region.slot, region.layer)?;
            Ok((region, expect::place(region, inspect, &geometry)?, expected))
        })
        .collect::<Result<Vec<_>, Error>>()?;
    let marker = read_marker(image, inspect, &geometry)?;
    if let (Some(generations), Ok(value)) = (report.generation.as_mut(), marker) {
        generations.png = Some(value);
    }
    let not_observed = match marker {
        Err(unreadable) => Some(format!("маркер поколения не читается: {unreadable}")),
        Ok(value) if value != generation => Some(format!(
            "в снимке поколение {value}, ожидалось {generation}"
        )),
        Ok(_) if inspect.generation != generation => Some(format!(
            "ui.inspect снят на поколении {}, ожидалось {generation}",
            inspect.generation
        )),
        Ok(_) => None,
    };
    if let Some(reason) = not_observed {
        report.verdict = Verdict::Generation;
        report.reason = Some(reason);
        return Ok(report);
    }
    for (region, place, expected) in placed {
        let (median, transparent) = sample(image, place.pixels)?;
        let max_delta = median.max_delta(expected);
        let pass = max_delta <= TOLERANCE && transparent == 0;
        report.regions.push(RegionReport {
            id: region.id.clone(),
            component: region.component.clone(),
            slot: region.slot.clone(),
            layer: region.layer,
            points: place.points,
            pixels: place.pixels,
            expected,
            median,
            max_delta,
            transparent,
            pass,
        });
    }
    let failed: Vec<String> = report
        .regions
        .iter()
        .filter(|region| !region.pass)
        .map(|region| {
            if region.transparent > 0 {
                format!("{}: прозрачных пикселей {}", region.id, region.transparent)
            } else {
                format!(
                    "{}: медиана {}, ожидалось {}, расхождение {}",
                    region.id, region.median, region.expected, region.max_delta
                )
            }
        })
        .collect();
    if !failed.is_empty() {
        report.verdict = Verdict::Mismatch;
        report.reason = Some(failed.join("; "));
    }
    Ok(report)
}
