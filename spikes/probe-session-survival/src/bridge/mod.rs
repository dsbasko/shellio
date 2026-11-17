//! Байтовый мост (план P0.4, задача 10): кодировщик «состояние → VT-байты» и фильтр режимов
//! отчётов. Чистый модуль без FFI: состояние снимает `bridge::extract` (функция `vt`), здесь только
//! собственная структура [`State`], сборка байтов и потоковый фильтр.
//!
//! Кодировщик рассчитан на свежий терминал клиента: всё, чем состояние отличается от сброса,
//! выражается последовательностями, после которых терминал клиента равен исходному. Порядок
//! выведен из `src/terminal/Terminal.zig` ревизии поверхности:
//!
//! - содержимое пишется при полных полях и выключенном DECOM: DECSTBM и DECSLRM переводят курсор в
//!   начало, а поле прокрутки не даёт истории;
//! - режим 2027 включается до содержимого, он меняет разбиение на графемы;
//! - у каждого экрана свой курсор, сохранённый курсор и кольцо Kitty; переключение 47 и 1047
//!   копирует курсор, 1049 при входе сохраняет курсор основного экрана и очищает alternate, при
//!   выходе восстанавливает сохранённый курсор. Поэтому собственный курсор неактивного экрана
//!   снаружи не наблюдаем, а его сохранённый курсор задаётся до переключения;
//! - сохранённый курсор, затем курсор с отложенным переносом и пером, затем режимы, последней —
//!   continuation.
//!
//! Политика отчётов (часть B): режимы [`REPORT_MODES`] ведёт хост, кодировщик их set и reset не
//! выдаёт, а [`ReportFilter`] вырезает их из живого потока хост→клиент.

#[cfg(feature = "vt")]
pub mod extract;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::oracle::model::{Color, Rgb, Screen, Style, TRACKED_MODES, Width};

/// Режимы отчётов, которые поверхность ревизии `83fb95624b04` порождает сама: включение 1004
/// посылает текущий фокус, 2033 — видимость, 2048 — размер (`setMode` в
/// `src/termio/stream_handler.zig`); при включённых 1004, 2031, 2033 и 2048 `Termio.zig` сам шлёт
/// отчёты при фокусе, смене цветовой схемы, видимости и ресайзе, а `fullReset` — отчёт схемы.
/// 2026 и 5522 в группу не входят: 2026 удерживает отрисовку, 5522 меняет кодирование вставки.
pub const REPORT_MODES: [u16; 4] = [1004, 2031, 2033, 2048];

pub fn is_report_mode(mode: u16) -> bool {
    REPORT_MODES.contains(&mode)
}

/// Что кодировщик делает с режимами отчётов.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReportPolicy {
    /// Режимы ведёт хост: клиент их не получает.
    Canonical,
    /// Режимы передаются клиенту как остальные (для сравнения с политикой).
    Passthrough,
}

/// Клетка: кластер графем (пусто — никогда не напечатанная клетка), ширина, стиль, URI
/// гиперссылки, защита DECSCA и фон пустой клетки, стёртой с цветом фона (такой фон хранится в
/// содержимом клетки, а не в стиле).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cell {
    pub text: String,
    pub width: Width,
    pub style: Style,
    pub hyperlink: Option<String>,
    pub protected: bool,
    pub bg_only: Option<Color>,
}

impl Cell {
    pub fn blank() -> Cell {
        Cell {
            text: String::new(),
            width: Width::Narrow,
            style: Style::default(),
            hyperlink: None,
            protected: false,
            bg_only: None,
        }
    }

    fn is_blank(&self) -> bool {
        *self == Cell::blank()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
    pub cells: Vec<Cell>,
    pub wrap: bool,
    pub wrap_continuation: bool,
}

impl Row {
    pub fn blank(cols: u16) -> Row {
        Row {
            cells: vec![Cell::blank(); usize::from(cols)],
            wrap: false,
            wrap_continuation: false,
        }
    }

    fn is_blank(&self) -> bool {
        !self.wrap && !self.wrap_continuation && self.cells.iter().all(Cell::is_blank)
    }
}

/// Перо курсора: стиль, открытая гиперссылка и защита.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pen {
    pub style: Style,
    pub hyperlink: Option<String>,
    pub protected: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub x: u16,
    pub y: u16,
    pub pending_wrap: bool,
    pub pen: Pen,
}

/// Сохранённый курсор (`DECSC`): позиция, отложенный перенос, стиль, защита и режим origin.
/// Гиперссылку `DECSC` не сохраняет, наборы символов вне профиля.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedCursor {
    pub x: u16,
    pub y: u16,
    pub pending_wrap: bool,
    pub style: Style,
    pub protected: bool,
    pub origin: bool,
}

/// Один экран.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenState {
    /// История от дальней строки к ближней.
    pub history: Vec<Row>,
    /// Активная область сверху вниз.
    pub active: Vec<Row>,
    /// Курсор активного экрана. У неактивного экрана не наблюдаем: переключение экрана его
    /// перезаписывает (копия курсора или восстановление сохранённого).
    pub cursor: Cursor,
    pub saved: SavedCursor,
    /// Кольцо флагов Kitty от текущего значения: `kitty[i]` — значение после `i` снятий.
    pub kitty: Vec<u8>,
}

impl ScreenState {
    pub fn blank(cols: u16, rows: u16) -> ScreenState {
        ScreenState {
            history: Vec::new(),
            active: (0..rows).map(|_| Row::blank(cols)).collect(),
            cursor: Cursor::default(),
            saved: SavedCursor::default(),
            kitty: Vec::new(),
        }
    }

    /// Экран не отличается от только что созданного.
    pub fn is_blank(&self) -> bool {
        self.history.is_empty()
            && self.active.iter().all(Row::is_blank)
            && self.saved == SavedCursor::default()
            && self.kitty.iter().all(|flags| *flags == 0)
    }
}

/// Поля прокрутки, номера с нуля включительно.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Margins {
    pub top: u16,
    pub bottom: u16,
    pub left: u16,
    pub right: u16,
}

impl Margins {
    pub fn full(cols: u16, rows: u16) -> Margins {
        Margins {
            top: 0,
            bottom: rows.saturating_sub(1),
            left: 0,
            right: cols.saturating_sub(1),
        }
    }
}

/// Текущий режим отслеживания мыши: последний включённый из 9, 1000, 1002, 1003.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MouseEvent {
    None,
    X10,
    Normal,
    Button,
    Any,
}

impl MouseEvent {
    pub const MODES: [u16; 4] = [9, 1000, 1002, 1003];

    fn mode(self) -> Option<u16> {
        match self {
            MouseEvent::None => None,
            MouseEvent::X10 => Some(9),
            MouseEvent::Normal => Some(1000),
            MouseEvent::Button => Some(1002),
            MouseEvent::Any => Some(1003),
        }
    }
}

/// Текущий формат мыши: последний включённый из 1005, 1006, 1015, 1016.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MouseFormat {
    X10,
    Utf8,
    Sgr,
    Urxvt,
    SgrPixels,
}

impl MouseFormat {
    pub const MODES: [u16; 4] = [1005, 1006, 1015, 1016];

    fn mode(self) -> Option<u16> {
        match self {
            MouseFormat::X10 => None,
            MouseFormat::Utf8 => Some(1005),
            MouseFormat::Sgr => Some(1006),
            MouseFormat::Urxvt => Some(1015),
            MouseFormat::SgrPixels => Some(1016),
        }
    }
}

/// Состояние терминала, которое кодирует мост.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub cols: u16,
    pub rows: u16,
    pub screen: Screen,
    pub primary: ScreenState,
    /// Alternate; пустой, если не отличается от только что созданного.
    pub alternate: ScreenState,
    /// Режимы `TRACKED_MODES` по ключу `Mode::key`.
    pub modes: BTreeMap<String, bool>,
    pub mouse_event: MouseEvent,
    pub mouse_format: MouseFormat,
    /// Позиции табуляторов левее последней колонки (у края HT останавливается и без табулятора).
    pub tabstops: Vec<u16>,
    pub margins: Margins,
    /// Цвета палитры, отличные от палитры по умолчанию.
    pub palette: Vec<(u8, Rgb)>,
    /// Переопределения OSC 10, 11 и 12; `None` — цвет по умолчанию.
    pub foreground: Option<Rgb>,
    pub background: Option<Rgb>,
    pub cursor_color: Option<Rgb>,
    pub title: String,
    pub pwd: String,
    /// Незавершённая последовательность на конце потока.
    pub continuation: Vec<u8>,
}

impl State {
    /// Свежий терминал: заготовка для тестов.
    pub fn blank(cols: u16, rows: u16) -> State {
        let mut modes: BTreeMap<String, bool> = TRACKED_MODES
            .iter()
            .map(|mode| (mode.key(), false))
            .collect();
        for on in ["?7", "?25"] {
            modes.insert(on.to_owned(), true);
        }
        State {
            cols,
            rows,
            screen: Screen::Primary,
            primary: ScreenState::blank(cols, rows),
            alternate: ScreenState::blank(cols, rows),
            modes,
            mouse_event: MouseEvent::None,
            mouse_format: MouseFormat::X10,
            tabstops: default_tabstops(cols),
            margins: Margins::full(cols, rows),
            palette: Vec::new(),
            foreground: None,
            background: None,
            cursor_color: None,
            title: String::new(),
            pwd: String::new(),
            continuation: Vec::new(),
        }
    }

    fn mode(&self, key: &str) -> bool {
        self.modes.get(key).copied().unwrap_or(false)
    }
}

/// Табуляторы свежего терминала левее последней колонки: каждые восемь колонок.
pub fn default_tabstops(cols: u16) -> Vec<u16> {
    (1..cols.saturating_sub(1)).filter(|x| x % 8 == 0).collect()
}

/// Состояние без режимов отчётов: их ведёт хост, клиент их не получает.
pub fn without_report_modes(state: &State) -> State {
    let mut state = state.clone();
    state.modes.retain(|key, _| {
        !key.strip_prefix('?')
            .and_then(|value| value.parse().ok())
            .is_some_and(is_report_mode)
    });
    state
}

/// Сколько различий [`differences`] перечисляет.
const MAX_DIFFERENCES: usize = 20;

/// Пути, по которым два состояния различаются, с обоими значениями; не больше
/// [`MAX_DIFFERENCES`].
pub fn differences(a: &State, b: &State) -> Vec<String> {
    let (Ok(left), Ok(right)) = (serde_json::to_value(a), serde_json::to_value(b)) else {
        return vec!["состояние не сериализуется".to_owned()];
    };
    let mut out = Vec::new();
    diff_values("", &left, &right, &mut out);
    out
}

fn diff_values(path: &str, a: &serde_json::Value, b: &serde_json::Value, out: &mut Vec<String>) {
    use serde_json::Value;
    if a == b || out.len() >= MAX_DIFFERENCES {
        return;
    }
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            for (key, left) in x {
                let right = y.get(key).unwrap_or(&Value::Null);
                diff_values(&format!("{path}.{key}"), left, right, out);
            }
            for (key, right) in y {
                if !x.contains_key(key) {
                    diff_values(&format!("{path}.{key}"), &Value::Null, right, out);
                }
            }
        }
        (Value::Array(x), Value::Array(y)) if x.len() == y.len() => {
            for (index, (left, right)) in x.iter().zip(y).enumerate() {
                diff_values(&format!("{path}[{index}]"), left, right, out);
            }
        }
        _ => {
            let short = |value: &Value| {
                let text = value.to_string();
                if text.chars().count() > 120 {
                    format!("{}…", text.chars().take(120).collect::<String>())
                } else {
                    text
                }
            };
            out.push(format!("{path}: {} против {}", short(a), short(b)));
        }
    }
}

/// Части состояния, которые VT-байтами в свежем терминале не выражаются (раздел о порядке в
/// заголовке модуля): мягкий перенос без печатной клетки на краю или в начале следующей строки,
/// DECCOLM, флаг режима alternate при активном основном экране, alternate без флага режима, мышь с
/// включёнными флагами всех режимов группы и выключенной группой.
pub fn inexpressible(state: &State) -> Vec<String> {
    let mut out = Vec::new();
    for (name, screen) in [("primary", &state.primary), ("alternate", &state.alternate)] {
        let rows: Vec<&Row> = screen.history.iter().chain(&screen.active).collect();
        for (index, pair) in rows.windows(2).enumerate() {
            let (row, next) = (pair[0], pair[1]);
            if !row.wrap {
                continue;
            }
            let edge = row
                .cells
                .last()
                .is_some_and(|cell| !cell.text.is_empty() || cell.width == Width::SpacerHead)
                || row
                    .cells
                    .iter()
                    .rev()
                    .nth(1)
                    .is_some_and(|cell| cell.width == Width::Wide);
            let starts = next.cells.first().is_some_and(|cell| !cell.text.is_empty());
            if !edge || !starts {
                out.push(format!(
                    "{name}: мягкий перенос строки {index} без печатной клетки"
                ));
            }
        }
    }
    if state.mode("?3") {
        out.push("DECCOLM (?3) меняет размер и очищает экран".to_owned());
    }
    let alt_flags = ["?47", "?1047", "?1049"].iter().any(|key| state.mode(key));
    match state.screen {
        Screen::Primary if alt_flags => {
            out.push("флаг режима alternate при активном основном экране".to_owned());
        }
        Screen::Alternate if !alt_flags => out.push("alternate без флага режима".to_owned()),
        Screen::Primary | Screen::Alternate => {}
    }
    for (group, current) in [
        (MouseEvent::MODES, state.mouse_event.mode()),
        (MouseFormat::MODES, state.mouse_format.mode()),
    ] {
        if current.is_none() && group.iter().all(|mode| state.mode(&format!("?{mode}"))) {
            out.push(format!(
                "группа мыши {group:?}: все флаги включены, группа выключена"
            ));
        }
    }
    out
}

// --- кодировщик ---

/// Режимы с особым порядком: экраны, сохранение курсора, поля, origin, графемы, мышь, 2026 и
/// DECCOLM (без режима 40 он не включается, с ним меняет размер и очищает экран).
fn special_mode(value: u16, ansi: bool) -> bool {
    !ansi
        && (matches!(value, 3 | 6 | 47 | 69 | 1047 | 1048 | 1049 | 2026 | 2027)
            || MouseEvent::MODES.contains(&value)
            || MouseFormat::MODES.contains(&value))
}

/// Байты, после которых свежий терминал клиента равен `state`. Без FFI.
pub fn encode(state: &State, policy: ReportPolicy) -> Vec<u8> {
    let mut w = Writer::default();
    let full = Margins::full(state.cols, state.rows);
    if state.mode("?2027") {
        w.csi("?2027h");
    }
    for (index, rgb) in &state.palette {
        w.osc(&format!("4;{index};{}", xcolor(*rgb)));
    }
    for (code, color) in [
        (10, state.foreground),
        (11, state.background),
        (12, state.cursor_color),
    ] {
        if let Some(rgb) = color {
            w.osc(&format!("{code};{}", xcolor(rgb)));
        }
    }
    if !state.title.is_empty() {
        w.osc(&format!("2;{}", state.title));
    }
    if !state.pwd.is_empty() {
        w.osc(&format!("7;{}", state.pwd));
    }

    match state.screen {
        Screen::Alternate => {
            write_rows(&mut w, &state.primary, state.cols, true);
            kitty(&mut w, &state.primary.kitty);
            let entry = if state.mode("?1049") {
                1049
            } else if state.mode("?1047") {
                1047
            } else {
                47
            };
            let saved = target_of_saved(&state.primary.saved);
            if entry == 1049 {
                // 1049 при входе сохраняет курсор основного экрана: курсор ставится в его
                // сохранённое состояние, поля — по origin.
                place(&mut w, state, &state.primary, &saved, &full);
            } else if state.primary.saved != SavedCursor::default() {
                place(&mut w, state, &state.primary, &saved, &full);
                w.raw(b"\x1b7");
            }
            if state.margins != full && saved.origin {
                set_margins(&mut w, state, &full);
            }
            w.csi(&format!("?{entry}h"));
            w.forget_pen();
            for other in [47u16, 1047] {
                if other != entry && state.mode(&format!("?{other}")) {
                    w.csi(&format!("?{other}h"));
                }
            }
            w.csi("?6l");
            write_rows(&mut w, &state.alternate, state.cols, false);
            kitty(&mut w, &state.alternate.kitty);
        }
        Screen::Primary => {
            if !state.alternate.is_blank() {
                w.csi("?47h");
                w.forget_pen();
                write_rows(&mut w, &state.alternate, state.cols, false);
                kitty(&mut w, &state.alternate.kitty);
                if state.alternate.saved != SavedCursor::default() {
                    let saved = target_of_saved(&state.alternate.saved);
                    place(&mut w, state, &state.alternate, &saved, &full);
                    w.raw(b"\x1b7");
                    if state.margins != full && saved.origin {
                        set_margins(&mut w, state, &full);
                    }
                }
                w.csi("?6l");
                w.csi("?47l");
                w.forget_pen();
            }
            write_rows(&mut w, &state.primary, state.cols, true);
            kitty(&mut w, &state.primary.kitty);
        }
    }

    let active = match state.screen {
        Screen::Primary => &state.primary,
        Screen::Alternate => &state.alternate,
    };
    if state.tabstops != default_tabstops(state.cols) {
        w.csi("?6l");
        w.csi("3g");
        for x in &state.tabstops {
            w.csi(&format!("{}G", x + 1));
            w.raw(b"\x1bH");
        }
    }
    set_margins(&mut w, state, &state.margins);

    // Никогда не сохранённый курсор C API и пробы не отличают от сохранённого в начале с пером по
    // умолчанию, а reflow двигает только сохранённый: такой курсор не сохраняется.
    if active.saved != SavedCursor::default() || state.mode("?1048") {
        let saved = target_of_saved(&active.saved);
        place(&mut w, state, active, &saved, &state.margins);
        if state.mode("?1048") {
            w.csi("?1048h");
        } else {
            w.raw(b"\x1b7");
        }
    }
    let cursor = Target {
        x: active.cursor.x,
        y: active.cursor.y,
        pending_wrap: active.cursor.pending_wrap,
        pen: active.cursor.pen.clone(),
        origin: state.mode("?6"),
    };
    place(&mut w, state, active, &cursor, &state.margins);

    modes(&mut w, state, policy);
    w.raw(&state.continuation);
    w.out
}

/// Цвет в форме `rgb:rr/gg/bb` для OSC 4, 10, 11 и 12.
fn xcolor(rgb: Rgb) -> String {
    format!("rgb:{:02x}/{:02x}/{:02x}", rgb.r, rgb.g, rgb.b)
}

/// Писатель байтов с памятью о выданном пере: SGR, OSC 8 и DECSCA выдаются при смене.
#[derive(Default)]
struct Writer {
    out: Vec<u8>,
    /// Выданное перо; `None` — неизвестно (начало или переключение экрана).
    pen: Option<Pen>,
}

impl Writer {
    fn raw(&mut self, bytes: &[u8]) {
        self.out.extend_from_slice(bytes);
    }

    fn csi(&mut self, body: &str) {
        self.out.extend_from_slice(b"\x1b[");
        self.out.extend_from_slice(body.as_bytes());
    }

    fn osc(&mut self, body: &str) {
        self.out.extend_from_slice(b"\x1b]");
        self.out.extend_from_slice(body.as_bytes());
        self.out.extend_from_slice(b"\x1b\\");
    }

    fn forget_pen(&mut self) {
        self.pen = None;
    }

    fn pen(&mut self, style: &Style, hyperlink: Option<&str>, protected: bool) {
        let known = self.pen.clone();
        if known.as_ref().is_none_or(|pen| pen.style != *style) {
            self.csi(&format!("{}m", sgr(style)));
        }
        if known
            .as_ref()
            .is_none_or(|pen| pen.hyperlink.as_deref() != hyperlink)
        {
            self.osc(&format!("8;;{}", hyperlink.unwrap_or("")));
        }
        if known.as_ref().is_none_or(|pen| pen.protected != protected) {
            self.csi(if protected { "1\"q" } else { "0\"q" });
        }
        self.pen = Some(Pen {
            style: *style,
            hyperlink: hyperlink.map(str::to_owned),
            protected,
        });
    }
}

/// Параметры SGR стиля от сброса: `0;…`.
pub fn sgr(style: &Style) -> String {
    let mut params = vec!["0".to_owned()];
    let flags = [
        (style.bold, "1"),
        (style.faint, "2"),
        (style.italic, "3"),
        (style.blink, "5"),
        (style.inverse, "7"),
        (style.invisible, "8"),
        (style.strikethrough, "9"),
        (style.overline, "53"),
    ];
    for (on, code) in flags {
        if on {
            params.push(code.to_owned());
        }
    }
    if style.underline > 0 {
        params.push(format!("4:{}", style.underline));
    }
    for (base, color) in [(38, style.fg), (48, style.bg), (58, style.underline_color)] {
        match color {
            Color::Default => {}
            Color::Palette(index) => params.push(format!("{base};5;{index}")),
            Color::Rgb(rgb) => params.push(format!("{base};2;{};{};{}", rgb.r, rgb.g, rgb.b)),
        }
    }
    params.join(";")
}

/// Стиль пустой клетки с фоном: только фон.
fn bg_style(color: Color) -> Style {
    Style {
        bg: color,
        ..Style::default()
    }
}

/// Колонка курсора на текущей строке; `cols` — курсор ждёт переноса после последней колонки.
struct RowEnd {
    x: u16,
}

/// Первая клетка строки, которую надо выдать: напечатанная или пустая с фоном.
fn first_output(row: &Row) -> Option<usize> {
    row.cells.iter().position(|cell| {
        !matches!(cell.width, Width::SpacerTail | Width::SpacerHead)
            && (!cell.text.is_empty() || cell.bg_only.is_some())
    })
}

/// Перенос с предыдущей строки сработает печатью первой клетки: курсор ждёт переноса после
/// последней колонки или стоит на последней колонке перед широким символом (spacer head), а
/// следующая строка начинается напечатанной клеткой в колонке 0.
fn wrap_works(previous: &Row, end: &RowEnd, next: &Row, cols: u16) -> bool {
    let Some(first) = first_output(next) else {
        return false;
    };
    let starts = first == 0 && !next.cells[0].text.is_empty();
    let waits = end.x == cols
        || (end.x + 1 == cols
            && previous
                .cells
                .last()
                .is_some_and(|cell| cell.width == Width::SpacerHead)
            && next.cells[0].width == Width::Wide);
    previous.wrap && starts && waits
}

/// Содержимое экрана с позиции (0, 0) при полных полях и выключенном DECOM. `scroll` — основной
/// экран: история и активная область подряд, жёсткий конец строки — CR LF, так история попадает в
/// историю; иначе (alternate) каждая строка с CUP.
fn write_rows(w: &mut Writer, screen: &ScreenState, cols: u16, scroll: bool) {
    w.csi("H");
    let rows: Vec<&Row> = screen.history.iter().chain(&screen.active).collect();
    let mut end = RowEnd { x: 0 };
    for (index, row) in rows.iter().enumerate() {
        if index > 0 {
            let previous = rows[index - 1];
            if !wrap_works(previous, &end, row, cols) {
                if scroll {
                    w.raw(b"\r\n");
                } else {
                    if row.is_blank() {
                        end = RowEnd { x: 0 };
                        continue;
                    }
                    w.csi(&format!("{}H", index + 1));
                }
                end = RowEnd { x: 0 };
            } else {
                end = RowEnd { x: 0 };
                // Курсор физически ждёт переноса: первая клетка печатается без перемещения.
                write_cells(w, row, &mut end, true);
                continue;
            }
        }
        write_cells(w, row, &mut end, false);
    }
}

fn write_cells(w: &mut Writer, row: &Row, end: &mut RowEnd, mut wrapping: bool) {
    for (c, cell) in row.cells.iter().enumerate() {
        if matches!(cell.width, Width::SpacerTail | Width::SpacerHead) {
            continue;
        }
        let c = u16::try_from(c).unwrap_or(u16::MAX);
        if cell.text.is_empty() {
            if let Some(color) = cell.bg_only {
                move_to(w, end, c);
                w.pen(&bg_style(color), None, false);
                w.csi("X");
            }
            continue;
        }
        if wrapping {
            wrapping = false;
        } else {
            move_to(w, end, c);
        }
        w.pen(&cell.style, cell.hyperlink.as_deref(), cell.protected);
        w.raw(cell.text.as_bytes());
        end.x = c + if cell.width == Width::Wide { 2 } else { 1 };
    }
}

fn move_to(w: &mut Writer, end: &mut RowEnd, c: u16) {
    if c > end.x {
        w.csi(&format!("{}C", c - end.x));
    } else if c < end.x {
        w.csi(&format!("{}G", c + 1));
    }
    end.x = c;
}

/// Кольцо Kitty: `CSI = r7 ; 1 u` при полном кольце, затем push от дальнего значения к текущему.
fn kitty(w: &mut Writer, ring: &[u8]) {
    let used = ring
        .iter()
        .rposition(|flags| *flags != 0)
        .map_or(0, |at| at + 1);
    if used == 0 {
        return;
    }
    let pushes = if used >= crate::oracle::model::KITTY_STACK_DEPTH {
        let deepest = crate::oracle::model::KITTY_STACK_DEPTH - 1;
        w.csi(&format!("={};1u", ring[deepest]));
        deepest
    } else {
        used
    };
    for flags in ring[..pushes].iter().rev() {
        w.csi(&format!(">{flags}u"));
    }
}

/// Поля прокрутки: DECSTBM всегда (сброс к полному экрану — `CSI r`), DECSLRM — после режима 69.
fn set_margins(w: &mut Writer, state: &State, margins: &Margins) {
    let full = Margins::full(state.cols, state.rows);
    if margins.top == full.top && margins.bottom == full.bottom {
        w.csi("r");
    } else {
        w.csi(&format!("{};{}r", margins.top + 1, margins.bottom + 1));
    }
    if state.mode("?69") {
        w.csi("?69h");
        if margins.left == full.left && margins.right == full.right {
            w.csi("s");
        } else {
            w.csi(&format!("{};{}s", margins.left + 1, margins.right + 1));
        }
    }
}

/// Куда поставить курсор: позиция, отложенный перенос, перо и режим origin.
struct Target {
    x: u16,
    y: u16,
    pending_wrap: bool,
    pen: Pen,
    origin: bool,
}

fn target_of_saved(saved: &SavedCursor) -> Target {
    Target {
        x: saved.x,
        y: saved.y,
        pending_wrap: saved.pending_wrap,
        pen: Pen {
            style: saved.style,
            hyperlink: None,
            protected: saved.protected,
        },
        origin: saved.origin,
    }
}

/// Курсор в состояние `target` на активном сейчас экране `screen`. При origin поля должны быть уже
/// заданы (`margins`): CUP считается от них. Отложенный перенос — повторной печатью клетки под
/// курсором с её собственным пером, затем перо курсора.
fn place(w: &mut Writer, state: &State, screen: &ScreenState, target: &Target, margins: &Margins) {
    if target.origin {
        set_margins(w, state, margins);
        w.csi("?6h");
    } else {
        w.csi("?6l");
    }
    let row = screen.active.get(usize::from(target.y));
    let cell_at = |x: u16| row.and_then(|row| row.cells.get(usize::from(x)));
    let print_x = match cell_at(target.x) {
        Some(cell) if target.pending_wrap && cell.width == Width::SpacerTail => {
            target.x.saturating_sub(1)
        }
        _ => target.x,
    };
    let (top, left) = if target.origin {
        (margins.top, margins.left)
    } else {
        (0, 0)
    };
    let x = if target.pending_wrap {
        print_x
    } else {
        target.x
    };
    w.csi(&format!(
        "{};{}H",
        target.y.saturating_sub(top) + 1,
        x.saturating_sub(left) + 1
    ));
    if target.pending_wrap
        && let Some(cell) = cell_at(print_x)
        && !cell.text.is_empty()
    {
        w.pen(&cell.style, cell.hyperlink.as_deref(), cell.protected);
        w.raw(cell.text.as_bytes());
    }
    w.pen(
        &target.pen.style,
        target.pen.hyperlink.as_deref(),
        target.pen.protected,
    );
}

/// Режимы после курсора: сбросы раньше включений (сброс любого режима мыши выключает отслеживание,
/// сброс формата возвращает X10), текущий режим и формат мыши — последними из своих групп, 2026 —
/// последним из всех. Режимы отчётов — по политике.
fn modes(w: &mut Writer, state: &State, policy: ReportPolicy) {
    let mut resets = Vec::new();
    let mut sets = Vec::new();
    let mut trailing = Vec::new();
    let mut ansi_resets = Vec::new();
    let mut ansi_sets = Vec::new();
    for mode in TRACKED_MODES {
        if special_mode(mode.value, mode.ansi) || (!mode.ansi && is_report_mode(mode.value)) {
            continue;
        }
        let on = state.mode(&mode.key());
        match (mode.ansi, on) {
            (true, true) => ansi_sets.push(mode.value),
            (true, false) => ansi_resets.push(mode.value),
            (false, true) => sets.push(mode.value),
            (false, false) => resets.push(mode.value),
        }
    }
    for (group, current) in [
        (MouseEvent::MODES, state.mouse_event.mode()),
        (MouseFormat::MODES, state.mouse_format.mode()),
    ] {
        let on = |mode: &u16| state.mode(&format!("?{mode}"));
        resets.extend(group.iter().filter(|mode| !on(mode)));
        sets.extend(
            group
                .iter()
                .filter(|mode| on(mode) && Some(**mode) != current),
        );
        match current {
            Some(mode) => sets.push(mode),
            // Флаги включены, но группа выключена: её выключает последний сброс.
            None if group.iter().any(on) => {
                trailing.extend(group.iter().find(|mode| !on(mode)));
            }
            None => {}
        }
    }
    let joined = |values: &[u16]| {
        values
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(";")
    };
    if !resets.is_empty() {
        w.csi(&format!("?{}l", joined(&resets)));
    }
    if !ansi_resets.is_empty() {
        w.csi(&format!("{}l", joined(&ansi_resets)));
    }
    if !sets.is_empty() {
        w.csi(&format!("?{}h", joined(&sets)));
    }
    if !trailing.is_empty() {
        w.csi(&format!("?{}l", joined(&trailing)));
    }
    if !ansi_sets.is_empty() {
        w.csi(&format!("{}h", joined(&ansi_sets)));
    }
    if policy == ReportPolicy::Passthrough {
        let (on, off): (Vec<u16>, Vec<u16>) = REPORT_MODES
            .iter()
            .partition(|mode| state.mode(&format!("?{mode}")));
        if !off.is_empty() {
            w.csi(&format!("?{}l", joined(&off)));
        }
        if !on.is_empty() {
            w.csi(&format!("?{}h", joined(&on)));
        }
    }
    if state.mode("?2026") {
        w.csi("?2026h");
    }
}

// --- фильтр режимов отчётов ---

/// Состояние разбора фильтра.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Parse {
    Ground,
    Escape,
    /// CSI: байты держатся до финального.
    Csi,
    /// OSC, DCS, SOS, PM, APC: байты идут насквозь до ST, BEL (у OSC) или CAN/SUB.
    Text,
    /// ESC внутри строки: `ESC \` закрывает её, другой байт начинает новую последовательность.
    TextEscape,
}

/// Потоковый фильтр хост→клиент: вырезает set, reset, XTSAVE и XTRESTORE режимов
/// [`REPORT_MODES`] из CSI, сохраняя остальные режимы той же последовательности, и ведёт их
/// каноническое значение. Ответы клиента на обратном потоке он не трогает (ADR-0005).
#[derive(Clone, Debug)]
pub struct ReportFilter {
    parse: Parse,
    /// Начатая последовательность ESC или CSI, которую фильтр держит до решения.
    held: Vec<u8>,
    canonical: BTreeMap<u16, bool>,
    saved: BTreeMap<u16, bool>,
    /// Сколько режимов вырезано.
    removed: usize,
}

impl Default for ReportFilter {
    fn default() -> ReportFilter {
        ReportFilter::new()
    }
}

/// Предел удерживаемой CSI: длиннее — передаётся как есть.
const MAX_HELD: usize = 256;

impl ReportFilter {
    pub fn new() -> ReportFilter {
        ReportFilter {
            parse: Parse::Ground,
            held: Vec::new(),
            canonical: REPORT_MODES.iter().map(|mode| (*mode, false)).collect(),
            saved: BTreeMap::new(),
            removed: 0,
        }
    }

    /// Фильтр с каноническими значениями, которые хост уже ведёт (например из восстановленного
    /// состояния).
    pub fn with_canonical(modes: &BTreeMap<String, bool>) -> ReportFilter {
        let mut filter = ReportFilter::new();
        for mode in REPORT_MODES {
            let on = modes.get(&format!("?{mode}")).copied().unwrap_or(false);
            filter.canonical.insert(mode, on);
        }
        filter
    }

    /// Каноническое значение режима отчётов у хоста.
    pub fn canonical(&self, mode: u16) -> Option<bool> {
        self.canonical.get(&mode).copied()
    }

    /// Сколько режимов отчётов вырезано из потока.
    pub fn removed(&self) -> usize {
        self.removed
    }

    /// Удерживаемая начатая последовательность (continuation фильтра).
    pub fn held(&self) -> &[u8] {
        &self.held
    }

    /// Байты потока хост→клиент; возвращает, что передать клиенту сейчас.
    pub fn push(&mut self, input: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(input.len());
        for &byte in input {
            self.byte(byte, &mut out);
        }
        out
    }

    fn byte(&mut self, byte: u8, out: &mut Vec<u8>) {
        const ESC: u8 = 0x1b;
        const CAN: u8 = 0x18;
        const SUB: u8 = 0x1a;
        const BEL: u8 = 0x07;
        match self.parse {
            Parse::Ground => {
                if byte == ESC {
                    self.held.push(byte);
                    self.parse = Parse::Escape;
                } else {
                    out.push(byte);
                }
            }
            Parse::Escape => match byte {
                b'[' => {
                    self.held.push(byte);
                    self.parse = Parse::Csi;
                }
                b']' | b'P' | b'X' | b'^' | b'_' => {
                    self.flush(out);
                    out.push(byte);
                    self.parse = Parse::Text;
                }
                CAN | SUB => {
                    self.flush(out);
                    out.push(byte);
                    self.parse = Parse::Ground;
                }
                ESC => {
                    self.flush(out);
                    self.held.push(byte);
                }
                b'c' => {
                    self.flush(out);
                    out.push(byte);
                    self.reset();
                    self.parse = Parse::Ground;
                }
                // Промежуточный байт ESC (например `ESC ( B`): решать нечего, остаток идёт насквозь.
                _ => {
                    self.flush(out);
                    out.push(byte);
                    self.parse = Parse::Ground;
                }
            },
            Parse::Csi => match byte {
                CAN | SUB => {
                    self.flush(out);
                    out.push(byte);
                    self.parse = Parse::Ground;
                }
                ESC => {
                    self.flush(out);
                    self.held.push(byte);
                    self.parse = Parse::Escape;
                }
                0x40..=0x7e => {
                    self.held.push(byte);
                    let held = std::mem::take(&mut self.held);
                    self.csi(&held, out);
                    self.parse = Parse::Ground;
                }
                _ => {
                    self.held.push(byte);
                    if self.held.len() > MAX_HELD {
                        self.flush(out);
                        self.parse = Parse::Ground;
                    }
                }
            },
            Parse::Text => match byte {
                ESC => {
                    self.held.push(byte);
                    self.parse = Parse::TextEscape;
                }
                BEL | CAN | SUB => {
                    out.push(byte);
                    self.parse = Parse::Ground;
                }
                _ => out.push(byte),
            },
            Parse::TextEscape => {
                if byte == b'\\' {
                    self.flush(out);
                    out.push(byte);
                    self.parse = Parse::Ground;
                } else {
                    // ESC закрыл строку и начал новую последовательность: он уже в held.
                    self.parse = Parse::Escape;
                    self.byte(byte, out);
                }
            }
        }
    }

    fn flush(&mut self, out: &mut Vec<u8>) {
        out.append(&mut self.held);
    }

    /// RIS: режимы отчётов выключены, сохранённых нет.
    fn reset(&mut self) {
        for value in self.canonical.values_mut() {
            *value = false;
        }
        self.saved.clear();
    }

    /// Полная CSI `ESC [ … final`. Управляющие C0 внутри неё передаются раньше переписанной CSI.
    fn csi(&mut self, held: &[u8], out: &mut Vec<u8>) {
        let body = &held[2..held.len() - 1];
        let Some(&final_byte) = held.last() else {
            return;
        };
        let controls: Vec<u8> = body.iter().copied().filter(|b| *b < 0x20).collect();
        let text: Vec<u8> = body.iter().copied().filter(|b| *b >= 0x20).collect();
        let rewritten = match (text.first(), final_byte) {
            (Some(b'?'), b'h' | b'l' | b's' | b'r') => self.modes(&text[1..], final_byte),
            _ => None,
        };
        match rewritten {
            None => out.extend_from_slice(held),
            Some(kept) => {
                out.extend_from_slice(&controls);
                if !kept.is_empty() {
                    out.extend_from_slice(b"\x1b[?");
                    out.extend_from_slice(kept.as_bytes());
                    out.push(final_byte);
                }
            }
        }
    }

    /// Параметры частной CSI режимов. `None` — переписывать нечего (нет режимов отчётов или
    /// разбор не уверен); иначе — оставшиеся параметры.
    fn modes(&mut self, params: &[u8], final_byte: u8) -> Option<String> {
        if params.is_empty() || !params.iter().all(|b| b.is_ascii_digit() || *b == b';') {
            return None;
        }
        let text = std::str::from_utf8(params).ok()?;
        let values: Vec<&str> = text.split(';').collect();
        let numbers: Vec<Option<u16>> = values.iter().map(|value| value.parse().ok()).collect();
        if !numbers
            .iter()
            .any(|value| value.is_some_and(is_report_mode))
        {
            return None;
        }
        let mut kept = Vec::new();
        for (raw, number) in values.iter().zip(&numbers) {
            match number {
                Some(mode) if is_report_mode(*mode) => {
                    self.removed += 1;
                    match final_byte {
                        b'h' => {
                            self.canonical.insert(*mode, true);
                        }
                        b'l' => {
                            self.canonical.insert(*mode, false);
                        }
                        b's' => {
                            let value = self.canonical.get(mode).copied().unwrap_or(false);
                            self.saved.insert(*mode, value);
                        }
                        _ => {
                            if let Some(value) = self.saved.get(mode).copied() {
                                self.canonical.insert(*mode, value);
                            }
                        }
                    }
                }
                _ => kept.push(*raw),
            }
        }
        Some(kept.join(";"))
    }
}

#[cfg(test)]
mod tests;
