//! Снимок окна для наблюдений приёмочного прохода (второй план, задача 2): внешний
//! `/usr/sbin/screencapture -l <окно>` снимает окно по номеру WindowServer, даже перекрытое, и
//! снимок разбирается. Отказ снимка — не ошибка исполнения: часть-наблюдение получает `not_run`
//! с причиной, а выводы обязательных частей от снимка не зависят.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::{Duration, Instant};

use objc2::AnyThread;
use objc2::rc::autoreleasepool;
use objc2_app_kit::NSBitmapImageRep;
use objc2_foundation::NSData;
use serde::Serialize;

use crate::kq::{Event, Queue};
use crate::spawn;

use super::smoke::{Rgb, WindowEntry, srgb_at};

const SCREENCAPTURE: &str = "/usr/sbin/screencapture";
/// Срок одного запуска `screencapture`.
pub const CAPTURE_TIMEOUT: Duration = Duration::from_secs(10);
/// Сторона сетки точек, по которой считаются цвета снимка.
pub const GRID: isize = 64;
/// Владельцы окон системных запросов: Gatekeeper, TCC, Keychain и авторизация.
pub const DIALOG_OWNERS: [&str; 5] = [
    "CoreServicesUIAgent",
    "UserNotificationCenter",
    "universalAccessAuthWarn",
    "SecurityAgent",
    "coreautha",
];

/// Итог снимка одного окна.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Shot {
    pub command: String,
    pub window: i64,
    /// Путь снимка внутри каталога процесса.
    pub artifact: String,
    pub pixels: Option<(isize, isize)>,
    /// Разных цветов в сетке `GRID × GRID` точек.
    pub distinct: usize,
    pub error: Option<String>,
}

impl Shot {
    pub fn taken(&self) -> bool {
        self.error.is_none() && self.pixels.is_some()
    }

    /// Снимок не меньше рамки окна в точках: с `-o` пикселей — точки × масштаб, тени нет.
    pub fn covers(&self, bounds: [f64; 4]) -> bool {
        self.pixels.is_some_and(|(wide, high)| {
            wide as f64 + 1.0 >= bounds[2] && high as f64 + 1.0 >= bounds[3]
        })
    }
}

/// Окна владельцев системных запросов.
pub fn dialogs(windows: &[WindowEntry]) -> Vec<WindowEntry> {
    windows
        .iter()
        .filter(|window| DIALOG_OWNERS.contains(&window.owner.as_str()))
        .cloned()
        .collect()
}

/// Точки сетки в серединах клеток `GRID × GRID`.
pub fn grid_points(pixels: (isize, isize)) -> Vec<(isize, isize)> {
    let (wide, high) = pixels;
    if wide <= 0 || high <= 0 {
        return Vec::new();
    }
    let mut points = Vec::new();
    for row in 0..GRID {
        for column in 0..GRID {
            points.push((
                (wide * (2 * column + 1)) / (2 * GRID),
                (high * (2 * row + 1)) / (2 * GRID),
            ));
        }
    }
    points
}

/// Снимает окно `window` в `<out>/<artifact>`. Отказ записывается в `error`.
pub fn capture(window: i64, out: &Path, artifact: &str) -> Shot {
    let path = out.join(artifact);
    let args = vec![
        "-x".to_owned(),
        "-o".to_owned(),
        "-l".to_owned(),
        window.to_string(),
        path.display().to_string(),
    ];
    let mut shot = Shot {
        command: format!("{SCREENCAPTURE} {}", args.join(" ")),
        window,
        artifact: artifact.to_owned(),
        pixels: None,
        distinct: 0,
        error: None,
    };
    match run(&args).and_then(|()| examine(&path)) {
        Ok((pixels, distinct)) => {
            shot.pixels = Some(pixels);
            shot.distinct = distinct;
        }
        Err(error) => shot.error = Some(error),
    }
    shot
}

/// `screencapture` с ограниченным сроком; зависший снимается `SIGKILL`.
fn run(args: &[String]) -> Result<(), String> {
    let pid = spawn::spawn(spawn::Spec {
        program: Path::new(SCREENCAPTURE),
        args,
        env: None,
        stdio: [None, None, None],
        setsid: false,
        disclaim: false,
    })
    .map_err(|err| format!("{SCREENCAPTURE} не запущен: {err}"))?;
    let queue = Queue::new()?;
    let deadline = Instant::now() + CAPTURE_TIMEOUT;
    if queue.watch_exit(pid)? {
        loop {
            match queue.wait(deadline)? {
                Event::Exited(exited) if exited == pid => break,
                Event::TimedOut => {
                    // SAFETY: сигнал собственному потомку по PID, памяти вызов не касается.
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                    let _ = spawn::wait(pid);
                    return Err(format!(
                        "screencapture не завершился за {} с",
                        CAPTURE_TIMEOUT.as_secs()
                    ));
                }
                _ => {}
            }
        }
    }
    let exit = spawn::wait(pid).map_err(|err| format!("waitpid {pid}: {err}"))?;
    if exit.success() {
        Ok(())
    } else {
        Err(format!("screencapture: {exit}"))
    }
}

/// Размер снимка и число разных цветов sRGB в точках сетки.
fn examine(path: &Path) -> Result<((isize, isize), usize), String> {
    let bytes = std::fs::read(path).map_err(|err| format!("{}: {err}", path.display()))?;
    autoreleasepool(|_| {
        let data = NSData::with_bytes(&bytes);
        let rep = NSBitmapImageRep::initWithData(NSBitmapImageRep::alloc(), &data)
            .ok_or_else(|| format!("{}: PNG не разобран", path.display()))?;
        let pixels = (rep.pixelsWide(), rep.pixelsHigh());
        let colours: BTreeSet<(u8, u8, u8)> = grid_points(pixels)
            .into_iter()
            .filter_map(|(x, y)| srgb_at(&rep, x, y).map(|Rgb(r, g, b)| (r, g, b)))
            .collect();
        Ok((pixels, colours.len()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(owner: &str) -> WindowEntry {
        WindowEntry {
            number: 7,
            pid: 70,
            layer: 0,
            owner: owner.to_owned(),
            bounds: [0.0, 0.0, 10.0, 10.0],
        }
    }

    #[test]
    fn grid_covers_the_image_inside_its_bounds() {
        let points = grid_points((1280, 801));
        assert_eq!(points.len(), (GRID * GRID) as usize);
        assert!(
            points
                .iter()
                .all(|(x, y)| (0..1280).contains(x) && (0..801).contains(y))
        );
        assert_eq!(points[0], (10, 6));
        assert!(grid_points((0, 10)).is_empty());
    }

    #[test]
    fn shot_covers_window_frame_in_points() {
        let mut shot = Shot {
            command: String::new(),
            window: 7,
            artifact: "a.png".to_owned(),
            pixels: Some((1200, 900)),
            distinct: 3,
            error: None,
        };
        assert!(shot.taken());
        assert!(shot.covers([10.0, 20.0, 600.0, 450.0]));
        assert!(shot.covers([10.0, 20.0, 1200.4, 900.0]));
        assert!(!shot.covers([10.0, 20.0, 1300.0, 450.0]));
        shot.error = Some("screencapture: код 1".to_owned());
        assert!(!shot.taken());
    }

    #[test]
    fn dialogs_are_windows_of_known_owners() {
        let windows = [
            entry("probe-update-app"),
            entry("CoreServicesUIAgent"),
            entry("UserNotificationCenter"),
        ];
        let found: Vec<String> = dialogs(&windows)
            .into_iter()
            .map(|window| window.owner)
            .collect();
        assert_eq!(found, ["CoreServicesUIAgent", "UserNotificationCenter"]);
    }
}
