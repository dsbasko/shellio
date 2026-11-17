//! Сценарий `apps-surface` (задача 19): части F4 в поверхности на кандидате процесса. Модель
//! частей — `matrix::apps`, клавиши и источники ввода — `matrix::apps_surface`, вид с клавиатурой
//! — `surface::keyboard`, события и источники — `surface::input`.
//!
//! Каждая часть сначала идёт контролем без хоста: нагрузка прямо в PTY поверхности GUI с видом
//! клавиатуры. Затем кандидат: GUI №1 с attach-клиентом, настоящий `SIGKILL` GUI №1, GUI №2 к той
//! же сессии. Источник ввода GUI выбирает на время ввода и возвращает до `SIGKILL`.
//!
//! Отнесение провала контроля (записано в отчёт до прогона): в журнале ввода вида нет ожидаемого
//! события или текста — `stand` (событие не прошло через источник ввода); в журнале есть, а у
//! нагрузки нет — `surface`. То же правило журнала действует у GUI кандидата: без ожидаемого текста
//! в журнале провал — `not_run (stand)`, а не вывод о хосте.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::args::Target;
use crate::backend::{self, HostRecord, Lead, SessionId, Workload};
use crate::fixture::channel::{Command, Event, Link};
use crate::fixture::{self, Mode};
use crate::launch;
use crate::matrix::apps::{
    self, NVIM_AFTER, NVIM_BEFORE, NVIM_MID, NvimFile, Outcome, PASTE_TEXT, Place, Screen,
};
use crate::matrix::apps_surface::{
    self as keys, KEY_ESCAPE, KEY_RETURN, Mods, SOURCE_ABC, SOURCE_ABC_EXTENDED, SOURCE_JAPANESE,
    SOURCE_RUSSIAN, Stroke,
};
use crate::matrix::bridge::GuiCommand;
use crate::matrix::report_policy;
use crate::results::{Cell, DefectOrigin, Environment, NotRunCause, Report};
use crate::surface::bridge::{self, Driver, Mark, WAIT, Window};
use crate::surface::input;

const COLS: u16 = 80;
const ROWS: u16 = 24;
/// `TERM` нагрузки у обоих кандидатов; контроль без хоста получает тот же.
const TERM_VALUE: &str = "xterm-256color";
const POLL: Duration = Duration::from_millis(5);

pub fn run(target: &Target, environment: &Environment) -> Result<Report, String> {
    let candidate = target
        .backend
        .ok_or("предусловие: сценарий apps-surface идёт на кандидате (--backend)")?;
    let mut driver = Driver::create(&target.out, environment, candidate)?;
    let outcome = parts(&mut driver);
    let mut notes = driver.teardown();
    notes.extend(input::restore_all(&target.out));
    let mut report = Report {
        cells: outcome?,
        ..Report::default()
    };
    report.diagnostics.notes.extend(notes);
    Ok(report)
}

type Part = fn(&mut Driver) -> Result<Outcome, String>;

fn parts(driver: &mut Driver) -> Result<Vec<Cell>, String> {
    let built: [(&str, Part); 7] = [
        ("nvim-surface", nvim_surface),
        ("paste", paste),
        ("kitty", kitty),
        ("layout-ru", layout_ru),
        ("dead-key", dead_key),
        ("ime", ime),
        ("switch", switch),
    ];
    let mut cells = Vec::new();
    for spec in apps::parts_of(Place::Surface) {
        let outcome = match built.iter().find(|(name, _)| *name == spec.name) {
            Some((_, part)) => {
                let outcome = part(driver);
                // Источник ввода возвращается после каждой части, даже если GUI уже убита.
                input::restore_all(&driver.out);
                outcome?
            }
            None => Outcome::not_run(NotRunCause::Pending, "водитель части ещё не построен"),
        };
        cells.push(apps::cell(spec.name, &outcome)?);
    }
    Ok(cells)
}

// --- общее ---

/// Итог клавиш: журнал вида и отказ стенда.
struct Typed {
    journal: Vec<String>,
    stand: Option<String>,
}

impl Typed {
    /// В журнале есть нажатие каждой клавиши, дошедшее до `ghostty_surface_key`.
    fn delivered(&self, count: usize) -> bool {
        self.stand.is_none()
            && self
                .journal
                .iter()
                .filter(|line| line.starts_with("keyDown "))
                .count()
                >= count
    }
}

fn strokes(window: &mut Window, strokes: Vec<Stroke>) -> Result<Typed, String> {
    let reply = window.command(&GuiCommand::Strokes {
        surface: 1,
        strokes,
    })?;
    Ok(Typed {
        journal: reply.input.unwrap_or_default(),
        stand: reply.stand,
    })
}

/// Источник ввода `id` в GUI; `Some` — стенд не смог его выбрать.
fn select(window: &mut Window, id: &str) -> Result<Option<String>, String> {
    let reply = window.command(&GuiCommand::Source {
        id: Some(id.to_owned()),
    })?;
    Ok(match reply.stand {
        Some(reason) => Some(reason),
        None if reply.source.as_deref() != Some(id) => Some(format!(
            "источник {id} не стал текущим: {}",
            reply.source.unwrap_or_default()
        )),
        None => None,
    })
}

/// Прежний источник ввода; GUI возвращает его до `SIGKILL`.
fn restore(window: &mut Window) -> Result<(), String> {
    window.command(&GuiCommand::Source { id: None }).map(drop)
}

fn env_with_term(driver: &Driver) -> Vec<(String, String)> {
    let mut env = driver.env().to_vec();
    env.push(("TERM".to_owned(), TERM_VALUE.to_owned()));
    env
}

/// Контроль без хоста: фикстура `mode` прямо в PTY поверхности GUI с клавиатурой.
fn direct_fixture(driver: &mut Driver, label: &str, mode: Mode) -> Result<(Window, Link), String> {
    let nonce = format!("{}-{label}-ctl", driver.nonce);
    let args = fixture::command_args(mode, driver.listener()?.path(), &nonce, None);
    let command = bridge::surface_command(driver.fixture(), &args, &env_with_term(driver));
    let window = driver.open_surface(command, COLS, ROWS, true)?;
    let mut link = driver
        .listener()?
        .accept(WAIT)
        .map_err(|err| format!("фикстура контроля не подключилась: {err}"))?;
    let ready = link
        .wait_for(WAIT, |receipt| match &receipt.event {
            Event::Ready(ready) => Some(ready.clone()),
            _ => None,
        })
        .map_err(|err| format!("готовность фикстуры контроля: {err}"))?;
    if ready.nonce != nonce {
        return Err(format!(
            "предусловие: подключилась фикстура {}, ожидалась {nonce}",
            ready.nonce
        ));
    }
    Ok((window, link))
}

/// `SIGKILL` GUI с возвратом источника ввода до него; нагрузка и хост должны пережить его.
fn kill_gui(
    driver: &Driver,
    window: &mut Window,
    id: &SessionId,
    outcome: &mut Outcome,
) -> Result<bool, String> {
    restore(window)?;
    let status = driver.kill(window)?;
    driver.wait_clients(id, 0)?;
    use std::os::unix::process::ExitStatusExt;
    Ok(outcome.evidence.check(
        &format!("SIGKILL {}", window.label),
        status.signal() == Some(libc::SIGKILL),
        format!("{} завершилась {status:?}", window.label),
    ))
}

/// Строки вида, когда `done` выполнено, или последние строки по сроку.
fn wait_view(
    window: &mut Window,
    done: impl Fn(&[String]) -> bool,
) -> Result<(bool, Vec<String>), String> {
    bridge::wait_lines(window, |lines| done(lines))
}

/// Ждёт `done` до срока, опрашивая с шагом [`POLL`].
fn poll_until(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if done() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::park_timeout(POLL);
    }
}

// --- kitty ---

/// Кодирование клавиш части `kitty`: каждая клавиша, затем маркер-вставка; байты до маркера — её
/// кодирование. `Err` вида «стенд» — журнал без нажатия; `Ok(None)` — маркер не дошёл.
fn encode_keys(
    window: &mut Window,
    link: &mut Link,
    tag: &str,
) -> Result<Result<Vec<(String, String)>, String>, String> {
    let mut mark = Mark::at(link.log());
    let mut encodings = Vec::new();
    for (index, (label, stroke)) in keys::kitty_keys().into_iter().enumerate() {
        let typed = strokes(window, vec![stroke])?;
        if !typed.delivered(1) {
            return Ok(Err(format!(
                "{tag}: {label} не дошла до вида: {}",
                typed.stand.unwrap_or_else(|| typed.journal.join(" | "))
            )));
        }
        let marker = format!("pss-{tag}{index}-");
        match bridge::fence(window, link, &mut mark, &marker)? {
            Some(bytes) => encodings.push((label.to_owned(), apps::hex(&bytes))),
            None => {
                return Ok(Err(format!(
                    "{tag}: маркер после {label} не дошёл до фикстуры; журнал вида: {}",
                    typed.journal.join(" | ")
                )));
            }
        }
    }
    Ok(Ok(encodings))
}

fn kitty(driver: &mut Driver) -> Result<Outcome, String> {
    let mut outcome = Outcome {
        origin: Some(DefectOrigin::Host),
        ..Outcome::default()
    };
    // Контроль без хоста.
    let (mut gui, mut link) = direct_fixture(driver, "kitty", Mode::Emitter)?;
    if let Some(reason) = select(&mut gui, SOURCE_ABC)? {
        restore(&mut gui)?;
        gui.quit();
        return Ok(Outcome::not_run(NotRunCause::Stand, reason));
    }
    let pushed = bridge::barrier(&mut link, keys::KITTY_PUSH)?;
    let control = if pushed {
        encode_keys(&mut gui, &mut link, "c")?
    } else {
        Err("контроль: поверхность не ответила на барьер после push Kitty".to_owned())
    };
    restore(&mut gui)?;
    gui.quit();
    outcome.evidence.note("control", &control);
    let control = match control {
        Ok(control) => control,
        Err(reason) => {
            let cause = if reason.contains("не дошла до вида") {
                NotRunCause::Stand
            } else {
                NotRunCause::Surface
            };
            outcome.mark_not_run(cause, format!("контроль без хоста: {reason}"));
            return finish(driver, outcome, "f4-kitty.json");
        }
    };
    // Кандидат.
    let mut session = driver.ensure("kitty", Mode::Emitter)?;
    let mut first = driver.attach_window(&session.id, true)?;
    driver.wait_clients(&session.id, 1)?;
    if let Some(reason) = select(&mut first, SOURCE_ABC)? {
        outcome.mark_not_run(NotRunCause::Stand, reason);
        restore(&mut first)?;
        first.quit();
        return finish(driver, outcome, "f4-kitty.json");
    }
    if !bridge::barrier(&mut session.link, keys::KITTY_PUSH)? {
        return Err("предусловие kitty: GUI №1 не ответила на барьер".to_owned());
    }
    let before = encode_keys(&mut first, &mut session.link, "b")?;
    outcome.evidence.note("before", &before);
    kill_gui(driver, &mut first, &session.id, &mut outcome)?;
    outcome.evidence.check(
        "нагрузка и хост живы после SIGKILL GUI",
        driver.alive(&session)?,
        "нагрузка или хост погибли",
    );
    let mut second = driver.attach_window(&session.id, true)?;
    driver.wait_clients(&session.id, 1)?;
    if let Some(reason) = select(&mut second, SOURCE_ABC)? {
        outcome.mark_not_run(NotRunCause::Stand, reason);
    } else if !bridge::barrier(&mut session.link, b"")? {
        outcome
            .evidence
            .check("GUI №2 отвечает на барьер", false, "ответа нет");
    } else {
        let after = encode_keys(&mut second, &mut session.link, "a")?;
        outcome.evidence.note("after", &after);
        for (name, result) in [("до SIGKILL", &before), ("после attach", &after)] {
            match result {
                Ok(encodings) => {
                    let mismatches = apps::encoding_mismatches(&control, encodings);
                    outcome.evidence.check(
                        &format!("кодирование {name} как без хоста"),
                        mismatches.is_empty(),
                        format!("клавиша, без хоста, через кандидата: {mismatches:?}"),
                    );
                }
                Err(reason) if reason.contains("не дошла до вида") => {
                    outcome.mark_not_run(NotRunCause::Stand, reason.clone());
                }
                Err(reason) => {
                    outcome
                        .evidence
                        .check(&format!("кодирование {name}"), false, reason);
                }
            }
        }
    }
    restore(&mut second)?;
    second.quit();
    finish(driver, outcome, "f4-kitty.json")
}

fn finish(driver: &Driver, mut outcome: Outcome, artifact: &str) -> Result<Outcome, String> {
    outcome.artifacts.push(driver.artifact(
        artifact,
        &serde_json::json!({
            "checks": &outcome.evidence.checks,
            "observed": &outcome.evidence.observed,
            "not_proven": &outcome.evidence.not_proven,
        }),
    )?);
    Ok(outcome)
}

// --- nvim ---

/// Строк в файле части `nvim-surface`: несколько экранов при 24 строках.
const NVIM_LINES: usize = 120;

fn nvim_program(driver: &Driver) -> Option<PathBuf> {
    let path = driver
        .env()
        .iter()
        .find(|(name, _)| name == "PATH")
        .map(|(_, value)| value.clone())?;
    apps::which("nvim", &path)
}

fn nvim_args(file: &Path) -> Vec<String> {
    vec![
        "--clean".to_owned(),
        "-n".to_owned(),
        file.display().to_string(),
    ]
}

/// Контроль без хоста: nvim прямо в PTY поверхности GUI с клавиатурой и тем же `TERM`.
fn direct_nvim(driver: &mut Driver, program: &Path, file: &Path) -> Result<Window, String> {
    let command = bridge::surface_command(program, &nvim_args(file), &env_with_term(driver));
    driver.open_surface(command, COLS, ROWS, true)
}

/// Сессия кандидата с nvim, который стартует по строке от первого клиента.
fn nvim_session(
    driver: &mut Driver,
    label: &str,
    program: &Path,
    file: &Path,
) -> Result<(SessionId, HostRecord), String> {
    let mut args = vec![
        "-c".to_owned(),
        apps::START_ON_LINE.to_owned(),
        program.display().to_string(),
    ];
    args.extend(nvim_args(file));
    driver.ensure_program(
        label,
        &Workload {
            program: PathBuf::from("/bin/sh"),
            args,
        },
    )
}

fn insert_mode(lines: &[String]) -> bool {
    lines
        .iter()
        .rev()
        .find(|line| !line.trim().is_empty())
        .is_some_and(|line| line.contains("-- INSERT --"))
}

fn empty_buffer(lines: &[String]) -> bool {
    lines.iter().filter(|line| line.starts_with('~')).count() >= 3
}

/// Шаги ввода в GUI с ожиданием вида: первый провал ожидания записывается проверкой части.
struct Steps<'a> {
    window: &'a mut Window,
    /// Журнал ввода вида за часть.
    journal: Vec<String>,
    /// Отказ стенда: событие не построено или не дошло до вида.
    stand: Option<String>,
    expected_keys: usize,
}

impl<'a> Steps<'a> {
    fn new(window: &'a mut Window) -> Steps<'a> {
        Steps {
            window,
            journal: Vec::new(),
            stand: None,
            expected_keys: 0,
        }
    }

    fn keys(&mut self, strokes_list: Vec<Stroke>) -> Result<(), String> {
        let count = strokes_list.len();
        let typed = strokes(self.window, strokes_list)?;
        if !typed.delivered(count) && self.stand.is_none() {
            self.stand = Some(format!(
                "событие не дошло до вида: {}",
                typed
                    .stand
                    .clone()
                    .unwrap_or_else(|| typed.journal.join(" | "))
            ));
        }
        self.expected_keys += count;
        self.journal.extend(typed.journal);
        Ok(())
    }

    fn latin(&mut self, text: &str) -> Result<(), String> {
        self.keys(keys::latin(text)?)
    }

    fn key(&mut self, stroke: Stroke) -> Result<(), String> {
        self.keys(vec![stroke])
    }

    /// Вид до условия; `None` — срок вышел, последние строки — в `last`.
    fn wait(&mut self, done: impl Fn(&[String]) -> bool) -> Result<(bool, Vec<String>), String> {
        wait_view(self.window, done)
    }

    fn inserted(&self, text: &str) -> bool {
        self.journal
            .iter()
            .any(|line| line == &format!("insertText {text:?}"))
    }
}

/// Отнесение провала: журнал без ожидаемого ввода — `stand`, иначе — `surface` у контроля и
/// вывод о кандидате у кандидата.
fn blame_stand(steps: &Steps<'_>) -> Option<String> {
    steps.stand.clone()
}

/// Путь nvim части `nvim-surface` до строки «pss-mid» во втором окне: номера, разбиение, вставка,
/// Ctrl-V Shift-Enter. `Err` — шаг, на котором вид не дошёл до условия.
fn nvim_split_insert(
    steps: &mut Steps<'_>,
    file: &NvimFile,
) -> Result<Result<Vec<String>, String>, String> {
    let first = file.original[0].clone();
    if !steps
        .wait(|lines| lines.first().is_some_and(|line| line.contains(&first)))?
        .0
    {
        return Ok(Err("файл на экране".to_owned()));
    }
    steps.latin(":set number\r")?;
    if !steps
        .wait(|lines| {
            lines
                .first()
                .is_some_and(|line| line.trim_start().starts_with(&format!("1 {first}")))
        })?
        .0
    {
        return Ok(Err(":set number".to_owned()));
    }
    steps.latin(":vsplit\r")?;
    if !steps
        .wait(|lines| {
            lines
                .first()
                .is_some_and(|line| line.matches(first.as_str()).count() == 2)
        })?
        .0
    {
        return Ok(Err(":vsplit".to_owned()));
    }
    steps.key(keys::ctrl('w')?)?;
    steps.latin(&format!("l{}Go", file.after))?;
    if !steps.wait(insert_mode)?.0 {
        return Ok(Err("вставка во втором окне".to_owned()));
    }
    steps.latin(NVIM_BEFORE)?;
    steps.key(keys::ctrl('v')?)?;
    steps.key(Stroke::with(KEY_RETURN, Mods::SHIFT))?;
    steps.latin(NVIM_MID)?;
    let (reached, lines) =
        steps.wait(|lines| lines.iter().any(|line| line.contains(NVIM_MID.trim())))?;
    if !reached {
        return Ok(Err("строка «pss-mid»".to_owned()));
    }
    Ok(Ok(lines))
}

/// Конец пути: второй Ctrl-V Shift-Enter, «pss-after», выход из вставки, `:wqa`.
fn nvim_finish(steps: &mut Steps<'_>) -> Result<bool, String> {
    steps.key(keys::ctrl('v')?)?;
    steps.key(Stroke::with(KEY_RETURN, Mods::SHIFT))?;
    steps.latin(NVIM_AFTER)?;
    steps.key(Stroke::key(KEY_ESCAPE))?;
    if !steps.wait(|lines| !insert_mode(lines))?.0 {
        return Ok(false);
    }
    // `:wq` закрыл бы только окно разбиения.
    steps.latin(":wqa\r")?;
    Ok(true)
}

/// Файл nvim после выхода: ждёт, пока разбор пройдёт, или срок.
fn wait_file<T>(path: &Path, parse: impl Fn(&[u8]) -> Option<T>) -> Option<T> {
    let mut found = None;
    poll_until(WAIT, || {
        found = fs::read(path).ok().and_then(|bytes| parse(&bytes));
        found.is_some()
    });
    found
}

fn screen_with_mark(lines: Vec<String>) -> Screen {
    let cursor =
        apps::mark_position(&lines, NVIM_MID.trim()).map(|(x, y)| (f64::from(x), f64::from(y)));
    Screen { lines, cursor }
}

fn nvim_surface(driver: &mut Driver) -> Result<Outcome, String> {
    let Some(program) = nvim_program(driver) else {
        return Ok(Outcome::not_run(
            NotRunCause::Stand,
            "nvim нет в PATH окружения процесса",
        ));
    };
    let file = NvimFile::numbered(NVIM_LINES);
    let mut outcome = Outcome {
        origin: Some(DefectOrigin::Host),
        ..Outcome::default()
    };
    // Контроль без хоста.
    let control_path = driver.out.join("nvim-surface-ctl.txt");
    fs::write(&control_path, file.contents()).map_err(|err| format!("{err}"))?;
    let mut gui = direct_nvim(driver, &program, &control_path)?;
    let control = nvim_control_run(&mut gui, &file, &control_path)?;
    gui.quit();
    outcome.evidence.note("control", &control);
    let control = match control {
        Ok(insert) => insert,
        Err((cause, reason)) => {
            outcome.mark_not_run(cause, format!("контроль без хоста: {reason}"));
            return finish(driver, outcome, "f4-nvim-surface.json");
        }
    };
    if control.first == apps::hex(b"\r") {
        outcome.mark_not_run(
            NotRunCause::Stand,
            "контроль: Ctrl-V Shift-Enter вставил голый CR, сравнение ничего не доказывает",
        );
    }
    // Кандидат.
    let path = driver.out.join("nvim-surface.txt");
    fs::write(&path, file.contents()).map_err(|err| format!("{err}"))?;
    let (id, record) = nvim_session(driver, "nvim", &program, &path)?;
    let mut first = driver.attach_window(&id, true)?;
    driver.wait_clients(&id, 1)?;
    if let Some(reason) = select(&mut first, SOURCE_ABC)? {
        outcome.mark_not_run(NotRunCause::Stand, reason);
        restore(&mut first)?;
        first.quit();
        return finish(driver, outcome, "f4-nvim-surface.json");
    }
    let mut steps = Steps::new(&mut first);
    steps.key(Stroke::key(KEY_RETURN))?;
    let reached = nvim_split_insert(&mut steps, &file)?;
    let stand = blame_stand(&steps);
    let before = match reached {
        Ok(lines) => screen_with_mark(lines),
        Err(step) => {
            match stand {
                Some(reason) => outcome.mark_not_run(NotRunCause::Stand, reason),
                None => {
                    outcome.evidence.check(
                        &format!("GUI №1: {step}"),
                        false,
                        "вид не дошёл до условия",
                    );
                }
            }
            restore(&mut first)?;
            first.quit();
            return finish(driver, outcome, "f4-nvim-surface.json");
        }
    };
    outcome.evidence.note("screen_before", &before);
    kill_gui(driver, &mut first, &id, &mut outcome)?;
    stand_alive(&mut outcome, &record)?;
    let mut second = driver.attach_window(&id, true)?;
    driver.wait_clients(&id, 1)?;
    if let Some(reason) = select(&mut second, SOURCE_ABC)? {
        outcome.mark_not_run(NotRunCause::Stand, reason);
        restore(&mut second)?;
        second.quit();
        return finish(driver, outcome, "f4-nvim-surface.json");
    }
    let (_, lines) = wait_view(&mut second, |lines| {
        apps::compare_screens(&before, &screen_with_mark(lines.to_vec())).is_empty()
    })?;
    let after = screen_with_mark(lines);
    let diff = apps::compare_screens(&before, &after);
    outcome.evidence.note("screen_after", &after);
    outcome.evidence.check(
        "вид после attach равен виду до SIGKILL GUI",
        diff.is_empty(),
        if diff.is_empty() {
            "строки, разбиение, номера, строка режима и маркер курсора совпали".to_owned()
        } else {
            diff.summary()
        },
    );
    let mut steps = Steps::new(&mut second);
    let finished = nvim_finish(&mut steps)?;
    let stand = blame_stand(&steps);
    restore(&mut second)?;
    if !finished {
        match stand {
            Some(reason) => outcome.mark_not_run(NotRunCause::Stand, reason),
            None => {
                outcome.evidence.check(
                    "выход из режима вставки",
                    false,
                    "строка режима не сменилась",
                );
            }
        }
        second.quit();
        return finish(driver, outcome, "f4-nvim-surface.json");
    }
    let left = launch::wait_exit(&[record.workload.pid], WAIT);
    outcome.evidence.check(
        "nvim вышел по :wqa",
        left.is_empty(),
        format!("PID {} жив", record.workload.pid),
    );
    second.quit();
    match file.parse(&fs::read(&path).map_err(|err| format!("{err}"))?) {
        Ok(insert) => {
            outcome.evidence.note("file_insert", &insert);
            outcome.evidence.check(
                "Ctrl-V Shift-Enter до SIGKILL вставил то же, что без хоста",
                insert.first == control.first,
                format!("кандидат {}, без хоста {}", insert.first, control.first),
            );
            outcome.evidence.check(
                "Ctrl-V Shift-Enter после attach даёт то же, что до SIGKILL",
                insert.same(),
                format!("до {} после {}", insert.first, insert.second),
            );
            outcome.evidence.check(
                "остальные строки файла не тронуты",
                insert.rest_intact,
                "строки вне вставки изменились",
            );
        }
        Err(err) => {
            outcome.evidence.check("файл после :wqa", false, err);
        }
    }
    finish(driver, outcome, "f4-nvim-surface.json")
}

type ControlResult<T> = Result<T, (NotRunCause, String)>;

/// Тот же путь nvim без хоста; провал — причина `stand` или `surface` по журналу вида.
fn nvim_control_run(
    gui: &mut Window,
    file: &NvimFile,
    path: &Path,
) -> Result<ControlResult<apps::NvimInsert>, String> {
    if let Some(reason) = select(gui, SOURCE_ABC)? {
        restore(gui)?;
        return Ok(Err((NotRunCause::Stand, reason)));
    }
    let mut steps = Steps::new(gui);
    let reached = nvim_split_insert(&mut steps, file)?;
    let finished = match reached {
        Ok(_) => nvim_finish(&mut steps)?,
        Err(_) => false,
    };
    let stand = blame_stand(&steps);
    restore(gui)?;
    let insert = finished
        .then(|| wait_file(path, |bytes| file.parse(bytes).ok()))
        .flatten();
    Ok(match (insert, stand) {
        (Some(insert), _) => Ok(insert),
        (None, Some(reason)) => Err((NotRunCause::Stand, reason)),
        (None, None) => Err((
            NotRunCause::Surface,
            match reached {
                Err(step) => format!("вид не дошёл до шага «{step}»"),
                Ok(_) => "файл после :wqa без ожидаемой вставки".to_owned(),
            },
        )),
    })
}

fn stand_alive(outcome: &mut Outcome, record: &HostRecord) -> Result<(), String> {
    let keys = [record.workload, record.host];
    let alive = backend::alive(&keys).map_err(|err| format!("{err}"))?;
    outcome.evidence.check(
        "нагрузка и хост живы после SIGKILL GUI",
        alive.len() == keys.len(),
        "нагрузка или хост погибли",
    );
    Ok(())
}

// --- nvim с вводом после attach: paste, layout-ru, dead-key ---

/// Ввод части после attach в режиме вставки nvim; итог — отказ стенда, если журнал вида без
/// ожидаемого текста.
type NvimInput = fn(&Driver, &mut Steps<'_>) -> Result<Option<String>, String>;

/// Общий ход: контроль без хоста — тот же ввод в nvim прямо в PTY поверхности; кандидат — nvim на
/// пустом файле, `i`, `SIGKILL` GUI №1, ввод в GUI №2, `:wq`; файл сравнивается с `expected`.
fn nvim_input_part(
    driver: &mut Driver,
    name: &str,
    input: NvimInput,
    expected: &str,
) -> Result<Outcome, String> {
    let artifact = format!("f4-{name}.json");
    let Some(program) = nvim_program(driver) else {
        return Ok(Outcome::not_run(
            NotRunCause::Stand,
            "nvim нет в PATH окружения процесса",
        ));
    };
    let mut outcome = Outcome {
        origin: Some(DefectOrigin::Host),
        ..Outcome::default()
    };
    let control_path = driver.out.join(format!("{name}-ctl.txt"));
    let mut gui = direct_nvim(driver, &program, &control_path)?;
    let control = nvim_input_run(driver, &mut gui, input, &control_path, expected, true)?;
    gui.quit();
    outcome.evidence.note("control", &control);
    if let Err((cause, reason)) = control {
        outcome.mark_not_run(cause, format!("контроль без хоста: {reason}"));
        return finish(driver, outcome, &artifact);
    }
    let path = driver.out.join(format!("{name}.txt"));
    let (id, record) = nvim_session(driver, name, &program, &path)?;
    let mut first = driver.attach_window(&id, true)?;
    driver.wait_clients(&id, 1)?;
    if let Some(reason) = select(&mut first, SOURCE_ABC)? {
        outcome.mark_not_run(NotRunCause::Stand, reason);
        restore(&mut first)?;
        first.quit();
        return finish(driver, outcome, &artifact);
    }
    let mut steps = Steps::new(&mut first);
    steps.key(Stroke::key(KEY_RETURN))?;
    let ready = steps.wait(empty_buffer)?.0 && {
        steps.latin("i")?;
        steps.wait(insert_mode)?.0
    };
    if !ready {
        match blame_stand(&steps) {
            Some(reason) => outcome.mark_not_run(NotRunCause::Stand, reason),
            None => {
                outcome
                    .evidence
                    .check("GUI №1: режим вставки", false, "вид не дошёл до условия");
            }
        }
        restore(&mut first)?;
        first.quit();
        return finish(driver, outcome, &artifact);
    }
    kill_gui(driver, &mut first, &id, &mut outcome)?;
    stand_alive(&mut outcome, &record)?;
    let mut second = driver.attach_window(&id, true)?;
    driver.wait_clients(&id, 1)?;
    let candidate = nvim_input_run(driver, &mut second, input, &path, expected, false)?;
    second.quit();
    outcome.evidence.note("candidate", &candidate);
    match candidate {
        Ok(()) => {
            outcome
                .evidence
                .check("файл nvim как без хоста", true, "ok");
        }
        Err((NotRunCause::Stand, reason)) => outcome.mark_not_run(NotRunCause::Stand, reason),
        Err((_, reason)) => {
            outcome
                .evidence
                .check("файл nvim как без хоста", false, reason);
        }
    }
    let left = launch::wait_exit(&[record.workload.pid], WAIT);
    outcome.evidence.check(
        "nvim вышел по :wq",
        left.is_empty(),
        format!("PID {} жив", record.workload.pid),
    );
    finish(driver, outcome, &artifact)
}

/// Ввод в nvim и `:wq`, затем файл против `expected`. `fresh` — nvim только что запущен на
/// пустом файле (контроль): сначала `i`; иначе режим вставки уже восстановлен attach.
fn nvim_input_run(
    driver: &Driver,
    gui: &mut Window,
    input: NvimInput,
    path: &Path,
    expected: &str,
    fresh: bool,
) -> Result<ControlResult<()>, String> {
    if let Some(reason) = select(gui, SOURCE_ABC)? {
        restore(gui)?;
        return Ok(Err((NotRunCause::Stand, reason)));
    }
    let mut steps = Steps::new(gui);
    let ready = if fresh {
        steps.wait(empty_buffer)?.0 && {
            steps.latin("i")?;
            steps.wait(insert_mode)?.0
        }
    } else {
        steps.wait(insert_mode)?.0
    };
    if !ready {
        let reason = blame_stand(&steps);
        restore(steps.window)?;
        return Ok(Err(match reason {
            Some(reason) => (NotRunCause::Stand, reason),
            None => (
                NotRunCause::Surface,
                "режим вставки не виден в поверхности".to_owned(),
            ),
        }));
    }
    let stand = input(driver, &mut steps)?;
    steps.key(Stroke::key(KEY_ESCAPE))?;
    let normal = steps.wait(|lines| !insert_mode(lines))?.0;
    if normal {
        steps.latin(":wq\r")?;
    }
    let stand = stand.or_else(|| blame_stand(&steps));
    restore(steps.window)?;
    let written = wait_file(path, |bytes| (bytes == expected.as_bytes()).then_some(()));
    if written.is_some() {
        return Ok(Ok(()));
    }
    let actual = fs::read(path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_else(|err| format!("<{err}>"));
    Ok(Err(match stand {
        Some(reason) => (NotRunCause::Stand, reason),
        None => (
            NotRunCause::Surface,
            format!("файл {actual:?}, ожидался {expected:?}"),
        ),
    }))
}

// --- paste ---

fn paste_clipboard(driver: &Driver, window: &mut Window) -> Result<bool, String> {
    let count = driver.clipboard_reads(&window.label)?.len();
    window.command(&GuiCommand::Binding {
        surface: 1,
        action: "paste_from_clipboard".to_owned(),
    })?;
    let Some(request) = driver.wait_clipboard_read(&window.label, count + 1)? else {
        return Ok(false);
    };
    window.command(&GuiCommand::ClipboardReply {
        request,
        text: PASTE_TEXT.to_owned(),
    })?;
    Ok(true)
}

fn paste_input(driver: &Driver, steps: &mut Steps<'_>) -> Result<Option<String>, String> {
    if !paste_clipboard(driver, steps.window)? {
        return Ok(None);
    }
    steps.wait(|lines| lines.iter().any(|line| line.contains("pss paste begin")))?;
    Ok(None)
}

const BRACKETED_ON: &[u8] = b"\x1b[?2004h";

/// Вставка в фикстуру: блоки 2004 в её вводе после вставки; `None` — чтение буфера обмена не
/// пришло.
fn paste_into_fixture(
    driver: &Driver,
    window: &mut Window,
    link: &mut Link,
) -> Result<Option<Vec<Vec<u8>>>, String> {
    let mark = crate::matrix::bridge::input_since(link.log(), 0).len();
    if !paste_clipboard(driver, window)? {
        return Ok(None);
    }
    bridge::pull_until(link, WAIT, |log| {
        let input = crate::matrix::bridge::input_since(log, 0);
        input
            .get(mark..)
            .is_some_and(|rest| rest.windows(3).any(|window| window == b"end"))
    })?;
    let input = crate::matrix::bridge::input_since(link.log(), 0);
    Ok(Some(apps::paste_blocks(
        input.get(mark..).unwrap_or_default(),
    )))
}

fn one_block(blocks: &Option<Vec<Vec<u8>>>) -> bool {
    blocks
        .as_ref()
        .is_some_and(|blocks| blocks.len() == 1 && apps::paste_text(&blocks[0]) == PASTE_TEXT)
}

fn paste(driver: &mut Driver) -> Result<Outcome, String> {
    let mut outcome = nvim_input_part(driver, "paste", paste_input, &format!("{PASTE_TEXT}\n"))?;
    // Фикстура: контроль без хоста, затем кандидат.
    let (mut gui, mut link) = direct_fixture(driver, "paste", Mode::Emitter)?;
    let control = if bridge::barrier(&mut link, BRACKETED_ON)? {
        paste_into_fixture(driver, &mut gui, &mut link)?
    } else {
        None
    };
    gui.quit();
    outcome.evidence.note(
        "fixture_control",
        control.as_ref().map(|blocks| {
            blocks
                .iter()
                .map(|block| apps::hex(block))
                .collect::<Vec<_>>()
        }),
    );
    if !one_block(&control) {
        outcome.mark_not_run(
            NotRunCause::Surface,
            "контроль без хоста: фикстура не получила один блок 2004 с текстом вставки",
        );
        return finish(driver, outcome, "f4-paste-fixture.json");
    }
    let mut session = driver.ensure("paste-fixture", Mode::Emitter)?;
    let mut first = driver.attach_window(&session.id, true)?;
    driver.wait_clients(&session.id, 1)?;
    if !bridge::barrier(&mut session.link, BRACKETED_ON)? {
        return Err("предусловие paste: GUI №1 не ответила на барьер".to_owned());
    }
    kill_gui(driver, &mut first, &session.id, &mut outcome)?;
    outcome.evidence.check(
        "фикстура и хост живы после SIGKILL GUI",
        driver.alive(&session)?,
        "нагрузка или хост погибли",
    );
    let mut second = driver.attach_window(&session.id, true)?;
    driver.wait_clients(&session.id, 1)?;
    if !bridge::barrier(&mut session.link, b"")? {
        outcome
            .evidence
            .check("GUI №2 отвечает на барьер", false, "ответа нет");
    } else {
        let blocks = paste_into_fixture(driver, &mut second, &mut session.link)?;
        outcome.evidence.note(
            "fixture_candidate",
            blocks.as_ref().map(|blocks| {
                blocks
                    .iter()
                    .map(|block| apps::hex(block))
                    .collect::<Vec<_>>()
            }),
        );
        outcome.evidence.check(
            "фикстура: одна квитанция блока 2004 с текстом вставки",
            one_block(&blocks),
            format!("блоков {}", blocks.as_ref().map_or(0, std::vec::Vec::len)),
        );
    }
    second.quit();
    finish(driver, outcome, "f4-paste-fixture.json")
}

// --- layout-ru и dead-key ---

fn ru_strokes() -> Vec<Stroke> {
    keys::RU_KEYS
        .iter()
        .map(|code| Stroke::key(*code))
        .collect()
}

fn layout_input(_driver: &Driver, steps: &mut Steps<'_>) -> Result<Option<String>, String> {
    steps.keys(ru_strokes())?;
    steps.latin(" ")?;
    if let Some(reason) = select(steps.window, SOURCE_RUSSIAN)? {
        return Ok(Some(reason));
    }
    steps.keys(ru_strokes())?;
    let missing: Vec<String> = keys::RU_TEXT
        .chars()
        .map(String::from)
        .filter(|letter| !steps.inserted(letter))
        .collect();
    let stand = select(steps.window, SOURCE_ABC)?;
    if !missing.is_empty() {
        return Ok(Some(format!(
            "журнал вида без insertText {missing:?}: событие не прошло через источник «Русская»"
        )));
    }
    Ok(stand)
}

fn dead_input(_driver: &Driver, steps: &mut Steps<'_>) -> Result<Option<String>, String> {
    if let Some(reason) = select(steps.window, SOURCE_ABC_EXTENDED)? {
        return Ok(Some(reason));
    }
    steps.keys(keys::DEAD_KEYS.to_vec())?;
    let inserted = steps.inserted(keys::DEAD_TEXT);
    let stand = select(steps.window, SOURCE_ABC)?;
    if !inserted {
        return Ok(Some(format!(
            "журнал вида без insertText {:?}: мёртвая клавиша не прошла через ABC Extended",
            keys::DEAD_TEXT
        )));
    }
    Ok(stand)
}

fn layout_ru(driver: &mut Driver) -> Result<Outcome, String> {
    nvim_input_part(driver, "layout-ru", layout_input, &keys::ru_control_file())
}

fn dead_key(driver: &mut Driver) -> Result<Outcome, String> {
    nvim_input_part(
        driver,
        "dead-key",
        dead_input,
        &format!("{}\n", keys::DEAD_TEXT),
    )
}

// --- ime ---

/// Журнал вида после ожидания: пустые `Strokes` забирают журнал, пока `done` не выполнено.
fn wait_journal(
    window: &mut Window,
    journal: &mut Vec<String>,
    done: impl Fn(&[String]) -> bool,
) -> Result<bool, String> {
    let deadline = Instant::now() + WAIT;
    loop {
        if done(journal) {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::park_timeout(POLL);
        journal.extend(strokes(window, Vec::new())?.journal);
    }
}

/// Composition «nihonn» и пробел, коммит Return, «ka» и Esc внутри composition; после каждого шага
/// маркер-вставка. Итог — байты фикстуры до каждого маркера.
fn ime_run(window: &mut Window, link: &mut Link) -> Result<ControlResult<Vec<String>>, String> {
    if let Some(reason) = select(window, SOURCE_JAPANESE)? {
        restore(window)?;
        return Ok(Err((NotRunCause::Stand, reason)));
    }
    let mut first = keys::latin(keys::IME_ROMAJI)?;
    first.push(Stroke::key(keys::KEY_SPACE));
    let mut third = keys::latin(keys::IME_ESCAPED)?;
    third.extend([Stroke::key(KEY_ESCAPE), Stroke::key(KEY_ESCAPE)]);
    let mut mark = Mark::at(link.log());
    let mut journal = Vec::new();
    let mut chunks = Vec::new();
    for (index, list) in [first, vec![Stroke::key(KEY_RETURN)], third]
        .into_iter()
        .enumerate()
    {
        let count = list.len();
        let typed = strokes(window, list)?;
        let delivered = typed.delivered(count);
        journal.extend(typed.journal);
        if !delivered {
            restore(window)?;
            return Ok(Err((
                NotRunCause::Stand,
                format!("событие не дошло до вида: {}", journal.join(" | ")),
            )));
        }
        if index == 1 {
            wait_journal(window, &mut journal, |lines| {
                lines.iter().any(|line| line.starts_with("insertText"))
            })?;
        }
        match bridge::fence(window, link, &mut mark, &format!("pss-i{index}-"))? {
            Some(bytes) => chunks.push(apps::hex(&bytes)),
            None => {
                restore(window)?;
                return Ok(Err((
                    NotRunCause::Surface,
                    format!("маркер шага {index} не дошёл до фикстуры"),
                )));
            }
        }
    }
    restore(window)?;
    if !journal.iter().any(|line| line.starts_with("setMarkedText")) {
        return Ok(Err((
            NotRunCause::Stand,
            "IME не начал composition: в журнале вида нет setMarkedText".to_owned(),
        )));
    }
    Ok(Ok(chunks))
}

fn ime(driver: &mut Driver) -> Result<Outcome, String> {
    let mut outcome = Outcome {
        origin: Some(DefectOrigin::Host),
        ..Outcome::default()
    };
    let (mut gui, mut link) = direct_fixture(driver, "ime", Mode::Emitter)?;
    let control = ime_run(&mut gui, &mut link)?;
    gui.quit();
    outcome.evidence.note("control", &control);
    let control = match control {
        Ok(chunks) => chunks,
        Err((cause, reason)) => {
            outcome.mark_not_run(cause, format!("контроль без хоста: {reason}"));
            return finish(driver, outcome, "f4-ime.json");
        }
    };
    let mut session = driver.ensure("ime", Mode::Emitter)?;
    let mut first = driver.attach_window(&session.id, true)?;
    driver.wait_clients(&session.id, 1)?;
    kill_gui(driver, &mut first, &session.id, &mut outcome)?;
    outcome.evidence.check(
        "фикстура и хост живы после SIGKILL GUI",
        driver.alive(&session)?,
        "нагрузка или хост погибли",
    );
    let mut second = driver.attach_window(&session.id, true)?;
    driver.wait_clients(&session.id, 1)?;
    if !bridge::barrier(&mut session.link, b"")? {
        return Err("предусловие ime: GUI №2 не ответила на барьер".to_owned());
    }
    let candidate = ime_run(&mut second, &mut session.link)?;
    second.quit();
    outcome.evidence.note("candidate", &candidate);
    match candidate {
        Ok(chunks) => {
            outcome.evidence.check(
                "marked text не уходит в PTY до коммита",
                chunks.first().is_some_and(String::is_empty),
                format!("до коммита: {:?}", chunks.first()),
            );
            outcome.evidence.check(
                "ввод как без хоста",
                chunks == control,
                format!("кандидат {chunks:?}, без хоста {control:?}"),
            );
        }
        Err((NotRunCause::Stand, reason)) => outcome.mark_not_run(NotRunCause::Stand, reason),
        Err((_, reason)) => {
            outcome.evidence.check("ввод через IME", false, reason);
        }
    }
    finish(driver, outcome, "f4-ime.json")
}

// --- switch ---

const SWITCH_LINE: &str = "pss switch a";
/// Темп фоновой сессии `b`: 10 строк в секунду.
const FLOOD_INTERVAL_US: u64 = 100_000;

/// Аргументы attach-клиента, который перед `exec` пишет свой PID в `pidfile`: стенд отключает
/// клиента сигналом, а не клавишей, одинаково у обоих кандидатов.
fn attach_with_pid(driver: &Driver, id: &SessionId, pidfile: &Path) -> Result<String, String> {
    let attach = driver
        .backend
        .attach_command(id, Lead::IfFree)
        .map_err(|err| format!("команда attach: {err}"))?;
    let mut args = vec![
        "-c".to_owned(),
        "echo $$ > \"$0\"; exec \"$@\"".to_owned(),
        pidfile.display().to_string(),
        "/usr/bin/env".to_owned(),
        "-i".to_owned(),
    ];
    args.extend(
        attach
            .env
            .iter()
            .map(|(name, value)| format!("{name}={value}")),
    );
    args.push(attach.program.display().to_string());
    args.extend(attach.args.iter().cloned());
    Ok(report_policy::shell_command("/bin/sh", &args))
}

/// Команда поверхности: цикл, который перед каждой строкой сбрасывает терминал (RIS) и режим
/// tty, читает имя сессии и подключается к ней.
fn switch_loop(choices: &[(&str, String)]) -> String {
    let cases: String = choices
        .iter()
        .map(|(name, command)| format!("{name}) {command};; "))
        .collect();
    let script = format!(
        "while :; do printf '\\033c'; stty sane; IFS= read -r s || exit 0; case \"$s\" in {cases}esac; done"
    );
    report_policy::shell_command("/bin/sh", &["-c".to_owned(), script])
}

/// Отключение клиента сессии `id`: `SIGTERM` процессу из `pidfile`, затем ноль клиентов.
fn detach(driver: &Driver, id: &SessionId, pidfile: &Path) -> Result<(), String> {
    let text =
        fs::read_to_string(pidfile).map_err(|err| format!("{}: {err}", pidfile.display()))?;
    let pid: i32 = text
        .trim()
        .parse()
        .map_err(|err| format!("PID клиента «{text}»: {err}"))?;
    backend::signal(pid, libc::SIGTERM).map_err(|err| format!("SIGTERM клиента {pid}: {err}"))?;
    driver.wait_clients(id, 0)
}

fn last_progress(link: &Link) -> u64 {
    link.log()
        .iter()
        .filter_map(|receipt| match &receipt.event {
            Event::FloodProgress { lines, .. } => Some(*lines),
            _ => None,
        })
        .max()
        .unwrap_or(0)
}

fn flood_max(lines: &[String], nonce: &str) -> Option<u64> {
    lines
        .iter()
        .filter_map(|line| fixture::flood_index(nonce, line.as_bytes()))
        .max()
}

fn switch(driver: &mut Driver) -> Result<Outcome, String> {
    let Some(program) = nvim_program(driver) else {
        return Ok(Outcome::not_run(
            NotRunCause::Stand,
            "nvim нет в PATH окружения процесса",
        ));
    };
    let mut outcome = Outcome {
        origin: Some(DefectOrigin::Host),
        ..Outcome::default()
    };
    let pidfile = driver.root().join("switch.pid");
    // Контроль без хоста: тот же цикл, `a` — nvim прямо в PTY поверхности, отключение — SIGTERM.
    let control_file = driver.out.join("switch-ctl.txt");
    fs::write(&control_file, format!("{SWITCH_LINE}\n")).map_err(|err| format!("{err}"))?;
    let mut args = vec![
        "-c".to_owned(),
        "echo $$ > \"$0\"; exec \"$@\"".to_owned(),
        pidfile.display().to_string(),
        "/usr/bin/env".to_owned(),
        "-i".to_owned(),
    ];
    args.extend(
        env_with_term(driver)
            .iter()
            .map(|(name, value)| format!("{name}={value}")),
    );
    args.push(program.display().to_string());
    args.extend(nvim_args(&control_file));
    let direct = report_policy::shell_command("/bin/sh", &args);
    let mut gui = driver.open_surface(switch_loop(&[("a", direct)]), COLS, ROWS, true)?;
    let control = switch_control(&mut gui, &pidfile)?;
    gui.quit();
    outcome.evidence.note("control", &control);
    if let Err((cause, reason)) = control {
        outcome.mark_not_run(cause, format!("контроль без хоста: {reason}"));
        return finish(driver, outcome, "f4-switch.json");
    }
    // Кандидат: две живые сессии.
    let file = driver.out.join("switch-a.txt");
    fs::write(&file, format!("{SWITCH_LINE}\n")).map_err(|err| format!("{err}"))?;
    let (a, record_a) = driver.ensure_program(
        "switch-a",
        &Workload {
            program: program.clone(),
            args: nvim_args(&file),
        },
    )?;
    let mut b = driver.ensure("switch-b", Mode::Flood)?;
    let flood_nonce = format!("{}-w", b.id);
    bridge::request(
        &mut b.link,
        Command::Flood {
            lines: 1_000_000,
            width: 40,
            interval_us: FLOOD_INTERVAL_US,
            report_every: 1,
        },
    )?;
    let choices = [
        ("a", attach_with_pid(driver, &a, &pidfile)?),
        ("b", attach_with_pid(driver, &b.id, &pidfile)?),
    ];
    let mut gui = driver.open_surface(switch_loop(&choices), COLS, ROWS, true)?;
    if let Some(reason) = select(&mut gui, SOURCE_ABC)? {
        outcome.mark_not_run(NotRunCause::Stand, reason);
        restore(&mut gui)?;
        gui.quit();
        return finish(driver, outcome, "f4-switch.json");
    }
    let mut digits = String::new();
    let mut cycles = Vec::new();
    let mut failed = None;
    for cycle in 0..keys::SWITCH_CYCLES {
        let mut steps = Steps::new(&mut gui);
        // a
        steps.latin("a\r")?;
        driver.wait_clients(&a, 1)?;
        let expected = format!("{SWITCH_LINE}{digits}");
        let (a_screen, a_lines) = steps.wait(|lines| {
            lines
                .first()
                .is_some_and(|line| line.trim_end() == expected)
                && flood_max(lines, &flood_nonce).is_none()
        })?;
        if cycle == 0 {
            steps.latin("A")?;
        }
        let digit = keys::digit(cycle);
        steps.latin(&digit.to_string())?;
        digits.push(digit);
        let typed = format!("{SWITCH_LINE}{digits}");
        let (a_input, _) =
            steps.wait(|lines| lines.first().is_some_and(|line| line.trim_end() == typed))?;
        let stand = blame_stand(&steps);
        detach(driver, &a, &pidfile)?;
        // b
        let progress = last_progress(&b.link);
        let mut steps = Steps::new(&mut gui);
        steps.latin("b\r")?;
        driver.wait_clients(&b.id, 1)?;
        let (b_screen, b_lines) = steps.wait(|lines| {
            flood_max(lines, &flood_nonce).is_some_and(|max| max >= progress)
                && !lines.iter().any(|line| line.contains(SWITCH_LINE))
        })?;
        let mark = crate::matrix::bridge::input_since(b.link.log(), 0).len();
        steps.latin(&digit.to_string())?;
        let stand = stand.or_else(|| blame_stand(&steps));
        let b_input = bridge::pull_until(&mut b.link, WAIT, |log| {
            crate::matrix::bridge::input_since(log, 0)
                .get(mark..)
                .is_some_and(|rest| rest.contains(&(digit as u8)))
        })?;
        detach(driver, &b.id, &pidfile)?;
        let record = serde_json::json!({
            "cycle": cycle,
            "a_screen": a_screen,
            "a_input": a_input,
            "b_progress_before_attach": progress,
            "b_max_visible": flood_max(&b_lines, &flood_nonce),
            "b_screen": b_screen,
            "b_input": b_input,
        });
        if !(a_screen && a_input && b_screen && b_input) && failed.is_none() {
            failed = Some((cycle, stand, a_lines, b_lines));
        }
        cycles.push(record);
        if failed.is_some() {
            break;
        }
    }
    outcome.evidence.note("cycles", &cycles);
    match failed {
        Some((cycle, Some(reason), _, _)) => {
            outcome.mark_not_run(NotRunCause::Stand, format!("цикл {cycle}: {reason}"));
        }
        Some((cycle, None, a_lines, b_lines)) => {
            outcome.evidence.note("failed_a_view", &a_lines);
            outcome.evidence.note("failed_b_view", &b_lines);
            outcome.evidence.check(
                "каждый attach показывает свою сессию, ввод доходит",
                false,
                format!(
                    "цикл {cycle}: {}",
                    cycles.last().map_or_else(String::new, |c| c.to_string())
                ),
            );
        }
        None => {
            outcome.evidence.check(
                "каждый attach показывает свою сессию, ввод доходит",
                true,
                format!("{} циклов", keys::SWITCH_CYCLES),
            );
        }
    }
    // Конец: a — выход из вставки и :wq, файл на диске.
    let mut steps = Steps::new(&mut gui);
    steps.latin("a\r")?;
    driver.wait_clients(&a, 1)?;
    steps.key(Stroke::key(KEY_ESCAPE))?;
    if steps.wait(|lines| !insert_mode(lines))?.0 {
        steps.latin(":wq\r")?;
    }
    restore(&mut gui)?;
    let left = launch::wait_exit(&[record_a.workload.pid], WAIT);
    outcome.evidence.check(
        "nvim a вышел по :wq",
        left.is_empty(),
        format!("PID {} жив", record_a.workload.pid),
    );
    let expected = format!("{SWITCH_LINE}{digits}\n");
    let written = fs::read_to_string(&file).unwrap_or_default();
    outcome.evidence.check(
        "файл a на диске: ввод всех циклов без потерь",
        written == expected,
        format!("файл {written:?}, ожидался {expected:?}"),
    );
    outcome.evidence.check(
        "фикстура b жива",
        driver.alive(&b)?,
        "нагрузка или хост b погибли",
    );
    gui.quit();
    finish(driver, outcome, "f4-switch.json")
}

/// Контроль цикла: nvim прямо в PTY поверхности, ввод цифры, отключение SIGTERM.
fn switch_control(gui: &mut Window, pidfile: &Path) -> Result<ControlResult<()>, String> {
    if let Some(reason) = select(gui, SOURCE_ABC)? {
        restore(gui)?;
        return Ok(Err((NotRunCause::Stand, reason)));
    }
    let mut steps = Steps::new(gui);
    steps.latin("a\r")?;
    let shown = steps
        .wait(|lines| {
            lines
                .first()
                .is_some_and(|line| line.trim_end() == SWITCH_LINE)
        })?
        .0;
    steps.latin("A0")?;
    let typed = format!("{SWITCH_LINE}0");
    let input = shown
        && steps
            .wait(|lines| lines.first().is_some_and(|line| line.trim_end() == typed))?
            .0;
    let stand = blame_stand(&steps);
    restore(steps.window)?;
    if let Ok(text) = fs::read_to_string(pidfile)
        && let Ok(pid) = text.trim().parse::<i32>()
    {
        let _ = backend::signal(pid, libc::SIGTERM);
    }
    Ok(match (shown && input, stand) {
        (true, _) => Ok(()),
        (false, Some(reason)) => Err((NotRunCause::Stand, reason)),
        (false, None) => Err((
            NotRunCause::Surface,
            "nvim в цикле поверхности не показал ввод".to_owned(),
        )),
    })
}
