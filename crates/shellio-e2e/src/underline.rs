//! Поперечная серия через линию под активной вкладкой (план P1.3, пункт `tab-underline`).
//!
//! Линия тоньше 8 точек, поэтому медиана участка её не меряет. Оракул берёт столбец пикселей
//! через середину вкладки, от 4 точек над её низом до 10 точек под ним, и ищет самую длинную
//! непрерывную серию пикселей в допуске от цвета `--focus` ячейки под слоем вкладки. Оба края
//! серии должны лежать внутри столбца: за ними — пиксели другого цвета (фон полосы, кольцо
//! выбранной вкладки или линия под полосой). Толщина серии в точках сравнивается с `--focus-h`
//! ячейки из `tokens.css` с допуском [`THICKNESS_TOLERANCE`], цвет середины — с допуском
//! [`TOLERANCE`].

use serde::Serialize;

use crate::css::{Cell, Layer, Palette, Rgb};
use crate::expect::{Geometry, InspectView, WindowView};
use crate::png::Image;
use crate::{
    Error, Generations, Previous, TOLERANCE, Verdict, generations_differ, read_marker, unsettled,
};

/// Допуск толщины линии в точках («Матрица приёмки», `tab-underline`).
pub const THICKNESS_TOLERANCE: f64 = 0.5;
/// Столбец начинается над низом вкладки и кончается под ним, в точках.
pub const ABOVE: f64 = 4.0;
pub const BELOW: f64 = 10.0;

/// Входы поперечной серии.
pub struct UnderlineInputs<'a> {
    pub palette: &'a Palette,
    pub window: &'a WindowView,
    pub inspect: &'a InspectView,
    pub image: &'a Image,
    /// Узел выбранной вкладки в `ui.inspect`.
    pub tab: &'a str,
    /// Слой над вкладкой: тон или вуаль подложки.
    pub layer: Layer,
    pub generation: u64,
    /// Режим без маркера: предыдущий снимок той же серии, столбец сравнивается попиксельно.
    pub previous: Option<Previous<'a>>,
}

/// Отчёт поперечной серии.
#[derive(Debug, Clone, Serialize)]
pub struct UnderlineReport {
    pub verdict: Verdict,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cell: Option<Cell>,
    pub tolerance: u8,
    pub thickness_tolerance: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scale: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<Generations>,
    /// Режим без маркера: при вердикте `generation` — нужен ли следующий снимок до срока.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry: Option<bool>,
    pub tab: String,
    pub layer: Layer,
    /// Ожидаемый цвет линии и толщина `--focus-h` в точках.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected: Option<Rgb>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_points: Option<f64>,
    /// Столбец в пикселях снимка и строки его начала и конца.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows: Option<(u32, u32)>,
    /// Цвета столбца сверху вниз.
    pub series: Vec<Rgb>,
    /// Найденная линия: первая и последняя строка серии, толщина в точках, цвет середины.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edges: Option<(u32, u32)>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thickness_points: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub middle: Option<Rgb>,
}

impl UnderlineReport {
    fn new(tab: &str, layer: Layer) -> UnderlineReport {
        UnderlineReport {
            verdict: Verdict::Pass,
            reason: None,
            cell: None,
            tolerance: TOLERANCE,
            thickness_tolerance: THICKNESS_TOLERANCE,
            scale: None,
            generation: None,
            retry: None,
            tab: tab.to_owned(),
            layer,
            expected: None,
            expected_points: None,
            column: None,
            rows: None,
            series: Vec::new(),
            edges: None,
            thickness_points: None,
            middle: None,
        }
    }

    /// Отчёт об ошибке исполнения.
    pub fn error(tab: &str, layer: Layer, error: &Error) -> UnderlineReport {
        UnderlineReport {
            verdict: Verdict::Error,
            reason: Some(error.message().to_owned()),
            ..UnderlineReport::new(tab, layer)
        }
    }
}

/// Длина в точках из значения `tokens.css` вида `2px`.
pub fn points(value: &str) -> Result<f64, Error> {
    value
        .trim()
        .strip_suffix("px")
        .and_then(|number| number.trim().parse::<f64>().ok())
        .filter(|number| number.is_finite() && *number > 0.0)
        .ok_or_else(|| Error::new(format!("не длина в px: {value:?}")))
}

/// Самая длинная непрерывная серия индексов, где `matches` истинно: `(начало, конец)`
/// включительно. Первая из равных по длине.
fn longest_run(matches: &[bool]) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize)> = None;
    let mut start = None;
    for (index, &hit) in matches.iter().chain(std::iter::once(&false)).enumerate() {
        match (hit, start) {
            (true, None) => start = Some(index),
            (false, Some(first)) => {
                let run = (first, index - 1);
                if best.is_none_or(|(a, b)| run.1 - run.0 > b - a) {
                    best = Some(run);
                }
                start = None;
            }
            _ => {}
        }
    }
    best
}

/// Поперечная серия через линию под вкладкой. Ошибки исполнения — внешний `Err`.
pub fn underline(inputs: &UnderlineInputs<'_>) -> Result<UnderlineReport, Error> {
    let UnderlineInputs {
        palette,
        window,
        inspect,
        image,
        tab,
        layer,
        generation,
        previous,
    } = *inputs;
    let geometry = Geometry::new(window, inspect, (image.width, image.height))?;
    let cell = window.cell();
    let expected = palette.expected(cell, "focus", layer)?;
    let expected_points = points(&palette.variable(cell, "--focus-h")?)?;
    let mut report = UnderlineReport {
        cell: Some(cell),
        scale: Some(geometry.scale),
        generation: Some(Generations {
            expected: generation,
            inspect: inspect.generation,
            png: None,
        }),
        expected: Some(expected),
        expected_points: Some(expected_points),
        ..UnderlineReport::new(tab, layer)
    };
    let component = inspect.component(tab)?;
    if !component.visible {
        return Err(Error::new(format!("вкладка {tab} не видима")));
    }
    geometry.check_inside(component)?;
    let not_observed = match previous {
        Some(previous) => {
            if let Some(generations) = report.generation.as_mut() {
                generations.expected = window.generation;
            }
            generations_differ(&previous, window, inspect, image)?
        }
        None => {
            let marker = read_marker(image, inspect, &geometry)?;
            if let (Some(generations), Ok(value)) = (report.generation.as_mut(), marker) {
                generations.png = Some(value);
            }
            match marker {
                Err(unreadable) => Some(format!("маркер поколения не читается: {unreadable}")),
                Ok(value) if value != generation => Some(format!(
                    "в снимке поколение {value}, ожидалось {generation}"
                )),
                Ok(_) if inspect.generation != generation => Some(format!(
                    "ui.inspect снят на поколении {}, ожидалось {generation}",
                    inspect.generation
                )),
                Ok(_) => None,
            }
        }
    };
    if let Some(reason) = not_observed {
        report.verdict = Verdict::Generation;
        if let Some(previous) = previous {
            let (reason, retry) = unsettled(reason, previous.elapsed_ms);
            report.reason = Some(reason);
            report.retry = Some(retry);
        } else {
            report.reason = Some(reason);
        }
        return Ok(report);
    }
    let top = geometry.flip(component.frame);
    let scale = geometry.scale;
    let bottom = top.y + top.height;
    let column = ((top.x + top.width / 2.0) * scale).floor();
    let first = ((bottom - ABOVE) * scale).floor();
    let last = ((bottom + BELOW) * scale).ceil() - 1.0;
    let (width, height) = geometry.pixels;
    if column < 0.0 || first < 0.0 || column >= f64::from(width) || last >= f64::from(height) {
        return Err(Error::new(format!(
            "столбец {column} строк {first}…{last} за кадром снимка {width}×{height}"
        )));
    }
    // Значения проверены на диапазон снимка выше.
    let (column, first, last) = (column as u32, first as u32, last as u32);
    report.column = Some(column);
    report.rows = Some((first, last));
    let mut matches = Vec::new();
    let mut moved = Vec::new();
    for y in first..=last {
        let pixel = image
            .pixel(column, y)
            .ok_or_else(|| Error::new(format!("пиксель {column}×{y} за кадром снимка")))?;
        if let Some(previous) = previous {
            let before = previous
                .image
                .pixel(column, y)
                .ok_or_else(|| Error::new(format!("пиксель {column}×{y} за кадром снимка")))?;
            if before.rgb.max_delta(pixel.rgb) > TOLERANCE {
                moved.push(format!("строка {y}: {} → {}", before.rgb, pixel.rgb));
            }
        }
        report.series.push(pixel.rgb);
        matches.push(pixel.alpha == 255 && pixel.rgb.max_delta(expected) <= TOLERANCE);
    }
    if let Some(previous) = previous {
        if !moved.is_empty() {
            let (reason, retry) = unsettled(
                format!("столбец между снимками не совпал: {}", moved.join("; ")),
                previous.elapsed_ms,
            );
            report.verdict = Verdict::Generation;
            report.reason = Some(reason);
            report.retry = Some(retry);
            return Ok(report);
        }
        report.retry = Some(false);
    }
    let fail = |report: &mut UnderlineReport, reason: String| {
        report.verdict = Verdict::Mismatch;
        report.reason = Some(reason);
    };
    let Some((start, end)) = longest_run(&matches) else {
        fail(
            &mut report,
            format!("в столбце нет пикселей цвета линии {expected}"),
        );
        return Ok(report);
    };
    let edges = (first + start as u32, first + end as u32);
    report.edges = Some(edges);
    let thickness = (end - start + 1) as f64 / scale;
    report.thickness_points = Some(thickness);
    let middle = report.series[(start + end) / 2];
    report.middle = Some(middle);
    if start == 0 || end == matches.len() - 1 {
        fail(
            &mut report,
            "край линии не найден: серия цвета линии упирается в конец столбца".to_owned(),
        );
    } else if (thickness - expected_points).abs() > THICKNESS_TOLERANCE {
        fail(
            &mut report,
            format!(
                "толщина линии {thickness} точки, ожидалось {expected_points} ± {THICKNESS_TOLERANCE}"
            ),
        );
    } else if middle.max_delta(expected) > TOLERANCE {
        fail(
            &mut report,
            format!("цвет середины линии {middle}, ожидалось {expected}"),
        );
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lengths_are_read_in_points() {
        assert_eq!(points("2px"), Ok(2.0));
        assert_eq!(points(" 3px "), Ok(3.0));
        assert!(points("2").is_err());
        assert!(points("0px").is_err());
        assert!(points("abcpx").is_err());
    }

    #[test]
    fn longest_run_takes_the_first_longest() {
        assert_eq!(longest_run(&[]), None);
        assert_eq!(longest_run(&[false, false]), None);
        assert_eq!(
            longest_run(&[false, true, true, false, true, true, true, false]),
            Some((4, 6))
        );
        assert_eq!(longest_run(&[true, true, false, true, true]), Some((0, 1)));
        assert_eq!(longest_run(&[false, true]), Some((1, 1)));
    }
}
