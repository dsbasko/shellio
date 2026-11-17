//! Сценарий `report-policy` (план P0.4, задача 10, часть B): политика отчётов в минимальной
//! поверхности. Клетки стенда `surface-*`, как `capi-*`: доказательство осуществимости, а не
//! `pass` клеток кандидата.
//!
//! Чистая часть — сцены, живой поток, стадии, разбор ввода от поверхности, независимая проверка
//! записанных байтов и причины провалов. Байты сцен строит [`build_scenes`] на VT (функция `vt`)
//! в каталог выхода прогона, прогон в поверхности — `surface::scene` (функция `surface`).
//!
//! Части: сцена × доставка. Доставка `control` отдаёт поверхности сцену с режимами отчётов и живой
//! поток без фильтра: она доказывает, что поверхность на каждой стадии шлёт отчёт и квитанции
//! фикстуры его видят. `policy` — канонический кодировщик и фильтр, `filter` — сцена с режимами
//! отчётов через фильтр. Стадия, на которой контроль отчёта не показал, у политики не доказана.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::bridge::{self, ReportPolicy};

pub const SCENES_DIR: &str = "scenes";
pub const MANIFEST: &str = "manifest.json";
pub const COLS: u16 = 80;
pub const ROWS: u16 = 24;
/// Сетка после ресайза стадии `resize`.
pub const RESIZE_COLS: u16 = 100;
pub const RESIZE_ROWS: u16 = 30;
/// Префикс attach — RIS. Команду поверхности libghostty на macOS запускает через `login(1)`, а
/// он без `~/.hushlogin` печатает «Last login»; кодировщик рассчитан на свежий терминал. RIS на
/// свежей поверхности отчётов не даёт: отчёт схемы при сбросе идёт только с включённым 2031.
pub const ATTACH_PREFIX: &[u8] = b"\x1bc";
/// Символ, который живой поток печатает в восстановленной позиции курсора.
pub const CURSOR_MARK: char = '@';
/// Режимы вне группы отчётов, которые сцены включают и которые должны дойти до поверхности.
pub const KEPT_MODES: [u16; 2] = [2004, 1];
/// Нажатие «вверх» при DECCKM.
pub const KEY_UP_APPLICATION: &[u8] = b"\x1bOA";

const SEPARATE: &str = concat!(
    "\x1b]2;pss report-policy\x07",
    "\x1b[1;31mкрасный\x1b[0m текст\r\n",
    "line two\r\n",
    "\x1b[?1004h\x1b[?2048h\x1b[?2033h\x1b[?2031h\x1b[?2004h\x1b[?1h",
    "\x1b[5;10Hcursor",
);

const MIXED: &str = concat!(
    "\x1b[?1004;2004h\x1b[?2048;1;2033h\x1b[?2031;25h",
    "\x1b[?1004s\x1b[?1004l\x1b[?1004r",
    "\x1b[?2048l",
    "заголовок 日本\r\n",
    "\x1b[4mподчёркнуто\x1b[24m\r\n",
    "\x1b[7;3H",
);

/// Живой поток нагрузки после восстановления: смешанные CSI с режимами отчётов и вне группы,
/// первое настоящее включение 2048 в сцене `mixed`, сохранение и восстановление режима отчётов,
/// затем символ в позиции курсора.
pub const LIVE: &str = concat!(
    "\x1b[?1004;2004h",
    "\x1b[?2048;1h",
    "\x1b[?2033;25h",
    "\x1b[?2031s\x1b[?2031l\x1b[?2031r",
    "@",
);

/// Исходник сцены: что нагрузка напечатала до отключения клиента.
#[derive(Clone, Copy, Debug)]
pub struct Scene {
    pub name: &'static str,
    pub source: &'static str,
    pub description: &'static str,
}

pub const SCENES: [Scene; 2] = [
    Scene {
        name: "separate",
        source: SEPARATE,
        description: "текст со стилем, 1004, 2048, 2033, 2031, 2004 и DECCKM отдельными CSI",
    },
    Scene {
        name: "mixed",
        source: MIXED,
        description: "смешанные CSI, XTSAVE и XTRESTORE 1004, 2048 выключен в конце, широкие символы",
    },
];

/// Как поверхность получает сцену и живой поток.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Delivery {
    /// Кодировщик с режимами отчётов, без фильтра: положительный контроль.
    Control,
    /// Канонический кодировщик и фильтр: выбранная политика.
    Policy,
    /// Кодировщик с режимами отчётов через фильтр: фильтр один закрывает отчёты.
    Filter,
}

impl Delivery {
    pub const ALL: [Delivery; 3] = [Delivery::Control, Delivery::Policy, Delivery::Filter];

    pub fn name(self) -> &'static str {
        match self {
            Delivery::Control => "control",
            Delivery::Policy => "policy",
            Delivery::Filter => "filter",
        }
    }

    pub fn encoding(self) -> ReportPolicy {
        match self {
            Delivery::Policy => ReportPolicy::Canonical,
            Delivery::Control | Delivery::Filter => ReportPolicy::Passthrough,
        }
    }

    pub fn filtered(self) -> bool {
        self != Delivery::Control
    }
}

pub fn encoding_name(policy: ReportPolicy) -> &'static str {
    match policy {
        ReportPolicy::Canonical => "canonical",
        ReportPolicy::Passthrough => "passthrough",
    }
}

/// Стадия части: стимул и граница, после которой ввод от поверхности считается.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Stage {
    Replay,
    Key,
    Live,
    FocusOut,
    FocusIn,
    Hidden,
    Visible,
    Resize,
    Scheme,
}

impl Stage {
    pub const ALL: [Stage; 9] = [
        Stage::Replay,
        Stage::Key,
        Stage::Live,
        Stage::FocusOut,
        Stage::FocusIn,
        Stage::Hidden,
        Stage::Visible,
        Stage::Resize,
        Stage::Scheme,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Stage::Replay => "replay",
            Stage::Key => "key",
            Stage::Live => "live",
            Stage::FocusOut => "focus-out",
            Stage::FocusIn => "focus-in",
            Stage::Hidden => "hidden",
            Stage::Visible => "visible",
            Stage::Resize => "resize",
            Stage::Scheme => "scheme",
        }
    }

    /// Стадия, на которой поверхность с режимами отчётов шлёт отчёт: у контроля он обязан быть.
    /// Нажатие клавиши отчётов не порождает.
    pub fn reports_in_control(self) -> bool {
        self != Stage::Key
    }

    /// Маркер-вставка, закрывающая стадию.
    pub fn marker(self) -> String {
        format!("pss-mark-{}", self.name())
    }
}

/// Сцена в каталоге выхода: файлы байтов по кодировке и ожидаемое состояние поверхности.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SceneEntry {
    pub name: String,
    pub description: String,
    /// Файл байтов по имени кодировки (`canonical`, `passthrough`), путь от каталога сцен.
    pub files: BTreeMap<String, String>,
    /// Строки видимой области после восстановления, без хвостовых пустых клеток.
    pub rows: Vec<String>,
    /// Курсор после восстановления, с нуля.
    pub cursor_x: u16,
    pub cursor_y: u16,
    /// Каноническое значение режимов отчётов у источника.
    pub report_modes: BTreeMap<String, bool>,
    /// Ожидаемые режимы вне группы, которые проверяются в поверхности.
    pub kept_modes: BTreeMap<String, bool>,
    /// Что состояние не выражает VT-байтами (`bridge::inexpressible`).
    pub inexpressible: Vec<String>,
}

/// `manifest.json` каталога сцен: сцены, префикс attach, живой поток, штамп набора VT и версия
/// профиля нормализаций, с которыми сцены построены.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub profile: u32,
    pub vt_stamp: Option<String>,
    pub vt_version: Option<String>,
    pub cols: u16,
    pub rows: u16,
    pub prefix_hex: String,
    pub live_hex: String,
    pub scenes: Vec<SceneEntry>,
}

// --- разбор ввода от поверхности ---

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

/// Граница стадии во вводе от поверхности: байты до маркера, обёрнут ли маркер bracketed paste и
/// конец маркера в буфере.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fence {
    pub before: Vec<u8>,
    pub bracketed: bool,
    pub end: usize,
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Маркер стадии в накопленном вводе: сначала форма вставки с 2004, затем голый маркер.
pub fn split_at_marker(buffer: &[u8], marker: &str) -> Option<Fence> {
    let mut wrapped = PASTE_START.to_vec();
    wrapped.extend_from_slice(marker.as_bytes());
    wrapped.extend_from_slice(PASTE_END);
    if let Some(at) = find(buffer, &wrapped) {
        return Some(Fence {
            before: buffer[..at].to_vec(),
            bracketed: true,
            end: at + wrapped.len(),
        });
    }
    let at = find(buffer, marker.as_bytes())?;
    Some(Fence {
        before: buffer[..at].to_vec(),
        bracketed: false,
        end: at + marker.len(),
    })
}

/// Байты стадии без ожидаемых последовательностей, каждая снимается один раз: ответ на запрос
/// стенда-границы и кодирование нажатой клавиши.
pub fn unexpected(before: &[u8], allowed: &[Vec<u8>]) -> Vec<u8> {
    let mut rest = before.to_vec();
    for token in allowed {
        if let Some(at) = find(&rest, token) {
            rest.drain(at..at + token.len());
        }
    }
    rest
}

/// Отчёты в байтах от поверхности по формам ревизии поверхности: фокус `CSI I` и `CSI O`, размер
/// `CSI 48 ; … t`, видимость `CSI ? 999 ; n n`, схема `CSI ? 997 ; n n`; остальное — `other`.
pub fn report_kinds(bytes: &[u8]) -> Vec<String> {
    let mut kinds = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] != 0x1b || bytes.get(at + 1) != Some(&b'[') {
            let start = at;
            while at < bytes.len() && bytes[at] != 0x1b {
                at += 1;
            }
            kinds.push(format!(
                "other:{}",
                hex_of(&bytes[start..at.max(start + 1)])
            ));
            at = at.max(start + 1);
            continue;
        }
        let start = at;
        at += 2;
        while at < bytes.len() && !(0x40..=0x7e).contains(&bytes[at]) {
            at += 1;
        }
        let end = (at + 1).min(bytes.len());
        let sequence = &bytes[start..end];
        let params =
            std::str::from_utf8(&sequence[2..sequence.len().saturating_sub(1)]).unwrap_or_default();
        let kind = match (sequence.last(), params) {
            (Some(b'I'), "") => "focus-in".to_owned(),
            (Some(b'O'), "") => "focus-out".to_owned(),
            (Some(b't'), p) if p.starts_with("48;") => "size".to_owned(),
            (Some(b'n'), p) if p.starts_with("?999;") => "visibility".to_owned(),
            (Some(b'n'), p) if p.starts_with("?997;") => "scheme".to_owned(),
            _ => format!("other:{}", hex_of(sequence)),
        };
        kinds.push(kind);
        at = end;
    }
    kinds
}

fn hex_of(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Ответ CPR `CSI row ; col R`: строка и столбец с единицы.
pub fn parse_cpr(bytes: &[u8]) -> Option<(u16, u16)> {
    let text = std::str::from_utf8(bytes).ok()?;
    let body = text.strip_prefix("\x1b[")?.strip_suffix('R')?;
    let (row, col) = body.split_once(';')?;
    Some((row.parse().ok()?, col.parse().ok()?))
}

// --- независимая проверка записанных байтов ---

/// Параметры частных CSI режимов (`CSI ? … h|l|s|r`) во всём потоке целиком, с финальным байтом.
/// Проверка не использует [`bridge::ReportFilter`]: сканирует склеенные байты, а не поток по
/// кускам, и не держит состояния между записями.
pub fn private_modes(bytes: &[u8]) -> Vec<(u16, u8)> {
    let mut found = Vec::new();
    let mut at = 0;
    while let Some(offset) = find(&bytes[at..], b"\x1b[?") {
        let start = at + offset + 3;
        let mut end = start;
        while end < bytes.len() && (bytes[end].is_ascii_digit() || bytes[end] == b';') {
            end += 1;
        }
        if let Some(&final_byte) = bytes.get(end)
            && matches!(final_byte, b'h' | b'l' | b's' | b'r')
            && let Ok(params) = std::str::from_utf8(&bytes[start..end])
        {
            for param in params.split(';') {
                if let Ok(mode) = param.parse() {
                    found.push((mode, final_byte));
                }
            }
        }
        at = end.max(start);
    }
    found
}

/// Режимы отчётов в записанных байтах: у политики их быть не должно.
pub fn leaked_report_modes(bytes: &[u8]) -> Vec<(u16, u8)> {
    private_modes(bytes)
        .into_iter()
        .filter(|(mode, _)| bridge::is_report_mode(*mode))
        .collect()
}

/// Включён ли режим вне группы последней из его записей в байтах.
pub fn sets_mode(bytes: &[u8], mode: u16) -> bool {
    private_modes(bytes)
        .into_iter()
        .rfind(|(found, final_byte)| *found == mode && matches!(final_byte, b'h' | b'l'))
        .is_some_and(|(_, final_byte)| final_byte == b'h')
}

// --- текст и курсор ---

/// Строки видимой области: без хвостовых пробелов и хвостовых пустых строк.
pub fn normalize_rows<S: AsRef<str>>(rows: &[S]) -> Vec<String> {
    let mut rows: Vec<String> = rows
        .iter()
        .map(|row| row.as_ref().trim_end().to_owned())
        .collect();
    while rows.last().is_some_and(String::is_empty) {
        rows.pop();
    }
    rows
}

pub fn text_rows(text: &str) -> Vec<String> {
    normalize_rows(&text.split('\n').collect::<Vec<_>>())
}

/// Ожидаемые строки после печати `mark` в позиции курсора. Строка курсора в сценах из узких
/// символов, поэтому столбец — номер символа.
pub fn with_mark(rows: &[String], x: u16, y: u16, mark: char) -> Vec<String> {
    let mut rows = rows.to_vec();
    let y = usize::from(y);
    while rows.len() <= y {
        rows.push(String::new());
    }
    let mut chars: Vec<char> = rows[y].chars().collect();
    let x = usize::from(x);
    while chars.len() <= x {
        chars.push(' ');
    }
    chars[x] = mark;
    rows[y] = chars.into_iter().collect();
    normalize_rows(&rows)
}

// --- команда поверхности ---

/// Команда для shell libghostty: программа и аргументы в одинарных кавычках.
pub fn shell_command(program: &str, args: &[String]) -> String {
    let quote = |text: &str| format!("'{}'", text.replace('\'', "'\\''"));
    std::iter::once(quote(program))
        .chain(args.iter().map(|arg| quote(arg)))
        .collect::<Vec<_>>()
        .join(" ")
}

// --- причины провалов ---

/// Причина провала части: `surface-effect` — поверхность шлёт отчёт мимо политики, `filter` —
/// ошибка фильтра, `bytes` — состояние не выражается байтами или восстановлено не так.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Cause {
    SurfaceEffect,
    Filter,
    Bytes,
}

impl Cause {
    pub fn name(self) -> &'static str {
        match self {
            Cause::SurfaceEffect => "surface-effect",
            Cause::Filter => "filter",
            Cause::Bytes => "bytes",
        }
    }
}

/// Лишний ввод от поверхности при политике: утечка режима отчётов в записанных байтах — фильтр,
/// иначе поверхность сама.
pub fn report_cause(emitted: &[u8]) -> Cause {
    if leaked_report_modes(emitted).is_empty() {
        Cause::SurfaceEffect
    } else {
        Cause::Filter
    }
}

/// Режим вне группы не восстановлен: не дошёл в записанных байтах — фильтр, дошёл — байты.
pub fn kept_cause(emitted: &[u8], mode: u16) -> Cause {
    if sets_mode(emitted, mode) {
        Cause::Bytes
    } else {
        Cause::Filter
    }
}

#[cfg(feature = "vt")]
pub use build::build_scenes;

#[cfg(feature = "vt")]
mod build {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::Path;

    use super::{
        ATTACH_PREFIX, COLS, KEPT_MODES, LIVE, MANIFEST, Manifest, ROWS, SCENES, SCENES_DIR,
        SceneEntry, encoding_name,
    };
    use crate::bridge::{self, ReportPolicy, extract};
    use crate::fixture::channel::hex;
    use crate::matrix::capi::CONTINUATION_BUDGET;
    use crate::oracle::model::PROFILE_VERSION;
    use crate::vt::Terminal;

    /// Сцены в `<out>/scenes/`: исходник подаётся в VT ревизии поверхности, состояние снимается
    /// мостом части A и кодируется в обеих формах. Возвращает путь манифеста.
    pub fn build_scenes(out: &Path) -> Result<std::path::PathBuf, String> {
        let dir = out.join(SCENES_DIR);
        fs::create_dir_all(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
        let mut scenes = Vec::new();
        for scene in SCENES {
            let mut term =
                Terminal::new(COLS, ROWS).map_err(|err| format!("сцена {}: {err}", scene.name))?;
            term.set_continuation_max_bytes(CONTINUATION_BUDGET)
                .map_err(|err| format!("сцена {}: {err}", scene.name))?;
            term.write(scene.source.as_bytes());
            let state = extract::extract(&term, CONTINUATION_BUDGET)
                .map_err(|err| format!("сцена {}: извлечение: {err}", scene.name))?;
            let mut files = BTreeMap::new();
            for policy in [ReportPolicy::Canonical, ReportPolicy::Passthrough] {
                let name = format!("{}.{}.bin", scene.name, encoding_name(policy));
                let path = dir.join(&name);
                fs::write(&path, bridge::encode(&state, policy))
                    .map_err(|err| format!("{}: {err}", path.display()))?;
                files.insert(encoding_name(policy).to_owned(), name);
            }
            let rows = (0..ROWS)
                .map(|y| term.row_text(y))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|err| format!("сцена {}: строки: {err}", scene.name))?;
            let mode_of = |mode: u16| {
                let key = format!("?{mode}");
                let value = state.modes.get(&key).copied().unwrap_or(false);
                (key, value)
            };
            scenes.push(SceneEntry {
                name: scene.name.to_owned(),
                description: scene.description.to_owned(),
                files,
                rows: super::normalize_rows(&rows),
                cursor_x: state.primary.cursor.x,
                cursor_y: state.primary.cursor.y,
                report_modes: bridge::REPORT_MODES.into_iter().map(mode_of).collect(),
                kept_modes: KEPT_MODES.into_iter().map(mode_of).collect(),
                inexpressible: bridge::inexpressible(&state),
            });
        }
        let env = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
        let manifest = Manifest {
            profile: PROFILE_VERSION,
            vt_stamp: env("SHELLIO_PROBE_STAMP_VT"),
            vt_version: env("SHELLIO_PROBE_VERSION_VT"),
            cols: COLS,
            rows: ROWS,
            prefix_hex: hex(ATTACH_PREFIX),
            live_hex: hex(LIVE.as_bytes()),
            scenes,
        };
        let path = dir.join(MANIFEST);
        let text = serde_json::to_string_pretty(&manifest)
            .map_err(|err| format!("манифест сцен: {err}"))?;
        fs::write(&path, text).map_err(|err| format!("{}: {err}", path.display()))?;
        Ok(path)
    }
}

#[cfg(test)]
mod tests;
