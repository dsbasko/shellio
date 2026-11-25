//! Оракул снимков `make e2e` (план P1.3, «Матрица приёмки»; ADR-0021, «Уточнено в P1.3»).
//!
//! Ожидаемые цвета участков оракул вычисляет сам из `prototype/css/tokens.css` и таблицы
//! ADR-0027 ([`css`]), без кода темы приложения. Снимок окна в sRGB ([`png`]) сравнивается по
//! участкам из файла ожиданий ([`expect`]): медиана пикселей участка против ожидаемого цвета с
//! допуском [`TOLERANCE`] на канал. Участки сравниваются только на снимке, чей маркер поколения
//! равен ожидаемому поколению. Линию под активной вкладкой меряет поперечная серия
//! ([`underline`]).
//!
//! Обычное окно маркера не рисует (план P1.4, «Наблюдение кадра в обычном окне»; ADR-0021,
//! «Уточнено в P1.4 (планирование)»). Там снимок принимается без маркера ([`Previous`]): на двух
//! снимках подряд медианы каждого участка совпадают в пределах [`TOLERANCE`], а поколение
//! `ui.window` между ними не изменилось. Срок ожидания совпадения — [`SETTLE_MS`].

#![forbid(unsafe_code)]

pub mod calibrate;
pub mod conform;
pub mod css;
pub mod expect;
pub mod png;
pub mod underline;

use std::fmt;

use serde::Serialize;

use crate::css::{Cell, Layer, Palette, Rgb};
use crate::expect::{Expectations, Geometry, InspectView, PixelRect, Rect, WindowView};
use crate::png::{Image, SrgbTag};

/// Допуск на канал в sRGB («Матрица приёмки»): фиксирован до первого прогона.
pub const TOLERANCE: u8 = 3;

/// Срок ожидания двух совпадающих снимков в режиме без маркера, мс: как у маркера в `appkit.sh`.
pub const SETTLE_MS: u64 = 3000;

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

/// Предыдущий снимок режима без маркера.
#[derive(Clone, Copy)]
pub struct Previous<'a> {
    pub window: &'a WindowView,
    pub image: &'a Image,
    /// Сколько прошло от первого снимка серии после квитанции действия, мс.
    pub elapsed_ms: u64,
}

/// Снимки ещё не устоялись: причина и признак повтора. Повтор нужен, пока не истёк
/// [`SETTLE_MS`]; после срока вердикт `generation` окончательный.
fn unsettled(reason: String, elapsed_ms: u64) -> (String, bool) {
    if elapsed_ms >= SETTLE_MS {
        (
            format!("снимки не устоялись за {SETTLE_MS} мс (прошло {elapsed_ms}): {reason}"),
            false,
        )
    } else {
        (reason, true)
    }
}

/// Поколения окна, дерева и предыдущего снимка в режиме без маркера: `Some` — причина, по
/// которой пара снимков не принимается.
fn generations_differ(
    previous: &Previous<'_>,
    window: &WindowView,
    inspect: &InspectView,
    image: &Image,
) -> Result<Option<String>, Error> {
    if (previous.image.width, previous.image.height) != (image.width, image.height)
        || previous.window.frame != window.frame
    {
        return Err(Error::new(format!(
            "предыдущий снимок {}×{} окна {:?}, текущий {}×{} окна {:?}",
            previous.image.width,
            previous.image.height,
            previous.window.frame,
            image.width,
            image.height,
            window.frame
        )));
    }
    Ok(if previous.window.generation != window.generation {
        Some(format!(
            "поколение ui.window между снимками изменилось: {} → {}",
            previous.window.generation, window.generation
        ))
    } else if inspect.generation != window.generation {
        Some(format!(
            "ui.inspect снят на поколении {}, ui.window — {}",
            inspect.generation, window.generation
        ))
    } else {
        None
    })
}

/// Пара снимков режима без маркера для сверки всего окна. `None` — пара принята: поколение
/// `ui.window` между снимками то же, `ui.inspect` снят на нём, и ни один пиксель окна не изменился
/// больше допуска [`TOLERANCE`]. Иначе — причина и признак повтора до срока [`SETTLE_MS`].
pub fn frame_unsettled(
    previous: &Previous<'_>,
    window: &WindowView,
    inspect: &InspectView,
    image: &Image,
) -> Result<Option<(String, bool)>, Error> {
    let mut reason = generations_differ(previous, window, inspect, image)?;
    if reason.is_none() {
        let (mut moved, mut worst) = (0_u64, 0_u8);
        for y in 0..image.height {
            for x in 0..image.width {
                let (Some(before), Some(now)) = (previous.image.pixel(x, y), image.pixel(x, y))
                else {
                    return Err(Error::new(format!("пиксель {x}×{y} за кадром снимка")));
                };
                let delta = before.rgb.max_delta(now.rgb);
                if delta > TOLERANCE {
                    moved += 1;
                    worst = worst.max(delta);
                }
            }
        }
        if moved > 0 {
            reason = Some(format!(
                "между снимками изменилось пикселей: {moved}, наибольшая разница канала {worst}"
            ));
        }
    }
    Ok(reason.map(|reason| unsettled(reason, previous.elapsed_ms)))
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
    /// Режим без маркера: при вердикте `generation` — нужен ли следующий снимок до срока.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry: Option<bool>,
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
            retry: None,
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
    /// Ожидаемое поколение: поколение ответа команды. Без маркера не сравнивается.
    pub generation: u64,
    /// Режим без маркера: предыдущий снимок той же серии.
    pub previous: Option<Previous<'a>>,
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
        previous,
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
        retry: None,
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
    if let Some(previous) = previous {
        if let Some(generations) = report.generation.as_mut() {
            generations.expected = window.generation;
        }
        let mut reason = generations_differ(&previous, window, inspect, image)?;
        if reason.is_none() {
            let mut moved = Vec::new();
            for (region, place, _) in &placed {
                let (before, _) = sample(previous.image, place.pixels)?;
                let (now, _) = sample(image, place.pixels)?;
                let delta = before.max_delta(now);
                if delta > TOLERANCE {
                    moved.push(format!("{}: {before} → {now}", region.id));
                }
            }
            if !moved.is_empty() {
                reason = Some(format!(
                    "участки между снимками не совпали: {}",
                    moved.join("; ")
                ));
            }
        }
        if let Some(reason) = reason {
            let (reason, retry) = unsettled(reason, previous.elapsed_ms);
            report.verdict = Verdict::Generation;
            report.reason = Some(reason);
            report.retry = Some(retry);
            return Ok(report);
        }
        report.retry = Some(false);
        return judge(report, placed, image);
    }
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
    judge(report, placed, image)
}

/// Участки снимка против ожиданий после того, как отрисовка наблюдалась.
fn judge(
    mut report: Report,
    placed: Vec<(&expect::Region, expect::Placed, Rgb)>,
    image: &Image,
) -> Result<Report, Error> {
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
