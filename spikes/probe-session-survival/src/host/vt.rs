//! VT-экземпляр Rust-хоста на `libghostty-vt` (функция `vt`): continuation с объявленным бюджетом,
//! ответы на объявленный набор запросов через `WRITE_PTY`, чтение буфера обмена через роль GUI
//! (`host::effects`), восстановление для attach и управляющий снимок.
//!
//! Восстановление идёт путём части `bridge` сценария `capi`: `bridge::extract::extract` снимает
//! состояние, `bridge::encode` с политикой `Canonical` выражает его VT-байтами. `GHOSTSNP` остаётся
//! внутренним форматом хоста: им пользуется только `extract` для копий с пробами.

use std::sync::Arc;

use crate::backend::Snapshot;
use crate::bridge::{self, ReportPolicy, extract};
use crate::host::effects::Clipboard;
use crate::host::responder::{Answer, client_continuation};
use crate::pty::Winsize;
use crate::vt::{self, Terminal};

/// Строк истории VT хоста (раздел «Замеры»), как у оракулов.
pub const SCROLLBACK: usize = 10_000;
/// Бюджет continuation VT хоста: начатая последовательность длиннее не восстанавливается, и
/// attach во время неё получает явный отказ.
pub const CONTINUATION_MAX: usize = 64 << 10;
/// Размер клетки, если окно не сообщило пикселей.
const CELL_W: u32 = 10;
const CELL_H: u32 = 20;
const FOREGROUND: vt::Rgb = vt::Rgb {
    r: 0xdd,
    g: 0xdd,
    b: 0xdd,
};
const BACKGROUND: vt::Rgb = vt::Rgb {
    r: 0x1e,
    g: 0x1e,
    b: 0x1e,
};
const CURSOR: vt::Rgb = vt::Rgb {
    r: 0xff,
    g: 0xcc,
    b: 0x00,
};
/// Режим отчёта видимости.
pub const REPORT_VISIBILITY: vt::Mode = vt::Mode::dec(2033);

/// Ответ DA1 хоста `CSI ? 62 ; 22 c` отличается от профиля приёмника (`… ; 52 c`): по квитанции
/// фикстуры видно, кто ответил.
fn device_attributes() -> vt::DeviceAttributes {
    vt::DeviceAttributes {
        conformance_level: 62,
        features: vec![22],
        device_type: 1,
        firmware_version: 12,
        unit_id: 0,
    }
}

pub fn xtversion() -> String {
    format!("probe-session-host({})", env!("CARGO_PKG_VERSION"))
}

fn cell_size(size: Winsize) -> (u32, u32) {
    let per = |pixels: u16, cells: u16, fallback: u32| {
        if pixels == 0 || cells == 0 {
            fallback
        } else {
            (u32::from(pixels) / u32::from(cells)).max(1)
        }
    };
    (
        per(size.xpixel, size.cols, CELL_W),
        per(size.ypixel, size.rows, CELL_H),
    )
}

/// Восстановление для одного attach.
#[derive(Clone, Debug)]
pub struct Restored {
    pub bytes: Vec<u8>,
    /// Continuation, отданная клиенту, и удержанная от него часть continuation VT.
    pub continuation: usize,
    pub held: usize,
    pub inexpressible: Vec<String>,
}

pub struct HostVt {
    term: Terminal,
    size: Winsize,
    revision: u64,
}

impl HostVt {
    pub fn new(size: Winsize, clipboard: Arc<Clipboard>) -> vt::Result<HostVt> {
        let mut term = Terminal::new(size.cols, size.rows)?;
        term.set_scrollback_max_lines(SCROLLBACK)?;
        term.set_continuation_max_bytes(CONTINUATION_MAX)?;
        term.set_default_colors(Some(FOREGROUND), Some(BACKGROUND), Some(CURSOR))?;
        let (cell_w, cell_h) = cell_size(size);
        term.resize(size.cols, size.rows, cell_w, cell_h)?;
        term.enable_effects(responder(size))?;
        term.set_clipboard_source(Some(Box::new(move |_request: &vt::ClipboardRequest| {
            clipboard.read().data()
        })));
        Ok(HostVt {
            term,
            size,
            revision: 0,
        })
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn ground(&self) -> bool {
        self.term.vt_ground().unwrap_or(false)
    }

    pub fn mode(&self, mode: vt::Mode) -> bool {
        self.term.mode(mode).unwrap_or(false)
    }

    /// Ресайз VT; возвращает отчёт размера 2048, если режим включён.
    pub fn resize(&mut self, size: Winsize) -> vt::Result<Vec<u8>> {
        let (cell_w, cell_h) = cell_size(size);
        self.term.resize(size.cols, size.rows, cell_w, cell_h)?;
        self.term.set_responder(responder(size));
        self.size = size;
        self.revision += 1;
        let _ = self.term.take_effects();
        let out = self.term.take_pty_output();
        if !out.is_empty() || !self.mode(vt::Mode::IN_BAND_RESIZE) {
            return Ok(out);
        }
        Ok(format!(
            "\x1b[48;{};{};{};{}t",
            size.rows,
            size.cols,
            u32::from(size.rows) * cell_h,
            u32::from(size.cols) * cell_w
        )
        .into_bytes())
    }

    /// Отчёт фокуса, если нагрузка включила 1004.
    pub fn focus(&self, gained: bool) -> Vec<u8> {
        if !self.mode(vt::Mode::FOCUS_EVENT) {
            return Vec::new();
        }
        vt::focus_report(gained).unwrap_or_default()
    }

    /// Отчёт видимости, если нагрузка включила 2033 (`device_status.zig` ревизии поверхности).
    pub fn visibility(&self, visible: bool) -> Vec<u8> {
        if !self.mode(REPORT_VISIBILITY) {
            return Vec::new();
        }
        if visible {
            b"\x1b[?999;1n".to_vec()
        } else {
            b"\x1b[?999;2n".to_vec()
        }
    }

    /// Состояние VT-байтами для свежего терминала клиента. `held` — удержанный от клиентов хвост
    /// начатой последовательности: клиент получит его живым потоком или не получит вовсе.
    pub fn restore(&self, held: &[u8]) -> Result<Restored, String> {
        let mut state =
            extract::extract(&self.term, CONTINUATION_MAX).map_err(|err| err.to_string())?;
        if !self.ground() && state.continuation.is_empty() {
            return Err(format!(
                "снимок недоступен: начатая последовательность длиннее бюджета continuation \
                 {CONTINUATION_MAX} байт"
            ));
        }
        state.continuation = client_continuation(&state.continuation, held)?;
        let inexpressible = bridge::inexpressible(&state);
        let bytes = bridge::encode(&state, ReportPolicy::Canonical);
        Ok(Restored {
            continuation: state.continuation.len(),
            held: held.len(),
            bytes,
            inexpressible,
        })
    }

    pub fn snapshot(&self) -> Result<Snapshot, String> {
        let failed = |err: vt::Error| err.to_string();
        let cursor = self.term.cursor().map_err(failed)?;
        let rows = self.term.rows().map_err(failed)?;
        let text = (0..rows)
            .map(|y| self.term.row_text(y))
            .collect::<vt::Result<Vec<_>>>()
            .map_err(failed)?
            .join("\n");
        Ok(Snapshot {
            revision: self.revision,
            cols: self.term.cols().map_err(failed)?,
            rows,
            cursor_x: cursor.x,
            cursor_y: cursor.y,
            alternate: self.term.active_screen().map_err(failed)? == vt::Screen::Alternate,
            text,
        })
    }
}

impl Answer for HostVt {
    fn answer(&mut self, raw: &[u8]) -> Vec<u8> {
        self.term.write(raw);
        self.revision += 1;
        // Эффекты VT хоста не исполняются: запись OSC 52 и заголовок идут клиенту потоком.
        let _ = self.term.take_effects();
        self.term.take_pty_output()
    }
}

fn responder(size: Winsize) -> vt::Responder {
    let (cell_width, cell_height) = cell_size(size);
    vt::Responder {
        device_attributes: Some(device_attributes()),
        xtversion: Some(xtversion()),
        size: Some(vt::SizeReport {
            rows: size.rows,
            columns: size.cols,
            cell_width,
            cell_height,
        }),
        color_scheme: Some(vt::ColorScheme::Dark),
        clipboard: None,
    }
}
