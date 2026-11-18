//! Восстановление попытки обновления по таблице восстановления плана P0.5, строка в строку.
//! Решение — чистая функция журнала и раскладки, уже классифицированной проверяльщиком
//! поколений ([`Checker`]); драйвер выполняет один шаг, перечитывает журнал и раскладку и
//! решает заново. Каждая мутация диска стоит после записанного в журнал решения. Срок попытки
//! восстановление не продлевает: запись идёт через [`Store::advance`], которая это запрещает.
//!
//! Строки, которые заканчиваются запуском процессов («далее как помощник», «запуск N»,
//! «передают аргументы»), возвращают вызывающему [`Next`]: запуск — задача помощника и владельца.

use std::fs;
use std::io;
use std::path::Path;

use crate::barrier::{self, Mutation, Point};
use crate::generation::{self, Verify};
use crate::journal::{Journal, JournalError, Outcome, STAGED_PREFIX, State, Store};
use crate::observer::ProcessKey;
use crate::sys;

/// Что нашёл проверяльщик по пути.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "seen", rename_all = "kebab-case")]
pub enum Seen {
    Absent,
    /// Подпись цела, требование выполнено; CDHash бандла.
    Valid {
        cdhash: String,
    },
    /// Путь есть, но проверку не проходит.
    Invalid {
        reason: String,
    },
}

/// Шов проверки: в `make check` — подставной проверяльщик, в сценариях — [`Codesign`].
pub trait Checker {
    /// `Err` — сама проверка не выполнилась, бандл не классифицирован.
    fn check(&self, path: &Path) -> Result<Seen, String>;
}

/// `codesign --verify --strict --deep -R=<требование>` вместе с CDHash из `codesign -d`.
pub struct Codesign {
    pub requirement: String,
}

impl Checker for Codesign {
    fn check(&self, path: &Path) -> Result<Seen, String> {
        match fs::symlink_metadata(path) {
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Seen::Absent),
            Err(err) => return Err(format!("{}: {err}", path.display())),
        }
        let mut runs = Vec::new();
        match generation::verify(path, &self.requirement, &mut runs)? {
            Verify::Satisfied => {}
            Verify::Failed { reason, .. } => return Err(reason),
            other => {
                return Ok(Seen::Invalid {
                    reason: other.name().to_owned(),
                });
            }
        }
        let details = generation::details(path, &mut runs)?
            .map_err(|err| format!("{}: codesign -d: {err}", path.display()))?;
        Ok(Seen::Valid {
            cdhash: details.cdhash,
        })
    }
}

/// Место раскладки относительно поколений попытки.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Slot {
    Absent,
    /// Поколение N попытки (CDHash `cdhash_from`).
    From,
    /// Поколение N+1 попытки (CDHash `cdhash_to`).
    To,
    /// Есть, но проверку не проходит: частичный каталог или повреждённый бандл.
    Partial(String),
    /// Проходит проверку, но CDHash не N и не N+1.
    Unknown(String),
}

impl Slot {
    fn name(&self) -> &'static str {
        match self {
            Slot::Absent => "нет",
            Slot::From => "N",
            Slot::To => "N+1",
            Slot::Partial(_) => "не проходит проверку",
            Slot::Unknown(_) => "незнакомое поколение",
        }
    }
}

/// Раскладка попытки: установленный путь, кандидат и отказавший N+1 этой попытки, чужие
/// каталоги `.shellio-update-*`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observed {
    pub installed: Slot,
    pub staged: Slot,
    pub failed: Slot,
    pub strangers: Vec<String>,
}

impl Observed {
    fn describe(&self) -> String {
        format!(
            "installed {}, кандидат {}, отказавший {}",
            self.installed.name(),
            self.staged.name(),
            self.failed.name()
        )
    }
}

pub fn observe(journal: &Journal, checker: &dyn Checker) -> Result<Observed, RecoverError> {
    let slot = |path: &Path| -> Result<Slot, RecoverError> {
        Ok(match checker.check(path).map_err(RecoverError::Checker)? {
            Seen::Absent => Slot::Absent,
            Seen::Invalid { reason } => Slot::Partial(reason),
            Seen::Valid { cdhash } if cdhash == journal.cdhash_from => Slot::From,
            Seen::Valid { cdhash } if cdhash == journal.cdhash_to => Slot::To,
            Seen::Valid { cdhash } => Slot::Unknown(cdhash),
        })
    };
    let own = journal
        .staged_dir()
        .file_name()
        .map(|name| name.to_string_lossy().into_owned());
    let install_dir = journal.installed.parent().unwrap_or(Path::new("/"));
    let mut strangers = Vec::new();
    match fs::read_dir(install_dir) {
        Ok(entries) => {
            for entry in entries {
                let name = entry
                    .map_err(|err| RecoverError::Io(err.to_string()))?
                    .file_name()
                    .to_string_lossy()
                    .into_owned();
                if name.starts_with(STAGED_PREFIX) && Some(&name) != own.as_ref() {
                    strangers.push(name);
                }
            }
        }
        Err(err) => {
            return Err(RecoverError::Io(format!(
                "{}: {err}",
                install_dir.display()
            )));
        }
    }
    strangers.sort();
    Ok(Observed {
        installed: slot(&journal.installed)?,
        staged: slot(&journal.staged_dir())?,
        failed: slot(&journal.failed_dir())?,
        strangers,
    })
}

/// Кто взял блокировку.
#[derive(Clone, Debug)]
pub struct Actor {
    pub key: ProcessKey,
    /// Номер попытки, если взявший — новый N+1 этой попытки: он получил `--update-attempt`, и
    /// вызывающий сверил CDHash его процесса с `cdhash_to` (задача 6).
    pub launched_for: Option<String>,
}

pub struct Context<'a> {
    pub now_ms: u64,
    pub actor: Actor,
    pub alive: &'a dyn Fn(&ProcessKey) -> bool,
}

/// Один шаг восстановления.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Запись состояния; у `idle` — с итогом.
    Write(State, Option<Outcome>),
    /// N+1 этой попытки записывает `launched` со своим ключом.
    Claim,
    /// Удаление каталога кандидата попытки (целого, частичного).
    Remove,
    /// Обратный `RENAME_SWAP` установленного пути и кандидата.
    Swap,
    /// Переименование отказавшего N+1 в `.shellio-failed-<attempt>`.
    Retire,
}

/// Чем закончилось восстановление для вызывающего.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next {
    /// Попытки нет или она закончена, запускать никого не нужно.
    Nothing,
    /// Попытка откачена: запустить установленный N.
    LaunchInstalled,
    /// `swapped`: выполнить `swapped → launched` как помощник.
    LaunchAsHelper,
    /// `launched`, взявший — владелец с ключом: продолжает.
    Owner,
    /// `launched`, владелец жив и срок не истёк: отпустить блокировку и передать аргументы.
    Yield,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Step(Step),
    Stop(Next),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoverError {
    Journal(JournalError),
    Checker(String),
    Io(String),
    /// Раскладка, не предусмотренная таблицей: незнакомое или не проходящее проверку
    /// поколение, каталог не этой попытки или сочетание вне строк.
    OffTable(String),
}

impl std::fmt::Display for RecoverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecoverError::Journal(err) => write!(f, "восстановление: {err}"),
            RecoverError::Checker(reason) => write!(f, "восстановление: проверка: {reason}"),
            RecoverError::Io(reason) => write!(f, "восстановление: {reason}"),
            RecoverError::OffTable(reason) => write!(f, "восстановление отказано: {reason}"),
        }
    }
}

impl From<JournalError> for RecoverError {
    fn from(err: JournalError) -> RecoverError {
        RecoverError::Journal(err)
    }
}

/// Решение по строке таблицы восстановления.
pub fn decide(journal: &Journal, seen: &Observed, ctx: &Context) -> Result<Decision, RecoverError> {
    use Decision::{Step as Do, Stop};
    use Slot::{Absent, From, Partial, To};
    if matches!(journal.state, State::Idle | State::StagedDeferred) {
        return Ok(Stop(Next::Nothing));
    }
    let off = |why: &str| {
        RecoverError::OffTable(format!(
            "{} {}: {}; {why}",
            journal.attempt,
            journal.state.name(),
            seen.describe()
        ))
    };
    if let Some(name) = seen.strangers.first() {
        return Err(off(&format!("каталог не этой попытки {name}")));
    }
    for slot in [&seen.installed, &seen.staged, &seen.failed] {
        if let Slot::Unknown(cdhash) = slot {
            return Err(off(&format!("незнакомое поколение {cdhash}")));
        }
    }
    if let Partial(reason) = &seen.installed {
        return Err(off(&format!(
            "установленный бандл не проходит проверку: {reason}"
        )));
    }
    if let Partial(reason) = &seen.failed {
        return Err(off(&format!(
            "отказавший N+1 не проходит проверку: {reason}"
        )));
    }
    let deletable = matches!(seen.staged, To | Partial(_) | Absent);
    let decision = match (journal.state, &seen.installed, &seen.staged, &seen.failed) {
        (State::Staged, From, _, Absent) if deletable => Do(Step::Write(State::Aborting, None)),
        (State::Intent, From, To, Absent) => Do(Step::Write(State::Aborting, None)),
        (State::Intent, To, From, Absent) => Do(Step::Write(State::Swapped, None)),
        (State::Aborting, From, To | Partial(_), Absent) => Do(Step::Remove),
        (State::Aborting, From, Absent, Absent) => {
            Do(Step::Write(State::Idle, Some(Outcome::Aborted)))
        }
        (State::Swapped, To, From, Absent) => {
            if ctx.actor.launched_for.as_deref() == Some(journal.attempt.as_str()) {
                Do(Step::Claim)
            } else {
                Stop(Next::LaunchAsHelper)
            }
        }
        (State::Launched, To, From, Absent) => {
            let expired = ctx.now_ms >= journal.deadline_ms.unwrap_or(0);
            let alive = journal.owner.is_some_and(|key| (ctx.alive)(&key));
            if expired || !alive {
                Do(Step::Write(State::RollbackIntent, None))
            } else if journal.owner == Some(ctx.actor.key) {
                Stop(Next::Owner)
            } else {
                Stop(Next::Yield)
            }
        }
        (State::RollbackIntent, To, From, Absent) => Do(Step::Swap),
        (State::RollbackIntent, From, To, Absent) => Do(Step::Write(State::RolledBack, None)),
        (State::RolledBack, From, To, Absent) => Do(Step::Retire),
        (State::RolledBack, From, Absent, To) => Do(Step::Write(State::Retired, None)),
        (State::Retired, From, Absent, To) => {
            Do(Step::Write(State::Idle, Some(Outcome::RolledBack)))
        }
        (State::Committed, To, From | Partial(_) | Absent, Absent) => {
            Do(Step::Write(State::Cleaning, None))
        }
        (State::Cleaning, To, From | Partial(_), Absent) => Do(Step::Remove),
        (State::Cleaning, To, Absent, Absent) => {
            Do(Step::Write(State::Idle, Some(Outcome::Committed)))
        }
        _ => return Err(off("раскладка вне таблицы")),
    };
    Ok(decision)
}

/// Что сделало восстановление.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub steps: Vec<Step>,
    pub next: Next,
}

/// Шагов в одной попытке не больше длины самой длинной цепочки таблицы с запасом.
const MAX_STEPS: usize = 16;

/// Восстановление под блокировкой вызывающего. Ошибка до первого шага ничего не меняет.
pub fn recover(
    store: &Store,
    checker: &dyn Checker,
    ctx: &Context,
) -> Result<Report, RecoverError> {
    let mut steps = Vec::new();
    loop {
        let Some(journal) = store.read()? else {
            return Ok(Report {
                steps,
                next: Next::Nothing,
            });
        };
        if journal.state == State::Idle {
            let next = if !steps.is_empty() && journal.outcome == Some(Outcome::RolledBack) {
                Next::LaunchInstalled
            } else {
                Next::Nothing
            };
            return Ok(Report { steps, next });
        }
        if journal.state == State::StagedDeferred {
            return Ok(Report {
                steps,
                next: Next::Nothing,
            });
        }
        if steps.len() >= MAX_STEPS {
            return Err(RecoverError::OffTable(format!(
                "восстановление не сходится: {steps:?}"
            )));
        }
        let seen = observe(&journal, checker)?;
        match decide(&journal, &seen, ctx)? {
            Decision::Stop(next) => return Ok(Report { steps, next }),
            Decision::Step(step) => {
                apply(store, &journal, step, ctx)?;
                steps.push(step);
            }
        }
    }
}

fn apply(store: &Store, journal: &Journal, step: Step, ctx: &Context) -> Result<(), RecoverError> {
    let io = |what: &str, err: io::Error| RecoverError::Io(format!("{what}: {err}"));
    let install_dir = journal.installed.parent().unwrap_or(Path::new("/"));
    let state = store.state();
    match step {
        Step::Write(state, outcome) => {
            store.advance(&journal.attempt, &[journal.state], |next| {
                next.state = state;
                next.outcome = outcome;
            })?;
        }
        Step::Claim => {
            store.advance(&journal.attempt, &[State::Swapped], |next| {
                next.state = State::Launched;
                next.owner = Some(ctx.actor.key);
            })?;
        }
        Step::Remove => mutate(state, Mutation::Remove, || {
            remove_bundle(state, &journal.staged_dir())
                .map_err(|err| io("удаление кандидата", err))?;
            sys::sync_dir(install_dir).map_err(|err| io("fsync каталога установки", err))
        })?,
        Step::Swap => mutate(state, Mutation::Swap, || {
            sys::swap_paths(&journal.installed, &journal.staged_dir())
                .map_err(|err| io("обратный обмен", err))?;
            sys::sync_dir(install_dir).map_err(|err| io("fsync каталога установки", err))
        })?,
        Step::Retire => mutate(state, Mutation::Retire, || {
            fs::rename(journal.staged_dir(), journal.failed_dir())
                .map_err(|err| io("переименование отказавшего N+1", err))?;
            sys::sync_dir(install_dir).map_err(|err| io("fsync каталога установки", err))
        })?,
    }
    Ok(())
}

/// Мутация диска между барьерами «до» и «после» (план P0.5, B2: «восстановление на каждой
/// своей мутации»).
fn mutate(
    state: &Path,
    mutation: Mutation,
    work: impl FnOnce() -> Result<(), RecoverError>,
) -> Result<(), RecoverError> {
    let point = |done| Point::Step { mutation, done };
    barrier::reach(state, point(false));
    work()?;
    barrier::reach(state, point(true));
    Ok(())
}

/// Удаление бандла в два шага: сначала исполняемые файлы, затем остальное. Между шагами — барьер
/// «посреди удаления»: каталог там уже частичный и проверку подписи не проходит.
pub fn remove_bundle(state: &Path, bundle: &Path) -> io::Result<()> {
    let gone = |result: io::Result<()>| match result {
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    };
    gone(fs::remove_dir_all(bundle.join("Contents").join("MacOS")))?;
    barrier::reach(state, Point::RemovePartial);
    gone(fs::remove_dir_all(bundle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::tests::{
        ATTEMPT, CD_FROM, CD_TO, DEADLINE, OWNER, journal_in, put, temp_dir,
    };
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    const MARK: &str = "cdhash";
    const OTHER: ProcessKey = ProcessKey {
        pid: 5151,
        start_us: 1,
    };

    /// Подставной проверяльщик: бандл с файлом `cdhash` проходит проверку с этим CDHash,
    /// каталог без него — частичный.
    struct Fake;

    impl Checker for Fake {
        fn check(&self, path: &Path) -> Result<Seen, String> {
            if !path.exists() {
                return Ok(Seen::Absent);
            }
            Ok(match fs::read_to_string(path.join(MARK)) {
                Ok(cdhash) => Seen::Valid { cdhash },
                Err(_) => Seen::Invalid {
                    reason: "нет подписи".to_owned(),
                },
            })
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum B {
        No,
        N,
        M,
        Part,
        Strange,
    }

    struct Bench {
        dir: PathBuf,
        store: Store,
        journal: Journal,
    }

    impl Bench {
        fn new(name: &str, state: State, installed: B, staged: B, failed: B) -> Bench {
            let dir = temp_dir(name);
            let app = dir.join("install").join("probe-update.app");
            fs::create_dir_all(dir.join("install")).unwrap();
            let journal = journal_in(state, &app);
            let store = Store::new(&dir.join("state"));
            put(&store, &journal);
            make(&app, installed);
            make(&journal.staged_dir(), staged);
            make(&journal.failed_dir(), failed);
            Bench {
                dir,
                store,
                journal,
            }
        }

        /// Журнал и дерево установки: байты журнала и отметки каждого бандла.
        fn snapshot(&self) -> (Vec<u8>, BTreeMap<String, String>) {
            let mut tree = BTreeMap::new();
            for entry in fs::read_dir(self.dir.join("install")).unwrap() {
                let path = entry.unwrap().path();
                let mark = fs::read_to_string(path.join(MARK)).unwrap_or_else(|_| "-".to_owned());
                tree.insert(
                    path.file_name().unwrap().to_string_lossy().into_owned(),
                    mark,
                );
            }
            (fs::read(self.store.path()).unwrap(), tree)
        }

        fn layout(&self) -> (Option<String>, Option<String>, Option<String>) {
            let mark = |path: &Path| fs::read_to_string(path.join(MARK)).ok();
            (
                mark(&self.journal.installed),
                mark(&self.journal.staged_dir()),
                mark(&self.journal.failed_dir()),
            )
        }

        fn run(&self, ctx: &Context) -> Result<Report, RecoverError> {
            recover(&self.store, &Fake, ctx)
        }

        fn journal(&self) -> Journal {
            self.store.read().unwrap().unwrap()
        }
    }

    impl Drop for Bench {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn make(path: &Path, bundle: B) {
        let mark = match bundle {
            B::No => return,
            B::N => Some(CD_FROM),
            B::M => Some(CD_TO),
            B::Part => None,
            B::Strange => Some("3333333333333333333333333333333333333333"),
        };
        fs::create_dir_all(path.join("Contents")).unwrap();
        if let Some(mark) = mark {
            fs::write(path.join(MARK), mark).unwrap();
        }
    }

    fn alive_all(_: &ProcessKey) -> bool {
        true
    }

    fn alive_none(_: &ProcessKey) -> bool {
        false
    }

    fn ctx(now_ms: u64, actor: ProcessKey, alive: &dyn Fn(&ProcessKey) -> bool) -> Context<'_> {
        Context {
            now_ms,
            actor: Actor {
                key: actor,
                launched_for: None,
            },
            alive,
        }
    }

    fn early() -> Context<'static> {
        ctx(DEADLINE - 1, OTHER, &alive_all)
    }

    fn some(cdhash: &str) -> Option<String> {
        Some(cdhash.to_owned())
    }

    /// Закончено, итог записан, срок прежний, повторное восстановление ничего не меняет.
    fn finished(bench: &Bench, outcome: Outcome) {
        let journal = bench.journal();
        assert_eq!(journal.state, State::Idle);
        assert_eq!(journal.outcome, Some(outcome));
        assert_eq!(journal.deadline_ms, bench.journal.deadline_ms);
        let before = bench.snapshot();
        let again = bench.run(&early()).unwrap();
        assert_eq!(
            again,
            Report {
                steps: vec![],
                next: Next::Nothing
            }
        );
        assert_eq!(bench.snapshot(), before);
    }

    use Step::{Remove, Retire, Swap, Write};

    #[test]
    fn staged_aborts_with_whole_partial_or_missing_candidate() {
        for staged in [B::M, B::Part, B::No] {
            let bench = Bench::new("staged", State::Staged, B::N, staged, B::No);
            let report = bench.run(&early()).unwrap();
            let mut steps = vec![Write(State::Aborting, None)];
            if !matches!(staged, B::No) {
                steps.push(Remove);
            }
            steps.push(Write(State::Idle, Some(Outcome::Aborted)));
            assert_eq!(
                report,
                Report {
                    steps,
                    next: Next::Nothing
                },
                "{staged:?}"
            );
            assert_eq!(bench.layout(), (some(CD_FROM), None, None));
            finished(&bench, Outcome::Aborted);
        }
    }

    #[test]
    fn intent_before_swap_aborts() {
        let bench = Bench::new("intent-n", State::Intent, B::N, B::M, B::No);
        let report = bench.run(&early()).unwrap();
        assert_eq!(
            report.steps,
            [
                Write(State::Aborting, None),
                Remove,
                Write(State::Idle, Some(Outcome::Aborted))
            ]
        );
        assert_eq!(bench.layout(), (some(CD_FROM), None, None));
        finished(&bench, Outcome::Aborted);
    }

    #[test]
    fn aborting_finishes_removal() {
        for staged in [B::M, B::Part, B::No] {
            let bench = Bench::new("aborting", State::Aborting, B::N, staged, B::No);
            let report = bench.run(&early()).unwrap();
            assert_eq!(
                report.steps.last(),
                Some(&Write(State::Idle, Some(Outcome::Aborted)))
            );
            assert_eq!(bench.layout(), (some(CD_FROM), None, None));
            finished(&bench, Outcome::Aborted);
        }
    }

    #[test]
    fn intent_after_swap_records_swapped_then_hands_launch_to_helper() {
        let bench = Bench::new("intent-m", State::Intent, B::M, B::N, B::No);
        let report = bench.run(&early()).unwrap();
        assert_eq!(
            report,
            Report {
                steps: vec![Write(State::Swapped, None)],
                next: Next::LaunchAsHelper
            }
        );
        assert_eq!(bench.journal().state, State::Swapped);
        assert_eq!(bench.journal().deadline_ms, Some(DEADLINE));
        assert_eq!(bench.layout(), (some(CD_TO), some(CD_FROM), None));
    }

    #[test]
    fn swapped_is_claimed_only_by_new_generation_of_this_attempt() {
        let bench = Bench::new("swapped", State::Swapped, B::M, B::N, B::No);
        // Не N+1 этой попытки: выполнить запуск как помощник.
        let mut other = early();
        other.actor.launched_for = Some("ffff".to_owned());
        assert_eq!(
            bench.run(&other).unwrap(),
            Report {
                steps: vec![],
                next: Next::LaunchAsHelper
            }
        );
        assert_eq!(bench.journal().state, State::Swapped);
        let mut own = ctx(DEADLINE - 1, OWNER, &alive_all);
        own.actor.launched_for = Some(ATTEMPT.to_owned());
        assert_eq!(
            bench.run(&own).unwrap(),
            Report {
                steps: vec![Step::Claim],
                next: Next::Owner
            }
        );
        let journal = bench.journal();
        assert_eq!(
            (journal.state, journal.owner),
            (State::Launched, Some(OWNER))
        );
        assert_eq!(journal.deadline_ms, Some(DEADLINE));
    }

    #[test]
    fn launched_with_live_owner_before_deadline_only_owner_continues() {
        let bench = Bench::new("launched-live", State::Launched, B::M, B::N, B::No);
        let before = bench.snapshot();
        assert_eq!(bench.run(&early()).unwrap().next, Next::Yield);
        let owner = ctx(DEADLINE - 1, OWNER, &alive_all);
        assert_eq!(bench.run(&owner).unwrap().next, Next::Owner);
        assert_eq!(bench.snapshot(), before);
    }

    #[test]
    fn launched_rolls_back_when_deadline_passed_or_owner_exited() {
        for (now, alive) in [
            (DEADLINE, &alive_all as &dyn Fn(&ProcessKey) -> bool),
            (DEADLINE - 1, &alive_none),
        ] {
            let bench = Bench::new("launched-dead", State::Launched, B::M, B::N, B::No);
            let report = bench.run(&ctx(now, OTHER, alive)).unwrap();
            assert_eq!(
                report,
                Report {
                    steps: vec![
                        Write(State::RollbackIntent, None),
                        Swap,
                        Write(State::RolledBack, None),
                        Retire,
                        Write(State::Retired, None),
                        Write(State::Idle, Some(Outcome::RolledBack)),
                    ],
                    next: Next::LaunchInstalled
                }
            );
            assert_eq!(bench.layout(), (some(CD_FROM), None, some(CD_TO)));
            finished(&bench, Outcome::RolledBack);
        }
    }

    #[test]
    fn rollback_intent_swaps_back_or_records_done_swap() {
        let bench = Bench::new("rb-m", State::RollbackIntent, B::M, B::N, B::No);
        assert_eq!(
            bench.run(&early()).unwrap().steps[..2],
            [Swap, Write(State::RolledBack, None)]
        );
        assert_eq!(bench.layout(), (some(CD_FROM), None, some(CD_TO)));
        finished(&bench, Outcome::RolledBack);
        let bench = Bench::new("rb-n", State::RollbackIntent, B::N, B::M, B::No);
        assert_eq!(
            bench.run(&early()).unwrap().steps[..2],
            [Write(State::RolledBack, None), Retire]
        );
        finished(&bench, Outcome::RolledBack);
    }

    #[test]
    fn rolled_back_retires_the_failed_generation() {
        let bench = Bench::new("rolled", State::RolledBack, B::N, B::M, B::No);
        let report = bench.run(&early()).unwrap();
        assert_eq!(
            report.steps,
            [
                Retire,
                Write(State::Retired, None),
                Write(State::Idle, Some(Outcome::RolledBack))
            ]
        );
        assert_eq!(report.next, Next::LaunchInstalled);
        assert_eq!(bench.layout(), (some(CD_FROM), None, some(CD_TO)));
        finished(&bench, Outcome::RolledBack);
    }

    #[test]
    fn rename_already_done_records_retired_then_idle() {
        let bench = Bench::new("renamed", State::RolledBack, B::N, B::No, B::M);
        assert_eq!(
            bench.run(&early()).unwrap().steps,
            [
                Write(State::Retired, None),
                Write(State::Idle, Some(Outcome::RolledBack))
            ]
        );
        finished(&bench, Outcome::RolledBack);
        let bench = Bench::new("retired", State::Retired, B::N, B::No, B::M);
        assert_eq!(
            bench.run(&early()).unwrap().steps,
            [Write(State::Idle, Some(Outcome::RolledBack))]
        );
        finished(&bench, Outcome::RolledBack);
    }

    #[test]
    fn committed_and_cleaning_finish_removal_of_previous_generation() {
        for state in [State::Committed, State::Cleaning] {
            for staged in [B::N, B::Part, B::No] {
                let bench = Bench::new("committed", state, B::M, staged, B::No);
                let report = bench.run(&early()).unwrap();
                assert_eq!(
                    report.steps.last(),
                    Some(&Write(State::Idle, Some(Outcome::Committed)))
                );
                assert_eq!(report.next, Next::Nothing);
                assert_eq!(
                    bench.layout(),
                    (some(CD_TO), None, None),
                    "{state:?} {staged:?}"
                );
                finished(&bench, Outcome::Committed);
            }
        }
    }

    #[test]
    fn idle_and_deferred_are_not_recovered() {
        for state in [State::Idle, State::StagedDeferred] {
            let bench = Bench::new("quiet", state, B::N, B::M, B::No);
            let before = bench.snapshot();
            assert_eq!(
                bench.run(&early()).unwrap(),
                Report {
                    steps: vec![],
                    next: Next::Nothing
                }
            );
            assert_eq!(bench.snapshot(), before);
        }
    }

    /// Строка «любой»: ничего не переставлено, журнал байт в байт прежний.
    fn refuses(bench: &Bench) -> RecoverError {
        let before = bench.snapshot();
        let err = bench
            .run(&ctx(DEADLINE + 1, OTHER, &alive_none))
            .unwrap_err();
        assert_eq!(bench.snapshot(), before, "{err}");
        err
    }

    #[test]
    fn unknown_or_unverified_generation_moves_nothing() {
        let cases = [
            (State::Launched, B::Strange, B::N, B::No),
            (State::Staged, B::N, B::Strange, B::No),
            (State::Launched, B::Part, B::N, B::No),
            (State::RollbackIntent, B::M, B::Part, B::No),
            (State::Retired, B::N, B::No, B::Part),
        ];
        for (state, installed, staged, failed) in cases {
            let bench = Bench::new("unknown", state, installed, staged, failed);
            assert!(
                matches!(refuses(&bench), RecoverError::OffTable(_)),
                "{state:?}"
            );
        }
    }

    #[test]
    fn directory_of_another_attempt_moves_nothing() {
        let bench = Bench::new("stranger", State::Staged, B::N, B::M, B::No);
        make(
            &bench.dir.join("install").join(".shellio-update-ffff"),
            B::M,
        );
        let err = refuses(&bench);
        assert!(err.to_string().contains(".shellio-update-ffff"), "{err}");
    }

    #[test]
    fn failed_directory_of_finished_attempt_does_not_block() {
        let bench = Bench::new("old-failed", State::Staged, B::N, B::M, B::No);
        make(
            &bench.dir.join("install").join(".shellio-failed-ffff"),
            B::M,
        );
        bench.run(&early()).unwrap();
        finished(&bench, Outcome::Aborted);
    }

    #[test]
    fn layouts_off_the_table_move_nothing() {
        let cases = [
            (State::Staged, B::M, B::N, B::No),
            (State::Intent, B::N, B::No, B::No),
            (State::Intent, B::M, B::M, B::No),
            (State::Swapped, B::N, B::M, B::No),
            (State::Launched, B::N, B::M, B::No),
            (State::RolledBack, B::N, B::M, B::M),
            (State::Retired, B::N, B::M, B::No),
            (State::Committed, B::N, B::M, B::No),
            (State::Aborting, B::M, B::N, B::No),
        ];
        for (state, installed, staged, failed) in cases {
            let bench = Bench::new("off", state, installed, staged, failed);
            assert!(
                matches!(refuses(&bench), RecoverError::OffTable(_)),
                "{state:?}"
            );
        }
    }

    #[test]
    fn damaged_journal_moves_nothing() {
        let bench = Bench::new("damaged", State::Launched, B::M, B::N, B::No);
        fs::write(bench.store.path(), "{\"attempt\":").unwrap();
        assert!(matches!(
            refuses(&bench),
            RecoverError::Journal(JournalError::Corrupt(_))
        ));
    }

    #[test]
    fn failing_journal_write_stops_before_any_move() {
        use std::os::unix::fs::PermissionsExt;
        let bench = Bench::new("readonly", State::Launched, B::M, B::N, B::No);
        let update = bench.store.path().parent().unwrap().to_path_buf();
        let before = bench.snapshot();
        fs::set_permissions(&update, fs::Permissions::from_mode(0o500)).unwrap();
        let result = bench.run(&ctx(DEADLINE, OTHER, &alive_none));
        fs::set_permissions(&update, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(matches!(
            result,
            Err(RecoverError::Journal(JournalError::Io(_)))
        ));
        assert_eq!(bench.snapshot(), before);
        // Когда запись снова возможна, следующий взявший блокировку доводит откат.
        bench.run(&ctx(DEADLINE, OTHER, &alive_none)).unwrap();
        finished(&bench, Outcome::RolledBack);
    }

    #[test]
    fn late_helper_of_another_attempt_writes_nothing() {
        let bench = Bench::new("late", State::Launched, B::M, B::N, B::No);
        let before = bench.snapshot();
        let late = bench.store.advance("ffff", &[State::Launched], |journal| {
            journal.state = State::RollbackIntent;
        });
        assert!(matches!(late, Err(JournalError::Stale { .. })));
        assert_eq!(bench.snapshot(), before);
    }
}
