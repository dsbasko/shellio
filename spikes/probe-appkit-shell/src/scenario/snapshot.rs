//! Снимки. Рендер отдельного вида через `cacheDisplay(in:to:)` — пиксельная проба, снимком
//! экрана он не считается. Снимок окна (`screencapture -l`, с тенью) и композиции
//! (`screencapture -R` по прямоугольнику окна с запасом) делаются только при разрешении Screen
//! Recording, иначе снимок получает статус `pending-manual`. Запись файлов и ожидание
//! `screencapture` идут в рабочем потоке, главный поток только опрашивает итог на таймере.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use objc2::rc::Retained;
use objc2_app_kit::{
    NSBitmapFormat, NSBitmapImageFileType, NSBitmapImageRep, NSColor, NSView, NSWindow,
};
use objc2_core_graphics::CGPreflightScreenCaptureAccess;
use objc2_foundation::{NSDictionary, NSPoint, NSRect};

use super::readback::{self, Rgba};
use super::{Poll, wait_until};
use crate::results::{Size, Snapshot, SnapshotKind, SnapshotStatus};

pub const NO_PERMISSION: &str = "нет разрешения Screen Recording";
const SCREENCAPTURE: &str = "/usr/sbin/screencapture";
/// Запас вокруг окна у снимка композиции: тень и палитра поверх края.
pub const COMPOSITION_MARGIN: f64 = 40.0;

type Slot<T> = Arc<Mutex<Option<Result<T, String>>>>;
/// Итог `wait_all`: ошибка срока или итог каждой работы по порядку.
pub type Outcomes<T> = Result<Vec<Result<T, String>>, String>;

/// Работа в рабочем потоке; итог забирает главный поток.
pub struct Background<T> {
    slot: Slot<T>,
    _thread: Option<thread::JoinHandle<()>>,
}

impl<T: Send + 'static> Background<T> {
    pub fn spawn(name: &str, job: impl FnOnce() -> Result<T, String> + Send + 'static) -> Self {
        let slot: Slot<T> = Arc::new(Mutex::new(None));
        let filled = Arc::clone(&slot);
        let spawned = thread::Builder::new()
            .name(format!("probe-{name}"))
            .spawn(move || {
                let result = job();
                *filled
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(result);
            });
        let thread = match spawned {
            Ok(thread) => Some(thread),
            Err(err) => {
                *slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
                    Some(Err(format!("рабочий поток не запущен: {err}")));
                None
            }
        };
        Background {
            slot,
            _thread: thread,
        }
    }

    pub fn ready(value: T) -> Self {
        Background {
            slot: Arc::new(Mutex::new(Some(Ok(value)))),
            _thread: None,
        }
    }

    fn take(&self) -> Option<Result<T, String>> {
        self.slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }
}

/// Ждёт итоги всех работ на таймере главного цикла. Истечение срока — ошибка.
pub fn wait_all<T: Send + 'static>(
    jobs: Vec<Background<T>>,
    timeout: Duration,
    done: impl FnOnce(Outcomes<T>) + 'static,
) {
    let results: Vec<Option<Result<T, String>>> = jobs.iter().map(|_| None).collect();
    let state = std::cell::RefCell::new((jobs, results));
    wait_until(
        timeout,
        move || {
            let mut state = state.borrow_mut();
            let (jobs, results) = &mut *state;
            for (job, result) in jobs.iter().zip(results.iter_mut()) {
                if result.is_none() {
                    *result = job.take();
                }
            }
            let pending = results.iter().filter(|result| result.is_none()).count();
            if pending > 0 {
                return Poll::Pending(format!("рабочих потоков не закончено: {pending}"));
            }
            Poll::Ready(results.drain(..).flatten().collect())
        },
        move |result| match result {
            Ok((results, _)) => done(Ok(results)),
            Err(timeout) => done(Err(format!(
                "снимки не готовы за {} мс: {}",
                timeout.waited.as_millis(),
                timeout.last_state
            ))),
        },
    );
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

    pub fn pixels(&self) -> Size {
        Size {
            width: self.rep.pixelsWide() as f64,
            height: self.rep.pixelsHigh() as f64,
        }
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
            "{} бит × {} сэмпла, формат {:#x}, альфа {}",
            self.rep.bitsPerSample(),
            self.rep.samplesPerPixel(),
            self.rep.bitmapFormat().0,
            self.rep.hasAlpha()
        )
    }

    /// Пиксель под точкой в координатах вида, в sRGB. Сэмплы читаются как есть и переводятся
    /// из профиля bitmap: `colorAtX:y:` у bitmap с профилем дисплея возвращает значения в
    /// другом пространстве и искажает тёмные тона.
    pub fn pixel(&self, point: NSPoint) -> Result<Rgba, String> {
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
        // SAFETY: четыре компонента — RGB пространства bitmap и альфа.
        let color = unsafe {
            NSColor::colorWithColorSpace_components_count(
                &self.rep.colorSpace(),
                NonNull::from(&mut components).cast(),
                4,
            )
        };
        readback::srgb(&color).ok_or_else(|| format!("пиксель {x},{y} без sRGB-представления"))
    }

    /// PNG кодируется на главном потоке, файл пишет рабочий поток.
    pub fn save(&self, out: &Path, name: &str) -> Background<Snapshot> {
        let mut snapshot = Snapshot {
            name: name.to_owned(),
            kind: SnapshotKind::ViewRender,
            status: SnapshotStatus::Error,
            file: None,
            reason: None,
            rect_points: Some(readback::rect(self.bounds)),
            pixels: Some(self.pixels()),
            color_space: Some(self.color_space()),
        };
        // SAFETY: пустой словарь свойств PNG допустим.
        let data = unsafe {
            self.rep.representationUsingType_properties(
                NSBitmapImageFileType::PNG,
                &NSDictionary::new(),
            )
        };
        let Some(data) = data else {
            snapshot.reason = Some("PNG не закодирован".to_owned());
            return Background::ready(snapshot);
        };
        let bytes = data.to_vec();
        let file = format!("{name}.png");
        let path = out.join(&file);
        Background::spawn("render-write", move || {
            match fs::write(&path, &bytes) {
                Ok(()) => {
                    snapshot.status = SnapshotStatus::Ok;
                    snapshot.file = Some(file);
                }
                Err(err) => snapshot.reason = Some(format!("{}: {err}", path.display())),
            }
            Ok(snapshot)
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

pub fn screen_capture_allowed() -> bool {
    CGPreflightScreenCaptureAccess()
}

fn pending(name: &str, kind: SnapshotKind, rect_points: Option<NSRect>) -> Snapshot {
    Snapshot {
        name: name.to_owned(),
        kind,
        status: SnapshotStatus::PendingManual,
        file: None,
        reason: Some(NO_PERMISSION.to_owned()),
        rect_points: rect_points.map(readback::rect),
        pixels: None,
        color_space: None,
    }
}

/// Снимок окна с тенью: `screencapture -x -l <номер окна>`.
pub fn capture_window(window: &NSWindow, out: &Path, name: &str) -> Background<Snapshot> {
    let frame = window.frame();
    if !screen_capture_allowed() {
        return Background::ready(pending(name, SnapshotKind::Window, Some(frame)));
    }
    let args = vec![
        "-x".to_owned(),
        "-l".to_owned(),
        window.windowNumber().to_string(),
    ];
    run_capture(name, SnapshotKind::Window, frame, args, out)
}

/// Снимок прямоугольника экрана вокруг окна: `screencapture -x -R x,y,w,h`. Координаты
/// `screencapture` — от верхнего левого угла главного экрана.
pub fn capture_composition(
    window: &NSWindow,
    primary_height: f64,
    out: &Path,
    name: &str,
) -> Background<Snapshot> {
    let area = super::events::inset(window.frame(), -COMPOSITION_MARGIN);
    if !screen_capture_allowed() {
        return Background::ready(pending(name, SnapshotKind::Composition, Some(area)));
    }
    let args = vec![
        "-x".to_owned(),
        "-R".to_owned(),
        composition_region(area, primary_height),
    ];
    run_capture(name, SnapshotKind::Composition, area, args, out)
}

/// Аргумент `-R` для прямоугольника в координатах AppKit.
pub fn composition_region(area: NSRect, primary_height: f64) -> String {
    let top = primary_height - (area.origin.y + area.size.height);
    format!(
        "{},{},{},{}",
        area.origin.x.round(),
        top.round(),
        area.size.width.round(),
        area.size.height.round()
    )
}

fn run_capture(
    name: &str,
    kind: SnapshotKind,
    rect_points: NSRect,
    mut args: Vec<String>,
    out: &Path,
) -> Background<Snapshot> {
    let file = format!("{name}.png");
    let path: PathBuf = out.join(&file);
    args.push(path.display().to_string());
    let mut snapshot = Snapshot {
        name: name.to_owned(),
        kind,
        status: SnapshotStatus::Error,
        file: None,
        reason: None,
        rect_points: Some(readback::rect(rect_points)),
        pixels: None,
        color_space: None,
    };
    Background::spawn("screencapture", move || {
        let status = Command::new(SCREENCAPTURE).args(&args).status();
        let written = fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
        match status {
            Ok(status) if status.success() && written > 0 => {
                snapshot.status = SnapshotStatus::Ok;
                snapshot.file = Some(file);
            }
            Ok(status) => {
                snapshot.reason = Some(format!(
                    "{SCREENCAPTURE} {}: {status}, файл {written} байт",
                    args.join(" ")
                ));
            }
            Err(err) => snapshot.reason = Some(format!("{SCREENCAPTURE} не запущен: {err}")),
        }
        Ok(snapshot)
    })
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
        assert_eq!(
            decode_samples(&[0, 0, 0, 0], rgba),
            Some([0.0, 0.0, 0.0, 0.0])
        );
    }

    #[test]
    fn composition_region_uses_top_left_origin() {
        let area = NSRect::new(NSPoint::new(126.0, 90.0), NSSize::new(1260.0, 840.0));
        assert_eq!(composition_region(area, 982.0), "126,52,1260,840");
        let above = NSRect::new(NSPoint::new(0.0, 1000.0), NSSize::new(100.0, 100.0));
        assert_eq!(composition_region(above, 982.0), "0,-118,100,100");
    }

    #[test]
    fn background_delivers_result() {
        let mut job = Background::spawn("test", || Ok(7));
        job._thread.take().unwrap().join().unwrap();
        assert_eq!(job.take(), Some(Ok(7)));
        assert_eq!(job.take(), None);
        let ready = Background::ready("x");
        assert_eq!(ready.take(), Some(Ok("x")));
    }
}
