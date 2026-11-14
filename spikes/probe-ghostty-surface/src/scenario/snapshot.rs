//! Снимки композиции экрана для пиксельных проверок. `screencapture -x -R` по прямоугольнику в
//! координатах экрана снимает окно вместе с дочерней палитрой и тем, что WindowServer показал
//! на самом деле; рендер вида через `cacheDisplay` снимком Metal-композиции не считается. Снимок
//! делается только при разрешении Screen Recording (проверка без запроса), иначе он получает
//! статус `pending-manual`. Запуск `screencapture`, чтение PNG и выборка пикселей идут в рабочем
//! потоке, главный поток только опрашивает итог.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;

use objc2::AnyThread;
use objc2::rc::autoreleasepool;
use objc2_app_kit::{NSBitmapImageRep, NSColorSpace, NSScreen};
use objc2_core_graphics::CGPreflightScreenCaptureAccess;
use objc2_foundation::{MainThreadMarker, NSData, NSPoint, NSRect};

use crate::results::{Rect, Size, Snapshot, SnapshotKind, SnapshotStatus};
use crate::scene::Rgb;

pub const NO_PERMISSION: &str = "нет разрешения Screen Recording";
const SCREENCAPTURE: &str = "/usr/sbin/screencapture";

pub fn allowed() -> bool {
    CGPreflightScreenCaptureAccess()
}

/// Высота главного экрана: `screencapture -R` считает от его верхнего левого угла.
pub fn primary_height(mtm: MainThreadMarker) -> f64 {
    NSScreen::screens(mtm)
        .firstObject()
        .map_or(0.0, |screen| screen.frame().size.height)
}

/// Аргумент `-R` для прямоугольника в координатах AppKit.
pub fn region_arg(area: NSRect, primary_height: f64) -> String {
    let top = primary_height - (area.origin.y + area.size.height);
    format!(
        "{},{},{},{}",
        area.origin.x.round(),
        top.round(),
        area.size.width.round(),
        area.size.height.round()
    )
}

/// Пиксель снимка под точкой экрана в координатах AppKit; `None` — вне снимка.
pub fn pixel_of(point: NSPoint, area: NSRect, pixels: (isize, isize)) -> Option<(isize, isize)> {
    if area.size.width <= 0.0 || area.size.height <= 0.0 {
        return None;
    }
    let scale_x = pixels.0 as f64 / area.size.width;
    let scale_y = pixels.1 as f64 / area.size.height;
    let x = ((point.x - area.origin.x) * scale_x).floor();
    let y = ((area.origin.y + area.size.height - point.y) * scale_y).floor();
    let inside = x >= 0.0 && y >= 0.0 && x < pixels.0 as f64 && y < pixels.1 as f64;
    inside.then_some((x as isize, y as isize))
}

/// Яркость sRGB 0…1 по весам Rec. 709 без линеаризации: только для сравнения порядка.
pub fn luma(rgb: Rgb) -> f64 {
    (0.2126 * f64::from(rgb.0) + 0.7152 * f64::from(rgb.1) + 0.0722 * f64::from(rgb.2)) / 255.0
}

/// Наибольшая разница каналов.
pub fn distance(a: Rgb, b: Rgb) -> u8 {
    [a.0.abs_diff(b.0), a.1.abs_diff(b.1), a.2.abs_diff(b.2)]
        .into_iter()
        .max()
        .unwrap_or(0)
}

/// Итог снимка: запись для результата и пиксели под запрошенными точками.
#[derive(Clone, Debug)]
pub struct Capture {
    pub snapshot: Snapshot,
    pub samples: Vec<Option<Rgb>>,
}

impl Capture {
    pub fn ok(&self) -> bool {
        self.snapshot.status == SnapshotStatus::Ok
    }
}

type Slot = Arc<Mutex<Option<Capture>>>;

/// Снимок в работе.
pub struct Pending {
    slot: Slot,
}

impl Pending {
    pub fn take(&self) -> Option<Capture> {
        self.slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }
}

fn rect(area: NSRect) -> Rect {
    Rect {
        x: area.origin.x,
        y: area.origin.y,
        width: area.size.width,
        height: area.size.height,
    }
}

/// Снимает прямоугольник `area` экрана в `<out>/<name>.png` и читает пиксели под `points`.
pub fn capture(
    mtm: MainThreadMarker,
    area: NSRect,
    points: Vec<NSPoint>,
    out: &Path,
    name: &str,
) -> Pending {
    let slot: Slot = Arc::new(Mutex::new(None));
    let mut snapshot = Snapshot {
        name: name.to_owned(),
        kind: SnapshotKind::Composition,
        status: SnapshotStatus::Error,
        file: None,
        reason: None,
        rect_points: Some(rect(area)),
        pixels: None,
    };
    if !allowed() {
        snapshot.status = SnapshotStatus::PendingManual;
        snapshot.reason = Some(NO_PERMISSION.to_owned());
        let samples = vec![None; points.len()];
        *slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(Capture { snapshot, samples });
        return Pending { slot };
    }
    let region = region_arg(area, primary_height(mtm));
    let file = format!("{name}.png");
    let path = out.join(&file);
    let filled = Arc::clone(&slot);
    let spawned = thread::Builder::new()
        .name("probe-screencapture".to_owned())
        .spawn(move || {
            let capture = run(snapshot, &region, &path, file, area, &points);
            *filled
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(capture);
        });
    if let Err(err) = spawned {
        let snapshot = Snapshot {
            name: name.to_owned(),
            kind: SnapshotKind::Composition,
            status: SnapshotStatus::Error,
            file: None,
            reason: Some(format!("рабочий поток не запущен: {err}")),
            rect_points: None,
            pixels: None,
        };
        *slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Capture {
            snapshot,
            samples: Vec::new(),
        });
    }
    Pending { slot }
}

fn run(
    mut snapshot: Snapshot,
    region: &str,
    path: &PathBuf,
    file: String,
    area: NSRect,
    points: &[NSPoint],
) -> Capture {
    let mut samples = vec![None; points.len()];
    let status = Command::new(SCREENCAPTURE)
        .args(["-x", "-R", region])
        .arg(path)
        .status();
    match status {
        Ok(status) if status.success() => {}
        Ok(status) => {
            snapshot.reason = Some(format!("screencapture: {status}"));
            return Capture { snapshot, samples };
        }
        Err(err) => {
            snapshot.reason = Some(format!("screencapture не запущен: {err}"));
            return Capture { snapshot, samples };
        }
    }
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) => {
            snapshot.reason = Some(format!("{}: {err}", path.display()));
            return Capture { snapshot, samples };
        }
    };
    autoreleasepool(|_| {
        let data = NSData::with_bytes(&bytes);
        let Some(rep) = NSBitmapImageRep::initWithData(NSBitmapImageRep::alloc(), &data) else {
            snapshot.reason = Some("PNG не разобран".to_owned());
            return;
        };
        let pixels = (rep.pixelsWide(), rep.pixelsHigh());
        snapshot.pixels = Some(Size {
            width: pixels.0 as f64,
            height: pixels.1 as f64,
        });
        let srgb = NSColorSpace::sRGBColorSpace();
        for (sample, point) in samples.iter_mut().zip(points) {
            let Some((x, y)) = pixel_of(*point, area, pixels) else {
                continue;
            };
            *sample = rep
                .colorAtX_y(x, y)
                .and_then(|color| color.colorUsingColorSpace(&srgb))
                .map(|color| {
                    let channel = |value: f64| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
                    Rgb(
                        channel(color.redComponent()),
                        channel(color.greenComponent()),
                        channel(color.blueComponent()),
                    )
                });
        }
        snapshot.status = SnapshotStatus::Ok;
        snapshot.file = Some(file);
    });
    Capture { snapshot, samples }
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_foundation::NSSize;

    #[test]
    fn region_counts_from_the_top_of_the_primary_screen() {
        let area = NSRect::new(NSPoint::new(100.0, 200.0), NSSize::new(300.0, 100.0));
        assert_eq!(region_arg(area, 1000.0), "100,700,300,100");
    }

    #[test]
    fn pixel_maps_points_with_scale_and_flip() {
        let area = NSRect::new(NSPoint::new(100.0, 200.0), NSSize::new(300.0, 100.0));
        assert_eq!(
            pixel_of(NSPoint::new(100.0, 300.0), area, (600, 200)),
            Some((0, 0))
        );
        assert_eq!(
            pixel_of(NSPoint::new(250.0, 250.0), area, (600, 200)),
            Some((300, 100))
        );
        assert_eq!(
            pixel_of(NSPoint::new(399.9, 200.1), area, (600, 200)),
            Some((599, 199))
        );
        assert_eq!(pixel_of(NSPoint::new(400.0, 250.0), area, (600, 200)), None);
        assert_eq!(pixel_of(NSPoint::new(99.0, 250.0), area, (600, 200)), None);
    }

    #[test]
    fn luma_and_distance_order_colors() {
        assert!(luma(Rgb(255, 255, 255)) > 0.99);
        assert!(luma(Rgb(0, 0, 0)) < 0.01);
        assert!(luma(Rgb(0xEF, 0xED, 0xE8)) > luma(Rgb(0x1E, 0x1D, 0x1B)));
        assert_eq!(distance(Rgb(10, 20, 30), Rgb(15, 5, 30)), 15);
    }
}
