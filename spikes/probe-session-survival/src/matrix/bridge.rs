//! Чистая часть сценария `bridge` (план P0.4, задача 15): клетки моста в настоящую поверхность и
//! их части. Процесс сценария — водитель без поверхности, обе GUI — дочерние процессы
//! `probe-session-surface --gui`; FFI и окна живут в `surface::bridge` под функцией `surface`.

use serde::{Deserialize, Serialize};

use crate::matrix::evidence;
use crate::results::Cell;

/// Журнал эффектов GUI в её каталоге: одна строка JSON [`Effect`] на колбэк, дозаписью без буфера,
/// поэтому он переживает `SIGKILL` GUI.
pub const EFFECTS_FILE: &str = "effects.jsonl";

/// Команда водителя GUI-процессу: одна строка JSON в сокете управления, ответ — [`Reply`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum GuiCommand {
    /// Новая поверхность `surface` с командой `command` (для shell libghostty) и сеткой.
    Open {
        surface: u32,
        command: String,
        cols: u16,
        rows: u16,
    },
    /// Текст видимой области по `ghostty_surface_read_text`.
    Text {
        surface: u32,
    },
    /// Нажатие и отпускание клавиши по коду macOS.
    Key {
        surface: u32,
        keycode: u32,
    },
    /// Текст как вставка (`ghostty_surface_text`).
    Paste {
        surface: u32,
        text: String,
    },
    /// Нажатие и отпускание левой кнопки мыши в клетке с нуля.
    Click {
        surface: u32,
        col: u16,
        row: u16,
    },
    Focus {
        surface: u32,
        focused: bool,
    },
    Visible {
        surface: u32,
        visible: bool,
    },
    /// Новая сетка поверхности в клетках.
    Size {
        surface: u32,
        cols: u16,
        rows: u16,
    },
    /// Освободить поверхность: её PTY закрывается, attach-клиент получает hangup.
    Close {
        surface: u32,
    },
    /// Асинхронный ответ на чтение буфера обмена `request` из журнала эффектов.
    ClipboardReply {
        request: u64,
        text: String,
    },
    Quit,
}

/// Ответ GUI на команду.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reply {
    pub error: Option<String>,
    /// Сетка поверхности после `Open` и `Size`.
    pub grid: Option<Grid>,
    pub text: Option<String>,
    /// Обработаны ли нажатие и отпускание (`Key`, `Click`).
    pub handled: Option<(bool, bool)>,
}

/// Сетка по `ghostty_surface_size`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grid {
    pub cols: u16,
    pub rows: u16,
    pub width_px: u32,
    pub height_px: u32,
    pub cell_width_px: u32,
    pub cell_height_px: u32,
}

/// Первая строка GUI водителю после подключения.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub pid: i32,
    pub nonce: String,
}

/// Строка журнала эффектов GUI.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Effect {
    /// OSC 52 записи дошла до поверхности.
    ClipboardWrite { surface: u32, text: String },
    /// Поверхность просит прочитать буфер обмена; ответ — [`GuiCommand::ClipboardReply`].
    ClipboardRead { surface: u32, request: u64 },
}

/// Строки журнала эффектов; неполная последняя строка (обрыв `SIGKILL` посреди записи) не
/// считается, повреждённая строка в середине — ошибка.
pub fn parse_effects(text: &str) -> Result<Vec<Effect>, String> {
    let complete = match text.rfind('\n') {
        Some(end) => &text[..end],
        None => "",
    };
    complete
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            serde_json::from_str(line).map_err(|err| format!("журнал эффектов: {err}: {line}"))
        })
        .collect()
}

/// Часть клетки, которую сценарий `bridge` добавляет к headless-частям той же клетки.
pub const SURFACE_PART: &str = "surface";

/// Клетки сценария и их часть: у C1–C4 и F2 одна часть, у D1, D3–D5 — `surface` рядом с
/// headless-частями сценария `queries`.
pub const CELLS: [(&str, Option<&str>); 9] = [
    ("C1", None),
    ("C2", None),
    ("C3", None),
    ("C4", None),
    ("D1", Some(SURFACE_PART)),
    ("D3", Some(SURFACE_PART)),
    ("D4", Some(SURFACE_PART)),
    ("D5", Some(SURFACE_PART)),
    ("F2", None),
];

/// Клетки, которые сценарий ещё не построил: `not_run` с причиной стенда.
pub fn pending(reason: &str) -> Vec<Cell> {
    CELLS
        .iter()
        .map(|(id, part)| evidence::later(id, *part, reason))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matrix::is_matrix_cell;
    use crate::results::Status;

    #[test]
    fn cells_are_matrix_cells_without_duplicates() {
        for (index, (id, part)) in CELLS.iter().enumerate() {
            assert!(is_matrix_cell(id), "{id}");
            assert!(
                !CELLS[..index].contains(&(*id, *part)),
                "клетка {id} повторяется"
            );
        }
    }

    #[test]
    fn gui_commands_round_trip_as_single_lines() {
        let commands = [
            GuiCommand::Open {
                surface: 1,
                command: "/bin/attach --session s".to_owned(),
                cols: 80,
                rows: 24,
            },
            GuiCommand::ClipboardReply {
                request: 3,
                text: "a\nb".to_owned(),
            },
            GuiCommand::Quit,
        ];
        for command in commands {
            let line = serde_json::to_string(&command).unwrap();
            assert!(!line.contains('\n'), "{line}");
            assert_eq!(serde_json::from_str::<GuiCommand>(&line).unwrap(), command);
        }
    }

    #[test]
    fn effects_ignore_torn_tail_and_reject_corrupt_line() {
        let write = Effect::ClipboardWrite {
            surface: 1,
            text: "x".to_owned(),
        };
        let line = serde_json::to_string(&write).unwrap();
        assert_eq!(
            parse_effects(&format!("{line}\n{{\"kind\":\"clip")).unwrap(),
            vec![write.clone()]
        );
        assert_eq!(parse_effects("").unwrap(), Vec::new());
        assert!(parse_effects(&format!("garbage\n{line}\n")).is_err());
    }

    #[test]
    fn pending_cells_are_not_run_with_reason() {
        let cells = pending("срез 1");
        assert_eq!(cells.len(), CELLS.len());
        for cell in &cells {
            assert_eq!(cell.status, Status::NotRun);
            assert!(cell.reason.contains("срез 1"), "{}", cell.reason);
        }
        let d1 = cells.iter().find(|cell| cell.id == "D1").unwrap();
        assert_eq!(d1.part.as_deref(), Some(SURFACE_PART));
    }
}
