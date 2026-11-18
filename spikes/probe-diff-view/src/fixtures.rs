//! Детерминированные генераторы синтетических фикстур P0.6: `added`, `modified`, `sparse`, `wrap`
//! и `threads`.
//!
//! Текст — синтетический Rust без ошибок разбора: функции, многострочные и вложенные блочные
//! комментарии, raw-строки `r#"…"#`, многострочные строковые литералы, макросы и длинные
//! выражения. Каждая строка несёт свой номер, поэтому все строки различны и diff однозначен.
//! Изменённая строка — та же форма с другим номером: правится лексема, а не разделитель, так что
//! строение комментариев и строк от правки не меняется.
//!
//! Diff пишется как у `git diff -U3 --inter-hunk-context=0 --no-ext-diff --no-textconv --no-color`:
//! изменения в одном ханке, если между ними не больше 6 неизменных строк, заголовок раздела —
//! ближайшая строка старой стороны выше ханка, которая начинается с буквы, `_` или `$`, до 80
//! байт без пробелов в конце. Хэши в строке `index` — не SHA-1 git, а FNV-1a содержимого.
//! Фикстура — сырые байты diff и обеих сторон и треды, разбор и индексы строит вид.

use std::fmt::Write as _;
use std::ops::RangeInclusive;
use std::sync::Arc;

use crate::model::state::ThreadId;
use crate::model::threads::{Comment, Thread, ThreadAnchor};
use crate::model::{LineAddr, Side};

/// Строк контекста ханка.
pub const CONTEXT: usize = 3;
/// Изменения сливаются в один ханк, если между ними не больше стольких неизменных строк.
const MERGE_GAP: usize = 2 * CONTEXT;
/// Прибавка к номеру изменённой строки: номера новой стороны не совпадают ни с одним старым.
const CHANGED: u64 = 1_000_000;
/// Длина заголовка раздела в байтах, как у git.
const SECTION_BYTES: usize = 80;

/// Строк в больших фикстурах и в `wrap`.
pub const LINES: usize = 100_000;
pub const WRAP_LINES: usize = 10_000;
/// Изменена каждая 5-я строка `modified`, `wrap` и `threads`.
pub const CHANGE_EVERY: usize = 5;
/// Ханков `sparse` и изменённых строк в каждом: 3 удалённых и 3 вставленных.
pub const SPARSE_HUNKS: usize = 50;
pub const SPARSE_CHANGED: usize = 3;
/// Первая строка последнего ханка `sparse` на обеих сторонах.
pub const SPARSE_LAST_HUNK: u32 = 99_900;
/// Строка, где на старой стороне `sparse` открывается комментарий, а на новой raw-строка.
pub const SPARSE_OPENER: u32 = 50_002;
/// Строка, где они закрываются: последняя изменённая строка последнего ханка.
pub const SPARSE_CLOSER: u32 = 99_905;
/// Пределы длины строки `wrap` в символах.
pub const WRAP_MIN_CHARS: usize = 200;
pub const WRAP_MAX_CHARS: usize = 4_000;
/// Тредов `threads`, из них диапазонов через границу ханка.
pub const THREADS: usize = 500;
pub const BOUNDARY_THREADS: usize = 10;
pub const MAX_COMMENTS: u64 = 20;

const SEED_BASE: u64 = 0x0006_D1FF_0000_0001;
const SEED_SPARSE: u64 = 0x0006_D1FF_0000_0002;
const SEED_WRAP: u64 = 0x0006_D1FF_0000_0003;
const SEED_THREADS: u64 = 0x0006_D1FF_0000_0004;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FixtureName {
    Added,
    Modified,
    Sparse,
    Wrap,
    Threads,
}

impl FixtureName {
    pub const ALL: [FixtureName; 5] = [
        FixtureName::Added,
        FixtureName::Modified,
        FixtureName::Sparse,
        FixtureName::Wrap,
        FixtureName::Threads,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            FixtureName::Added => "added",
            FixtureName::Modified => "modified",
            FixtureName::Sparse => "sparse",
            FixtureName::Wrap => "wrap",
            FixtureName::Threads => "threads",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        FixtureName::ALL
            .into_iter()
            .find(|fixture| fixture.as_str() == name)
    }
}

/// Сырые байты фикстуры: текст diff и полные стороны, у `added` старая сторона пуста.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fixture {
    pub name: FixtureName,
    pub path: String,
    pub diff: Arc<[u8]>,
    pub old: Arc<[u8]>,
    pub new: Arc<[u8]>,
    pub threads: Vec<Thread>,
}

impl Fixture {
    /// FNV-1a всех байт и тредов: отпечаток, по которому процессы сверяют одну фикстуру.
    pub fn digest(&self) -> u64 {
        let mut hash = Fnv::new();
        for part in [&self.diff, &self.old, &self.new] {
            hash.write(&(part.len() as u64).to_le_bytes());
            hash.write(part);
        }
        for thread in &self.threads {
            let side = match thread.anchor.side {
                Side::Old => 0u8,
                Side::New => 1u8,
            };
            hash.write(&thread.id.0.to_le_bytes());
            hash.write(thread.anchor.path.as_bytes());
            hash.write(&[side]);
            hash.write(&thread.anchor.start_line.to_le_bytes());
            hash.write(&thread.anchor.line.to_le_bytes());
            for comment in &thread.comments {
                hash.write(comment.author.as_bytes());
                hash.write(&[0]);
                hash.write(comment.body.as_bytes());
                hash.write(&[0]);
            }
        }
        hash.finish()
    }
}

pub fn generate(name: FixtureName) -> Fixture {
    let path = format!("src/{}.rs", name.as_str());
    let (diff, old, new, threads) = match name {
        FixtureName::Added => {
            let new = render(&base(), &[]).0;
            (write_added(&path, &new), Blob::default(), new, Vec::new())
        }
        FixtureName::Modified => {
            let changed = every_fifth(LINES, &[]);
            let (diff, old, new, _) = modified(&path, &base(), &changed);
            (diff, old, new, Vec::new())
        }
        FixtureName::Sparse => {
            let (shapes, changed) = sparse();
            let (diff, old, new, _) = modified(&path, &shapes, &changed);
            (diff, old, new, Vec::new())
        }
        FixtureName::Wrap => {
            let changed = every_fifth(WRAP_LINES, &[]);
            let (diff, old, new, _) = modified(&path, &wrap_shapes(), &changed);
            (diff, old, new, Vec::new())
        }
        FixtureName::Threads => {
            let reverted = boundary_lines();
            let changed = every_fifth(LINES, &reverted);
            let (diff, old, new, spans) = modified(&path, &base(), &changed);
            let threads = threads(&path, &spans, &reverted);
            (diff, old, new, threads)
        }
    };
    Fixture {
        name,
        path,
        diff: Arc::from(diff),
        old: Arc::from(old.text),
        new: Arc::from(new.text),
        threads,
    }
}

/// SplitMix64 с фиксированным зерном.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Число из `0..bound`, при `bound == 0` — 0.
    pub fn below(&mut self, bound: u64) -> u64 {
        self.next_u64().checked_rem(bound).unwrap_or(0)
    }

    /// Число из `low..=high`.
    pub fn between(&mut self, low: u64, high: u64) -> u64 {
        low + self.below(high.saturating_sub(low).saturating_add(1))
    }
}

struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Fnv(0xCBF2_9CE4_8422_2325)
    }

    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 ^= u64::from(byte);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01B3);
        }
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

/// Вид длинной строки `wrap`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Long {
    Expr,
    Comment,
    Text,
    Array,
}

/// Форма строки: шаблон, в который подставляется номер. У изменённой строки та же форма,
/// кроме открытия и закрытия области `sparse`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shape {
    Const,
    Use,
    LineComment,
    DocComment,
    FnOpen,
    Let,
    Print,
    Assert,
    VecMacro,
    LongExpr,
    InlineComment,
    ForOpen,
    ForBody,
    ForClose,
    FnTail,
    FnClose,
    CommentOpen,
    CommentText,
    NestedLine,
    NestedOpen,
    NestedText,
    NestedClose,
    CommentClose,
    RawOpen,
    RawText,
    RawClose,
    StrOpen,
    StrText,
    StrClose,
    JoinOpen,
    JoinClose,
    MacroOpen,
    MacroArm,
    MacroBody,
    MacroArmClose,
    MacroClose,
    StructOpen,
    StructField,
    StructClose,
    /// Строка области `sparse`: годится и как текст комментария, и как текст raw-строки.
    Content(u8),
    NotesCommentOpen,
    NotesRawOpen,
    NotesCommentClose,
    NotesRawClose,
    Long {
        kind: Long,
        chars: u32,
        seed: u32,
    },
}

fn single(rng: &mut Rng) -> Shape {
    match rng.below(3) {
        0 => Shape::Const,
        1 => Shape::Use,
        _ => Shape::LineComment,
    }
}

fn function(rng: &mut Rng, unit: &mut Vec<Shape>) {
    unit.extend([Shape::DocComment, Shape::FnOpen]);
    for _ in 0..rng.between(3, 12) {
        match rng.below(7) {
            0 => unit.push(Shape::Let),
            1 => unit.push(Shape::Print),
            2 => unit.push(Shape::Assert),
            3 => unit.push(Shape::VecMacro),
            4 => unit.push(Shape::LongExpr),
            5 => unit.push(Shape::InlineComment),
            _ => {
                unit.push(Shape::ForOpen);
                for _ in 0..rng.between(1, 3) {
                    unit.push(Shape::ForBody);
                }
                unit.push(Shape::ForClose);
            }
        }
    }
    unit.extend([Shape::FnTail, Shape::FnClose]);
}

fn block_comment(rng: &mut Rng, unit: &mut Vec<Shape>) {
    unit.push(Shape::CommentOpen);
    for _ in 0..rng.between(1, 5) {
        match rng.below(4) {
            0 | 1 => unit.push(Shape::CommentText),
            2 => unit.push(Shape::NestedLine),
            _ => {
                unit.push(Shape::NestedOpen);
                for _ in 0..rng.between(1, 2) {
                    unit.push(Shape::NestedText);
                }
                unit.push(Shape::NestedClose);
            }
        }
    }
    unit.push(Shape::CommentClose);
}

fn unit(rng: &mut Rng, unit: &mut Vec<Shape>) {
    match rng.below(100) {
        0..40 => function(rng, unit),
        40..52 => block_comment(rng, unit),
        52..62 => {
            unit.push(Shape::RawOpen);
            for _ in 0..rng.between(1, 6) {
                unit.push(Shape::RawText);
            }
            unit.push(Shape::RawClose);
        }
        62..70 => {
            unit.push(Shape::StrOpen);
            for _ in 0..rng.between(0, 3) {
                unit.push(Shape::StrText);
            }
            unit.push(Shape::StrClose);
        }
        70..75 => unit.extend([Shape::JoinOpen, Shape::JoinClose]),
        75..83 => {
            unit.push(Shape::MacroOpen);
            for _ in 0..rng.between(1, 2) {
                unit.extend([Shape::MacroArm, Shape::MacroBody, Shape::MacroArmClose]);
            }
            unit.push(Shape::MacroClose);
        }
        83..90 => {
            unit.push(Shape::StructOpen);
            for _ in 0..rng.between(1, 5) {
                unit.push(Shape::StructField);
            }
            unit.push(Shape::StructClose);
        }
        _ => unit.push(single(rng)),
    }
}

/// Добавляет ровно `count` строк целых элементов верхнего уровня: после них снова верхний
/// уровень, вне комментария и строки.
fn items(rng: &mut Rng, count: usize, out: &mut Vec<Shape>) {
    let target = out.len() + count;
    let mut next = Vec::new();
    while out.len() < target {
        next.clear();
        unit(rng, &mut next);
        if out.len() + next.len() <= target {
            out.extend_from_slice(&next);
        } else {
            while out.len() < target {
                out.push(single(rng));
            }
        }
    }
}

/// Старая сторона `modified` и `threads`, новая сторона `added`.
fn base() -> Vec<Shape> {
    let mut shapes = Vec::with_capacity(LINES);
    items(&mut Rng::new(SEED_BASE), LINES, &mut shapes);
    shapes
}

/// Индексы изменённых строк: каждая 5-я строка, кроме номеров `except`.
fn every_fifth(lines: usize, except: &[u32]) -> Vec<(usize, Option<Shape>)> {
    (CHANGE_EVERY..=lines)
        .step_by(CHANGE_EVERY)
        .filter(|line| !except.contains(&(*line as u32)))
        .map(|line| (line - 1, None))
        .collect()
}

/// `sparse`: 50 ханков по 3 изменённые строки. Ханк 25 открывает на старой стороне комментарий,
/// на новой raw-строку, последний ханк их закрывает. Между ними 24 ханка, все внутри.
fn sparse() -> (Vec<Shape>, Vec<(usize, Option<Shape>)>) {
    let mut rng = Rng::new(SEED_SPARSE);
    let opener = SPARSE_OPENER as usize;
    let closer = SPARSE_CLOSER as usize;
    let mut shapes = Vec::with_capacity(LINES);
    items(&mut rng, opener - 3, &mut shapes);
    shapes.extend([Shape::Const, Shape::Const, Shape::NotesCommentOpen]);
    while shapes.len() < closer - 1 {
        shapes.push(Shape::Content(rng.below(4) as u8));
    }
    shapes.push(Shape::NotesCommentClose);
    items(&mut rng, LINES - closer, &mut shapes);

    let mut changed = Vec::new();
    for hunk in 0..SPARSE_HUNKS {
        let first = if hunk + 1 == SPARSE_HUNKS {
            closer + 1 - SPARSE_CHANGED
        } else {
            2_000 * (hunk + 1)
        };
        for line in first..first + SPARSE_CHANGED {
            let new = match line {
                _ if line == opener => Some(Shape::NotesRawOpen),
                _ if line == closer => Some(Shape::NotesRawClose),
                _ => None,
            };
            changed.push((line - 1, new));
        }
    }
    (shapes, changed)
}

/// `wrap`: 10 000 длинных строк верхнего уровня. Длина в символах —
/// `200 + 3760 × u²`, `u` равномерно из `[0, 1]` с шагом 0,001, плюс закрытие строки.
fn wrap_shapes() -> Vec<Shape> {
    let mut rng = Rng::new(SEED_WRAP);
    (0..WRAP_LINES)
        .map(|_| {
            let kind = match rng.below(4) {
                0 => Long::Expr,
                1 => Long::Comment,
                2 => Long::Text,
                _ => Long::Array,
            };
            let u = rng.below(1_001);
            let chars = WRAP_MIN_CHARS as u64 + 3_760 * u * u / 1_000_000;
            Shape::Long {
                kind,
                chars: chars as u32,
                seed: rng.next_u64() as u32,
            }
        })
        .collect()
}

/// Номера строк `threads`, изменение которых снято: 10 границ ханков.
fn boundary_lines() -> Vec<u32> {
    (1..=BOUNDARY_THREADS as u32)
        .map(|k| 9_000 * k + 500)
        .collect()
}

const WORDS: &[&str] = &[
    "cache", "index", "range", "anchor", "数据", "wrap", "🚀", "split", "row", "column", "hunk",
    "gutter", "pair", "side",
];

/// Пишет длинную строку: префикс, затем лексемы, пока символов меньше `chars`, затем закрытие.
/// Номер в префиксе — 7 цифр, поэтому у изменённой строки та же длина.
fn render_long(out: &mut String, kind: Long, chars: u32, seed: u32, n: u64) {
    let start = out.len();
    let mut rng = Rng::new(u64::from(seed));
    let _ = match kind {
        Long::Expr => write!(out, "const EXPR_{n:07}: u64 = 1"),
        Long::Comment => write!(out, "// {n:07}:"),
        Long::Text => write!(out, "static TEXT_{n:07}: &str = \""),
        Long::Array => write!(out, "const ITEMS_{n:07}: &[u64] = &[0"),
    };
    let mut count = out.len() - start;
    let mut k = 0u64;
    while count < chars as usize {
        let before = out.len();
        k += 1;
        let _ = match kind {
            Long::Expr => write!(out, " + (w{k} * {} % 97)", rng.between(2, 999)),
            Long::Array => write!(out, ", {}", rng.below(1_000_000)),
            Long::Comment => match rng.below(16) {
                0 => write!(out, "\t{k}"),
                _ => write!(out, " {}", WORDS[rng.below(WORDS.len() as u64) as usize]),
            },
            Long::Text => match rng.below(16) {
                0 => write!(out, " \\\"q{k}\\\""),
                _ => write!(out, " {}", WORDS[rng.below(WORDS.len() as u64) as usize]),
            },
        };
        count += out.get(before..).map_or(0, |added| added.chars().count());
    }
    out.push_str(match kind {
        Long::Expr => ";",
        Long::Comment => "",
        Long::Text => "\";",
        Long::Array => "];",
    });
    out.push('\n');
}

fn render_line(out: &mut String, shape: Shape, n: u64) {
    let _ = match shape {
        Shape::Const => writeln!(out, "const LIMIT_{n}: u64 = {n} * 3 + 7;"),
        Shape::Use => writeln!(out, "use std::collections::BTreeMap as Map{n};"),
        Shape::LineComment => {
            writeln!(out, "// note {n}: keep the cache warm before the next pass")
        }
        Shape::DocComment => writeln!(out, "/// Checksum of block {n}."),
        Shape::FnOpen => writeln!(out, "fn block_{n}(input: &[u64]) -> u64 {{"),
        Shape::Let => writeln!(out, "    let value_{n} = input.len() as u64 + {n};"),
        Shape::Print => writeln!(out, "    println!(\"step {n}: {{}}\", input.len());"),
        Shape::Assert => writeln!(
            out,
            "    assert_eq!(input.len() % 2, {n} % 2, \"parity {n}\");"
        ),
        Shape::VecMacro => writeln!(out, "    let items_{n} = vec![{n}, {n} + 1, {n} + 2];"),
        Shape::LongExpr => writeln!(
            out,
            "    let mix_{n} = (input.len() as u64) * {n} + ({n} ^ 0x5f) - ({n} >> 3) \
             + (({n} << 2) % 97) * (input.first().copied().unwrap_or({n}) | 0x10);"
        ),
        Shape::InlineComment => {
            writeln!(out, "    let flag_{n} = /* inline {n} */ input.is_empty();")
        }
        Shape::ForOpen => writeln!(out, "    for value in input.iter() {{ // loop {n}"),
        Shape::ForBody => writeln!(out, "        let _ = value.wrapping_mul({n});"),
        Shape::ForClose => writeln!(out, "    }} // loop {n}"),
        Shape::FnTail => writeln!(out, "    input.iter().sum::<u64>() ^ {n}"),
        Shape::FnClose => writeln!(out, "}} // block {n}"),
        Shape::CommentOpen => writeln!(out, "/* Section {n}: storage notes."),
        Shape::CommentText => writeln!(out, "   The index keeps {n} entries between passes."),
        Shape::NestedLine => writeln!(out, "   /* nested {n}: inner remark */ still outer"),
        Shape::NestedOpen => writeln!(out, "   /* nested opener {n}"),
        Shape::NestedText => writeln!(out, "      nested body {n}"),
        Shape::NestedClose => writeln!(out, "   */ outer again {n}"),
        Shape::CommentClose => writeln!(out, "*/ // section {n}"),
        Shape::RawOpen => writeln!(out, "const PAGE_{n}: &str = r#\""),
        Shape::RawText => writeln!(out, "  <p class=\"row\" id=\"{n}\">\"quoted\" text {n}</p>"),
        Shape::RawClose => writeln!(out, "\"#; // page {n}"),
        Shape::StrOpen => writeln!(out, "const MESSAGE_{n}: &str = \"first line {n}"),
        Shape::StrText => writeln!(out, "with \\\"escape\\\" and tab\\t {n}"),
        Shape::StrClose => writeln!(out, "last line {n}\"; // message {n}"),
        Shape::JoinOpen => writeln!(out, "const JOINED_{n}: &str = \"joined {n} \\"),
        Shape::JoinClose => writeln!(out, "    continued {n}\";"),
        Shape::MacroOpen => writeln!(out, "macro_rules! square_{n} {{"),
        Shape::MacroArm => writeln!(out, "    ($x:expr, {n}) => {{"),
        Shape::MacroBody => writeln!(out, "        $x * $x + {n}"),
        Shape::MacroArmClose => writeln!(out, "    }}; // arm {n}"),
        Shape::MacroClose => writeln!(out, "}} // macro {n}"),
        Shape::StructOpen => writeln!(out, "struct Record{n} {{"),
        Shape::StructField => writeln!(out, "    field_{n}: u64,"),
        Shape::StructClose => writeln!(out, "}} // record {n}"),
        Shape::Content(0) => writeln!(out, "    let total_{n} = compute({n}) + \"label\".len();"),
        Shape::Content(1) => writeln!(
            out,
            "    if total_{n} > LIMIT {{ return Err(Error::Overflow({n})); }}"
        ),
        Shape::Content(2) => {
            writeln!(
                out,
                "    match value_{n} {{ Some(x) => x * 2, None => {n} }}"
            )
        }
        Shape::Content(_) => writeln!(out, "    fn helper_{n}(x: u32) -> u32 {{ x + {n} }}"),
        Shape::NotesCommentOpen => writeln!(out, "/* archived notes {n}"),
        Shape::NotesRawOpen => writeln!(out, "const ARCHIVED_{n}: &str = r#\""),
        Shape::NotesCommentClose => writeln!(out, "    end of notes {n} */"),
        Shape::NotesRawClose => writeln!(out, "    end of notes {n}\"#;"),
        Shape::Long { kind, chars, seed } => {
            render_long(out, kind, chars, seed, n);
            Ok(())
        }
    };
}

/// Сторона фикстуры: текст и начала строк, `starts` на одну запись длиннее числа строк.
#[derive(Clone, Debug, Default)]
struct Blob {
    text: Vec<u8>,
    starts: Vec<usize>,
}

impl Blob {
    fn lines(&self) -> usize {
        self.starts.len().saturating_sub(1)
    }

    /// Строка `index` с 0, без перевода строки.
    fn line(&self, index: usize) -> &[u8] {
        let (Some(&start), Some(&next)) = (self.starts.get(index), self.starts.get(index + 1))
        else {
            return &[];
        };
        self.text
            .get(start..next.saturating_sub(1))
            .unwrap_or_default()
    }
}

/// Старая и новая сторона по формам: строка `i` — номер `i + 1`, изменённая строка новой
/// стороны — номер `i + 1 + CHANGED` своей или заменённой формы.
fn render(shapes: &[Shape], changed: &[(usize, Option<Shape>)]) -> (Blob, Blob) {
    let side = |new: bool| {
        let mut text = String::new();
        let mut starts = Vec::with_capacity(shapes.len() + 1);
        let mut next = changed.iter().peekable();
        for (index, &shape) in shapes.iter().enumerate() {
            starts.push(text.len());
            let number = index as u64 + 1;
            match next.next_if(|(line, _)| *line == index) {
                Some(&(_, replaced)) if new => {
                    render_line(&mut text, replaced.unwrap_or(shape), number + CHANGED)
                }
                _ => render_line(&mut text, shape, number),
            }
        }
        starts.push(text.len());
        Blob {
            text: text.into_bytes(),
            starts,
        }
    };
    (side(false), side(true))
}

fn short_hash(bytes: &[u8]) -> String {
    let mut hash = Fnv::new();
    hash.write(bytes);
    format!("{:07x}", hash.finish() >> 36)
}

fn range(start: usize, count: usize) -> String {
    if count == 1 {
        start.to_string()
    } else {
        format!("{start},{count}")
    }
}

/// Заголовок раздела git по умолчанию: строка начинается с буквы, `_` или `$`.
fn section(line: &[u8]) -> Option<&[u8]> {
    let first = *line.first()?;
    if !(first.is_ascii_alphabetic() || first == b'_' || first == b'$') {
        return None;
    }
    let mut head = line.get(..line.len().min(SECTION_BYTES))?;
    while let Some((&last, rest)) = head.split_last()
        && last.is_ascii_whitespace()
    {
        head = rest;
    }
    Some(head)
}

fn header(diff: &mut Vec<u8>, path: &str, old: &Blob, new: &Blob, added: bool) {
    let _ = writeln!(Bytes(diff), "diff --git a/{path} b/{path}");
    if added {
        let _ = writeln!(Bytes(diff), "new file mode 100644");
        let _ = writeln!(Bytes(diff), "index 0000000..{}", short_hash(&new.text));
        let _ = writeln!(Bytes(diff), "--- /dev/null");
    } else {
        let _ = writeln!(
            Bytes(diff),
            "index {}..{} 100644",
            short_hash(&old.text),
            short_hash(&new.text)
        );
        let _ = writeln!(Bytes(diff), "--- a/{path}");
    }
    let _ = writeln!(Bytes(diff), "+++ b/{path}");
}

/// Запись в байты через `fmt::Write`.
struct Bytes<'a>(&'a mut Vec<u8>);

impl std::fmt::Write for Bytes<'_> {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        self.0.extend_from_slice(text.as_bytes());
        Ok(())
    }
}

fn push_line(diff: &mut Vec<u8>, prefix: u8, line: &[u8]) {
    diff.push(prefix);
    diff.extend_from_slice(line);
    diff.push(b'\n');
}

fn write_added(path: &str, new: &Blob) -> Vec<u8> {
    let mut diff = Vec::with_capacity(new.text.len() + new.lines() + 256);
    header(&mut diff, path, &Blob::default(), new, true);
    let _ = writeln!(Bytes(&mut diff), "@@ -0,0 +{} @@", range(1, new.lines()));
    for index in 0..new.lines() {
        push_line(&mut diff, b'+', new.line(index));
    }
    diff
}

/// Стороны и diff фикстуры с заменой строк один к одному. Возвращает и диапазоны номеров
/// строк ханков: номера сторон совпадают, потому что число строк не меняется.
fn modified(
    path: &str,
    shapes: &[Shape],
    changed: &[(usize, Option<Shape>)],
) -> (Vec<u8>, Blob, Blob, Vec<RangeInclusive<u32>>) {
    let (old, new) = render(shapes, changed);
    let lines = old.lines();
    let is_changed = {
        let mut mark = vec![false; lines];
        for &(line, _) in changed {
            if let Some(slot) = mark.get_mut(line) {
                *slot = true;
            }
        }
        mark
    };
    let mut groups: Vec<(usize, usize)> = Vec::new();
    for &(line, _) in changed {
        match groups.last_mut() {
            Some((_, last)) if line - (*last + 1) <= MERGE_GAP => *last = line,
            _ => groups.push((line, line)),
        }
    }
    let mut diff = Vec::with_capacity(old.text.len() + new.text.len() / 2 + 4096);
    header(&mut diff, path, &old, &new, false);
    let mut spans = Vec::with_capacity(groups.len());
    let mut func: Option<usize> = None;
    let mut searched = 0;
    for (first, last) in groups {
        let start = first.saturating_sub(CONTEXT);
        let end = (last + CONTEXT).min(lines.saturating_sub(1));
        let count = end - start + 1;
        // Как git: поиск от строки над ханком вниз до конца прошлого поиска, иначе прежняя.
        if let Some(found) = (searched..start)
            .rev()
            .find(|&line| section(old.line(line)).is_some())
        {
            func = Some(found);
        }
        searched = start;
        let _ = write!(
            Bytes(&mut diff),
            "@@ -{} +{} @@",
            range(start + 1, count),
            range(start + 1, count)
        );
        if let Some(text) = func.and_then(|line| section(old.line(line)))
            && !text.is_empty()
        {
            diff.push(b' ');
            diff.extend_from_slice(text);
        }
        diff.push(b'\n');
        let mut line = start;
        while line <= end {
            if !is_changed.get(line).copied().unwrap_or(false) {
                push_line(&mut diff, b' ', old.line(line));
                line += 1;
                continue;
            }
            let run = line;
            while is_changed.get(line).copied().unwrap_or(false) {
                line += 1;
            }
            for index in run..line {
                push_line(&mut diff, b'-', old.line(index));
            }
            for index in run..line {
                push_line(&mut diff, b'+', new.line(index));
            }
        }
        spans.push(start as u32 + 1..=end as u32 + 1);
    }
    (diff, old, new, spans)
}

const AUTHORS: &[&str] = &["anna", "boris", "vera", "gleb"];
const REMARKS: &[&str] = &[
    "Проверить границу диапазона",
    "Здесь нужен тест на пустой ввод",
    "Переименовать переменную",
    "Почему не через итератор?",
    "Этот номер должен совпасть с эталоном",
    "Длинное замечание, которое не помещается в одну строку блока треда и переносится по ширине",
];

fn comments(rng: &mut Rng) -> Vec<Comment> {
    (0..rng.between(1, MAX_COMMENTS))
        .map(|k| {
            let body = (0..rng.between(1, 3))
                .map(|line| {
                    let remark = REMARKS[rng.below(REMARKS.len() as u64) as usize];
                    format!("{remark} ({k}.{line})")
                })
                .collect::<Vec<_>>()
                .join("\n");
            Comment {
                author: AUTHORS[rng.below(AUTHORS.len() as u64) as usize].to_owned(),
                body,
            }
        })
        .collect()
}

/// 500 тредов `threads`: 10 диапазонов через границы ханков у снятых изменений и 490 строк
/// и диапазонов внутри ханков по обеим сторонам, по всему файлу.
fn threads(path: &str, spans: &[RangeInclusive<u32>], reverted: &[u32]) -> Vec<Thread> {
    let mut rng = Rng::new(SEED_THREADS);
    let side = |rng: &mut Rng| {
        if rng.below(2) == 0 {
            Side::Old
        } else {
            Side::New
        }
    };
    let mut anchors: Vec<ThreadAnchor> = reverted
        .iter()
        .enumerate()
        .map(|(k, &line)| {
            let side = if k % 2 == 0 { Side::Old } else { Side::New };
            // Последний контекст ханка до `line` — `line - 2`, первый контекст следующего —
            // `line + 2`.
            let from = line - 2 - rng.below(3) as u32;
            let to = line + 2 + rng.below(3) as u32;
            ThreadAnchor::range(path, side, from, to)
        })
        .collect();
    let regular = THREADS - BOUNDARY_THREADS;
    let last = spans.last().map_or(1, |span| *span.end());
    for i in 0..regular {
        let target = (2 + i as u64 * u64::from(last) / regular as u64 + rng.below(150)) as u32;
        let span = spans
            .iter()
            .find(|span| *span.end() >= target)
            .or(spans.last());
        let Some(span) = span else {
            break;
        };
        let line = target.clamp(*span.start(), *span.end());
        let side = side(&mut rng);
        let anchor = if rng.below(10) < 3 {
            let to = (line + rng.between(1, 9) as u32).min(*span.end());
            ThreadAnchor::range(path, side, line, to)
        } else {
            ThreadAnchor::at(path, LineAddr { side, line })
        };
        anchors.push(anchor);
    }
    anchors.sort_by_key(|anchor| (anchor.line, anchor.start_line, anchor.side));
    anchors
        .into_iter()
        .enumerate()
        .map(|(id, anchor)| Thread {
            id: ThreadId(id as u32),
            anchor,
            comments: comments(&mut rng),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::OnceLock;

    use super::*;
    use crate::model::rows::Mode;
    use crate::model::threads::{Placement, place};
    use crate::model::{Diff, FileChange, LineKind, TextBody, parse};
    use crate::worker::Cancel;

    fn fixture(name: FixtureName) -> &'static (Fixture, Diff) {
        static CELLS: [OnceLock<(Fixture, Diff)>; 5] = [const { OnceLock::new() }; 5];
        let index = FixtureName::ALL
            .iter()
            .position(|&other| other == name)
            .unwrap();
        CELLS[index].get_or_init(|| {
            let fixture = generate(name);
            let diff = parse::parse(fixture.diff.clone(), &Cancel::never()).unwrap();
            (fixture, diff)
        })
    }

    fn body(diff: &Diff) -> &TextBody {
        assert_eq!(diff.files().len(), 1);
        diff.files()[0].text().unwrap()
    }

    fn line_count(bytes: &[u8]) -> usize {
        bytes.iter().filter(|&&byte| byte == b'\n').count()
    }

    fn blob_lines(bytes: &[u8]) -> Vec<&[u8]> {
        let mut lines: Vec<&[u8]> = bytes.split(|&byte| byte == b'\n').collect();
        assert_eq!(
            lines.pop(),
            Some(&b""[..]),
            "сторона кончается переводом строки"
        );
        lines
    }

    /// Каждая строка diff совпадает со строкой своей стороны по номеру.
    fn assert_matches_blobs(fixture: &Fixture, diff: &Diff) {
        let old = blob_lines(&fixture.old);
        let new = blob_lines(&fixture.new);
        for line in body(diff).lines() {
            let text = diff.line_text(line);
            for (side, blob) in [(Side::Old, &old), (Side::New, &new)] {
                if let Some(number) = line.line(side) {
                    assert_eq!(
                        blob.get(number as usize - 1).copied(),
                        Some(text),
                        "{:?} {side:?} {number}",
                        fixture.name
                    );
                }
            }
        }
    }

    fn assert_shape(name: FixtureName, hunks: usize, unified: usize, side: usize) {
        let (fixture, diff) = fixture(name);
        let body = body(diff);
        assert_eq!(diff.files()[0].path(), fixture.path);
        assert_eq!(body.hunks().len(), hunks, "{name:?}");
        assert_eq!(body.row_count(Mode::Unified), unified, "{name:?}");
        assert_eq!(body.row_count(Mode::SideBySide), side, "{name:?}");
        assert_matches_blobs(fixture, diff);
    }

    #[test]
    fn added_is_new_file_of_100k_lines() {
        let (fixture, diff) = fixture(FixtureName::Added);
        assert!(fixture.old.is_empty());
        assert_eq!(line_count(&fixture.new), LINES);
        assert_eq!(diff.files()[0].change, FileChange::Added);
        assert_shape(FixtureName::Added, 1, LINES, LINES);
        assert!(
            body(diff)
                .lines()
                .iter()
                .all(|line| line.kind() == LineKind::Added)
        );
    }

    #[test]
    fn modified_changes_every_fifth_line_in_one_hunk() {
        let (fixture, diff) = fixture(FixtureName::Modified);
        assert_eq!(line_count(&fixture.old), LINES);
        assert_eq!(line_count(&fixture.new), LINES);
        // Строка 1 вне ханка, остальные 79 999 неизменных — контекст, 20 000 пар.
        assert_shape(FixtureName::Modified, 1, 119_999, 99_999);
        let hunk = &body(diff).hunks()[0];
        assert_eq!((hunk.old_start, hunk.old_count), (2, 99_999));
        assert_eq!((hunk.new_start, hunk.new_count), (2, 99_999));
        let removed: Vec<u32> = body(diff)
            .lines()
            .iter()
            .filter(|line| line.kind() == LineKind::Removed)
            .filter_map(|line| line.line(Side::Old))
            .collect();
        assert_eq!(removed.len(), LINES / CHANGE_EVERY);
        assert!(removed.iter().all(|line| line % 5 == 0));
        assert!(fixture.threads.is_empty());
    }

    #[test]
    fn sparse_last_hunk_starts_inside_comment_and_raw_string() {
        let (fixture, diff) = fixture(FixtureName::Sparse);
        assert_eq!(line_count(&fixture.old), LINES);
        assert_eq!(line_count(&fixture.new), LINES);
        // 50 ханков по 3 + 3 контекста и 3 + 3 изменённых строки.
        assert_shape(FixtureName::Sparse, SPARSE_HUNKS, 600, 450);
        let body = body(diff);
        let last = body.hunks().last().unwrap();
        assert_eq!((last.old_start, last.old_count), (SPARSE_LAST_HUNK, 9));
        assert_eq!((last.new_start, last.new_count), (SPARSE_LAST_HUNK, 9));
        let old = blob_lines(&fixture.old);
        let new = blob_lines(&fixture.new);
        let at = |lines: &[&[u8]], line: u32| {
            String::from_utf8_lossy(lines[line as usize - 1]).into_owned()
        };
        assert!(at(&old, SPARSE_OPENER).starts_with("/* archived notes"));
        assert!(at(&new, SPARSE_OPENER).ends_with("r#\""));
        assert!(at(&old, SPARSE_CLOSER).ends_with("*/"));
        assert!(at(&new, SPARSE_CLOSER).ends_with("\"#;"));
        // Внутри области нет разделителей ни комментария, ни raw-строки.
        for line in SPARSE_OPENER + 1..SPARSE_CLOSER {
            for side in [&old, &new] {
                let text = at(side, line);
                assert!(
                    !text.contains("/*") && !text.contains("*/") && !text.contains("\"#"),
                    "{line}: {text}"
                );
            }
        }
        // Открытие дальше 200 строк от последнего ханка и в ханке, над которым 24 ханка
        // области: при открытии на последнем ханке оно больше чем за экран строк показа.
        const { assert!(SPARSE_LAST_HUNK - SPARSE_OPENER > 200) };
        let opener_hunk = body
            .hunk_of_line(body.line_of_addr(LineAddr::old(SPARSE_OPENER)).unwrap())
            .unwrap();
        assert_eq!(opener_hunk, 24);
        let opener_row = body
            .row_of_addr(Mode::Unified, LineAddr::old(SPARSE_OPENER))
            .unwrap()
            .index;
        let last_row = body
            .row_of_addr(Mode::Unified, LineAddr::old(SPARSE_LAST_HUNK))
            .unwrap()
            .index;
        assert!(last_row - opener_row > 250, "{opener_row} {last_row}");
        // Ханки, кроме последнего, начинают изменения на кратных 2000.
        for (k, hunk) in body.hunks().iter().take(SPARSE_HUNKS - 1).enumerate() {
            assert_eq!(hunk.old_start as usize + CONTEXT, 2_000 * (k + 1));
        }
    }

    #[test]
    fn wrap_lines_are_long_and_within_limits() {
        let (fixture, _) = fixture(FixtureName::Wrap);
        for side in [&fixture.old, &fixture.new] {
            let lines = blob_lines(side);
            assert_eq!(lines.len(), WRAP_LINES);
            let mut longest = 0;
            for line in lines {
                let chars = std::str::from_utf8(line).unwrap().chars().count();
                assert!(
                    (WRAP_MIN_CHARS..=WRAP_MAX_CHARS).contains(&chars),
                    "{chars}"
                );
                longest = longest.max(chars);
            }
            assert!(longest > 3_500, "{longest}");
        }
        assert!(fixture.new.contains(&b'\t'));
        assert!(std::str::from_utf8(&fixture.new).unwrap().contains('数'));
        assert_shape(FixtureName::Wrap, 1, 11_999, 9_999);
    }

    #[test]
    fn threads_fixture_has_hunk_boundaries_and_500_threads() {
        // Снятые 10 изменений: на каждое минус 2 строки изменения и 2 строки контекста.
        assert_shape(FixtureName::Threads, 11, 119_959, 99_969);
        let (fixture, diff) = fixture(FixtureName::Threads);
        let body = body(diff);
        assert_eq!(fixture.threads.len(), THREADS);
        let ids: BTreeSet<u32> = fixture.threads.iter().map(|thread| thread.id.0).collect();
        assert_eq!(ids, (0..THREADS as u32).collect());
        let mut crossing = 0;
        let mut ranges = 0;
        let mut sides = BTreeSet::new();
        for thread in &fixture.threads {
            assert!((1..=MAX_COMMENTS as usize).contains(&thread.comments.len()));
            assert_eq!(thread.anchor.path, fixture.path);
            sides.insert(thread.anchor.side);
            let Placement::Placed { file, first, last } = place(diff, &thread.anchor) else {
                panic!("тред {:?} не нашёл якорь", thread.anchor);
            };
            assert_eq!(file, 0);
            if thread.anchor.is_range() {
                ranges += 1;
            }
            if body.hunk_of_line(first) != body.hunk_of_line(last) {
                crossing += 1;
            }
        }
        assert_eq!(crossing, BOUNDARY_THREADS);
        assert!(ranges > 100, "{ranges}");
        assert_eq!(sides.len(), 2);
        let counts: BTreeSet<usize> = fixture
            .threads
            .iter()
            .map(|thread| thread.comments.len())
            .collect();
        assert!(counts.contains(&1) && counts.contains(&20), "{counts:?}");
        let lines: Vec<u32> = fixture
            .threads
            .iter()
            .map(|thread| thread.anchor.line)
            .collect();
        assert!(lines.first().unwrap() < &1_000 && lines.last().unwrap() > &99_000);
    }

    #[test]
    fn fixtures_are_deterministic() {
        // Отпечатки закреплены: другой процесс, другой запуск и `--reference` дают те же байты.
        let pinned = [
            (FixtureName::Added, 0x9a2a_457a_670b_78d7),
            (FixtureName::Modified, 0x9076_17d6_abed_7ced),
            (FixtureName::Sparse, 0xecac_275e_2210_62cf),
            (FixtureName::Wrap, 0xc9d0_7d51_aa3b_0166),
            (FixtureName::Threads, 0x32ec_20d2_9d51_c52a),
        ];
        for (name, digest) in pinned {
            assert_eq!(fixture(name).0.digest(), digest, "{name:?}");
        }
        assert_eq!(
            &generate(FixtureName::Sparse),
            &fixture(FixtureName::Sparse).0
        );
    }

    #[test]
    fn names_round_trip() {
        for name in FixtureName::ALL {
            assert_eq!(FixtureName::parse(name.as_str()), Some(name));
        }
        assert_eq!(FixtureName::parse("real"), None);
    }

    /// Обе стороны без узлов ошибки: восстановление после ошибки исказило бы замеры подсветки.
    #[test]
    fn sources_parse_without_errors() {
        let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&language).unwrap();
        let mut check = |name: FixtureName, text: &[u8]| {
            let tree = parser.parse(text, None).unwrap();
            assert!(!tree.root_node().has_error(), "{name:?}");
            tree
        };
        check(FixtureName::Added, &fixture(FixtureName::Added).0.new);
        check(FixtureName::Modified, &fixture(FixtureName::Modified).0.new);
        check(FixtureName::Wrap, &fixture(FixtureName::Wrap).0.old);
        check(FixtureName::Wrap, &fixture(FixtureName::Wrap).0.new);
        let sparse = &fixture(FixtureName::Sparse).0;
        for (text, kind) in [
            (&sparse.old, "block_comment"),
            (&sparse.new, "string_content"),
        ] {
            let tree = check(FixtureName::Sparse, text);
            let lines = blob_lines(text);
            let row = SPARSE_LAST_HUNK as usize - 1;
            let column = lines[row].len() / 2;
            let point = tree_sitter::Point { row, column };
            let node = tree
                .root_node()
                .descendant_for_point_range(point, point)
                .unwrap();
            assert_eq!(node.kind(), kind);
        }
    }
}
