//! Чистая часть сценария `bridge` (план P0.4, задача 15): клетки моста в настоящую поверхность и
//! их части, протокол водителя и GUI, сцены и их ожидаемые виды. Процесс сценария — водитель без
//! поверхности, обе GUI — дочерние процессы `probe-session-surface --gui`; FFI и окна живут в
//! `surface::bridge` под функцией `surface`.
//!
//! Ожидаемое берётся у эталона, а не пишется руками: шаг `--scenes` (функция `vt`) подаёт исходник
//! сцены и суффиксы шагов в `libghostty-vt` ревизии поверхности и записывает строки вида, курсор
//! и кодирование ввода в `scenes/bridge.json` каталога процесса.

use serde::{Deserialize, Serialize};

use crate::fixture::channel::{Event, Receipt, unhex};
use crate::fixture::query::ResponseKind;
use crate::pty::Winsize;

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
    /// Сетка и масштаб поверхности без изменений.
    Grid {
        surface: u32,
    },
    /// Нажатие и отпускание клавиши по коду macOS; `text` — текст клавиши и кодовая точка без
    /// Shift, как их передаёт вид.
    Key {
        surface: u32,
        keycode: u32,
        #[serde(default)]
        text: Option<String>,
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
    /// Масштаб содержимого поверхности в процентах (`ghostty_surface_set_content_scale`).
    Scale {
        surface: u32,
        percent: u32,
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
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    pub error: Option<String>,
    /// Сетка поверхности после `Open`, `Size` и `Scale`.
    pub grid: Option<Grid>,
    /// `backingScaleFactor` окна GUI и масштаб содержимого поверхности после `Open`, `Size` и
    /// `Scale`.
    pub scale: Option<Scale>,
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

/// Масштаб окна и поверхности.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Scale {
    pub window: f64,
    pub content: f64,
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

// --- сцены ---

/// Файл сцен моста в каталоге сцен процесса.
pub const SCENES_FILE: &str = "bridge.json";
/// Текст вставки в пробах ввода.
pub const PASTE_TEXT: &str = "pss-paste";
/// Клетка щелчка мыши в пробах ввода, с нуля.
pub const CLICK: (u16, u16) = (5, 3);
/// Геометрия эталонного кодировщика мыши: от неё зависит только перевод пикселей в клетку, а
/// щелчок идёт в центр клетки.
pub const MOUSE_CELL_PX: (u32, u32) = (10, 20);
/// Барьер стенда: DSR состояния `CSI 5 n` в объявленный набор хоста не входит и отвечается
/// поверхностью после всех байтов перед ним.
pub const BARRIER: &[u8] = b"\x1b[5n";
pub const BARRIER_REPLY: &[u8] = b"\x1b[0n";
/// Коды клавиш macOS: `kVK_ANSI_A` и `kVK_UpArrow`.
pub const KEYCODE_A: u32 = 0x00;
pub const KEYCODE_UP: u32 = 0x7e;

/// Исходник сцены: что нагрузка печатает до `SIGKILL` GUI, и шаги после восстановления.
#[derive(Clone, Copy, Debug)]
pub struct SceneSource {
    pub name: &'static str,
    pub cols: u16,
    pub rows: u16,
    pub source: fn() -> Vec<u8>,
    /// Шаги после восстановления: имя и суффикс, который нагрузка пишет живым потоком. Первый шаг
    /// — само восстановление с пустым суффиксом.
    pub steps: &'static [(&'static str, &'static str)],
    /// Проверяется ли кодирование ввода на каждом шаге.
    pub probes: bool,
}

/// Сцена C1: история длиннее экрана, SGR, гиперссылка, мягкий перенос и строка ровно во всю
/// ширину с жёстким переводом строки (контроль переноса), курсор после приглашения.
pub fn text_source() -> Vec<u8> {
    let mut out = String::new();
    for n in 1..=30 {
        if n % 7 == 0 {
            out.push_str(&format!("\x1b[1;32mbridge-text-{n:02}\x1b[0m\r\n"));
        } else {
            out.push_str(&format!("bridge-text-{n:02}\r\n"));
        }
    }
    out.push_str("\x1b]8;;https://example.invalid/pss\x1b\\link\x1b]8;;\x1b\\ after link\r\n");
    out.extend((0..100u8).map(|i| char::from(b'a' + i % 26)));
    out.push_str("\r\n");
    out.extend((0..80u8).map(|i| char::from(b'0' + i % 10)));
    out.push_str("\r\nprompt>");
    out.into_bytes()
}

/// Сцена C2 и C3: основной экран, alternate с `DECSC`, режимы отчётов 1004 и 2048, режимы вне
/// группы 2004, DECCKM, мышь 1000 с SGR 1006 и стек Kitty из двух уровней.
pub fn modes_source() -> Vec<u8> {
    concat!(
        "main-1\r\nmain-2\r\n",
        "\x1b[?1049h\x1b[H",
        "alt-screen-text\r\n",
        "\x1b[8;20H\x1b7",
        "\x1b[3;1Halt-line-3",
        "\x1b[?1004h\x1b[?2048h\x1b[?2004h\x1b[?1h\x1b[?1000h\x1b[?1006h",
        "\x1b[>8u\x1b[>24u",
    )
    .as_bytes()
    .to_vec()
}

pub const SCENES: [SceneSource; 2] = [
    SceneSource {
        name: "text",
        cols: 80,
        rows: 24,
        source: text_source,
        steps: &[("restored", ""), ("marker", "@")],
        probes: false,
    },
    SceneSource {
        name: "modes",
        cols: 80,
        rows: 24,
        source: modes_source,
        steps: &[
            ("restored", ""),
            ("kitty-pop", "\x1b[<u"),
            ("kitty-pop-2", "\x1b[<u"),
            ("decrc", "\x1b8@"),
            ("leave-alt", "\x1b[?1049l#"),
        ],
        probes: true,
    },
];

/// Вид поверхности по `read_text`: строки видимой области, мягко перенесённая строка склеена со
/// следующей; курсор активной области с нуля.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct View {
    pub lines: Vec<String>,
    pub cursor_x: u16,
    pub cursor_y: u16,
    pub alternate: bool,
}

/// Ожидаемое кодирование проб ввода одного шага у эталона.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Probes {
    pub key_a_hex: String,
    pub key_up_hex: String,
    pub click_hex: String,
    pub paste_hex: String,
}

impl Probes {
    /// Байты всех проб подряд в порядке стенда: «a», «вверх», щелчок, вставка.
    pub fn bytes(&self) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        for part in [
            &self.key_a_hex,
            &self.key_up_hex,
            &self.click_hex,
            &self.paste_hex,
        ] {
            out.extend(unhex(part)?);
        }
        Some(out)
    }
}

/// Вставка `text` так, как её кодирует терминал с 2004 и без него.
pub fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        [b"\x1b[200~", text.as_bytes(), b"\x1b[201~"].concat()
    } else {
        text.as_bytes().to_vec()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub name: String,
    pub suffix_hex: String,
    pub view: View,
    pub probes: Option<Probes>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SceneSpec {
    pub name: String,
    pub cols: u16,
    pub rows: u16,
    pub source_hex: String,
    pub steps: Vec<Step>,
}

/// `scenes/bridge.json`: сцены, штамп набора VT и версия профиля нормализаций эталона.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scenes {
    pub profile: u32,
    pub vt_stamp: Option<String>,
    pub scenes: Vec<SceneSpec>,
}

impl Scenes {
    pub fn scene(&self, name: &str) -> Result<&SceneSpec, String> {
        self.scenes
            .iter()
            .find(|scene| scene.name == name)
            .ok_or_else(|| format!("предусловие: в {SCENES_FILE} нет сцены {name}"))
    }
}

/// Строки так, как их отдаёт `read_text`: строка с признаком мягкого переноса склеивается со
/// следующей; хвостовые пробелы и хвостовые пустые строки отбрасываются.
pub fn read_text_lines(rows: &[(String, bool)]) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for (index, (text, wrapped)) in rows.iter().enumerate() {
        current.push_str(text);
        if !*wrapped || index + 1 == rows.len() {
            lines.push(std::mem::take(&mut current));
        }
    }
    crate::matrix::report_policy::normalize_rows(&lines)
}

/// Первое расхождение строк вида: `None` — равны.
pub fn first_difference(expected: &[String], actual: &[String]) -> Option<String> {
    if expected == actual {
        return None;
    }
    let at = expected
        .iter()
        .zip(actual)
        .position(|(left, right)| left != right)
        .unwrap_or_else(|| expected.len().min(actual.len()));
    Some(format!(
        "строка {at}: ожидалось {:?}, в поверхности {:?} (строк {} и {})",
        expected.get(at),
        actual.get(at),
        expected.len(),
        actual.len()
    ))
}

// --- квитанции фикстуры ---

/// Байты, прочитанные фикстурой из PTY после квитанции `mark`.
pub fn input_since(log: &[Receipt], mark: usize) -> Vec<u8> {
    log.iter()
        .skip(mark)
        .filter_map(|receipt| match &receipt.event {
            Event::Read { hex, .. } => unhex(hex),
            _ => None,
        })
        .flatten()
        .collect()
}

/// Ответы, разобранные фикстурой после `mark`, кроме ответов на барьер стенда.
pub fn responses_since(log: &[Receipt], mark: usize) -> Vec<(ResponseKind, String)> {
    let barrier = crate::fixture::channel::hex(BARRIER_REPLY);
    log.iter()
        .skip(mark)
        .filter_map(|receipt| match &receipt.event {
            Event::Response { response, hex, .. } if *hex != barrier => {
                Some((*response, hex.clone()))
            }
            _ => None,
        })
        .collect()
}

/// Ответы на барьер стенда во всём журнале.
pub fn barrier_replies(log: &[Receipt]) -> usize {
    let barrier = crate::fixture::channel::hex(BARRIER_REPLY);
    log.iter()
        .filter(|receipt| matches!(&receipt.event, Event::Response { hex, .. } if *hex == barrier))
        .count()
}

/// Размеры из квитанций `SIGWINCH` после `mark`.
pub fn winches_since(log: &[Receipt], mark: usize) -> Vec<Winsize> {
    log.iter()
        .skip(mark)
        .filter_map(|receipt| match &receipt.event {
            Event::Winch { size } => *size,
            _ => None,
        })
        .collect()
}

/// Длины записей фикстуры по командам после `mark` (квитанции `ack` с байтами).
pub fn writes_since(log: &[Receipt], mark: usize) -> Vec<u64> {
    log.iter()
        .skip(mark)
        .filter_map(|receipt| match &receipt.event {
            Event::Ack {
                bytes, error: None, ..
            } if *bytes > 0 => Some(*bytes),
            _ => None,
        })
        .collect()
}

pub fn count(responses: &[(ResponseKind, String)], kind: ResponseKind) -> usize {
    responses.iter().filter(|(found, _)| *found == kind).count()
}

/// Отчёт размера `CSI 48 ; rows ; cols ; … t` с сеткой `cols × rows`.
pub fn is_size_report(hex: &str, cols: u16, rows: u16) -> bool {
    unhex(hex).is_some_and(|bytes| bytes.starts_with(format!("\x1b[48;{rows};{cols};").as_bytes()))
}

#[cfg(feature = "vt")]
pub use build::build_scenes;

#[cfg(feature = "vt")]
mod build;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matrix::is_matrix_cell;

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
    fn key_command_without_text_still_parses() {
        let parsed: GuiCommand =
            serde_json::from_str(r#"{"op":"key","surface":1,"keycode":126}"#).unwrap();
        assert_eq!(
            parsed,
            GuiCommand::Key {
                surface: 1,
                keycode: KEYCODE_UP,
                text: None,
            }
        );
    }

    fn rows(items: &[(&str, bool)]) -> Vec<(String, bool)> {
        items
            .iter()
            .map(|(text, wrapped)| ((*text).to_owned(), *wrapped))
            .collect()
    }

    #[test]
    fn read_text_lines_join_soft_wraps_only() {
        let lines = read_text_lines(&rows(&[
            ("abc", true),
            ("def", false),
            ("hard", false),
            ("tail  ", false),
            ("", false),
            ("", false),
        ]));
        assert_eq!(lines, ["abcdef", "hard", "tail"]);
        // Перенос последней строки экрана не склеивает её с несуществующей.
        assert_eq!(
            read_text_lines(&rows(&[("x", false), ("y", true)])),
            ["x", "y"]
        );
    }

    #[test]
    fn first_difference_names_line_and_lengths() {
        let expected = vec!["a".to_owned(), "b".to_owned()];
        assert_eq!(first_difference(&expected, &expected), None);
        let shifted = vec!["a".to_owned(), "a".to_owned(), "b".to_owned()];
        let diff = first_difference(&expected, &shifted).unwrap();
        assert!(diff.starts_with("строка 1"), "{diff}");
        assert!(diff.contains("строк 2 и 3"), "{diff}");
        let short = vec!["a".to_owned()];
        assert!(
            first_difference(&expected, &short)
                .unwrap()
                .starts_with("строка 1")
        );
    }

    #[test]
    fn probes_concatenate_in_stand_order() {
        let probes = Probes {
            key_a_hex: crate::fixture::channel::hex(b"a"),
            key_up_hex: crate::fixture::channel::hex(b"\x1bOA"),
            click_hex: crate::fixture::channel::hex(b"\x1b[<0;6;4M\x1b[<0;6;4m"),
            paste_hex: crate::fixture::channel::hex(&paste_bytes(PASTE_TEXT, true)),
        };
        assert_eq!(
            probes.bytes().unwrap(),
            b"a\x1bOA\x1b[<0;6;4M\x1b[<0;6;4m\x1b[200~pss-paste\x1b[201~".to_vec()
        );
        assert_eq!(paste_bytes("x", false), b"x");
        let broken = Probes {
            key_a_hex: "zz".to_owned(),
            ..probes
        };
        assert_eq!(broken.bytes(), None);
    }

    fn receipt(seq: u64, event: Event) -> Receipt {
        Receipt {
            seq,
            t_ns: seq,
            event,
        }
    }

    #[test]
    fn receipts_split_into_input_responses_winches_and_writes() {
        use crate::fixture::channel::hex;
        let log = vec![
            receipt(
                1,
                Event::Read {
                    index: 1,
                    hex: hex(b"old"),
                },
            ),
            receipt(
                2,
                Event::Read {
                    index: 2,
                    hex: hex(b"\x1b[I"),
                },
            ),
            receipt(
                3,
                Event::Response {
                    index: 1,
                    response: ResponseKind::FocusIn,
                    hex: hex(b"\x1b[I"),
                },
            ),
            receipt(
                4,
                Event::Response {
                    index: 2,
                    response: ResponseKind::OtherCsi,
                    hex: hex(BARRIER_REPLY),
                },
            ),
            receipt(
                5,
                Event::Winch {
                    size: Some(Winsize::cells(24, 80)),
                },
            ),
            receipt(
                6,
                Event::Ack {
                    id: 1,
                    bytes: 4,
                    error: None,
                },
            ),
            receipt(
                7,
                Event::Ack {
                    id: 2,
                    bytes: 0,
                    error: None,
                },
            ),
        ];
        assert_eq!(input_since(&log, 1), b"\x1b[I");
        let responses = responses_since(&log, 0);
        assert_eq!(responses.len(), 1, "ответ на барьер не считается");
        assert_eq!(count(&responses, ResponseKind::FocusIn), 1);
        assert_eq!(barrier_replies(&log), 1);
        assert_eq!(winches_since(&log, 0), vec![Winsize::cells(24, 80)]);
        assert_eq!(winches_since(&log, 5), Vec::new());
        assert_eq!(
            writes_since(&log, 0),
            vec![4],
            "пустые подтверждения не записи"
        );
    }

    #[test]
    fn size_report_matches_grid() {
        use crate::fixture::channel::hex;
        assert!(is_size_report(&hex(b"\x1b[48;24;80;720;800t"), 80, 24));
        assert!(!is_size_report(&hex(b"\x1b[48;30;100;900;1600t"), 80, 24));
        assert!(!is_size_report("zz", 80, 24));
    }

    #[test]
    fn scenes_start_with_restore_and_cover_history_and_wraps() {
        for (index, scene) in SCENES.iter().enumerate() {
            assert!(
                !SCENES[..index].iter().any(|other| other.name == scene.name),
                "сцена {} повторяется",
                scene.name
            );
            assert_eq!(
                scene.steps.first(),
                Some(&("restored", "")),
                "{}",
                scene.name
            );
        }
        let text = String::from_utf8(text_source()).unwrap();
        let lines: Vec<&str> = text.split("\r\n").collect();
        assert!(lines.len() > 24, "история длиннее экрана");
        assert!(lines.iter().any(|line| line.len() == 100), "мягкий перенос");
        assert!(
            lines.iter().any(|line| line.len() == 80),
            "строка во всю ширину"
        );
        let modes = String::from_utf8(modes_source()).unwrap();
        for needle in [
            "\x1b[?1049h",
            "\x1b7",
            "\x1b[?2004h",
            "\x1b[>8u",
            "\x1b[>24u",
        ] {
            assert!(modes.contains(needle), "{needle:?}");
        }
    }

    #[test]
    fn scenes_lookup_names_missing_scene() {
        let scenes = Scenes {
            profile: 2,
            vt_stamp: None,
            scenes: Vec::new(),
        };
        let err = scenes.scene("text").unwrap_err();
        assert!(err.contains("text"), "{err}");
    }
}
