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
    self, AGENT_COUNT, AGENT_NEXT, AGENT_PROMPT, NVIM_AFTER, NVIM_BEFORE, NVIM_MID, Numbers,
    NvimFile, Outcome, Screen,
};
use crate::matrix::stand::{self, Client, Stand, WAIT};
use crate::oracle::vt::{Capture, Receiver};
use crate::results::{Cell, DefectOrigin, Environment, NotRunCause, Report};
use crate::vt::{Key, KeyAction, KeyEncoder, KeyInput, MODS_SHIFT};

/// Срок начала и конца ответа агента: запрос идёт в сеть.
const AGENT_WAIT: Duration = Duration::from_secs(120);
/// Шаг ожидания агента: между шагами стенд проверяет экраны-заставки.
const AGENT_STEP: Duration = Duration::from_secs(2);
/// Сколько строк ответа должно прийти до `SIGKILL` клиента «посреди ответа».
const AGENT_MID: usize = 20;
/// Наибольшее число нажатий Enter на экранах-заставках агента (доверие к каталогу, обновление).
const AGENT_ENTERS: usize = 3;
/// Строк в файле nvim: несколько экранов при 24 строках.
const NVIM_LINES: usize = 120;

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
    let mut stand = Stand::for_candidate(&target.out, environment, candidate)?;
    let outcome = parts(&mut stand, &path);
    let notes = stand.teardown();
    let mut report = Report {
        cells: outcome?,
        ..Report::default()
    };
    report.diagnostics.notes.extend(notes);
    Ok(report)
}

fn parts(stand: &mut Stand, path: &str) -> Result<Vec<Cell>, String> {
    let mut cells = vec![apps::cell("nvim", &nvim(stand, path)?)?];
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
    client.route()?.with(|rx| {
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
    })
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

/// Программа по имени в каталогах `PATH`.
pub fn which(name: &str, path: &str) -> Option<PathBuf> {
    path.split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| PathBuf::from(dir).join(name))
        .find(|candidate| candidate.is_file())
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

fn nvim(stand: &mut Stand, path: &str) -> Result<Outcome, String> {
    let Some(program) = which("nvim", path) else {
        return Ok(Outcome::not_run(
            NotRunCause::Stand,
            "nvim нет в PATH окружения процесса",
        ));
    };
    let file = NvimFile::numbered(NVIM_LINES);
    let file_path = stand.out.join("nvim-headless.txt");
    fs::write(&file_path, file.contents())
        .map_err(|err| format!("{}: {err}", file_path.display()))?;
    let workload = Workload {
        program,
        args: vec![
            "--clean".to_owned(),
            "-n".to_owned(),
            file_path.display().to_string(),
        ],
    };
    let (id, record) = ensure_program(stand, "nvim", &workload)?;
    let mut outcome = Outcome {
        origin: Some(DefectOrigin::Host),
        ..Outcome::default()
    };
    let result = nvim_steps(stand, &id, &record, &file, &file_path, &mut outcome);
    outcome.artifacts.push(stand.artifact(
        "f4-nvim.json",
        &json!({ "session": id.as_str(), "evidence": &outcome.evidence }),
    )?);
    result.map(|()| outcome)
}

fn nvim_steps(
    stand: &mut Stand,
    id: &SessionId,
    record: &crate::backend::HostRecord,
    file: &NvimFile,
    file_path: &std::path::Path,
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

/// Экран-заставка агента, на котором стенд нажимает Enter: доверие к каталогу, обновление.
fn interstitial(lines: &[String]) -> bool {
    lines.iter().any(|line| {
        let line = line.to_lowercase();
        line.contains("trust") || line.contains("press enter")
    })
}

/// Ждёт `done` над номерами ответа до `deadline`, нажимая Enter на заставках; итог — достигнуто
/// ли условие и сколько раз нажат Enter.
fn wait_answer(
    client: &Client,
    deadline: Instant,
    enters: &mut usize,
    done: impl Fn(&[u64]) -> bool,
) -> Result<bool, String> {
    let route = client.route()?;
    let mut handled: Option<Vec<String>> = None;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(false);
        }
        let step = left.min(AGENT_STEP);
        let reached = route.wait_until(step, |rx, _| {
            let lines = all_lines(rx);
            done(&apps::answer_numbers(&lines))
        });
        if reached.is_ok() {
            return Ok(true);
        }
        if route.end().is_some() {
            return Ok(false);
        }
        let screen = route.with(|rx| screen_of(rx)).map(|s| s.lines);
        if let Some(lines) = screen
            && interstitial(&lines)
            && handled.as_ref() != Some(&lines)
            && *enters < AGENT_ENTERS
        {
            client.input(b"\r")?;
            *enters += 1;
            handled = Some(lines);
        }
    }
}

fn agent(stand: &mut Stand, name: &str, path: &str) -> Result<Outcome, String> {
    let Some(program) = which(name, path) else {
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
    let mut enters = 0;
    let started = Instant::now();
    let mid = wait_answer(&c1, started + AGENT_WAIT, &mut enters, |numbers| {
        numbers.len() >= AGENT_MID
    })?;
    facts.insert("enters".to_owned(), json!(enters));
    facts.insert(
        "start_ms".to_owned(),
        json!(started.elapsed().as_millis() as u64),
    );
    if !mid {
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
    let complete = wait_answer(&c2, finished + AGENT_WAIT, &mut enters, |numbers| {
        Numbers::of(numbers, AGENT_COUNT).missing == 0
    })?;
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
    evidence.check(
        "следующий запрос вводится",
        echoed,
        format!("эхо ввода не видно за {} с", WAIT.as_secs()),
    );
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
