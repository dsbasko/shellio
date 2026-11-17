//! Клетка F4 по проходу исполнителя (план P0.4, задача 19, раздел «Проход F4 исполнителем»):
//! каталог частей и их обязательность, свод частей в итог клетки, сравнение экранов до и после
//! повторного подключения, разбор вставки, кодирования клавиш, файла nvim и номеров в ответе
//! агента. Модуль чистый и идёт в `make check`. Сценарий `apps` (headless: `nvim`, `claude`,
//! `codex`) — `headless` с функцией `vt`, сценарий `apps-surface` — `crate::surface::apps`.
//!
//! Части F4 объявлены до первого прогона. Обязательная часть, которой нет ни в одном прогоне,
//! в своде — `not_run` с причиной `pending`. Итог: `fail`, если провалена любая обязательная
//! часть; `pass`, если все обязательные — `pass`; иначе `not_run` с перечнем частей. Части
//! исследования итог не меняют.

#[cfg(feature = "vt")]
pub mod headless;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::matrix::Candidate;
use crate::matrix::evidence::{self, Evidence};
use crate::results::{Cell, CellPart, DefectOrigin, NotRunCause, Requirement, Status, SummaryFile};

/// ID клетки прохода.
pub const F4: &str = "F4";

/// Где идёт часть: headless-сценарий `apps` или сценарий в поверхности `apps-surface`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Place {
    Headless,
    Surface,
}

/// Часть F4 по таблице раздела «Проход F4 исполнителем».
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PartSpec {
    pub name: &'static str,
    pub place: Place,
    pub requirement: Requirement,
    pub stimulus: &'static str,
    pub oracle: &'static str,
    pub completion: &'static str,
}

/// Объявленный профиль F4. Часть `nvim` таблицы идёт в двух местах, поэтому у неё две записи:
/// `nvim` (headless) и `nvim-surface`.
pub const PARTS: [PartSpec; 10] = [
    PartSpec {
        name: "nvim",
        place: Place::Headless,
        requirement: Requirement::Required,
        stimulus: "nvim --clean на файле в несколько экранов: :set number, :vsplit, курсор в \
                   середине второго окна, режим вставки, Ctrl-V Shift-Enter; SIGKILL клиента, \
                   новый attach, снова Ctrl-V Shift-Enter, :wqa",
        oracle: "приёмник attach-клиента до SIGKILL и после attach, наблюдатель процессов, файл \
                 на диске",
        completion: "экран нового клиента равен экрану до SIGKILL или срок; nvim вышел по :wqa",
    },
    PartSpec {
        name: "nvim-surface",
        place: Place::Surface,
        requirement: Requirement::Required,
        stimulus: "то же в поверхности синтетическими событиями клавиш; SIGKILL GUI, новая GUI к \
                   той же сессии",
        oracle: "read_text и точка курсора поверхности до SIGKILL и после attach, файл на диске, \
                 контроль без хоста",
        completion: "вид новой GUI равен виду до SIGKILL или срок; nvim вышел по :wqa",
    },
    PartSpec {
        name: "paste",
        place: Place::Surface,
        requirement: Requirement::Required,
        stimulus: "многострочная вставка с отступами действием paste_from_clipboard поверхности, \
                   колбэк чтения буфера обмена отвечает текстом пробы; в nvim и в фикстуру query \
                   после SIGKILL GUI и нового attach",
        oracle: "файл nvim на диске, квитанции прочитанных байтов фикстуры, контроль без хоста",
        completion: "nvim вышел по :wq; фикстура прочла конец блока 2004 или срок",
    },
    PartSpec {
        name: "kitty",
        place: Place::Surface,
        requirement: Requirement::Required,
        stimulus: "фикстура включает протокол Kitty стеком, клавиши с модификаторами \
                   синтетическими событиями до SIGKILL GUI и после нового attach",
        oracle: "квитанции прочитанных байтов фикстуры между маркерами, контроль без хоста",
        completion: "маркер после каждой клавиши дошёл до фикстуры или срок",
    },
    PartSpec {
        name: "layout-ru",
        place: Place::Surface,
        requirement: Requirement::Required,
        stimulus: "источник ввода «Русская» на время ввода, коды клавиш g h b d t n через \
                   interpretKeyEvents в nvim после SIGKILL GUI и нового attach",
        oracle: "файл nvim на диске; контроль без хоста с теми же кодами под ABC и «Русской»",
        completion: "nvim вышел по :wq",
    },
    PartSpec {
        name: "dead-key",
        place: Place::Surface,
        requirement: Requirement::Required,
        stimulus: "источник ввода ABC Extended, Option-e e в nvim после SIGKILL GUI и нового \
                   attach",
        oracle: "файл nvim на диске, журнал ввода вида, контроль без хоста",
        completion: "nvim вышел по :wq",
    },
    PartSpec {
        name: "ime",
        place: Place::Surface,
        requirement: Requirement::Research,
        stimulus: "японский IME: composition, выбор кандидата, коммит; Esc внутри composition; \
                   в фикстуру после SIGKILL GUI и нового attach",
        oracle: "квитанции прочитанных байтов фикстуры, журнал ввода вида, контроль без хоста",
        completion: "маркер после ввода дошёл до фикстуры или срок",
    },
    PartSpec {
        name: "switch",
        place: Place::Surface,
        requirement: Requirement::Required,
        stimulus: "две живые сессии (a — nvim, b — фикстура flood с малым темпом), одна \
                   поверхность, 10 циклов: RIS, attach a, ввод, отключение, RIS, attach b, ввод, \
                   отключение",
        oracle: "read_text и точка курсора после каждого attach, счётчик хода flood, квитанции \
                 ввода фикстуры, файл nvim на диске",
        completion: "10 циклов или срок одного ожидания",
    },
    PartSpec {
        name: "claude",
        place: Place::Headless,
        requirement: Requirement::Research,
        stimulus: "агент как есть со своей учётной записью и сетью, короткий запрос без \
                   инструментов с длинным ответом, SIGKILL клиента посреди ответа, attach, ввод \
                   следующего запроса",
        oracle: "наблюдатель процессов, номера строк ответа у приёмника до SIGKILL и после \
                 attach, эхо ввода",
        completion: "ответ дописан и эхо ввода видно или срок части",
    },
    PartSpec {
        name: "codex",
        place: Place::Headless,
        requirement: Requirement::Research,
        stimulus: "то же для Codex",
        oracle: "наблюдатель процессов, номера строк ответа у приёмника до SIGKILL и после \
                 attach, эхо ввода",
        completion: "ответ дописан и эхо ввода видно или срок части",
    },
];

pub fn part(name: &str) -> Option<&'static PartSpec> {
    PARTS.iter().find(|spec| spec.name == name)
}

pub fn parts_of(place: Place) -> impl Iterator<Item = &'static PartSpec> {
    PARTS.iter().filter(move |spec| spec.place == place)
}

/// Итог части прохода: что получено и почему.
#[derive(Debug, Default)]
pub struct Outcome {
    pub evidence: Evidence,
    pub artifacts: Vec<String>,
    /// Где дефект при `fail`.
    pub origin: Option<DefectOrigin>,
}

impl Outcome {
    /// `not_run` с причиной: стенд не доказал условие (`stand`) или контроль без хоста провален
    /// (`surface`).
    pub fn not_run(cause: NotRunCause, reason: impl Into<String>) -> Outcome {
        let mut outcome = Outcome::default();
        outcome.mark_not_run(cause, reason);
        outcome
    }

    /// Первая причина `not_run` остаётся: она объясняет, почему часть не дошла до конца.
    pub fn mark_not_run(&mut self, cause: NotRunCause, reason: impl Into<String>) {
        if self.evidence.not_proven.is_none() {
            self.evidence.not_proven = Some(reason.into());
            self.evidence.cause = Some(cause);
        }
    }
}

/// Клетка F4 для части `name` из каталога; неизвестная часть — ошибка стенда.
pub fn cell(name: &str, outcome: &Outcome) -> Result<Cell, String> {
    let spec = part(name).ok_or_else(|| format!("часть F4 «{name}» не из каталога"))?;
    let mut cell = evidence::cell(
        evidence::Spec {
            id: F4,
            part: Some(spec.name),
            stimulus: spec.stimulus.to_owned(),
            oracle: spec.oracle,
            completion: spec.completion,
            origin: outcome.origin.unwrap_or(DefectOrigin::Unknown),
        },
        &outcome.evidence,
        outcome.artifacts.clone(),
    );
    cell.requirement = spec.requirement;
    Ok(cell)
}

// --- свод частей ---

/// Итог одной части в одном прогоне.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartResult {
    pub part: String,
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<NotRunCause>,
}

/// Итог клетки F4 кандидата по частям всех прогонов.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdict {
    pub status: Status,
    /// Обязательные части не в `pass`: «часть: статус (причина)».
    pub open: Vec<String>,
    /// Части исследования с итогом.
    pub research: Vec<String>,
    /// Итог не исключает кандидата по пункту 1 правила выбора: `pass` или `not_run` только по
    /// причинам `surface` и `stand`.
    pub keeps_candidate: bool,
}

fn describe(part: &str, status: Status, cause: Option<NotRunCause>) -> String {
    match cause {
        Some(cause) => format!("{part}: {} ({})", status.name(), cause.name()),
        None => format!("{part}: {}", status.name()),
    }
}

/// Тяжесть причины `not_run` для свода повторов части: причина, исключающая кандидата, важнее.
fn cause_weight(cause: Option<NotRunCause>) -> u8 {
    match cause {
        Some(NotRunCause::Surface) => 1,
        Some(NotRunCause::Stand) => 2,
        Some(NotRunCause::Excluded | NotRunCause::Pending) | None => 3,
    }
}

/// Свод частей F4 одного кандидата. Повтор части в нескольких прогонах сводится по
/// [`Status::combine`]; у `not_run` остаётся самая тяжёлая причина.
pub fn rollup(results: &[PartResult]) -> Result<Verdict, String> {
    let mut parts: BTreeMap<&str, (Status, Option<NotRunCause>)> = BTreeMap::new();
    for result in results {
        let spec = part(&result.part)
            .ok_or_else(|| format!("часть F4 «{}» не из каталога", result.part))?;
        let cause = (result.status == Status::NotRun)
            .then_some(result.cause)
            .flatten();
        let entry = parts.entry(spec.name).or_insert((result.status, cause));
        let combined = entry.0.combine(result.status);
        entry.1 = match (entry.0, result.status) {
            _ if combined != Status::NotRun => None,
            (Status::NotRun, Status::NotRun) if cause_weight(cause) > cause_weight(entry.1) => {
                cause
            }
            (Status::NotRun, _) => entry.1,
            _ => cause,
        };
        entry.0 = combined;
    }
    let mut status = Status::Pass;
    let mut open = Vec::new();
    let mut research = Vec::new();
    let mut keeps = true;
    for spec in &PARTS {
        let (part_status, cause) = parts
            .get(spec.name)
            .copied()
            .unwrap_or((Status::NotRun, Some(NotRunCause::Pending)));
        match spec.requirement {
            Requirement::Required => {
                status = status.combine(part_status);
                if part_status != Status::Pass {
                    open.push(describe(spec.name, part_status, cause));
                    keeps &= part_status == Status::NotRun
                        && matches!(cause, Some(NotRunCause::Surface | NotRunCause::Stand));
                }
            }
            Requirement::Research => {
                research.push(describe(spec.name, part_status, cause));
            }
        }
    }
    Ok(Verdict {
        status,
        open,
        research,
        keeps_candidate: keeps,
    })
}

/// Части F4 строки сводки как итоги частей; часть без имени — ошибка стенда.
pub fn part_results(parts: &[CellPart]) -> Result<Vec<PartResult>, String> {
    parts
        .iter()
        .map(|part| {
            let name = part
                .part
                .clone()
                .ok_or_else(|| format!("часть F4 без имени в {}", part.dir))?;
            Ok(PartResult {
                part: name,
                status: part.status,
                cause: part.not_run_cause,
            })
        })
        .collect()
}

/// Итог F4 каждого кандидата по сводкам прогонов: части `apps` и `apps-surface` идут разными
/// целями и лежат в разных каталогах прогонов.
pub fn verdicts(summaries: &[SummaryFile]) -> Result<Vec<(Candidate, Verdict)>, String> {
    Candidate::ALL
        .into_iter()
        .map(|candidate| {
            let mut results = Vec::new();
            for summary in summaries {
                for row in summary
                    .cells
                    .iter()
                    .filter(|row| row.id == F4 && row.backend == Some(candidate))
                {
                    results.extend(part_results(&row.parts)?);
                }
            }
            Ok((candidate, rollup(&results)?))
        })
        .collect()
}

/// Текст итога F4 по кандидатам.
pub fn render_verdicts(verdicts: &[(Candidate, Verdict)]) -> String {
    let mut text = String::new();
    for (candidate, verdict) in verdicts {
        text.push_str(&format!(
            "F4 {}: {}{}\n",
            candidate.name(),
            verdict.status.name(),
            if verdict.keeps_candidate {
                ""
            } else {
                " (исключает кандидата по пункту 1)"
            }
        ));
        for open in &verdict.open {
            text.push_str(&format!("  обязательная {open}\n"));
        }
        for research in &verdict.research {
            text.push_str(&format!("  исследование {research}\n"));
        }
    }
    text
}

// --- экраны ---

/// Видимый экран: строки и позиция курсора в единицах источника (клетки приёмника или точки
/// поверхности).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Screen {
    pub lines: Vec<String>,
    pub cursor: Option<Point>,
}

/// Позиция курсора в единицах источника.
pub type Point = (f64, f64);

/// Расхождение экранов: строки с разным текстом и курсор.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ScreenDiff {
    /// Номер строки с нуля, строка до и строка после.
    pub rows: Vec<(usize, String, String)>,
    /// Курсор до и после, если различаются.
    pub cursor: Option<(Option<Point>, Option<Point>)>,
}

impl ScreenDiff {
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty() && self.cursor.is_none()
    }

    pub fn summary(&self) -> String {
        let mut parts: Vec<String> = self
            .rows
            .iter()
            .take(3)
            .map(|(row, before, after)| format!("строка {row}: «{before}» → «{after}»"))
            .collect();
        if self.rows.len() > 3 {
            parts.push(format!("и ещё строк: {}", self.rows.len() - 3));
        }
        if let Some((before, after)) = &self.cursor {
            parts.push(format!("курсор {before:?} → {after:?}"));
        }
        parts.join("; ")
    }
}

/// Сравнение экрана после повторного подключения с экраном до него. Хвостовые пробелы строк не
/// различаются, недостающая строка равна пустой. Курсор без наблюдения не равен никакому.
pub fn compare_screens(before: &Screen, after: &Screen) -> ScreenDiff {
    let rows = before.lines.len().max(after.lines.len());
    let line = |lines: &[String], row: usize| {
        lines
            .get(row)
            .map(|text| text.trim_end().to_owned())
            .unwrap_or_default()
    };
    let mut diff = ScreenDiff::default();
    for row in 0..rows {
        let (old, new) = (line(&before.lines, row), line(&after.lines, row));
        if old != new {
            diff.rows.push((row, old, new));
        }
    }
    let same_cursor = match (before.cursor, after.cursor) {
        (Some(old), Some(new)) => (old.0 - new.0).abs() < 0.5 && (old.1 - new.1).abs() < 0.5,
        _ => false,
    };
    if !same_cursor {
        diff.cursor = Some((before.cursor, after.cursor));
    }
    diff
}

// --- nvim ---

/// Файл nvim части: исходные строки и строка, вставленная после `after`.
pub struct NvimFile {
    pub original: Vec<String>,
    /// Номер строки с единицы, после которой открыта вставка (`o`).
    pub after: usize,
}

/// Метки вставленной строки: текст до первого Ctrl-V Shift-Enter, между ними и после второго.
pub const NVIM_BEFORE: &str = "pss-before ";
pub const NVIM_MID: &str = " pss-mid";
pub const NVIM_AFTER: &str = " pss-after";

/// Разбор файла после `:wqa`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct NvimInsert {
    /// Что дал Ctrl-V Shift-Enter до SIGKILL и после нового attach, hex.
    pub first: String,
    pub second: String,
    /// Остальные строки файла равны исходным.
    pub rest_intact: bool,
}

impl NvimInsert {
    pub fn same(&self) -> bool {
        !self.first.is_empty() && self.first == self.second
    }
}

impl NvimFile {
    /// `n` строк вида «NNN pss line NNN»: несколько экранов при 24 строках.
    pub fn numbered(n: usize) -> NvimFile {
        NvimFile {
            original: (1..=n).map(|i| format!("{i:03} pss line {i:03}")).collect(),
            after: n / 2,
        }
    }

    pub fn contents(&self) -> String {
        let mut text = self.original.join("\n");
        text.push('\n');
        text
    }

    /// Разбор файла: вставленная строка после `after` и сохранность остальных. `Err` — строки с
    /// метками нет или метки не по порядку.
    pub fn parse(&self, bytes: &[u8]) -> Result<NvimInsert, String> {
        let lines: Vec<&[u8]> = bytes
            .strip_suffix(b"\n")
            .unwrap_or(bytes)
            .split(|byte| *byte == b'\n')
            .collect();
        let inserted = lines
            .get(self.after)
            .ok_or_else(|| format!("в файле {} строк, вставки нет", lines.len()))?;
        let (first, second) = split_marks(inserted)
            .ok_or_else(|| format!("строка {} без меток: {}", self.after + 1, hex(inserted)))?;
        let mut rest: Vec<&[u8]> = lines.clone();
        rest.remove(self.after);
        let rest_intact = rest.len() == self.original.len()
            && rest
                .iter()
                .zip(&self.original)
                .all(|(line, original)| *line == original.as_bytes());
        Ok(NvimInsert {
            first: hex(first),
            second: hex(second),
            rest_intact,
        })
    }
}

fn split_marks(line: &[u8]) -> Option<(&[u8], &[u8])> {
    let rest = line.strip_prefix(NVIM_BEFORE.as_bytes())?;
    let mid = find(rest, NVIM_MID.as_bytes())?;
    let (first, tail) = (&rest[..mid], &rest[mid + NVIM_MID.len()..]);
    let second = tail.strip_suffix(NVIM_AFTER.as_bytes())?;
    Some((first, second))
}

/// Строка экрана, в которой видна метка, и столбец сразу после неё: курсор после ввода метки.
pub fn mark_position(lines: &[String], mark: &str) -> Option<(u16, u16)> {
    lines.iter().enumerate().find_map(|(row, line)| {
        line.find(mark).map(|at| {
            let col = line[..at].chars().count() + mark.chars().count();
            (
                u16::try_from(col).unwrap_or(u16::MAX),
                u16::try_from(row).unwrap_or(u16::MAX),
            )
        })
    })
}

// --- вставка ---

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

/// Блоки скобочной вставки во входе фикстуры: содержимое между `CSI 200 ~` и `CSI 201 ~`;
/// незакрытый блок не считается.
pub fn paste_blocks(input: &[u8]) -> Vec<Vec<u8>> {
    let mut blocks = Vec::new();
    let mut rest = input;
    while let Some(start) = find(rest, PASTE_START) {
        let body = &rest[start + PASTE_START.len()..];
        let Some(end) = find(body, PASTE_END) else {
            break;
        };
        blocks.push(body[..end].to_vec());
        rest = &body[end + PASTE_END.len()..];
    }
    blocks
}

/// Текст вставки части `paste`: строки с отступами, без которых «лестница» автоотступа nvim
/// ничего бы не доказала.
pub const PASTE_TEXT: &str = "pss paste begin\n  two spaces\n    four spaces\n  two again\nend";

/// Содержимое блока вставки как текст вставки: поверхность передаёт перевод строки как CR.
pub fn paste_text(block: &[u8]) -> String {
    String::from_utf8_lossy(block)
        .replace("\r\n", "\n")
        .replace('\r', "\n")
}

// --- кодирование клавиш ---

/// Расхождения кодирования клавиш с ожидаемым: (клавиша, ожидалось hex, получено hex).
pub fn encoding_mismatches(
    expected: &[(String, String)],
    actual: &[(String, String)],
) -> Vec<(String, String, String)> {
    let mut mismatches = Vec::new();
    for (label, want) in expected {
        let got = actual
            .iter()
            .find(|(name, _)| name == label)
            .map(|(_, bytes)| bytes.clone())
            .unwrap_or_else(|| "нет".to_owned());
        if &got != want {
            mismatches.push((label.clone(), want.clone(), got));
        }
    }
    for (label, got) in actual {
        if !expected.iter().any(|(name, _)| name == label) {
            mismatches.push((label.clone(), "нет".to_owned(), got.clone()));
        }
    }
    mismatches
}

// --- ответ агента ---

/// Запрос агенту: длинный ответ без инструментов, по номеру в строке.
pub const AGENT_COUNT: u64 = 150;
pub const AGENT_PROMPT: &str = "Reply with the integers from 1 to 150, one per line, digits only, \
                                and nothing else. Do not use any tools.";
/// Ввод следующего запроса без отправки: эхо в поле ввода агента.
pub const AGENT_NEXT: &str = "pss next";
/// Контроль эха у первого клиента до `SIGKILL`.
pub const AGENT_CONTROL: &str = "pss ctl ";

/// Номера строк ответа: строка, в которой после рамок, маркеров и пробелов стоит только число.
pub fn answer_numbers(lines: &[String]) -> Vec<u64> {
    lines
        .iter()
        .filter_map(|line| {
            let trimmed = line
                .trim()
                .trim_start_matches(|c: char| !c.is_ascii_alphanumeric())
                .trim_end_matches(|c: char| !c.is_ascii_alphanumeric());
            (!trimmed.is_empty() && trimmed.bytes().all(|byte| byte.is_ascii_digit()))
                .then(|| trimmed.parse().ok())
                .flatten()
        })
        .collect()
}

/// Сводка номеров ответа без текста переписки.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Numbers {
    pub count: usize,
    pub first: Option<u64>,
    pub last: Option<u64>,
    /// Номера из 1..=`total`, которых нет.
    pub missing: usize,
    /// Повторы номеров.
    pub repeats: usize,
}

impl Numbers {
    pub fn of(numbers: &[u64], total: u64) -> Numbers {
        let mut seen = std::collections::BTreeSet::new();
        let mut repeats = 0;
        for number in numbers {
            if !seen.insert(*number) {
                repeats += 1;
            }
        }
        Numbers {
            count: numbers.len(),
            first: numbers.first().copied(),
            last: numbers.last().copied(),
            missing: (1..=total).filter(|n| !seen.contains(n)).count(),
            repeats,
        }
    }

    /// Ответ дописан целиком: все номера на месте, по одному разу.
    pub fn complete(&self) -> bool {
        self.missing == 0 && self.repeats == 0
    }
}

/// `before` — подпоследовательность `after`: всё, что приёмник видел до SIGKILL, осталось по
/// порядку.
pub fn kept_in_order(before: &[u64], after: &[u64]) -> bool {
    let mut rest = after.iter();
    before.iter().all(|number| rest.any(|seen| seen == number))
}

// --- общее ---

/// Нагрузка ждёт строку от первого клиента и только потом становится программой: её запросы при
/// старте (протокол Kitty, DA1) уходят терминалу клиента, а не в пустоту. Аргументы `sh -c`:
/// программа и её аргументы.
pub const START_ON_LINE: &str = "IFS= read -r _ && exec \"$0\" \"$@\"";

/// Программа по имени в каталогах `PATH`.
pub fn which(name: &str, path: &str) -> Option<std::path::PathBuf> {
    path.split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| std::path::PathBuf::from(dir).join(name))
        .find(|candidate| candidate.is_file())
}

pub fn hex(bytes: &[u8]) -> String {
    crate::fixture::channel::hex(bytes)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(part: &str, status: Status, cause: Option<NotRunCause>) -> PartResult {
        PartResult {
            part: part.to_owned(),
            status,
            cause,
        }
    }

    fn all_required(status: Status) -> Vec<PartResult> {
        PARTS
            .iter()
            .filter(|spec| spec.requirement == Requirement::Required)
            .map(|spec| result(spec.name, status, None))
            .collect()
    }

    #[test]
    fn catalog_matches_declared_profile() {
        let names: Vec<&str> = PARTS.iter().map(|spec| spec.name).collect();
        assert_eq!(
            names,
            [
                "nvim",
                "nvim-surface",
                "paste",
                "kitty",
                "layout-ru",
                "dead-key",
                "ime",
                "switch",
                "claude",
                "codex"
            ]
        );
        let research: Vec<&str> = PARTS
            .iter()
            .filter(|spec| spec.requirement == Requirement::Research)
            .map(|spec| spec.name)
            .collect();
        assert_eq!(research, ["ime", "claude", "codex"]);
        assert_eq!(parts_of(Place::Headless).count(), 3);
        assert_eq!(parts_of(Place::Surface).count(), 7);
        assert!(part("nvim").is_some() && part("vim").is_none());
    }

    #[test]
    fn rollup_passes_only_with_every_required_part() {
        let verdict = rollup(&all_required(Status::Pass)).expect("свод");
        assert_eq!(verdict.status, Status::Pass);
        assert!(verdict.open.is_empty() && verdict.keeps_candidate);
        assert_eq!(
            verdict.research,
            [
                "ime: not_run (pending)",
                "claude: not_run (pending)",
                "codex: not_run (pending)"
            ]
        );

        let mut missing = all_required(Status::Pass);
        missing.retain(|result| result.part != "switch");
        let verdict = rollup(&missing).expect("свод");
        assert_eq!(verdict.status, Status::NotRun);
        assert_eq!(verdict.open, ["switch: not_run (pending)"]);
        assert!(!verdict.keeps_candidate);
    }

    #[test]
    fn rollup_failure_wins_and_research_does_not_count() {
        let mut results = all_required(Status::Pass);
        results.push(result("claude", Status::Fail, None));
        results.push(result("ime", Status::NotRun, Some(NotRunCause::Stand)));
        let verdict = rollup(&results).expect("свод");
        assert_eq!(verdict.status, Status::Pass);
        assert_eq!(
            verdict.research,
            [
                "ime: not_run (stand)",
                "claude: fail",
                "codex: not_run (pending)"
            ]
        );

        results.push(result("kitty", Status::Fail, None));
        results.push(result("paste", Status::NotRun, Some(NotRunCause::Surface)));
        let verdict = rollup(&results).expect("свод");
        assert_eq!(verdict.status, Status::Fail);
        assert_eq!(verdict.open, ["paste: not_run (surface)", "kitty: fail"]);
        assert!(!verdict.keeps_candidate);
    }

    #[test]
    fn rollup_keeps_candidate_on_surface_and_stand_only() {
        let mut results = all_required(Status::Pass);
        results.push(result(
            "layout-ru",
            Status::NotRun,
            Some(NotRunCause::Stand),
        ));
        results.push(result(
            "dead-key",
            Status::NotRun,
            Some(NotRunCause::Surface),
        ));
        let verdict = rollup(&results).expect("свод");
        assert_eq!(verdict.status, Status::NotRun);
        assert_eq!(
            verdict.open,
            ["layout-ru: not_run (stand)", "dead-key: not_run (surface)"]
        );
        assert!(verdict.keeps_candidate);

        results.push(result("switch", Status::NotRun, Some(NotRunCause::Pending)));
        assert!(!rollup(&results).expect("свод").keeps_candidate);
        assert!(rollup(&[result("vim", Status::Pass, None)]).is_err());
    }

    #[test]
    fn cell_takes_requirement_and_cause_from_catalog_and_outcome() {
        let outcome = Outcome::not_run(NotRunCause::Surface, "контроль без хоста провален");
        let research = cell("ime", &outcome).expect("клетка");
        assert_eq!(research.id, F4);
        assert_eq!(research.part.as_deref(), Some("ime"));
        assert_eq!(research.requirement, Requirement::Research);
        assert_eq!(research.status, Status::NotRun);
        assert_eq!(research.not_run_cause, Some(NotRunCause::Surface));
        assert!(research.problems().is_empty(), "{:?}", research.problems());

        let mut outcome = Outcome::default();
        outcome.evidence.check("файл", false, "лестница отступов");
        outcome.origin = Some(DefectOrigin::Host);
        outcome.artifacts.push("f4-paste.json".to_owned());
        let required = cell("paste", &outcome).expect("клетка");
        assert_eq!(required.status, Status::Fail);
        assert_eq!(required.requirement, Requirement::Required);
        assert_eq!(required.defect_origin, Some(DefectOrigin::Host));
        assert!(required.problems().is_empty(), "{:?}", required.problems());
        assert!(cell("vim", &Outcome::default()).is_err());
    }

    #[test]
    fn screens_compare_rows_and_cursor() {
        let before = Screen {
            lines: vec!["  1 one".to_owned(), "-- INSERT --".to_owned()],
            cursor: Some((4.0, 1.0)),
        };
        let mut after = before.clone();
        after.lines[0].push_str("   ");
        after.lines.push(String::new());
        assert!(compare_screens(&before, &after).is_empty());

        after.lines[1] = String::new();
        after.cursor = Some((5.0, 1.0));
        let diff = compare_screens(&before, &after);
        assert_eq!(diff.rows, [(1, "-- INSERT --".to_owned(), String::new())]);
        assert_eq!(diff.cursor, Some((Some((4.0, 1.0)), Some((5.0, 1.0)))));
        assert!(diff.summary().contains("строка 1"));

        after = before.clone();
        after.cursor = None;
        assert!(!compare_screens(&before, &after).is_empty());
    }

    #[test]
    fn nvim_file_parses_both_inserts() {
        let file = NvimFile::numbered(6);
        assert_eq!(file.after, 3);
        assert_eq!(file.original[0], "001 pss line 001");
        let mut lines = file.original.clone();
        lines.insert(3, "pss-before <S-CR> pss-mid<S-CR> pss-after".to_owned());
        let written = format!("{}\n", lines.join("\n"));
        let insert = file.parse(written.as_bytes()).expect("разбор");
        assert_eq!(insert.first, hex(b"<S-CR>"));
        assert!(insert.same() && insert.rest_intact);

        lines[3] = "pss-before \r pss-mid<S-CR> pss-after".to_owned();
        lines[0] = "changed".to_owned();
        let insert = file
            .parse(format!("{}\n", lines.join("\n")).as_bytes())
            .expect("разбор");
        assert!(!insert.same() && !insert.rest_intact);
        assert!(file.parse(file.contents().as_bytes()).is_err());

        let screen = vec!["  61 pss-before x pss-mid".to_owned()];
        assert_eq!(mark_position(&screen, NVIM_MID), Some((25, 0)));
        assert_eq!(mark_position(&screen, "нет"), None);
    }

    #[test]
    fn paste_blocks_are_split_and_normalized() {
        let input = b"x\x1b[200~a\rb\x1b[201~y\x1b[200~c\x1b[201~\x1b[200~open";
        let blocks = paste_blocks(input);
        assert_eq!(blocks, [b"a\rb".to_vec(), b"c".to_vec()]);
        assert_eq!(paste_text(&blocks[0]), "a\nb");
        assert_eq!(paste_text(b"a\r\nb"), "a\nb");
        assert!(paste_blocks(b"plain").is_empty());
        assert!(PASTE_TEXT.lines().any(|line| line.starts_with("    ")));
    }

    #[test]
    fn encodings_report_every_mismatch() {
        let pair = |label: &str, bytes: &str| (label.to_owned(), bytes.to_owned());
        let expected = vec![pair("ctrl-a", "01"), pair("shift-enter", "1b5b31333b3275")];
        assert!(encoding_mismatches(&expected, &expected).is_empty());
        let actual = vec![
            pair("ctrl-a", "01"),
            pair("shift-enter", "0d"),
            pair("x", "78"),
        ];
        assert_eq!(
            encoding_mismatches(&expected, &actual),
            [
                (
                    "shift-enter".to_owned(),
                    "1b5b31333b3275".to_owned(),
                    "0d".to_owned()
                ),
                ("x".to_owned(), "нет".to_owned(), "78".to_owned()),
            ]
        );
        assert_eq!(
            encoding_mismatches(&expected, &actual[..1]),
            [(
                "shift-enter".to_owned(),
                "1b5b31333b3275".to_owned(),
                "нет".to_owned()
            )]
        );
    }

    #[test]
    fn answer_numbers_skip_frames_and_text() {
        let lines: Vec<String> = ["⏺ 1", "  2", "│ 3 │", "text 4", "", "5.", "• 6", "1e3"]
            .iter()
            .map(|line| (*line).to_owned())
            .collect();
        assert_eq!(answer_numbers(&lines), [1, 2, 3, 5, 6]);
        let numbers = Numbers::of(&[1, 2, 2, 4], 4);
        assert_eq!(
            numbers,
            Numbers {
                count: 4,
                first: Some(1),
                last: Some(4),
                missing: 1,
                repeats: 1
            }
        );
        assert!(!numbers.complete());
        assert!(Numbers::of(&[1, 2, 3], 3).complete());
        assert!(kept_in_order(&[2, 3], &[1, 2, 3, 4]));
        assert!(!kept_in_order(&[3, 2], &[1, 2, 3, 4]));
        assert!(kept_in_order(&[], &[1]));
    }
}
