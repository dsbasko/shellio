//! Жизненный цикл клетки (план P0.7, «Живой контур»): ветки, коммиты фикстуры через API, PR/MR,
//! смена head и уборка, в том числе после ошибки и паники сценария.
//!
//! У клетки две ветки: цель `probe/<run-id>-<клетка>-base` от головы ветки по умолчанию с исходной
//! версией фикстуры и ветка клетки `probe/<run-id>-<клетка>` от неё с изменённой версией; PR/MR идёт
//! из второй в первую. Ветка по умолчанию не меняется, а diff содержит удалённые, изменённые и
//! добавленные строки.
//!
//! Созданное пишется в журнал каталога процесса ([`LEDGER_FILE`], строка JSON на объект): имя
//! ветки — до запроса создания, номер PR/MR — сразу после ответа. Уборка идёт по журналу и
//! дополнительно ищет открытые PR/MR по исходным веткам журнала: ответ на создание мог потеряться.
//! [`resume_cleanup`] продолжает уборку по журналу из другого процесса, например после `SIGKILL`.

use std::collections::BTreeSet;
use std::fmt;
use std::fs::OpenOptions;
use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::repo::{Account, FileChange, LiveError, LiveRepo};

pub const LEDGER_FILE: &str = "live-objects.jsonl";
pub const FIXTURE_PATH: &str = "probe-review/fixture.txt";
const BRANCH_ROOT: &str = "probe/";

/// Объект песочницы, созданный клеткой.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Created {
    /// Ветка: записывается до запроса создания, имя известно заранее.
    Branch { name: String },
    /// PR (GitHub) или MR (`iid` GitLab): записывается сразу после ответа на создание.
    Pull { number: u64 },
}

/// Журнал созданного в каталоге процесса.
pub struct Ledger {
    path: PathBuf,
    entries: Vec<Created>,
}

impl Ledger {
    /// Новый журнал; существующий файл не перезаписывается: в нём может быть недоубранное.
    pub fn create(path: &Path) -> Result<Ledger, String> {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|err| format!("журнал созданного не создан: {}", err.kind()))?;
        Ok(Ledger {
            path: path.to_path_buf(),
            entries: Vec::new(),
        })
    }

    /// Записи журнала. Последняя строка без перевода строки — оборванная запись, она
    /// пропускается: ветка записывается до создания, а PR/MR уборка находит по ветке.
    pub fn load(path: &Path) -> Result<Vec<Created>, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|err| format!("журнал созданного не прочитан: {}", err.kind()))?;
        let complete = match text.rfind('\n') {
            Some(end) => &text[..end],
            None => "",
        };
        complete
            .lines()
            .filter(|line| !line.trim().is_empty())
            .enumerate()
            .map(|(index, line)| {
                serde_json::from_str(line)
                    .map_err(|err| format!("журнал созданного, запись {}: {err}", index + 1))
            })
            .collect()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn entries(&self) -> &[Created] {
        &self.entries
    }

    /// Дописывает запись и сбрасывает её на диск до следующего запроса.
    pub fn record(&mut self, entry: Created) -> Result<(), String> {
        let mut line = serde_json::to_string(&entry).map_err(|err| err.to_string())?;
        line.push('\n');
        let mut file = OpenOptions::new()
            .append(true)
            .open(&self.path)
            .map_err(|err| format!("журнал созданного не открыт: {}", err.kind()))?;
        file.write_all(line.as_bytes())
            .and_then(|()| file.sync_data())
            .map_err(|err| format!("журнал созданного не записан: {}", err.kind()))?;
        self.entries.push(entry);
        Ok(())
    }
}

/// Версия фикстуры: 30 строк основы; у клетки строка 03 удалена, 05 изменена, после 24
/// добавлены две; смена head меняет строку 27 во втором блоке diff, первый блок не трогает.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fixture {
    Base,
    Head,
    Moved,
}

impl Fixture {
    pub fn content(self) -> String {
        let mut lines = Vec::new();
        for n in 1..=30 {
            let line = format!("line {n:02}");
            match (self, n) {
                (Fixture::Head | Fixture::Moved, 3) => {}
                (Fixture::Head | Fixture::Moved, 5) => lines.push(format!("{line} changed")),
                (Fixture::Head | Fixture::Moved, 24) => {
                    lines.push(line);
                    lines.push("line 24a".to_owned());
                    lines.push("line 24b".to_owned());
                }
                (Fixture::Moved, 27) => lines.push(format!("{line} moved")),
                _ => lines.push(line),
            }
        }
        let mut text = lines.join("\n");
        text.push('\n');
        text
    }

    fn message(self) -> &'static str {
        match self {
            Fixture::Base => "probe-review: fixture base",
            Fixture::Head => "probe-review: fixture head",
            Fixture::Moved => "probe-review: move head",
        }
    }
}

/// Ветки клетки.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CellNames {
    /// Цель PR/MR: `probe/<run-id>-<клетка>-base`.
    pub base: String,
    /// Исходная ветка: `probe/<run-id>-<клетка>`.
    pub head: String,
}

impl CellNames {
    pub fn new(run_id: &str, cell: &str) -> Result<CellNames, String> {
        for (what, value) in [("run-id", run_id), ("клетка", cell)] {
            let valid = !value.is_empty()
                && !value.starts_with('-')
                && value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'));
            if !valid {
                return Err(format!("{what} «{value}»: только буквы, цифры, - и _"));
            }
        }
        let head = format!("{}{cell}", run_prefix(run_id));
        Ok(CellNames {
            base: format!("{head}-base"),
            head,
        })
    }
}

/// Общее начало веток всех клеток прогона: `probe/<run-id>-`.
pub fn run_prefix(run_id: &str) -> String {
    format!("{BRANCH_ROOT}{run_id}-")
}

/// Роли клетки: автор создаёт ветки и PR/MR, ревьюер ведёт черновики (их видит только он).
#[derive(Clone, Copy)]
pub struct Roles<'a> {
    pub author: &'a dyn LiveRepo,
    pub reviewer: Option<&'a dyn LiveRepo>,
}

/// Открытая клетка.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    pub names: CellNames,
    pub default_branch: String,
    pub number: u64,
    pub base_commit: String,
    pub head_commit: String,
    /// Аккаунт ревьюера; на GitLab он назначен reviewer у MR.
    pub reviewer: Option<Account>,
}

/// Шаг клетки и его ошибка.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepError {
    pub step: &'static str,
    pub error: LiveError,
}

impl fmt::Display for StepError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.step, self.error)
    }
}

impl std::error::Error for StepError {}

fn step<T>(step: &'static str, result: Result<T, LiveError>) -> Result<T, StepError> {
    result.map_err(|error| StepError { step, error })
}

fn record(ledger: &mut Ledger, entry: Created) -> Result<(), StepError> {
    ledger.record(entry).map_err(|reason| StepError {
        step: "журнал созданного",
        error: LiveError::Unexpected(reason),
    })
}

/// Ветки, фикстура и PR/MR клетки. Сторож каждой роли проверяет репозиторий до первой записи.
pub fn open_cell(
    roles: &Roles<'_>,
    ledger: &mut Ledger,
    names: &CellNames,
    title: &str,
) -> Result<Cell, StepError> {
    let author = roles.author;
    let info = step("сторож автора", author.check().map_err(LiveError::Guard))?;
    let default_sha = step(
        "голова ветки по умолчанию",
        author.branch_head(&info.default_branch).and_then(|sha| {
            sha.ok_or_else(|| LiveError::Unexpected("ветки по умолчанию нет".to_owned()))
        }),
    )?;
    let reviewer = match roles.reviewer {
        Some(reviewer) => {
            step(
                "сторож ревьюера",
                reviewer.check().map_err(LiveError::Guard),
            )?;
            Some(step("аккаунт ревьюера", reviewer.whoami())?)
        }
        None => None,
    };
    record(
        ledger,
        Created::Branch {
            name: names.base.clone(),
        },
    )?;
    step(
        "ветка цели",
        author.create_branch(&names.base, &default_sha),
    )?;
    let base_content = Fixture::Base.content();
    let base_commit = step(
        "коммит основы",
        author.commit_file(
            &names.base,
            &FileChange {
                path: FIXTURE_PATH,
                content: &base_content,
                message: Fixture::Base.message(),
                create: true,
            },
        ),
    )?;
    record(
        ledger,
        Created::Branch {
            name: names.head.clone(),
        },
    )?;
    step(
        "ветка клетки",
        author.create_branch(&names.head, &base_commit),
    )?;
    let head_content = Fixture::Head.content();
    let head_commit = step(
        "коммит клетки",
        author.commit_file(
            &names.head,
            &FileChange {
                path: FIXTURE_PATH,
                content: &head_content,
                message: Fixture::Head.message(),
                create: false,
            },
        ),
    )?;
    let number = step(
        "создание PR/MR",
        author.open_pull(&names.head, &names.base, title, reviewer.as_ref()),
    )?;
    record(ledger, Created::Pull { number })?;
    Ok(Cell {
        names: names.clone(),
        default_branch: info.default_branch,
        number,
        base_commit,
        head_commit,
        reviewer,
    })
}

impl Cell {
    /// Смена head: новый коммит фикстуры в ветку клетки тем же API.
    pub fn move_head(&mut self, author: &dyn LiveRepo, to: Fixture) -> Result<String, StepError> {
        let content = to.content();
        let commit = step(
            "смена head",
            author.commit_file(
                &self.names.head,
                &FileChange {
                    path: FIXTURE_PATH,
                    content: &content,
                    message: to.message(),
                    create: false,
                },
            ),
        )?;
        self.head_commit = commit.clone();
        Ok(commit)
    }
}

/// Итог уборки.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cleanup {
    pub removed: Vec<String>,
    /// Шаги, которые не удались; что осталось, решает чтение после уборки.
    pub errors: Vec<String>,
    /// Не убранное или не подтверждённое чтением.
    pub leftovers: Vec<String>,
}

/// Уборка по записям журнала: черновики ревьюера, PR/MR, ветки (ветка клетки до цели), затем
/// чтение каждого объекта и повторный поиск открытых PR/MR по каждой ветке. Неудачный шаг не
/// останавливает остальные.
pub fn cleanup(roles: &Roles<'_>, entries: &[Created]) -> Cleanup {
    let mut report = Cleanup::default();
    let mut branches: Vec<&str> = Vec::new();
    let mut pulls: BTreeSet<u64> = BTreeSet::new();
    for entry in entries {
        match entry {
            Created::Branch { name } if !branches.contains(&name.as_str()) => {
                branches.push(name);
            }
            Created::Branch { .. } => {}
            Created::Pull { number } => {
                pulls.insert(*number);
            }
        }
    }
    if branches.is_empty() && pulls.is_empty() {
        return report;
    }
    let author = roles.author;
    let word = author.pull_word();
    if let Err(err) = author.check() {
        report.leftovers.extend(
            pulls
                .iter()
                .map(|number| format!("{word} #{number}: не убран: {err}")),
        );
        report.leftovers.extend(
            branches
                .iter()
                .map(|branch| format!("ветка {branch}: не убрана: {err}")),
        );
        return report;
    }
    for branch in &branches {
        match author.open_pulls_from(branch) {
            Ok(found) => pulls.extend(found),
            Err(err) => report
                .errors
                .push(format!("поиск {word} из ветки {branch}: {err}")),
        }
    }
    let reviewer: Result<Option<&dyn LiveRepo>, String> = match roles.reviewer {
        Some(reviewer) => match reviewer.check() {
            Ok(_) => Ok(Some(reviewer)),
            Err(err) => Err(err.to_string()),
        },
        None => Ok(None),
    };
    if let Ok(Some(reviewer)) = reviewer {
        for &number in &pulls {
            match reviewer.own_drafts(number) {
                Ok(ids) => {
                    for id in ids {
                        match reviewer.delete_draft(number, &id) {
                            Ok(()) => report
                                .removed
                                .push(format!("черновик {id} в {word} #{number} удалён")),
                            Err(err) => report
                                .errors
                                .push(format!("удаление черновика {id} в {word} #{number}: {err}")),
                        }
                    }
                }
                Err(err) => report
                    .errors
                    .push(format!("черновики ревьюера в {word} #{number}: {err}")),
            }
        }
    }
    for &number in &pulls {
        let open = author.pull_is_open(number);
        if let Err(err) = &open {
            report
                .errors
                .push(format!("состояние {word} #{number}: {err}"));
        }
        if !matches!(open, Ok(false)) {
            match author.close_pull(number) {
                Ok(()) => report.removed.push(format!("{word} #{number} закрыт")),
                Err(err) => report
                    .errors
                    .push(format!("закрытие {word} #{number}: {err}")),
            }
        }
    }
    for branch in branches.iter().rev() {
        match author.delete_branch(branch) {
            Ok(()) => report.removed.push(format!("ветка {branch} удалена")),
            // Ветки нет: не создана или уже удалена (GitHub — 422, GitLab — 404).
            Err(err) if matches!(err.status(), Some(404 | 422)) => {}
            Err(err) => report
                .errors
                .push(format!("удаление ветки {branch}: {err}")),
        }
    }
    for &number in &pulls {
        match author.pull_is_open(number) {
            Ok(false) => {}
            Ok(true) => report.leftovers.push(format!("{word} #{number} открыт")),
            Err(err) => report
                .leftovers
                .push(format!("{word} #{number}: закрытие не подтверждено: {err}")),
        }
        match &reviewer {
            Ok(Some(reviewer)) => match reviewer.own_drafts(number) {
                Ok(ids) if ids.is_empty() => {}
                Ok(ids) => report.leftovers.push(format!(
                    "черновики ревьюера в {word} #{number}: {}",
                    ids.join(", ")
                )),
                Err(err) => report.leftovers.push(format!(
                    "черновики ревьюера в {word} #{number}: не подтверждено: {err}"
                )),
            },
            Ok(None) => {}
            Err(err) => report.leftovers.push(format!(
                "черновики ревьюера в {word} #{number}: не проверены: {err}"
            )),
        }
    }
    for branch in &branches {
        match author.branch_head(branch) {
            Ok(None) => {}
            Ok(Some(_)) => report.leftovers.push(format!("ветка {branch}")),
            Err(err) => report
                .leftovers
                .push(format!("ветка {branch}: удаление не подтверждено: {err}")),
        }
    }
    // Повторный поиск ловит PR/MR, который не попал в уборку: ответ на создание потерян, а первый
    // поиск отказал или прошёл раньше, чем создание дошло. MR GitLab переживает удаление ветки.
    let mut judged = pulls;
    for branch in &branches {
        match author.open_pulls_from(branch) {
            Ok(found) => {
                for number in found {
                    if judged.insert(number) {
                        report.leftovers.push(format!(
                            "{word} #{number} открыт: найден повторным поиском из ветки {branch}"
                        ));
                    }
                }
            }
            Err(err) => report.leftovers.push(format!(
                "поиск {word} из ветки {branch}: не подтверждено: {err}"
            )),
        }
    }
    report
}

/// Продолжение уборки по журналу созданного, например из другого процесса.
pub fn resume_cleanup(roles: &Roles<'_>, ledger: &Path) -> Result<Cleanup, String> {
    Ledger::load(ledger).map(|entries| cleanup(roles, &entries))
}

/// Итог клетки: итог сценария (ошибка открытия, ошибка или паника тела) и уборка.
#[derive(Debug)]
pub struct CellRun<R> {
    pub outcome: Result<R, String>,
    pub cleanup: Cleanup,
}

/// Открывает клетку, исполняет тело и убирает созданное в любом случае: после ошибки открытия,
/// после ошибки тела и после паники.
pub fn run_cell<R>(
    roles: &Roles<'_>,
    ledger: &mut Ledger,
    names: &CellNames,
    title: &str,
    body: impl FnOnce(&mut Cell) -> Result<R, String>,
) -> CellRun<R> {
    let outcome = match open_cell(roles, ledger, names, title) {
        Ok(mut cell) => match catch_unwind(AssertUnwindSafe(|| body(&mut cell))) {
            Ok(result) => result,
            Err(panic) => Err(format!("паника сценария: {}", panic_message(&*panic))),
        },
        Err(err) => Err(err.to_string()),
    };
    let cleanup = cleanup(roles, ledger.entries());
    CellRun { outcome, cleanup }
}

fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = panic.downcast_ref::<&str>() {
        (*text).to_owned()
    } else if let Some(text) = panic.downcast_ref::<String>() {
        text.clone()
    } else {
        "без сообщения".to_owned()
    }
}
