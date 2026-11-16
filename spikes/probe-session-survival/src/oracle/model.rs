//! Состояние терминала для оракулов восстановления, сравнение эталона с приёмником и профиль
//! нормализаций (раздел плана «Оракулы»). Чистый модуль без FFI: состояние снимает `oracle::vt`.
//!
//! Профиль версии 1 заморожен до первого прогона матрицы. Его правка — новая версия с перепрогоном
//! и записью в отчёте; тест `profile_v1_is_frozen` держит описание профиля дословно.

use std::collections::BTreeMap;
use std::fmt::{self, Debug};

use serde::{Deserialize, Serialize};

pub const PROFILE_VERSION: u32 = 1;

/// Сколько расхождений и нормализаций сравнение хранит поимённо; остальные только считает.
pub const MAX_LISTED: usize = 100;

/// Глубина стека режимов клавиатуры Kitty у ghostty: кольцо из восьми значений на экран.
pub const KITTY_STACK_DEPTH: usize = 8;

/// Режимы, которые снимает оракул: все режимы `include/ghostty/vt/modes.h` ревизии поверхности.
/// Ключ в [`State::modes`] — номер, у частных режимов DEC с префиксом `?`.
pub const TRACKED_MODES: [Mode; 43] = [
    Mode::ansi(2),
    Mode::ansi(4),
    Mode::ansi(12),
    Mode::ansi(20),
    Mode::dec(1),
    Mode::dec(3),
    Mode::dec(4),
    Mode::dec(5),
    Mode::dec(6),
    Mode::dec(7),
    Mode::dec(8),
    Mode::dec(9),
    Mode::dec(12),
    Mode::dec(25),
    Mode::dec(40),
    Mode::dec(45),
    Mode::dec(47),
    Mode::dec(66),
    Mode::dec(67),
    Mode::dec(69),
    Mode::dec(1000),
    Mode::dec(1002),
    Mode::dec(1003),
    Mode::dec(1004),
    Mode::dec(1005),
    Mode::dec(1006),
    Mode::dec(1007),
    Mode::dec(1015),
    Mode::dec(1016),
    Mode::dec(1035),
    Mode::dec(1036),
    Mode::dec(1039),
    Mode::dec(1045),
    Mode::dec(1047),
    Mode::dec(1048),
    Mode::dec(1049),
    Mode::dec(2004),
    Mode::dec(2026),
    Mode::dec(2027),
    Mode::dec(2031),
    Mode::dec(2033),
    Mode::dec(2048),
    Mode::dec(5522),
];

/// События ввода, кодирование которых оракул снимает кодировщиками ghostty по режимам терминала:
/// так режимы клавиатуры, мыши и Kitty сравниваются по тому, что получит нагрузка.
pub const INPUT_PROBES: [&str; 12] = [
    "key:a",
    "key:ctrl-a",
    "key:enter",
    "key:tab",
    "key:backspace",
    "key:escape",
    "key:up",
    "key:shift-up",
    "key:numpad-1",
    "mouse:left-press",
    "mouse:left-release",
    "mouse:motion",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Mode {
    pub value: u16,
    pub ansi: bool,
}

impl Mode {
    pub const fn ansi(value: u16) -> Mode {
        Mode { value, ansi: true }
    }

    pub const fn dec(value: u16) -> Mode {
        Mode { value, ansi: false }
    }

    pub fn key(self) -> String {
        if self.ansi {
            self.value.to_string()
        } else {
            format!("?{}", self.value)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Color {
    Default,
    Palette(u8),
    Rgb(Rgb),
}

/// SGR-стиль клетки или пера курсора. `underline` — значение `GHOSTTY_SGR_UNDERLINE_*`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub underline_color: Color,
    pub bold: bool,
    pub italic: bool,
    pub faint: bool,
    pub blink: bool,
    pub inverse: bool,
    pub invisible: bool,
    pub strikethrough: bool,
    pub overline: bool,
    pub underline: i32,
}

impl Default for Style {
    fn default() -> Style {
        Style {
            fg: Color::Default,
            bg: Color::Default,
            underline_color: Color::Default,
            bold: false,
            italic: false,
            faint: false,
            blink: false,
            inverse: false,
            invisible: false,
            strikethrough: false,
            overline: false,
            underline: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Width {
    Narrow,
    Wide,
    SpacerTail,
    SpacerHead,
}

/// Клетка: кластер графем (пусто у никогда не напечатанной клетки, что отличает её от пробела),
/// ширина, стиль, URI гиперссылки и защита DECSCA.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cell {
    pub text: String,
    pub width: Width,
    pub style: Style,
    pub hyperlink: Option<String>,
    pub protected: bool,
}

impl Cell {
    pub fn blank() -> Cell {
        Cell {
            text: String::new(),
            width: Width::Narrow,
            style: Style::default(),
            hyperlink: None,
            protected: false,
        }
    }
}

/// Строка: клетки и признаки мягкого переноса на следующую строку и с предыдущей.
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

    /// Текст строки: пустые клетки — пробелами, хвосты широких символов пропускаются.
    pub fn text(&self) -> String {
        let mut text = String::new();
        for cell in &self.cells {
            match cell.width {
                Width::SpacerTail | Width::SpacerHead => {}
                Width::Narrow | Width::Wide if cell.text.is_empty() => text.push(' '),
                Width::Narrow | Width::Wide => text.push_str(&cell.text),
            }
        }
        text.trim_end().to_owned()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub x: u16,
    pub y: u16,
    pub pending_wrap: bool,
    pub visible: bool,
    pub style: Style,
}

/// Сохранённый курсор (`DECSC`), каким его восстанавливает `DECRC`: позиция, отложенный перенос,
/// перо и режим origin.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedCursor {
    pub x: u16,
    pub y: u16,
    pub pending_wrap: bool,
    pub style: Style,
    pub origin: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Screen {
    Primary,
    Alternate,
}

/// Цвета терминала: текущие динамические (`None` — не заданы) и палитра.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Colors {
    pub foreground: Option<Rgb>,
    pub background: Option<Rgb>,
    pub cursor: Option<Rgb>,
    pub palette: Vec<Rgb>,
}

/// Значение, которое стенд наблюдал, или причина, по которой не наблюдал. Ненаблюдённое не равно
/// никакому значению: сравнение записывает его отдельно, и клетка не проходит отсутствием.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", content = "value", rename_all = "kebab-case")]
pub enum Observed<T> {
    Value(T),
    NotObserved(String),
}

/// Состояние одного терминала-оракула.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub cols: u16,
    pub rows: u16,
    pub screen: Screen,
    /// Активная область сверху вниз.
    pub active: Vec<Row>,
    /// История активного экрана над активной областью, ближайшая к ней строка первой. Снимается
    /// не глубже заданного; `history_total` — сколько строк истории всего.
    pub history: Vec<Row>,
    pub history_total: usize,
    pub cursor: Cursor,
    /// Режимы [`TRACKED_MODES`] по ключу [`Mode::key`].
    pub modes: BTreeMap<String, bool>,
    /// Текущие флаги клавиатуры Kitty активного экрана.
    pub kitty_flags: u8,
    pub colors: Colors,
    pub title: String,
    pub pwd: String,
    /// Кодирование [`INPUT_PROBES`] в hex.
    pub input: BTreeMap<String, String>,
    /// Разборщик на ground: ни одной начатой последовательности.
    pub ground: bool,
    /// Скрытое состояние, которого C API не отдаёт; снимается повтором ввода на копии.
    pub saved_cursor: Observed<SavedCursor>,
    /// Стек Kitty активного экрана от вершины, без нулей в хвосте.
    pub kitty_stack: Observed<Vec<u8>>,
}

impl State {
    /// Пустой экран после сброса: заготовка для тестов и сравнения.
    pub fn blank(cols: u16, rows: u16) -> State {
        State {
            cols,
            rows,
            screen: Screen::Primary,
            active: (0..rows).map(|_| Row::blank(cols)).collect(),
            history: Vec::new(),
            history_total: 0,
            cursor: Cursor {
                x: 0,
                y: 0,
                pending_wrap: false,
                visible: true,
                style: Style::default(),
            },
            modes: TRACKED_MODES
                .iter()
                .map(|mode| (mode.key(), false))
                .collect(),
            kitty_flags: 0,
            colors: Colors {
                foreground: None,
                background: None,
                cursor: None,
                palette: vec![Rgb { r: 0, g: 0, b: 0 }; 256],
            },
            title: String::new(),
            pwd: String::new(),
            input: BTreeMap::new(),
            ground: true,
            saved_cursor: Observed::Value(SavedCursor {
                x: 0,
                y: 0,
                pending_wrap: false,
                style: Style::default(),
                origin: false,
            }),
            kitty_stack: Observed::Value(Vec::new()),
        }
    }

    /// Текст активной области по строкам.
    pub fn text(&self) -> Vec<String> {
        self.active.iter().map(Row::text).collect()
    }
}

// --- профиль ---

/// Правило, по которому два разных значения считаются равными.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Rule {
    /// Цвет по индексу палитры равен RGB того же цвета в палитре терминала, который хранит индекс.
    PaletteAsRgb,
}

impl Rule {
    pub fn name(self) -> &'static str {
        match self {
            Rule::PaletteAsRgb => "palette-as-rgb",
        }
    }
}

/// Как оракул проверяет часть состояния терминала.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Coverage {
    /// Снимается C API и сравнивается в [`State`].
    Direct,
    /// C API не отдаёт; снимается повтором журнала ввода на копии терминала.
    Replay,
    /// Не снимается; видно только по суффиксу (`oracle::suffix`).
    Suffix,
    /// Объявлено вне профиля: не сравнивается.
    Excluded,
}

impl Coverage {
    pub fn name(self) -> &'static str {
        match self {
            Coverage::Direct => "direct",
            Coverage::Replay => "replay",
            Coverage::Suffix => "suffix",
            Coverage::Excluded => "excluded",
        }
    }
}

pub struct Profile {
    pub version: u32,
    pub rules: &'static [Rule],
    /// Часть состояния, способ проверки и пояснение.
    pub coverage: &'static [(&'static str, Coverage, &'static str)],
}

/// Профиль нормализаций версии 1. Пустая клетка и напечатанный пробел, отложенный перенос,
/// признаки мягкого переноса и URI гиперссылки не нормализуются: их проверяют B1 и B6.
pub const PROFILE: Profile = Profile {
    version: PROFILE_VERSION,
    rules: &[Rule::PaletteAsRgb],
    coverage: &[
        (
            "cells",
            Coverage::Direct,
            "текст, ширина, стиль, URI гиперссылки и защита каждой клетки",
        ),
        (
            "rows",
            Coverage::Direct,
            "признаки мягкого переноса строки активной области и истории",
        ),
        (
            "history",
            Coverage::Direct,
            "история активного экрана, выровненная от активной области, и её длина",
        ),
        (
            "cursor",
            Coverage::Direct,
            "позиция, отложенный перенос, видимость и перо",
        ),
        ("screen", Coverage::Direct, "активный экран"),
        ("modes", Coverage::Direct, "режимы TRACKED_MODES"),
        ("kitty-flags", Coverage::Direct, "текущие флаги Kitty"),
        (
            "colors",
            Coverage::Direct,
            "текущие основной цвет, фон, цвет курсора и палитра",
        ),
        ("title", Coverage::Direct, "заголовок"),
        ("pwd", Coverage::Direct, "рабочий каталог OSC 7"),
        (
            "input",
            Coverage::Direct,
            "кодирование INPUT_PROBES кодировщиками ghostty",
        ),
        (
            "parser",
            Coverage::Direct,
            "разборщик на ground или внутри последовательности",
        ),
        (
            "saved-cursor",
            Coverage::Replay,
            "DECRC на копии: позиция, перенос, перо, origin",
        ),
        (
            "kitty-stack",
            Coverage::Replay,
            "снятие вершин стека Kitty на копии",
        ),
        (
            "inactive-screen",
            Coverage::Suffix,
            "содержимое неактивного экрана: выход из alternate",
        ),
        ("tabstops", Coverage::Suffix, "табуляция после attach"),
        (
            "scroll-margins",
            Coverage::Suffix,
            "DECSTBM и DECSLRM: прокрутка в полях",
        ),
        (
            "partial-sequence",
            Coverage::Suffix,
            "незавершённая последовательность: остаток после attach",
        ),
        (
            "reflow",
            Coverage::Suffix,
            "перенос строк при ресайзе после attach",
        ),
        (
            "hyperlink-id",
            Coverage::Excluded,
            "C API клетки отдаёт только URI",
        ),
        (
            "cursor-shape",
            Coverage::Excluded,
            "форма курсора DECSCUSR не читается из терминала C API",
        ),
        ("mouse-shape", Coverage::Excluded, "форма указателя OSC 22"),
        ("selection", Coverage::Excluded, "выделение — состояние GUI"),
        (
            "viewport",
            Coverage::Excluded,
            "положение прокрутки области просмотра — состояние клиента",
        ),
        (
            "kitty-graphics",
            Coverage::Excluded,
            "изображения Kitty вне возможностей пробы",
        ),
        (
            "charsets",
            Coverage::Excluded,
            "наборы символов G0–G3 не проверяются ни одной клеткой матрицы",
        ),
    ],
};

impl Profile {
    /// Описание профиля одной строкой на правило и часть состояния, без пояснений.
    pub fn describe(&self) -> String {
        let mut out = format!("profile {}\n", self.version);
        for rule in self.rules {
            out.push_str(&format!("rule {}\n", rule.name()));
        }
        for (part, coverage, _) in self.coverage {
            out.push_str(&format!("{} {part}\n", coverage.name()));
        }
        let modes: Vec<String> = TRACKED_MODES.iter().map(|mode| mode.key()).collect();
        out.push_str(&format!("modes {}\n", modes.join(",")));
        out.push_str(&format!("input {}\n", INPUT_PROBES.join(",")));
        out
    }
}

// --- сравнение ---

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Difference {
    pub path: String,
    pub reference: String,
    pub receiver: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Normalized {
    pub path: String,
    pub rule: Rule,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unobserved {
    pub path: String,
    pub reference: Option<String>,
    pub receiver: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    /// Всё снятое равно с учётом профиля, ничего не осталось ненаблюдённым.
    Equal,
    /// Есть расхождение вне профиля.
    Differs,
    /// Расхождений нет, но часть состояния не наблюдалась.
    Incomplete,
}

/// Итог сравнения эталона с приёмником: расхождения вне профиля, нормализованные отличия и
/// ненаблюдённые части. Первые [`MAX_LISTED`] записей каждого вида перечислены, остальные
/// посчитаны.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comparison {
    pub profile: u32,
    pub mismatches: Vec<Difference>,
    pub mismatch_count: usize,
    pub normalized: Vec<Normalized>,
    pub normalized_count: usize,
    pub unobserved: Vec<Unobserved>,
}

impl Comparison {
    pub fn outcome(&self) -> Outcome {
        if self.mismatch_count > 0 {
            Outcome::Differs
        } else if !self.unobserved.is_empty() {
            Outcome::Incomplete
        } else {
            Outcome::Equal
        }
    }

    fn mismatch(
        &mut self,
        path: impl FnOnce() -> String,
        reference: &dyn Debug,
        receiver: &dyn Debug,
    ) {
        self.mismatch_count += 1;
        if self.mismatches.len() < MAX_LISTED {
            self.mismatches.push(Difference {
                path: path(),
                reference: format!("{reference:?}"),
                receiver: format!("{receiver:?}"),
            });
        }
    }

    fn normalize(&mut self, path: impl FnOnce() -> String, rule: Rule) {
        self.normalized_count += 1;
        if self.normalized.len() < MAX_LISTED {
            self.normalized.push(Normalized { path: path(), rule });
        }
    }

    fn field<T: PartialEq + Debug>(&mut self, path: &str, reference: &T, receiver: &T) {
        if reference != receiver {
            self.mismatch(|| path.to_owned(), reference, receiver);
        }
    }
}

impl fmt::Display for Comparison {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "профиль {}: {:?}, расхождений {}, нормализовано {}, не наблюдалось {}",
            self.profile,
            self.outcome(),
            self.mismatch_count,
            self.normalized_count,
            self.unobserved.len()
        )?;
        for diff in &self.mismatches {
            write!(
                f,
                "\n  {}: эталон {}, приёмник {}",
                diff.path, diff.reference, diff.receiver
            )?;
        }
        Ok(())
    }
}

/// Сравнение эталона с приёмником по профилю [`PROFILE`].
pub fn compare(reference: &State, receiver: &State) -> Comparison {
    let mut out = Comparison {
        profile: PROFILE.version,
        ..Comparison::default()
    };
    let palettes = Palettes {
        reference: &reference.colors.palette,
        receiver: &receiver.colors.palette,
    };
    out.field("cols", &reference.cols, &receiver.cols);
    out.field("rows", &reference.rows, &receiver.rows);
    out.field("screen", &reference.screen, &receiver.screen);
    compare_rows(
        &mut out,
        &palettes,
        "active",
        &reference.active,
        &receiver.active,
        0,
    );
    out.field(
        "history_total",
        &reference.history_total,
        &receiver.history_total,
    );
    compare_rows(
        &mut out,
        &palettes,
        "history",
        &reference.history,
        &receiver.history,
        1,
    );

    let (a, b) = (&reference.cursor, &receiver.cursor);
    out.field("cursor.x", &a.x, &b.x);
    out.field("cursor.y", &a.y, &b.y);
    out.field("cursor.pending_wrap", &a.pending_wrap, &b.pending_wrap);
    out.field("cursor.visible", &a.visible, &b.visible);
    compare_style(&mut out, &palettes, "cursor.style", &a.style, &b.style);

    compare_maps(&mut out, "modes", &reference.modes, &receiver.modes);
    out.field("kitty_flags", &reference.kitty_flags, &receiver.kitty_flags);
    let (a, b) = (&reference.colors, &receiver.colors);
    out.field("colors.foreground", &a.foreground, &b.foreground);
    out.field("colors.background", &a.background, &b.background);
    out.field("colors.cursor", &a.cursor, &b.cursor);
    out.field("colors.palette.len", &a.palette.len(), &b.palette.len());
    for (index, (x, y)) in a.palette.iter().zip(&b.palette).enumerate() {
        if x != y {
            out.mismatch(|| format!("colors.palette[{index}]"), x, y);
        }
    }
    out.field("title", &reference.title, &receiver.title);
    out.field("pwd", &reference.pwd, &receiver.pwd);
    compare_maps(&mut out, "input", &reference.input, &receiver.input);
    out.field("ground", &reference.ground, &receiver.ground);

    match (&reference.saved_cursor, &receiver.saved_cursor) {
        (Observed::Value(a), Observed::Value(b)) => {
            out.field("saved_cursor.x", &a.x, &b.x);
            out.field("saved_cursor.y", &a.y, &b.y);
            out.field(
                "saved_cursor.pending_wrap",
                &a.pending_wrap,
                &b.pending_wrap,
            );
            out.field("saved_cursor.origin", &a.origin, &b.origin);
            compare_style(
                &mut out,
                &palettes,
                "saved_cursor.style",
                &a.style,
                &b.style,
            );
        }
        (a, b) => unobserved(&mut out, "saved_cursor", a, b),
    }
    match (&reference.kitty_stack, &receiver.kitty_stack) {
        (Observed::Value(a), Observed::Value(b)) => out.field("kitty_stack", a, b),
        (a, b) => unobserved(&mut out, "kitty_stack", a, b),
    }
    out
}

struct Palettes<'a> {
    reference: &'a [Rgb],
    receiver: &'a [Rgb],
}

fn unobserved<T>(
    out: &mut Comparison,
    path: &str,
    reference: &Observed<T>,
    receiver: &Observed<T>,
) {
    let reason = |side: &Observed<T>| match side {
        Observed::Value(_) => None,
        Observed::NotObserved(reason) => Some(reason.clone()),
    };
    out.unobserved.push(Unobserved {
        path: path.to_owned(),
        reference: reason(reference),
        receiver: reason(receiver),
    });
}

fn compare_maps<V: PartialEq + Debug>(
    out: &mut Comparison,
    prefix: &str,
    reference: &BTreeMap<String, V>,
    receiver: &BTreeMap<String, V>,
) {
    for (key, a) in reference {
        match receiver.get(key) {
            Some(b) if a == b => {}
            b => out.mismatch(|| format!("{prefix}.{key}"), &Some(a), &b),
        }
    }
    for (key, b) in receiver {
        if !reference.contains_key(key) {
            out.mismatch(|| format!("{prefix}.{key}"), &None::<&V>, &Some(b));
        }
    }
}

/// Строки по порядку с индексом от `first`: активная область — с нуля сверху, история — с единицы
/// от активной области вверх.
fn compare_rows(
    out: &mut Comparison,
    palettes: &Palettes<'_>,
    name: &str,
    reference: &[Row],
    receiver: &[Row],
    first: usize,
) {
    if reference.len() != receiver.len() {
        out.mismatch(|| format!("{name}.len"), &reference.len(), &receiver.len());
    }
    for (offset, (a, b)) in reference.iter().zip(receiver).enumerate() {
        let index = first + offset;
        if a.wrap != b.wrap {
            out.mismatch(|| format!("{name}[{index}].wrap"), &a.wrap, &b.wrap);
        }
        if a.wrap_continuation != b.wrap_continuation {
            out.mismatch(
                || format!("{name}[{index}].wrap_continuation"),
                &a.wrap_continuation,
                &b.wrap_continuation,
            );
        }
        if a.cells.len() != b.cells.len() {
            out.mismatch(
                || format!("{name}[{index}].len"),
                &a.cells.len(),
                &b.cells.len(),
            );
        }
        for (x, (c, d)) in a.cells.iter().zip(&b.cells).enumerate() {
            if c == d {
                continue;
            }
            let path = |field: &str| format!("{name}[{index}][{x}].{field}");
            if c.text != d.text {
                out.mismatch(|| path("text"), &c.text, &d.text);
            }
            if c.width != d.width {
                out.mismatch(|| path("width"), &c.width, &d.width);
            }
            if c.hyperlink != d.hyperlink {
                out.mismatch(|| path("hyperlink"), &c.hyperlink, &d.hyperlink);
            }
            if c.protected != d.protected {
                out.mismatch(|| path("protected"), &c.protected, &d.protected);
            }
            compare_style(out, palettes, &path("style"), &c.style, &d.style);
        }
    }
}

fn compare_style(out: &mut Comparison, palettes: &Palettes<'_>, path: &str, a: &Style, b: &Style) {
    if a == b {
        return;
    }
    compare_color(out, palettes, &format!("{path}.fg"), a.fg, b.fg);
    compare_color(out, palettes, &format!("{path}.bg"), a.bg, b.bg);
    compare_color(
        out,
        palettes,
        &format!("{path}.underline_color"),
        a.underline_color,
        b.underline_color,
    );
    let flags = [
        ("bold", a.bold, b.bold),
        ("italic", a.italic, b.italic),
        ("faint", a.faint, b.faint),
        ("blink", a.blink, b.blink),
        ("inverse", a.inverse, b.inverse),
        ("invisible", a.invisible, b.invisible),
        ("strikethrough", a.strikethrough, b.strikethrough),
        ("overline", a.overline, b.overline),
    ];
    for (name, x, y) in flags {
        if x != y {
            out.mismatch(|| format!("{path}.{name}"), &x, &y);
        }
    }
    if a.underline != b.underline {
        out.mismatch(|| format!("{path}.underline"), &a.underline, &b.underline);
    }
}

fn compare_color(out: &mut Comparison, palettes: &Palettes<'_>, path: &str, a: Color, b: Color) {
    if a == b {
        return;
    }
    let same_by_palette = match (a, b) {
        (Color::Palette(index), Color::Rgb(rgb)) => {
            palettes.reference.get(usize::from(index)) == Some(&rgb)
        }
        (Color::Rgb(rgb), Color::Palette(index)) => {
            palettes.receiver.get(usize::from(index)) == Some(&rgb)
        }
        _ => false,
    };
    if same_by_palette && PROFILE.rules.contains(&Rule::PaletteAsRgb) {
        out.normalize(|| path.to_owned(), Rule::PaletteAsRgb);
    } else {
        out.mismatch(|| path.to_owned(), &a, &b);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RED: Rgb = Rgb {
        r: 0xcc,
        g: 0,
        b: 0,
    };

    fn state() -> State {
        let mut state = State::blank(10, 3);
        state.colors.palette[1] = RED;
        for (x, ch) in "hello".chars().enumerate() {
            state.active[0].cells[x].text = ch.to_string();
        }
        state.cursor.x = 5;
        state
    }

    #[test]
    fn equal_states_compare_equal() {
        let comparison = compare(&state(), &state());
        assert_eq!(comparison.outcome(), Outcome::Equal, "{comparison}");
        assert_eq!(comparison.profile, PROFILE_VERSION);
        assert!(comparison.normalized.is_empty());
    }

    #[test]
    fn one_attribute_differs() {
        let reference = state();
        let mut receiver = state();
        receiver.active[0].cells[1].style.bold = true;
        let comparison = compare(&reference, &receiver);
        assert_eq!(comparison.outcome(), Outcome::Differs);
        assert_eq!(comparison.mismatch_count, 1, "{comparison}");
        assert_eq!(comparison.mismatches[0].path, "active[0][1].style.bold");
        assert_eq!(comparison.mismatches[0].reference, "false");
        assert_eq!(comparison.mismatches[0].receiver, "true");
    }

    #[test]
    fn only_saved_cursor_differs() {
        let reference = state();
        let mut receiver = state();
        let Observed::Value(saved) = &mut receiver.saved_cursor else {
            panic!("заготовка с сохранённым курсором");
        };
        saved.y = 2;
        let comparison = compare(&reference, &receiver);
        assert_eq!(comparison.outcome(), Outcome::Differs);
        let paths: Vec<&str> = comparison
            .mismatches
            .iter()
            .map(|d| d.path.as_str())
            .collect();
        assert_eq!(paths, ["saved_cursor.y"]);
    }

    #[test]
    fn unobserved_hidden_state_is_incomplete_not_equal() {
        let reference = state();
        let mut receiver = state();
        receiver.saved_cursor = Observed::NotObserved("журнал ввода переполнен".to_owned());
        let comparison = compare(&reference, &receiver);
        assert_eq!(comparison.outcome(), Outcome::Incomplete);
        assert_eq!(comparison.unobserved.len(), 1);
        assert_eq!(comparison.unobserved[0].path, "saved_cursor");
        assert_eq!(comparison.unobserved[0].reference, None);
        assert_eq!(
            comparison.unobserved[0].receiver.as_deref(),
            Some("журнал ввода переполнен")
        );

        // Ненаблюдённое с обеих сторон тоже не равенство.
        let mut reference = state();
        reference.kitty_stack = Observed::NotObserved("не запрошено".to_owned());
        receiver = state();
        receiver.kitty_stack = Observed::NotObserved("не запрошено".to_owned());
        assert_eq!(
            compare(&reference, &receiver).outcome(),
            Outcome::Incomplete
        );
    }

    #[test]
    fn kitty_stack_differs_below_equal_top() {
        let mut reference = state();
        let mut receiver = state();
        reference.kitty_flags = 3;
        receiver.kitty_flags = 3;
        reference.kitty_stack = Observed::Value(vec![3, 1]);
        receiver.kitty_stack = Observed::Value(vec![3]);
        let comparison = compare(&reference, &receiver);
        assert_eq!(comparison.outcome(), Outcome::Differs);
        assert_eq!(comparison.mismatch_count, 1, "{comparison}");
        assert_eq!(comparison.mismatches[0].path, "kitty_stack");
    }

    #[test]
    fn palette_index_equals_same_rgb_within_profile() {
        let mut reference = state();
        let mut receiver = state();
        reference.active[0].cells[0].style.fg = Color::Palette(1);
        receiver.active[0].cells[0].style.fg = Color::Rgb(RED);
        // В обратную сторону индекс берётся из палитры приёмника.
        reference.cursor.style.bg = Color::Rgb(RED);
        receiver.cursor.style.bg = Color::Palette(1);
        let comparison = compare(&reference, &receiver);
        assert_eq!(comparison.outcome(), Outcome::Equal, "{comparison}");
        assert_eq!(comparison.normalized_count, 2);
        assert_eq!(
            comparison.normalized[0],
            Normalized {
                path: "active[0][0].style.fg".to_owned(),
                rule: Rule::PaletteAsRgb,
            }
        );
    }

    #[test]
    fn divergence_outside_profile_differs() {
        let mut reference = state();
        let mut receiver = state();
        reference.active[0].cells[0].style.fg = Color::Palette(1);
        receiver.active[0].cells[0].style.fg = Color::Rgb(Rgb {
            r: 0xcd,
            g: 0,
            b: 0,
        });
        let comparison = compare(&reference, &receiver);
        assert_eq!(comparison.outcome(), Outcome::Differs);
        assert_eq!(comparison.mismatches[0].path, "active[0][0].style.fg");
        assert_eq!(comparison.normalized_count, 0);

        // Палитровый цвет против цвета по умолчанию не нормализуется.
        receiver.active[0].cells[0].style.fg = Color::Default;
        assert_eq!(compare(&reference, &receiver).outcome(), Outcome::Differs);
    }

    #[test]
    fn blank_cell_is_not_a_printed_space() {
        let reference = state();
        let mut receiver = state();
        receiver.active[0].cells[7].text = " ".to_owned();
        let comparison = compare(&reference, &receiver);
        assert_eq!(comparison.outcome(), Outcome::Differs);
        assert_eq!(comparison.mismatches[0].path, "active[0][7].text");
        assert_eq!(
            reference.text(),
            receiver.text(),
            "текст строки их не различает"
        );
    }

    #[test]
    fn wrap_flags_pending_wrap_and_hyperlink_are_strict() {
        let reference = state();
        let mut receiver = state();
        receiver.active[0].wrap = true;
        receiver.active[1].wrap_continuation = true;
        receiver.cursor.pending_wrap = true;
        receiver.active[0].cells[0].hyperlink = Some("https://example.test".to_owned());
        let comparison = compare(&reference, &receiver);
        let paths: Vec<&str> = comparison
            .mismatches
            .iter()
            .map(|d| d.path.as_str())
            .collect();
        assert_eq!(
            paths,
            [
                "active[0].wrap",
                "active[0][0].hyperlink",
                "active[1].wrap_continuation",
                "cursor.pending_wrap"
            ]
        );
    }

    #[test]
    fn history_aligns_from_active_area_and_counts_length() {
        let mut reference = state();
        let mut receiver = state();
        let line = |text: &str| {
            let mut row = Row::blank(10);
            for (x, ch) in text.chars().enumerate() {
                row.cells[x].text = ch.to_string();
            }
            row
        };
        reference.history = vec![line("near"), line("far")];
        reference.history_total = 2;
        receiver.history = vec![line("near")];
        receiver.history_total = 1;
        let comparison = compare(&reference, &receiver);
        let paths: Vec<&str> = comparison
            .mismatches
            .iter()
            .map(|d| d.path.as_str())
            .collect();
        assert_eq!(paths, ["history_total", "history.len"]);
    }

    #[test]
    fn modes_input_and_size_are_compared() {
        let reference = state();
        let mut receiver = state();
        receiver.modes.insert("?2004".to_owned(), true);
        receiver
            .input
            .insert("key:up".to_owned(), "1b4f41".to_owned());
        receiver.rows = 4;
        let comparison = compare(&reference, &receiver);
        let paths: Vec<&str> = comparison
            .mismatches
            .iter()
            .map(|d| d.path.as_str())
            .collect();
        assert_eq!(paths, ["rows", "modes.?2004", "input.key:up"]);
    }

    #[test]
    fn listing_is_bounded_and_counted() {
        let reference = State::blank(200, 2);
        let mut receiver = State::blank(200, 2);
        for cell in &mut receiver.active[0].cells {
            cell.text = "x".to_owned();
        }
        let comparison = compare(&reference, &receiver);
        assert_eq!(comparison.mismatch_count, 200);
        assert_eq!(comparison.mismatches.len(), MAX_LISTED);
        let json = serde_json::to_string(&comparison).expect("сравнение сериализуется");
        let back: Comparison = serde_json::from_str(&json).expect("и читается обратно");
        assert_eq!(back, comparison);
    }

    #[test]
    fn state_round_trips_through_json() {
        let mut state = state();
        state.kitty_stack = Observed::NotObserved("не запрошено".to_owned());
        let json = serde_json::to_string(&state).expect("состояние сериализуется");
        let back: State = serde_json::from_str(&json).expect("и читается обратно");
        assert_eq!(back, state);
    }

    #[test]
    fn profile_v1_is_frozen() {
        assert_eq!(PROFILE.version, 1);
        assert_eq!(
            PROFILE.describe(),
            "profile 1\n\
             rule palette-as-rgb\n\
             direct cells\n\
             direct rows\n\
             direct history\n\
             direct cursor\n\
             direct screen\n\
             direct modes\n\
             direct kitty-flags\n\
             direct colors\n\
             direct title\n\
             direct pwd\n\
             direct input\n\
             direct parser\n\
             replay saved-cursor\n\
             replay kitty-stack\n\
             suffix inactive-screen\n\
             suffix tabstops\n\
             suffix scroll-margins\n\
             suffix partial-sequence\n\
             suffix reflow\n\
             excluded hyperlink-id\n\
             excluded cursor-shape\n\
             excluded mouse-shape\n\
             excluded selection\n\
             excluded viewport\n\
             excluded kitty-graphics\n\
             excluded charsets\n\
             modes 2,4,12,20,?1,?3,?4,?5,?6,?7,?8,?9,?12,?25,?40,?45,?47,?66,?67,?69,?1000,?1002,\
             ?1003,?1004,?1005,?1006,?1007,?1015,?1016,?1035,?1036,?1039,?1045,?1047,?1048,?1049,\
             ?2004,?2026,?2027,?2031,?2033,?2048,?5522\n\
             input key:a,key:ctrl-a,key:enter,key:tab,key:backspace,key:escape,key:up,key:shift-up,\
             key:numpad-1,mouse:left-press,mouse:left-release,mouse:motion\n"
        );
    }
}
