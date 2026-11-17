//! Сценарий `apps` (задача 19): части F4 `nvim`, `claude` и `codex` headless на кандидате
//! процесса. Стенд играет роль GUI, как в рубеже A: attach-клиент в PTY стенда, вывод — в
//! приёмник, ответы приёмника — обратно в PTY клиента. Нагрузка — настоящая программа из `PATH`
//! окружения процесса.
//!
//! Синхронизация — условия над экраном приёмника и выход процессов по `kqueue`; ожидание по сроку
//! только ограничивает их. Переписка с агентами в артефакты не попадает: у частей агентов
//! записываются только номера строк ответа, счётчики и проверки.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::json;

use crate::args::Target;
use crate::backend::{SessionId, Workload};
use crate::launch;
use crate::matrix::apps::{
    self, AGENT_CONTROL, AGENT_COUNT, AGENT_NEXT, AGENT_PROMPT, NVIM_AFTER, NVIM_BEFORE, NVIM_MID,
    Numbers, NvimFile, Outcome, Screen,
};
use crate::matrix::stand::{self, Client, Stand, WAIT};
use crate::oracle::vt::{Capture, Config, Journaled, Receiver, ReceiverProfile, Route};
use crate::pty::{self, PtyChild, Spawn};
use crate::results::{Cell, DefectOrigin, Environment, NotRunCause, Report};
use crate::vt::{Key, KeyAction, KeyEncoder, KeyInput, MODS_SHIFT};

/// Срок начала и конца ответа агента: запрос идёт в сеть.
const AGENT_WAIT: Duration = Duration::from_secs(120);
/// Шаг ожидания агента: между шагами стенд проверяет экраны-заставки.
const AGENT_STEP: Duration = Duration::from_secs(2);
/// Сколько строк ответа должно прийти до `SIGKILL` клиента «посреди ответа».
const AGENT_MID: usize = 20;
/// Строк в файле nvim: несколько экранов при 24 строках.
const NVIM_LINES: usize = 120;
/// `TERM` нагрузки у обоих кандидатов; контроль без хоста получает тот же.
const TERM_VALUE: &str = "xterm-256color";

pub fn run(target: &Target, environment: &Environment) -> Result<Report, String> {
    let candidate = target
        .backend
        .ok_or("предусловие: сценарий apps идёт на кандидате (--backend)")?;
    let path = environment
        .process
        .env
        .get("PATH")
        .cloned()
        .ok_or("предусловие: нет PATH в окружении процесса")?;
    let env: Vec<(String, String)> = launch::INHERITED
        .iter()
        .chain([launch::LANG].iter())
        .filter_map(|name| {
            environment
                .process
                .env
                .get(*name)
                .map(|value| ((*name).to_owned(), value.clone()))
        })
        .chain([("TERM".to_owned(), TERM_VALUE.to_owned())])
        .collect();
    let mut stand = Stand::for_candidate(&target.out, environment, candidate)?;
    let outcome = parts(&mut stand, &path, &env);
    let notes = stand.teardown();
    let mut report = Report {
        cells: outcome?,
        ..Report::default()
    };
    report.diagnostics.notes.extend(notes);
    Ok(report)
}

fn parts(stand: &mut Stand, path: &str, env: &[(String, String)]) -> Result<Vec<Cell>, String> {
    let mut cells = vec![apps::cell("nvim", &nvim(stand, path, env)?)?];
    for agent in ["claude", "codex"] {
        let outcome = self::agent(stand, agent, path)?;
        cells.push(apps::cell(agent, &outcome)?);
    }
    Ok(cells)
}

/// Сессия с программой вместо фикстуры: стенд видит только запись `ensure`. Новизну нагрузки
/// доказывает отсутствие сессии в `list` до `ensure`: признак `created` сверяет argv процесса, а
/// программа из `PATH` может заменить себя другим исполняемым файлом (так делает `codex`).
fn ensure_program(
    stand: &mut Stand,
    label: &str,
    workload: &Workload,
) -> Result<(SessionId, crate::backend::HostRecord), String> {
    let id = stand.session(label)?;
    let listed = stand
        .main()
        .list()
        .map_err(|err| format!("list до ensure {id}: {err}"))?;
    if listed.iter().any(|entry| entry.name == id.as_str()) {
        return Err(format!("предусловие: сессия {id} уже есть до ensure"));
    }
    stand.sessions.push(id.clone());
    let ensured = stand
        .main()
        .ensure(&id, workload, stand.size)
        .map_err(|err| format!("ensure {id}: {err}"))?;
    Ok((id, ensured.record))
}

/// Экран приёмника без истории и скрытого состояния: строки активной области и курсор.
fn screen_of(rx: &Receiver) -> Option<Screen> {
    let state = rx
        .capture(Capture {
            history: 0,
            hidden: false,
        })
        .ok()?;
    Some(Screen {
        lines: state.text(),
        cursor: Some((f64::from(state.cursor.x), f64::from(state.cursor.y))),
    })
}

/// Экран клиента, когда `done` выполнено; `None` — срок вышел или маршрут закончился.
fn wait_screen(
    client: &Client,
    timeout: Duration,
    done: impl Fn(&Screen) -> bool,
) -> Result<Option<Screen>, String> {
    let route = client.route()?;
    let reached = route
        .wait_until(timeout, |rx, _| {
            screen_of(rx).is_some_and(|screen| done(&screen))
        })
        .is_ok();
    let screen = route.with(|rx| screen_of(rx));
    Ok(if reached { screen } else { None })
}

fn last_screen(client: &Client) -> Result<Option<Screen>, String> {
    Ok(client.route()?.with(|rx| screen_of(rx)))
}

/// Shift-Enter кодировщиком приёмника клиента при его текущих режимах.
fn shift_enter(client: &Client) -> Result<Vec<u8>, String> {
    client.route()?.with(encode_shift_enter)
}

fn encode_shift_enter(rx: &mut Receiver) -> Result<Vec<u8>, String> {
    let mut keys = KeyEncoder::new().map_err(|err| format!("кодировщик клавиш: {err}"))?;
    keys.sync_from(rx.terminal());
    keys.encode(&KeyInput {
        key: Key::ENTER,
        action: KeyAction::Press,
        mods: MODS_SHIFT,
        text: String::new(),
        unshifted: None,
    })
    .map_err(|err| format!("Shift-Enter: {err}"))
}

fn kitty_flags(client: &Client) -> Result<u8, String> {
    client
        .route()?
        .with(|rx| {
            rx.capture(Capture {
                history: 0,
                hidden: false,
            })
        })
        .map(|state| state.kitty_flags)
        .map_err(|err| format!("состояние приёмника: {err}"))
}

/// Шаг ввода с ожиданием экрана: провал ожидания — проверка части с последним экраном.
struct Steps<'a> {
    outcome: &'a mut Outcome,
}

impl Steps<'_> {
    fn wait(
        &mut self,
        client: &Client,
        name: &str,
        done: impl Fn(&Screen) -> bool,
    ) -> Result<Option<Screen>, String> {
        match wait_screen(client, WAIT, done)? {
            Some(screen) => {
                self.outcome.evidence.check(name, true, "ok");
                Ok(Some(screen))
            }
            None => {
                let last = last_screen(client)?;
                self.outcome.evidence.note(&format!("{name}.screen"), &last);
                self.outcome.evidence.check(
                    name,
                    false,
                    format!("экран не дошёл до условия за {} с", WAIT.as_secs()),
                );
                Ok(None)
            }
        }
    }
}

fn nvim(stand: &mut Stand, path: &str, env: &[(String, String)]) -> Result<Outcome, String> {
    let Some(program) = apps::which("nvim", path) else {
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
    let control = nvim_control(stand, &program, env, &file)?;
    outcome.evidence.note("control", &control);
    let file_path = stand.out.join("nvim-headless.txt");
    fs::write(&file_path, file.contents())
        .map_err(|err| format!("{}: {err}", file_path.display()))?;
    let workload = Workload {
        program: PathBuf::from("/bin/sh"),
        args: vec![
            "-c".to_owned(),
            apps::START_ON_LINE.to_owned(),
            program.display().to_string(),
            "--clean".to_owned(),
            "-n".to_owned(),
            file_path.display().to_string(),
        ],
    };
    let (id, record) = ensure_program(stand, "nvim", &workload)?;
    let result = nvim_steps(
        stand,
        &id,
        &record,
        &file,
        &file_path,
        &control,
        &mut outcome,
    );
    outcome.artifacts.push(stand.artifact(
        "f4-nvim.json",
        &json!({ "session": id.as_str(), "evidence": &outcome.evidence }),
    )?);
    result.map(|()| outcome)
}

/// Что показал nvim без хоста в том же приёмнике стенда.
#[derive(Debug, Default, serde::Serialize)]
struct NvimControl {
    /// Экран с файлом дошёл до условия.
    started: bool,
    kitty_flags: u8,
    /// Shift-Enter кодировщиком приёмника при режимах nvim, hex.
    shift_enter: String,
    exited: bool,
}

/// Контроль без хоста: nvim прямо в PTY стенда с тем же приёмником и окружением, что у
/// нагрузки кандидата. Показывает, включает ли nvim протокол Kitty у этого приёмника вообще.
fn nvim_control(
    stand: &Stand,
    program: &std::path::Path,
    env: &[(String, String)],
    file: &NvimFile,
) -> Result<NvimControl, String> {
    let file_path = stand.out.join("nvim-control.txt");
    fs::write(&file_path, file.contents())
        .map_err(|err| format!("{}: {err}", file_path.display()))?;
    let args = vec![
        "--clean".to_owned(),
        "-n".to_owned(),
        file_path.display().to_string(),
    ];
    let config = Config {
        cols: stand.size.cols,
        rows: stand.size.rows,
        ..stand.config.clone()
    };
    let receiver = Receiver::new("ctl", &config, ReceiverProfile::for_client("ctl"))
        .map_err(|err| format!("приёмник контроля: {err}"))?;
    let PtyChild {
        master, mut child, ..
    } = pty::spawn(&Spawn {
        program,
        args: &args,
        env,
        size: stand.size,
    })
    .map_err(|err| format!("nvim без хоста: {err}"))?;
    let route = Route::start(master, receiver).map_err(|err| format!("маршрут контроля: {err}"))?;
    let first = file.original[0].clone();
    let mut control = NvimControl {
        started: route
            .wait_until(WAIT, |rx, _| {
                screen_of(rx)
                    .is_some_and(|s| s.lines.first().is_some_and(|line| line.contains(&first)))
            })
            .is_ok(),
        ..NvimControl::default()
    };
    if control.started {
        control.kitty_flags = route
            .with(|rx| {
                rx.capture(Capture {
                    history: 0,
                    hidden: false,
                })
            })
            .map(|state| state.kitty_flags)
            .map_err(|err| format!("состояние приёмника контроля: {err}"))?;
        control.shift_enter = apps::hex(&route.with(encode_shift_enter)?);
    }
    route.input(b"\x1b:qa!\r");
    let pid = child.id().cast_signed();
    control.exited = launch::wait_exit(&[pid], WAIT).is_empty();
    drop(route);
    if !control.exited {
        let _ = child.kill();
    }
    let _ = child.wait();
    Ok(control)
}

fn nvim_steps(
    stand: &mut Stand,
    id: &SessionId,
    record: &crate::backend::HostRecord,
    file: &NvimFile,
    file_path: &std::path::Path,
    control: &NvimControl,
    outcome: &mut Outcome,
) -> Result<(), String> {
    let mut c1 = stand.client(id, "c1")?;
    if outcome
        .evidence
        .require("c1 лидер", c1.wait_leader())
        .is_none()
    {
        return Ok(());
    }
    // Строка для `read` обёртки: nvim стартует при подключённом c1.
    c1.input(b"\r")?;
    let first = file.original[0].clone();
    let mut steps = Steps {
        outcome: &mut *outcome,
    };
    if steps
        .wait(&c1, "файл на экране", |s| {
            s.lines.first().is_some_and(|line| line.contains(&first))
        })?
        .is_none()
    {
        return Ok(());
    }
    c1.input(b":set number\r")?;
    if steps
        .wait(&c1, ":set number", |s| {
            s.lines
                .first()
                .is_some_and(|line| line.trim_start().starts_with(&format!("1 {first}")))
        })?
        .is_none()
    {
        return Ok(());
    }
    c1.input(b":vsplit\r")?;
    if steps
        .wait(&c1, ":vsplit", |s| {
            s.lines
                .first()
                .is_some_and(|line| line.matches(first.as_str()).count() == 2)
        })?
        .is_none()
    {
        return Ok(());
    }
    c1.input(format!("\x17l{}Go", file.after).as_bytes())?;
    if steps
        .wait(&c1, "вставка во втором окне", |s| {
            s.lines
                .last()
                .is_some_and(|line| line.contains("-- INSERT --"))
        })?
        .is_none()
    {
        return Ok(());
    }
    let flags_before = kitty_flags(&c1)?;
    let first_shift = shift_enter(&c1)?;
    c1.input(NVIM_BEFORE.as_bytes())?;
    c1.input(b"\x16")?;
    c1.input(&first_shift)?;
    c1.input(NVIM_MID.as_bytes())?;
    let Some(before) = steps.wait(&c1, "строка до SIGKILL", |s| {
        apps::mark_position(&s.lines, NVIM_MID)
            .is_some_and(|(x, y)| s.cursor == Some((f64::from(x), f64::from(y))))
    })?
    else {
        return Ok(());
    };
    let evidence = &mut outcome.evidence;
    evidence.note("kitty_flags_before", flags_before);
    evidence.note("shift_enter_before", apps::hex(&first_shift));
    evidence.note("screen_before", &before);
    // Ответы приёмника c1 по порядку: какие запросы nvim при старте дошли до терминала клиента.
    let replies: Vec<String> = c1.route()?.with(|rx| {
        rx.journal()
            .iter()
            .filter_map(|entry| match &entry.event {
                Journaled::Reply { hex } => Some(format!("{} {hex}", entry.at)),
                _ => None,
            })
            .collect()
    });
    evidence.note("c1_replies", &replies);
    // Наблюдение, а не проверка: в объявленном критерии части его нет (⚠️ задача 19).
    evidence.note(
        "kitty_vs_control",
        json!({
            "candidate": { "flags": flags_before, "shift_enter": apps::hex(&first_shift) },
            "control": { "flags": control.kitty_flags, "shift_enter": &control.shift_enter },
            "same": flags_before == control.kitty_flags
                && apps::hex(&first_shift) == control.shift_enter,
        }),
    );
    if !control.started || control.kitty_flags == 0 {
        outcome.mark_not_run(
            NotRunCause::Stand,
            "nvim без хоста не включил протокол Kitty у приёмника стенда: сравнение \
             Ctrl-V Shift-Enter не различает режимы клавиатуры",
        );
    }
    let evidence = &mut outcome.evidence;
    evidence.require("SIGKILL c1", c1.kill(stand.main()));
    drop(c1);
    if !stand::same_alive(evidence, "nvim жив после SIGKILL клиента", record.workload)?
    {
        return Ok(());
    }
    let c2 = stand.client(id, "c2")?;
    if evidence.require("c2 лидер", c2.wait_leader()).is_none() {
        return Ok(());
    }
    let restored = wait_screen(&c2, WAIT, |s| apps::compare_screens(&before, s).is_empty())?;
    let after = match restored {
        Some(screen) => screen,
        None => last_screen(&c2)?.unwrap_or(Screen {
            lines: Vec::new(),
            cursor: None,
        }),
    };
    let diff = apps::compare_screens(&before, &after);
    evidence.note("screen_after", &after);
    evidence.note("screen_diff", &diff);
    evidence.check(
        "экран после attach равен экрану до SIGKILL",
        diff.is_empty(),
        if diff.is_empty() {
            "строки, разбиение, номера, строка режима и курсор совпали".to_owned()
        } else {
            diff.summary()
        },
    );
    let flags_after = kitty_flags(&c2)?;
    let second_shift = shift_enter(&c2)?;
    evidence.note("kitty_flags_after", flags_after);
    evidence.note("shift_enter_after", apps::hex(&second_shift));
    c2.input(b"\x16")?;
    c2.input(&second_shift)?;
    c2.input(NVIM_AFTER.as_bytes())?;
    // Esc и «:» одной записью nvim читает как Alt-: в режиме вставки.
    c2.input(b"\x1b")?;
    let normal = wait_screen(&c2, WAIT, |s| {
        s.lines
            .last()
            .is_some_and(|line| !line.contains("-- INSERT --"))
    })?;
    if !evidence.check(
        "выход из режима вставки",
        normal.is_some(),
        format!("строка режима не сменилась за {} с", WAIT.as_secs()),
    ) {
        evidence.note("insert.screen", &last_screen(&c2)?);
        return Ok(());
    }
    // `:wq` закрыл бы только окно разбиения: второе окно того же файла держит nvim.
    c2.input(b":wqa\r")?;
    let left = launch::wait_exit(&[record.workload.pid], WAIT);
    if !evidence.check(
        "nvim вышел по :wqa",
        left.is_empty(),
        format!("PID {} жив через {} с", record.workload.pid, WAIT.as_secs()),
    ) {
        evidence.note("wqa.screen", &last_screen(&c2)?);
        return Ok(());
    }
    let written = fs::read(file_path).map_err(|err| format!("{}: {err}", file_path.display()))?;
    match file.parse(&written) {
        Ok(insert) => {
            evidence.note("file_insert", &insert);
            if insert.first == apps::hex(b"\r") {
                outcome.mark_not_run(
                    NotRunCause::Stand,
                    "Ctrl-V Shift-Enter вставил голый CR: сравнение до и после ничего не доказывает",
                );
            }
            let evidence = &mut outcome.evidence;
            evidence.check(
                "Ctrl-V Shift-Enter после attach даёт то же, что до SIGKILL",
                insert.same(),
                format!("до {} после {}", insert.first, insert.second),
            );
            evidence.check(
                "остальные строки файла не тронуты",
                insert.rest_intact,
                "строки вне вставки изменились",
            );
        }
        Err(err) => {
            evidence.check("файл после :wq", false, err);
        }
    }
    Ok(())
}

// --- агенты ---

/// Строки приёмника по порядку: история от старых к новым, затем активная область.
fn all_lines(rx: &Receiver) -> Vec<String> {
    let Ok(state) = rx.capture(Capture {
        history: usize::MAX,
        hidden: false,
    }) else {
        return Vec::new();
    };
    state
        .history
        .iter()
        .rev()
        .chain(state.active.iter())
        .map(crate::oracle::model::Row::text)
        .collect()
}

fn numbers_of(client: &Client) -> Result<Vec<u64>, String> {
    Ok(client
        .route()?
        .with(|rx| apps::answer_numbers(&all_lines(rx))))
}

/// Экран-заставка агента: доверие к каталогу, обновление. Стенд на неё не отвечает: выбор
/// по умолчанию меняет настройки агента пользователя или ставит обновление.
fn interstitial(lines: &[String]) -> bool {
    lines.iter().any(|line| {
        let line = line.to_lowercase();
        line.contains("trust") || line.contains("press enter")
    })
}

/// Итог ожидания ответа агента.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Waited {
    Reached,
    /// Агент ждёт ответа на экран-заставку.
    Interstitial,
    /// Срок вышел или клиент закончился.
    Missed,
}

/// Ждёт `done` над номерами ответа до `deadline`; экран-заставка заканчивает ожидание.
fn wait_answer(
    client: &Client,
    deadline: Instant,
    done: impl Fn(&[u64]) -> bool,
) -> Result<Waited, String> {
    let route = client.route()?;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(Waited::Missed);
        }
        let step = left.min(AGENT_STEP);
        let reached = route.wait_until(step, |rx, _| {
            let lines = all_lines(rx);
            done(&apps::answer_numbers(&lines))
        });
        if reached.is_ok() {
            return Ok(Waited::Reached);
        }
        if route.end().is_some() {
            return Ok(Waited::Missed);
        }
        let screen = route.with(|rx| screen_of(rx)).map(|s| s.lines);
        if screen.is_some_and(|lines| interstitial(&lines)) {
            return Ok(Waited::Interstitial);
        }
    }
}

/// Ответ на экран-заставку не даётся: часть не доказана стендом.
const INTERSTITIAL: &str = "агент показал экран-заставку (доверие к каталогу или обновление): \
                            стенд на неё не отвечает, чтобы не менять настройки агента";

fn agent(stand: &mut Stand, name: &str, path: &str) -> Result<Outcome, String> {
    let Some(program) = apps::which(name, path) else {
        return Ok(Outcome::not_run(
            NotRunCause::Stand,
            format!("{name} нет в PATH окружения процесса"),
        ));
    };
    let workload = Workload {
        program,
        args: vec![AGENT_PROMPT.to_owned()],
    };
    let (id, record) = ensure_program(stand, name, &workload)?;
    let mut outcome = Outcome {
        origin: Some(DefectOrigin::Host),
        ..Outcome::default()
    };
    let mut facts = serde_json::Map::new();
    let result = agent_steps(stand, &id, &record, &mut outcome, &mut facts);
    let notes = stand.discard(&id);
    facts.insert("discard".to_owned(), json!(notes));
    let survivors = crate::backend::alive(&[record.workload, record.host]).unwrap_or_default();
    facts.insert("survivors".to_owned(), json!(survivors.len()));
    outcome.artifacts.push(stand.artifact(
        &format!("f4-{name}.json"),
        &json!({
            "session": id.as_str(),
            "facts": facts,
            "checks": &outcome.evidence.checks,
            "not_proven": &outcome.evidence.not_proven,
        }),
    )?);
    // Наблюдения части агента — только числа и проверки: экран в результат не идёт.
    outcome.evidence.observed.clear();
    result.map(|()| outcome)
}

fn agent_steps(
    stand: &mut Stand,
    id: &SessionId,
    record: &crate::backend::HostRecord,
    outcome: &mut Outcome,
    facts: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<(), String> {
    let evidence = &mut outcome.evidence;
    let mut c1 = stand.client(id, "c1")?;
    if evidence.require("c1 лидер", c1.wait_leader()).is_none() {
        return Ok(());
    }
    let started = Instant::now();
    let mid = wait_answer(&c1, started + AGENT_WAIT, |numbers| {
        numbers.len() >= AGENT_MID
    })?;
    facts.insert(
        "start_ms".to_owned(),
        json!(started.elapsed().as_millis() as u64),
    );
    if mid == Waited::Interstitial {
        outcome.mark_not_run(NotRunCause::Stand, INTERSTITIAL);
        return Ok(());
    }
    if mid == Waited::Missed {
        outcome.mark_not_run(
            NotRunCause::Stand,
            format!(
                "ответ не начался за {} с: нет входа, сети или экран агента не распознан",
                AGENT_WAIT.as_secs()
            ),
        );
        return Ok(());
    }
    let before = numbers_of(&c1)?;
    let summary_before = Numbers::of(&before, AGENT_COUNT);
    facts.insert("before".to_owned(), json!(summary_before));
    // Контроль эха: тот же ввод без отправки у c1 до SIGKILL. Без него провал эха после attach
    // не отличить от экрана агента, который стенд не понимает.
    c1.input(AGENT_CONTROL.as_bytes())?;
    let control_echo = wait_screen(&c1, WAIT, |s| {
        s.lines.iter().any(|line| line.contains(AGENT_CONTROL))
    })?
    .is_some();
    facts.insert("control_echo".to_owned(), json!(control_echo));
    let evidence = &mut outcome.evidence;
    evidence.require("SIGKILL c1", c1.kill(stand.main()));
    drop(c1);
    if summary_before.complete() {
        outcome.mark_not_run(
            NotRunCause::Stand,
            "ответ пришёл целиком до SIGKILL клиента: середина ответа не застигнута",
        );
        return Ok(());
    }
    if !stand::same_alive(evidence, "агент жив после SIGKILL клиента", record.workload)?
    {
        return Ok(());
    }
    let c2 = stand.client(id, "c2")?;
    if evidence.require("c2 лидер", c2.wait_leader()).is_none() {
        return Ok(());
    }
    let finished = Instant::now();
    let waited = wait_answer(&c2, finished + AGENT_WAIT, |numbers| {
        Numbers::of(numbers, AGENT_COUNT).missing == 0
    })?;
    if waited == Waited::Interstitial {
        outcome.mark_not_run(NotRunCause::Stand, INTERSTITIAL);
        return Ok(());
    }
    let complete = waited == Waited::Reached;
    let after = numbers_of(&c2)?;
    let summary_after = Numbers::of(&after, AGENT_COUNT);
    facts.insert("after".to_owned(), json!(summary_after));
    facts.insert(
        "finish_ms".to_owned(),
        json!(finished.elapsed().as_millis() as u64),
    );
    let evidence = &mut outcome.evidence;
    evidence.check(
        "ответ дописан после attach",
        complete,
        format!(
            "нет {} номеров из {AGENT_COUNT} за {} с",
            summary_after.missing,
            AGENT_WAIT.as_secs()
        ),
    );
    evidence.check(
        "строки до SIGKILL на месте по порядку",
        apps::kept_in_order(&before, &after),
        format!(
            "до SIGKILL {} строк ответа, после attach {}",
            summary_before.count, summary_after.count
        ),
    );
    c2.input(AGENT_NEXT.as_bytes())?;
    let echoed = wait_screen(&c2, WAIT, |s| {
        s.lines.iter().any(|line| line.contains(AGENT_NEXT))
    })?
    .is_some();
    facts.insert("echo".to_owned(), json!(echoed));
    if control_echo {
        evidence.check(
            "следующий запрос вводится",
            echoed,
            format!("эхо ввода не видно за {} с", WAIT.as_secs()),
        );
    } else {
        outcome.mark_not_run(
            NotRunCause::Stand,
            "эхо ввода не видно и у c1 до SIGKILL: стенд не распознаёт поле ввода агента",
        );
    }
    let evidence = &mut outcome.evidence;
    stand::same_alive(evidence, "агент жив после attach", record.workload)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interstitial_screens_are_recognized() {
        let screen = |text: &str| vec![text.to_owned()];
        assert!(interstitial(&screen(
            " Do you trust the files in this folder?"
        )));
        assert!(interstitial(&screen("Press Enter to continue")));
        assert!(!interstitial(&screen("42")));
    }
}
