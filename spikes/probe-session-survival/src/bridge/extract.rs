//! Извлечение состояния моста из терминала `libghostty-vt` (функция `vt`). Способ для каждого поля
//! записан в [`FIELDS`]: прямо из C API или пробными последовательностями на копии, декодированной
//! из того же снимка `GHOSTSNP`. Копия получает CAN первым байтом: начатая последовательность
//! (B7) не съедает пробы. Собственный разбор хоста не понадобился ни одному полю.

use serde::Serialize;

use crate::bridge::{
    Cell, Cursor, Margins, MouseEvent, MouseFormat, Pen, Row, SavedCursor, ScreenState, State,
};
use crate::oracle::model::{self, Screen, TRACKED_MODES, Width};
use crate::oracle::vt::{color, rgb, style};
use crate::vt::{self, Area, Terminal};

/// Как снято поле.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Way {
    /// Прямо из C API исходного терминала.
    Api,
    /// Пробными последовательностями на копии из снимка.
    Probe,
}

/// Поле состояния, способ и как именно.
pub const FIELDS: [(&str, Way, &str); 14] = [
    (
        "cells",
        Way::Api,
        "клетки активной области и истории активного экрана по grid_ref; фон пустой клетки — \
         CONTENT_TAG и COLOR_* клетки",
    ),
    (
        "rows",
        Way::Api,
        "признаки мягкого переноса строки и продолжения",
    ),
    (
        "cursor",
        Way::Api,
        "позиция, отложенный перенос и стиль пера",
    ),
    (
        "pen-hyperlink-protection",
        Way::Probe,
        "печать X на копии: гиперссылка и защита напечатанной клетки",
    ),
    (
        "saved-cursor",
        Way::Probe,
        "DECRC на копии: позиция, перенос, стиль, origin; защита — печатью X после DECRC",
    ),
    (
        "kitty",
        Way::Probe,
        "восемь CSI < u на копии с чтением текущих флагов перед каждым",
    ),
    (
        "inactive-screen",
        Way::Probe,
        "?47 на копии (переключение без очистки): клетки, история, сохранённый курсор и Kitty \
         неактивного экрана; его собственный курсор переключение перезаписывает",
    ),
    (
        "tabstops",
        Way::Probe,
        "HT от колонки 0 на копии с полными полями и выключенным DECOM",
    ),
    (
        "margins",
        Way::Probe,
        "DECOM на копии: курсор в начало области, CUD и CUF до упора",
    ),
    ("modes", Way::Api, "TRACKED_MODES через get(MODE)"),
    (
        "mouse",
        Way::Probe,
        "кодировщик мыши по терминалу: нажатие, отпускание, движение с кнопкой и без, формат \
         по ответу на нажатие",
    ),
    (
        "colors",
        Way::Api,
        "текущие и по умолчанию основной цвет, фон, курсор и палитра",
    ),
    ("title-pwd", Way::Api, "TITLE и PWD"),
    (
        "continuation",
        Way::Api,
        "ghostty_terminal_continuation_alloc исходного терминала",
    ),
];

const CANCEL: &[u8] = b"\x18";
const PROBE: &[u8] = b"X";
/// Размер клетки для кодировщика мыши: пробе нужна только согласованная геометрия.
const CELL_W: u32 = 10;
const CELL_H: u32 = 20;

/// Состояние моста исходного терминала. `budget` — предел continuation копий из снимка: снимок с
/// незавершённой последовательностью декодируется только с ненулевым пределом.
pub fn extract(term: &Terminal, budget: usize) -> vt::Result<State> {
    let snapshot = term.snapshot()?;
    let copy = || -> vt::Result<Terminal> {
        let mut copy = Terminal::from_snapshot(&snapshot, budget.max(1))?;
        copy.write(CANCEL);
        Ok(copy)
    };
    let cols = term.cols()?;
    let rows = term.rows()?;
    let screen = match term.active_screen()? {
        vt::Screen::Primary => Screen::Primary,
        vt::Screen::Alternate => Screen::Alternate,
    };

    let mut active = screen_rows(term, cols, rows)?;
    let cursor = term.cursor()?;
    let pen = pen_probe(&mut copy()?)?;
    active.cursor = Cursor {
        x: cursor.x,
        y: cursor.y,
        pending_wrap: cursor.pending_wrap,
        pen: Pen {
            style: style(&cursor.style),
            hyperlink: pen.0,
            protected: pen.1,
        },
    };
    let mut probe = copy()?;
    active.kitty = kitty_ring(&mut probe)?;
    active.saved = saved_cursor(&mut probe)?;

    let mut inactive_copy = copy()?;
    inactive_copy.write(match screen {
        Screen::Primary => b"\x1b[?47h",
        Screen::Alternate => b"\x1b[?47l",
    });
    let mut inactive = screen_rows(&inactive_copy, cols, rows)?;
    inactive.kitty = kitty_ring(&mut inactive_copy)?;
    inactive.saved = saved_cursor(&mut inactive_copy)?;

    let mut geometry = copy()?;
    let margins = margins(&mut geometry, cols, rows)?;
    let tabstops = tabstops(&mut geometry, cols)?;

    let modes = TRACKED_MODES
        .iter()
        .map(|mode| {
            let value = if mode.ansi {
                vt::Mode::ansi(mode.value)
            } else {
                vt::Mode::dec(mode.value)
            };
            term.mode(value).map(|on| (mode.key(), on))
        })
        .collect::<vt::Result<_>>()?;
    let (mouse_event, mouse_format) = mouse(term, cols, rows, &modes)?;

    let colors = term.colors()?;
    let defaults = term.default_colors()?;
    let palette = colors
        .palette
        .iter()
        .zip(&defaults.palette)
        .enumerate()
        .filter(|(_, (current, default))| current != default)
        .map(|(index, (current, _))| (u8::try_from(index).unwrap_or(u8::MAX), rgb(*current)))
        .collect();
    let overridden = |current: Option<vt::Rgb>, default: Option<vt::Rgb>| {
        current.filter(|value| Some(*value) != default).map(rgb)
    };

    let (primary, alternate) = match screen {
        Screen::Primary => (active, inactive),
        Screen::Alternate => (inactive, active),
    };
    Ok(State {
        cols,
        rows,
        screen,
        primary,
        alternate,
        modes,
        mouse_event,
        mouse_format,
        tabstops,
        margins,
        palette,
        foreground: overridden(colors.foreground, defaults.foreground),
        background: overridden(colors.background, defaults.background),
        cursor_color: overridden(colors.cursor, defaults.cursor),
        title: String::from_utf8_lossy(&term.title()?).into_owned(),
        pwd: String::from_utf8_lossy(&term.pwd()?).into_owned(),
        // Без отслеживания continuation (терминал клиента) её нет.
        continuation: if term.continuation_max_bytes()? > 0 {
            term.continuation()?
        } else {
            Vec::new()
        },
    })
}

/// Клетки активного экрана терминала `term`: история от дальней строки и активная область.
fn screen_rows(term: &Terminal, cols: u16, rows: u16) -> vt::Result<ScreenState> {
    let total = term.scrollback_rows()?;
    let history = (0..total)
        .map(|y| {
            read_row(
                term,
                Area::History,
                cols,
                u32::try_from(y).unwrap_or(u32::MAX),
            )
        })
        .collect::<vt::Result<_>>()?;
    let active = (0..rows)
        .map(|y| read_row(term, Area::Active, cols, u32::from(y)))
        .collect::<vt::Result<_>>()?;
    Ok(ScreenState {
        history,
        active,
        cursor: Cursor::default(),
        saved: SavedCursor::default(),
        kitty: Vec::new(),
    })
}

fn read_row(term: &Terminal, area: Area, cols: u16, y: u32) -> vt::Result<Row> {
    let mut cells = Vec::with_capacity(usize::from(cols));
    let mut flags = (false, false);
    for x in 0..cols {
        let cell = term.cell(area, x, y)?;
        if x == 0 {
            flags = (cell.row_wrap, cell.row_wrap_continuation);
        }
        cells.push(Cell {
            text: cell
                .text
                .iter()
                .map(|&code| char::from_u32(code).unwrap_or(char::REPLACEMENT_CHARACTER))
                .collect(),
            width: match cell.wide {
                vt::Wide::Narrow => Width::Narrow,
                vt::Wide::Wide => Width::Wide,
                vt::Wide::SpacerTail => Width::SpacerTail,
                vt::Wide::SpacerHead => Width::SpacerHead,
            },
            style: style(&cell.style),
            hyperlink: cell.hyperlink,
            protected: cell.protected,
            bg_only: cell.bg_only.map(color),
        });
    }
    Ok(Row {
        cells,
        wrap: flags.0,
        wrap_continuation: flags.1,
    })
}

/// Печать X на копии и клетка, куда он попал: гиперссылка и защита пера.
fn printed_cell(copy: &mut Terminal) -> vt::Result<vt::Cell> {
    copy.write(PROBE);
    let after = copy.cursor()?;
    let x = if after.pending_wrap {
        after.x
    } else {
        after.x.saturating_sub(1)
    };
    copy.cell(Area::Active, x, u32::from(after.y))
}

fn pen_probe(copy: &mut Terminal) -> vt::Result<(Option<String>, bool)> {
    let cell = printed_cell(copy)?;
    Ok((cell.hyperlink, cell.protected))
}

/// Кольцо Kitty от текущего значения: флаги перед каждым из восьми снятий.
fn kitty_ring(copy: &mut Terminal) -> vt::Result<Vec<u8>> {
    let mut ring = Vec::with_capacity(model::KITTY_STACK_DEPTH);
    for _ in 0..model::KITTY_STACK_DEPTH {
        ring.push(copy.kitty_keyboard_flags()?);
        copy.write(b"\x1b[<u");
    }
    Ok(ring)
}

/// DECRC на копии: сохранённый курсор, затем печать X — его защита.
fn saved_cursor(copy: &mut Terminal) -> vt::Result<SavedCursor> {
    copy.write(b"\x1b8");
    let cursor = copy.cursor()?;
    let origin = copy.mode(vt::Mode::ORIGIN)?;
    let protected = printed_cell(copy)?.protected;
    Ok(SavedCursor {
        x: cursor.x,
        y: cursor.y,
        pending_wrap: cursor.pending_wrap,
        style: style(&cursor.style),
        protected,
        origin,
    })
}

/// DECOM ставит курсор в начало области, CUD и CUF упираются в нижнее и правое поле.
fn margins(copy: &mut Terminal, cols: u16, rows: u16) -> vt::Result<Margins> {
    copy.write(b"\x1b[?6h");
    let start = copy.cursor()?;
    copy.write(b"\x1b[9999B\x1b[9999C");
    let end = copy.cursor()?;
    let full = Margins::full(cols, rows);
    Ok(Margins {
        top: start.y,
        bottom: end.y.min(full.bottom),
        left: start.x,
        right: end.x.min(full.right),
    })
}

/// HT от колонки 0 при полных полях: позиции остановок левее последней колонки.
fn tabstops(copy: &mut Terminal, cols: u16) -> vt::Result<Vec<u16>> {
    copy.write(b"\x1b[?6l\x1b[?69h\x1b[s\x1b[r\x1b[1G");
    let mut stops = Vec::new();
    let mut x = copy.cursor()?.x;
    loop {
        copy.write(b"\t");
        let next = copy.cursor()?.x;
        if next <= x || next + 1 >= cols {
            break;
        }
        stops.push(next);
        x = next;
    }
    Ok(stops)
}

/// Текущий режим и формат мыши по кодировщику ghostty: флаги режимов этого не говорят, у групп
/// побеждает последний включённый.
fn mouse(
    term: &Terminal,
    cols: u16,
    rows: u16,
    modes: &std::collections::BTreeMap<String, bool>,
) -> vt::Result<(MouseEvent, MouseFormat)> {
    let mut encoder = vt::MouseEncoder::new(vt::MouseGeometry {
        screen_width: u32::from(cols) * CELL_W,
        screen_height: u32::from(rows) * CELL_H,
        cell_width: CELL_W,
        cell_height: CELL_H,
    })?;
    encoder.sync_from(term);
    encoder.set_track_last_cell(false);
    let at = |column: u32| {
        (
            (column * CELL_W + CELL_W / 2) as f32,
            (CELL_H + CELL_H / 2) as f32,
        )
    };
    let event = |action, button, column| {
        let (x, y) = at(column);
        vt::MouseInput {
            action,
            button,
            mods: 0,
            x,
            y,
        }
    };
    let flag = |mode: u16| modes.get(&format!("?{mode}")).copied().unwrap_or(false);
    let press = encoder.encode(&event(
        vt::MouseAction::Press,
        Some(vt::MouseButton::Left),
        2,
    ))?;
    if press.is_empty() {
        let format = MouseFormat::MODES
            .iter()
            .rev()
            .find(|mode| flag(**mode))
            .map_or(MouseFormat::X10, |mode| format_of(*mode));
        return Ok((MouseEvent::None, format));
    }
    let release = encoder.encode(&event(
        vt::MouseAction::Release,
        Some(vt::MouseButton::Left),
        2,
    ))?;
    encoder.set_any_button_pressed(true);
    let drag = encoder.encode(&event(
        vt::MouseAction::Motion,
        Some(vt::MouseButton::Left),
        3,
    ))?;
    encoder.set_any_button_pressed(false);
    let motion = encoder.encode(&event(vt::MouseAction::Motion, None, 4))?;
    let kind = if !motion.is_empty() {
        MouseEvent::Any
    } else if !drag.is_empty() {
        MouseEvent::Button
    } else if !release.is_empty() {
        MouseEvent::Normal
    } else {
        MouseEvent::X10
    };
    let format = if let Some(rest) = press.strip_prefix(b"\x1b[<") {
        // Клетка (2, 1) в SGR — `0;3;2M`, в пикселях — центр клетки.
        if rest.starts_with(b"0;3;2") {
            MouseFormat::Sgr
        } else {
            MouseFormat::SgrPixels
        }
    } else if press.starts_with(b"\x1b[M") {
        if flag(1005) {
            MouseFormat::Utf8
        } else {
            MouseFormat::X10
        }
    } else {
        MouseFormat::Urxvt
    };
    Ok((kind, format))
}

fn format_of(mode: u16) -> MouseFormat {
    match mode {
        1005 => MouseFormat::Utf8,
        1006 => MouseFormat::Sgr,
        1015 => MouseFormat::Urxvt,
        1016 => MouseFormat::SgrPixels,
        _ => MouseFormat::X10,
    }
}

#[cfg(test)]
mod tests;
