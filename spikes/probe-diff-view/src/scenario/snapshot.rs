//! Снимки окна и его окрестности через `screencapture` в рабочем потоке.
//!
//! Источник — `spikes/probe-appkit-shell/src/scenario/snapshot.rs` (P0.2), упрощён: нет рендера
//! вида в bitmap и чтения пикселей (P0.6 читает маркер поколения из кадров ScreenCaptureKit,
//! `scenario/marker.rs`), нет ожидания на таймере `wait_all`: главный поток забирает итог
//! `Background::try_take` из своего опроса. Без разрешения Screen Recording снимок получает
//! `not_run` с причиной.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;

use objc2_app_kit::NSWindow;
use objc2_core_graphics::CGPreflightScreenCaptureAccess;
use objc2_foundation::NSRect;
use serde::Serialize;

use crate::results::Rect;

pub const NO_PERMISSION: &str = "нет разрешения Screen Recording";
const SCREENCAPTURE: &str = "/usr/sbin/screencapture";
/// Запас вокруг окна у снимка окрестности: тень и соседние окна у края.
pub const SURROUNDING_MARGIN: f64 = 40.0;

type Slot<T> = Arc<Mutex<Option<Result<T, String>>>>;

/// Работа в рабочем потоке; итог забирает главный поток, не блокируясь.
pub struct Background<T> {
    slot: Slot<T>,
    thread: Option<thread::JoinHandle<()>>,
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
        Background { slot, thread }
    }

    pub fn ready(value: T) -> Self {
        Background {
            slot: Arc::new(Mutex::new(Some(Ok(value)))),
            thread: None,
        }
    }

    /// Итог, если работа закончилась; повторный вызов после итога даёт `None`.
    pub fn try_take(&self) -> Option<Result<T, String>> {
        self.slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }

    /// Ждёт конца рабочего потока. Только вне главного потока, например в тестах и `--aggregate`.
    pub fn join(mut self) -> Option<Result<T, String>> {
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            return Some(Err("рабочий поток снимка упал".to_owned()));
        }
        self.try_take()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotKind {
    /// Окно с тенью: `screencapture -l <номер окна>`.
    Window,
    /// Прямоугольник экрана вокруг окна: `screencapture -R`.
    Surrounding,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotStatus {
    Ok,
    NotRun,
    Error,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Snapshot {
    pub name: String,
    pub kind: SnapshotKind,
    pub status: SnapshotStatus,
    /// Имя файла в каталоге процесса.
    pub file: Option<String>,
    pub reason: Option<String>,
    pub rect_points: Rect,
}

pub fn screen_capture_allowed() -> bool {
    CGPreflightScreenCaptureAccess()
}

fn rect(rect: NSRect) -> Rect {
    Rect {
        x: rect.origin.x,
        y: rect.origin.y,
        width: rect.size.width,
        height: rect.size.height,
    }
}

fn not_run(name: &str, kind: SnapshotKind, area: NSRect) -> Snapshot {
    Snapshot {
        name: name.to_owned(),
        kind,
        status: SnapshotStatus::NotRun,
        file: None,
        reason: Some(NO_PERMISSION.to_owned()),
        rect_points: rect(area),
    }
}

/// Снимок окна с тенью.
pub fn capture_window(window: &NSWindow, out: &Path, name: &str) -> Background<Snapshot> {
    let frame = window.frame();
    if !screen_capture_allowed() {
        return Background::ready(not_run(name, SnapshotKind::Window, frame));
    }
    let args = vec![
        "-x".to_owned(),
        "-l".to_owned(),
        window.windowNumber().to_string(),
    ];
    run_capture(name, SnapshotKind::Window, frame, args, out)
}

/// Снимок прямоугольника экрана вокруг окна с запасом `SURROUNDING_MARGIN`.
pub fn capture_surrounding(
    window: &NSWindow,
    primary_height: f64,
    out: &Path,
    name: &str,
) -> Background<Snapshot> {
    let area = super::events::inset(window.frame(), -SURROUNDING_MARGIN);
    if !screen_capture_allowed() {
        return Background::ready(not_run(name, SnapshotKind::Surrounding, area));
    }
    let args = vec![
        "-x".to_owned(),
        "-R".to_owned(),
        capture_region(area, primary_height),
    ];
    run_capture(name, SnapshotKind::Surrounding, area, args, out)
}

/// Аргумент `-R` для прямоугольника в координатах AppKit. Координаты `screencapture` — от
/// верхнего левого угла главного экрана.
pub fn capture_region(area: NSRect, primary_height: f64) -> String {
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
    area: NSRect,
    args: Vec<String>,
    out: &Path,
) -> Background<Snapshot> {
    let snapshot = Snapshot {
        name: name.to_owned(),
        kind,
        status: SnapshotStatus::Error,
        file: None,
        reason: None,
        rect_points: rect(area),
    };
    let out = out.to_path_buf();
    Background::spawn("screencapture", move || {
        Ok(capture_with(SCREENCAPTURE, snapshot, args, &out))
    })
}

/// Запускает программу снимка с аргументами и путём файла последним. Успех — нулевой код и
/// непустой файл.
fn capture_with(
    program: &str,
    mut snapshot: Snapshot,
    mut args: Vec<String>,
    out: &Path,
) -> Snapshot {
    let file = format!("{}.png", snapshot.name);
    let path: PathBuf = out.join(&file);
    args.push(path.display().to_string());
    let status = Command::new(program).args(&args).status();
    let written = fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
    match status {
        Ok(status) if status.success() && written > 0 => {
            snapshot.status = SnapshotStatus::Ok;
            snapshot.file = Some(file);
        }
        Ok(status) => {
            snapshot.reason = Some(format!(
                "{program} {}: {status}, файл {written} байт",
                args.join(" ")
            ));
        }
        Err(err) => snapshot.reason = Some(format!("{program} не запущен: {err}")),
    }
    snapshot
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_foundation::{NSPoint, NSSize};

    fn blank(name: &str) -> Snapshot {
        Snapshot {
            name: name.to_owned(),
            kind: SnapshotKind::Window,
            status: SnapshotStatus::Error,
            file: None,
            reason: None,
            rect_points: Rect {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: 1.0,
            },
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "probe-diff-view-snapshot-{tag}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("временный каталог");
        dir
    }

    #[test]
    fn capture_region_uses_top_left_origin() {
        let area = NSRect::new(NSPoint::new(100.4, 50.0), NSSize::new(300.0, 200.6));
        assert_eq!(capture_region(area, 982.0), "100,731,300,201");
    }

    #[test]
    fn background_delivers_result_once() {
        let job = Background::spawn("test", || Ok(7));
        assert_eq!(job.join(), Some(Ok(7)));
        let ready = Background::ready("x");
        assert_eq!(ready.try_take(), Some(Ok("x")));
        assert_eq!(ready.try_take(), None);
        let failed: Background<()> = Background::spawn("test", || Err("нет".to_owned()));
        assert_eq!(failed.join(), Some(Err("нет".to_owned())));
    }

    #[test]
    fn capture_succeeds_only_with_written_file() {
        let dir = temp_dir("ok");
        // Подставная программа пишет непустой файл по последнему аргументу.
        let args = vec!["-c".to_owned(), "printf png > \"$0\"".to_owned()];
        let snapshot = capture_with("/bin/sh", blank("shot"), args, &dir);
        assert_eq!(snapshot.status, SnapshotStatus::Ok, "{:?}", snapshot.reason);
        assert_eq!(snapshot.file.as_deref(), Some("shot.png"));

        let args = vec!["-c".to_owned(), ": > \"$0\"".to_owned()];
        let empty = capture_with("/bin/sh", blank("empty"), args, &dir);
        assert_eq!(empty.status, SnapshotStatus::Error);
        assert!(
            empty
                .reason
                .as_deref()
                .is_some_and(|r| r.contains("0 байт"))
        );

        let args = vec!["-c".to_owned(), "exit 1".to_owned()];
        let failed = capture_with("/bin/sh", blank("failed"), args, &dir);
        assert_eq!(failed.status, SnapshotStatus::Error);

        let missing = capture_with(
            "/nonexistent/screencapture",
            blank("missing"),
            Vec::new(),
            &dir,
        );
        assert!(
            missing
                .reason
                .as_deref()
                .is_some_and(|r| r.contains("не запущен"))
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
