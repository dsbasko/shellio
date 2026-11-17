//! GUI-процесс сценария `bridge`: окно с одной поверхностью по командам водителя
//! (`matrix::bridge::GuiCommand`) строками JSON в сокете управления и журнал эффектов дозаписью.
//! Конец сокета водителя — штатный выход; `SIGKILL` стенд посылает сам.

use std::fs::OpenOptions;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_app_kit::NSView;

use super::ghostty::{self, App, Config, Journal, Spec, Surface};
use super::view::{self, Shell};
use crate::matrix::bridge::{EFFECTS_FILE, Grid, GuiCommand, Hello, Reply};

/// Конфигурация libghostty, которую водитель кладёт в каталог GUI.
pub const CONFIG_FILE: &str = "ghostty.conf";
const PUMP: Duration = Duration::from_millis(5);

struct Open {
    id: u32,
    surface: Surface,
    _view: Retained<NSView>,
}

pub fn run(control: &Path, out: &Path, nonce: &str) -> Result<(), String> {
    let mtm = MainThreadMarker::new().ok_or("GUI не на главном потоке")?;
    let mut stream = UnixStream::connect(control)
        .map_err(|err| format!("сокет водителя {}: {err}", control.display()))?;
    let journal_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(out.join(EFFECTS_FILE))
        .map_err(|err| format!("журнал эффектов: {err}"))?;
    let journal = Journal::new(journal_file);
    ghostty::init_once(crate::PROBE_NAME)?;
    let app = App::new(Config::load(&out.join(CONFIG_FILE))?)?;
    let shell = Shell::new(mtm, &format!("probe-session-surface {nonce}"));
    let hello = Hello {
        pid: std::process::id().cast_signed(),
        nonce: nonce.to_owned(),
    };
    send_line(&mut stream, &hello)?;
    stream
        .set_nonblocking(true)
        .map_err(|err| format!("сокет водителя: {err}"))?;
    let mut open: Option<Open> = None;
    let mut buffer = Vec::new();
    let outcome = loop {
        view::pump(PUMP);
        if app.wants_tick() {
            app.tick();
        }
        let mut chunk = [0u8; 4096];
        match stream.read(&mut chunk) {
            Ok(0) => break Ok(()),
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            Err(err) if err.kind() == ErrorKind::WouldBlock => {}
            Err(err) if err.kind() == ErrorKind::Interrupted => {}
            Err(err) => break Err(format!("сокет водителя: {err}")),
        }
        let mut quit = false;
        while let Some(end) = buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = buffer.drain(..=end).collect();
            let reply = match serde_json::from_slice::<GuiCommand>(&line) {
                Ok(GuiCommand::Quit) => {
                    quit = true;
                    Reply::default()
                }
                Ok(command) => execute(&app, &shell, &journal, &mut open, command),
                Err(err) => Reply {
                    error: Some(format!("команда не разобрана: {err}")),
                    ..Reply::default()
                },
            };
            stream
                .set_nonblocking(false)
                .and_then(|()| {
                    let mut text = serde_json::to_vec(&reply).map_err(std::io::Error::other)?;
                    text.push(b'\n');
                    stream.write_all(&text)
                })
                .and_then(|()| stream.set_nonblocking(true))
                .map_err(|err| format!("ответ водителю: {err}"))?;
        }
        if quit {
            break Ok(());
        }
    };
    drop(open);
    shell.close();
    drop(app);
    outcome
}

fn execute(
    app: &App,
    shell: &Shell,
    journal: &Arc<Journal>,
    open: &mut Option<Open>,
    command: GuiCommand,
) -> Reply {
    let result = match command {
        GuiCommand::Open {
            surface,
            command,
            cols,
            rows,
        } => open_surface(app, shell, journal, open, surface, command, cols, rows),
        GuiCommand::Close { surface } => match current(open, surface) {
            Ok(_) => {
                *open = None;
                Ok(Reply::default())
            }
            Err(error) => Err(error),
        },
        GuiCommand::Text { surface } => current(open, surface).map(|open| Reply {
            text: open.surface.viewport_text(),
            ..Reply::default()
        }),
        GuiCommand::Key { surface, keycode } => current(open, surface).map(|open| Reply {
            handled: Some(open.surface.tap(keycode)),
            ..Reply::default()
        }),
        GuiCommand::Paste { surface, text } => current(open, surface).map(|open| {
            open.surface.paste(&text);
            Reply::default()
        }),
        GuiCommand::Click { surface, col, row } => current(open, surface).map(|open| Reply {
            handled: Some(open.surface.click(col, row, shell.scale())),
            ..Reply::default()
        }),
        GuiCommand::Focus { surface, focused } => current(open, surface).map(|open| {
            open.surface.set_focus(focused);
            Reply::default()
        }),
        GuiCommand::Visible { surface, visible } => current(open, surface).map(|open| {
            open.surface.set_visible(visible);
            Reply::default()
        }),
        GuiCommand::Size {
            surface,
            cols,
            rows,
        } => current(open, surface).and_then(|open| resize(&open.surface, cols, rows)),
        GuiCommand::ClipboardReply { request, text } => match open.as_ref() {
            Some(open) if open.surface.complete_clipboard(request, &text) => Ok(Reply::default()),
            _ => Err(format!("нет ожидающего чтения буфера обмена {request}")),
        },
        GuiCommand::Quit => Ok(Reply::default()),
    };
    // Проход приложения отдаёт почту поверхности до ответа водителю.
    app.tick();
    result.unwrap_or_else(|error| Reply {
        error: Some(error),
        ..Reply::default()
    })
}

fn current(open: &Option<Open>, surface: u32) -> Result<&Open, String> {
    match open {
        Some(open) if open.id == surface => Ok(open),
        _ => Err(format!("поверхность {surface} не открыта")),
    }
}

#[allow(clippy::too_many_arguments)]
fn open_surface(
    app: &App,
    shell: &Shell,
    journal: &Arc<Journal>,
    open: &mut Option<Open>,
    id: u32,
    command: String,
    cols: u16,
    rows: u16,
) -> Result<Reply, String> {
    if let Some(current) = open {
        return Err(format!("уже открыта поверхность {}", current.id));
    }
    let spec = Spec {
        command,
        working_directory: "/tmp".to_owned(),
        scale: shell.scale(),
    };
    let view = shell.view(640.0, 400.0);
    let surface = Surface::tagged(app, &view, &spec, id, Arc::clone(journal))?;
    surface.set_content_scale(spec.scale);
    surface.set_focus(true);
    surface.set_visible(true);
    surface.set_dark(false);
    let reply = resize(&surface, cols, rows)?;
    *open = Some(Open {
        id,
        surface,
        _view: view,
    });
    Ok(reply)
}

/// Точная сетка в пикселях по размеру клетки поверхности.
fn resize(surface: &Surface, cols: u16, rows: u16) -> Result<Reply, String> {
    let cell = surface.size();
    if cell.cell_width_px == 0 || cell.cell_height_px == 0 {
        return Err(format!("нулевая клетка поверхности: {cell:?}"));
    }
    surface.set_size(
        u32::from(cols) * cell.cell_width_px,
        u32::from(rows) * cell.cell_height_px,
    );
    let size = surface.size();
    Ok(Reply {
        grid: Some(Grid {
            cols: size.columns,
            rows: size.rows,
            width_px: size.width_px,
            height_px: size.height_px,
            cell_width_px: size.cell_width_px,
            cell_height_px: size.cell_height_px,
        }),
        ..Reply::default()
    })
}

fn send_line(stream: &mut UnixStream, value: &impl serde::Serialize) -> Result<(), String> {
    let mut text = serde_json::to_vec(value).map_err(|err| format!("строка водителю: {err}"))?;
    text.push(b'\n');
    stream
        .write_all(&text)
        .map_err(|err| format!("строка водителю: {err}"))
}
