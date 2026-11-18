//! Сценарий `window-update` (задача 9): клетка H «Связный проход в окне». GUI N запускается
//! через `open` с `--window`, сессия `h1` идёт от хоста 1 из `runtime/1/`, окно N показывает её.
//! `update` поколением 2 и `activate`: GUI N выходит, помощник запускает N+1 через `open -n` с
//! аргументами режима из журнала, поэтому N+1 тоже с окном. N+1 стоит на барьере перед
//! публикацией сокета: клиентов у `h1` нет, стенд выводит строки через управляющее соединение.
//! После подтверждения окно N+1 само открывает ту же сессию с байта 0. Части:
//!
//! 1. `launch-services` — GUI N и N+1 запущены LaunchServices с окном, поколения 1 и 2, журнал
//!    `idle` «подтверждена», хост `h1` из `runtime/1/`;
//! 2. `tail` — квитанция окна N+1 (текст, прочитанный обратно из `NSTextView`): строки с 1 подряд
//!    без пропуска и повтора, в ней вывод без клиента, квитанция health и живой вывод;
//! 3. `ax-matches-receipt` — `AXValue` текстовой области окна N+1 совпадает с квитанцией окна;
//! 4. `synthetic-input` — нажатия клавиш в процесс N+1 доходят до нагрузки с квитанцией.
//!
//! Невыполненный оракул — `fail` части. Нет доверия Accessibility или права посылать события,
//! нечитаемая доступность, сломанный стенд — ошибка исполнения: клетка обязательная.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::json;

use crate::attach::{self, Attached, Connected};
use crate::barrier::{self, Barrier, Point};
use crate::compat::Staging;
use crate::fixture;
use crate::generation::Layout;
use crate::host::SessionRecord;
use crate::journal::{Outcome, State};
use crate::observer;
use crate::owner::{self, Answer, AppReport, OwnerInfo, Reply, Request};
use crate::proto::{ATTACH_VERSIONS, Purpose, Status as HostStatus};
use crate::pty::Winsize;
use crate::results::{Cell, Diagnostics, Environment, Report};
use crate::window::{self, AxRead, Receipt, Wait};
use crate::{launch, spawn};

use super::owner_basic::{Guard, ask, reports};
use super::update_flow::{self, Stand};
use super::{PartSpec, Verdict, required_cell};

pub const DEADLINE: Duration = Duration::from_secs(240);
pub const DIR: &str = "window";
pub const STEPS_ARTIFACT: &str = "window/steps.json";
pub const CELL: &str = "H";
const OPEN: &str = "/usr/bin/open";
/// Строк, которые нагрузка выводит без клиента.
pub const OFFLINE_LINES: u64 = 20;
/// Строк живого вывода в окно N+1.
pub const LIVE_LINES: u64 = 5;
/// Команда синтетического ввода.
pub const TYPED: &str = "emit 2";
/// Где стоит N+1, пока стенд выводит строки без клиента.
pub const HOLD: Barrier = Barrier::new(barrier::Role::Next, Point::BeforePublish);
const COMPLETION: &str = "квитанция окна, ответ владельца, барьер или выход процесса в срок шага (30 с); иначе ошибка исполнения";

pub const LAUNCH: PartSpec = PartSpec {
    name: "launch-services",
    stimulus: "GUI N через open с --window, сессия h1, update поколением 2 и activate; N+1 запускает помощник через open -n",
    oracle: "GUI N и N+1 — дети launchd (ppid 1), исполняемый файл каждого — установленный бандл, у обоих окно; N — поколение 1, N+1 — 2; GUI N вышел со строкой updated; журнал idle «подтверждена»; хост h1 из runtime/1/ с прежними PID и временем старта",
};
pub const TAIL: PartSpec = PartSpec {
    name: "tail",
    stimulus: "на барьере N+1 перед публикацией сокета (клиентов у h1 нет) — emit 20 через управляющее соединение; после подтверждения — emit 5 при открытом окне N+1",
    oracle: "квитанция окна N+1 — текст, прочитанный обратно из NSTextView после отрисовки: строки с 1 подряд без пропуска и повтора; в ней все 20 строк вывода без клиента и квитанция health, затем 5 строк живого вывода с квитанцией; окно подключено к h1 протоколом 1",
};
pub const AX: PartSpec = PartSpec {
    name: "ax-matches-receipt",
    stimulus: "чтение AXValue текстовой области окна N+1 через доступность из процесса сценария между двумя квитанциями окна",
    oracle: "квитанции до и после чтения совпадают; у окна N+1 ровно одна AXTextArea, её строки с номерами совпадают с квитанцией окна построчно",
};
pub const INPUT: PartSpec = PartSpec {
    name: "synthetic-input",
    stimulus: "нажатия «emit 2» и Return в очередь процесса N+1 (CGEventPostToPid)",
    oracle: "окно отправило ровно один ввод «emit 2» со своим номером; его квитанция ack и две строки emit — в квитанции окна после последней строки до ввода, без пропуска; та же квитанция в хвосте независимого attach",
};

fn violation(violations: &mut Vec<String>, ok: bool, what: impl FnOnce() -> String) {
    if !ok {
        violations.push(what());
    }
}

fn part(spec: &PartSpec, verdict: Verdict) -> Cell {
    required_cell(
        CELL,
        spec,
        COMPLETION,
        verdict,
        &[
            STEPS_ARTIFACT,
            "window/owner-n.stdout",
            "window/receipts.json",
        ],
    )
}

pub fn run(out: &Path, environment: &Environment) -> Result<Report, String> {
    let (state, nonce, layout) = super::stand_inputs(environment)?;
    let dir = out.join(DIR);
    fs::create_dir(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    let mut guard = Guard {
        state: state.clone(),
        apps: Vec::new(),
        sessions: Vec::new(),
        nonce: Some(nonce.clone()),
        armed: true,
    };
    let mut steps = Vec::new();
    let mut receipts = Vec::new();
    let walked = walk(
        &state,
        &nonce,
        &layout,
        &dir,
        &mut steps,
        &mut receipts,
        &mut guard,
    );
    drop(guard);
    if let Err(reason) = &walked {
        steps.push(json!({ "name": "error", "reason": reason }));
    }
    let written = crate::results::write_json(&dir, "steps.json", &steps)
        .and_then(|_| crate::results::write_json(&dir, "receipts.json", &receipts));
    let cells = walked?;
    written?;
    Ok(Report {
        cells,
        diagnostics: Diagnostics {
            notes: vec![format!(
                "срок health {} с от intent, срок шага стенда {} с; вывод без клиента — {OFFLINE_LINES} строк, живой — {LIVE_LINES}",
                crate::update::HEALTH_WINDOW.as_secs(),
                update_flow::STEP.as_secs()
            )],
        },
    })
}

/// Квитанция окна с меткой шага.
#[derive(Clone, Debug, Serialize)]
pub struct Named {
    pub name: &'static str,
    pub receipt: Receipt,
}

/// `open` установленного бандла с аргументами режима стенда; stdout и stderr — в `out`.
fn open_app(stand: &Stand, out: &Path, name: &str) -> Result<(), String> {
    let mut args = vec![
        "--env".to_owned(),
        format!("{}={}", launch::STATE_DIR, stand.state.display()),
        "--stdout".to_owned(),
        out.join(format!("{name}.stdout")).display().to_string(),
        "--stderr".to_owned(),
        out.join(format!("{name}.stderr")).display().to_string(),
        stand.installed.display().to_string(),
        "--args".to_owned(),
    ];
    args.extend(stand.options.relaunch_args());
    let output = spawn::output(Path::new(OPEN), &args).map_err(|err| format!("open: {err}"))?;
    if output.exit.success() {
        Ok(())
    } else {
        Err(format!(
            "стенд: open {}: {}; {}",
            stand.installed.display(),
            output.exit,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn window_receipt(state: &Path, wait: Wait) -> Result<Receipt, String> {
    let request = Request::WindowReceipt {
        wait,
        wait_ms: update_flow::STEP.as_millis() as u64,
    };
    match owner::request(state, &request, update_flow::STEP + update_flow::STEP)? {
        Answer::Reply(Reply::Window(receipt)) => Ok(*receipt),
        other => Err(format!("стенд: квитанция окна: {other:?}")),
    }
}

/// Управляющее соединение с хостом сессии диапазоном протокола её хоста.
fn control(record: &SessionRecord) -> Result<attach::Client, String> {
    let hello = attach::hello(Purpose::Control, record.versions, 0, owner::SESSION_SIZE);
    match attach::connect(Path::new(&record.socket), &hello, update_flow::STEP)? {
        Connected::Welcome(client) => Ok(client),
        Connected::Refused(reason) => Err(format!("стенд: control {}: {reason}", record.session)),
    }
}

/// Ввод через управляющее соединение и ожидание `lines` строк вывода; состояние хоста до и после.
fn emit_offline(
    record: &SessionRecord,
    seq: u64,
    command: &str,
    lines: u64,
) -> Result<(HostStatus, HostStatus), String> {
    let mut client = control(record)?;
    let before = client.status()?;
    client.input(seq, command)?;
    let after = client.await_lines(before.lines + lines, update_flow::STEP)?;
    if after.lines < before.lines + lines {
        return Err(format!(
            "стенд: после {command} строк {}, ждали {}",
            after.lines,
            before.lines + lines
        ));
    }
    Ok((before, after))
}

#[allow(clippy::too_many_arguments)]
fn walk(
    state: &Path,
    nonce: &str,
    layout: &Layout,
    out: &Path,
    steps: &mut Vec<serde_json::Value>,
    receipts: &mut Vec<Named>,
    guard: &mut Guard,
) -> Result<Vec<Cell>, String> {
    if !window::ax_trusted() {
        return Err(
            "предусловие: процесс сценария не доверен Accessibility (AXIsProcessTrusted)"
                .to_owned(),
        );
    }
    if !window::post_event_access() {
        return Err(
            "предусловие: нет права посылать события (CGPreflightPostEventAccess)".to_owned(),
        );
    }
    let mut stand = Stand::new(state, nonce, layout, "gen-1")?;
    stand.options.window = true;
    let gen2 = layout.get("gen-2")?;
    barrier::arm(state, HOLD)?;

    // GUI N через LaunchServices, сессия h1 и квитанция в окне N.
    open_app(&stand, out, "owner-n")?;
    let owner_n = owner::wait_for_owner(state, Instant::now() + update_flow::STEP)?;
    let n_process = observer::inspect(owner_n.key.pid)?;
    let record = stand.create_session(guard, "h1")?;
    let host_path = crate::sys::process_path(record.host.pid)
        .map(|path| path.display().to_string())
        .map_err(|err| format!("proc_pidpath хоста: {err}"))?;
    // Ввод только после строки ready: до неё PTY нагрузки ещё не в raw-режиме и отражает ввод.
    let mut first = control(&record)?;
    first.await_lines(1, update_flow::STEP)?;
    first.input(1, "ping")?;
    let receipt_n = window_receipt(
        state,
        Wait::Line {
            text: fixture::receipt(1, "ping"),
        },
    )?;
    receipts.push(Named {
        name: "n",
        receipt: receipt_n.clone(),
    });
    steps.push(json!({
        "name": "owner-n",
        "owner": owner_n,
        "process": n_process,
        "record": record,
        "host_path": host_path,
    }));
    if !receipt_n.satisfied {
        return Err(format!(
            "стенд: окно N не показало квитанцию ping: {:?}",
            receipt_n.attach_error
        ));
    }

    // Обновление.
    let staged = match ask(
        state,
        &Request::Update {
            candidate: gen2.app.display().to_string(),
        },
    )? {
        Reply::Staging(staging) => match *staging {
            Staging::Staged { attempt, .. } => attempt,
            other => return Err(format!("стенд: update gen-2: {other:?}")),
        },
        other => return Err(format!("стенд: update gen-2: {other:?}")),
    };
    let (attempt, helper) = update_flow::activate(state)?;
    if attempt != staged {
        return Err(format!("стенд: activate {attempt}, поставлена {staged}"));
    }
    if !launch::wait_exit(&[owner_n.key.pid], update_flow::STEP).is_empty() {
        return Err("стенд: GUI N не вышел после activate".to_owned());
    }
    let n_reports = reports(out, "owner-n")?;

    // Вывод без клиента: N+1 стоит перед публикацией сокета.
    let reached = barrier::wait_reached(state, HOLD, Instant::now() + update_flow::STEP)?;
    let offline = emit_offline(
        &record,
        2,
        &format!("emit {OFFLINE_LINES}"),
        OFFLINE_LINES + 1,
    );
    barrier::release(state, HOLD)?;
    let (offline_before, offline_after) = offline?;
    let next = owner::wait_for_owner(state, Instant::now() + update_flow::STEP)?;
    let next_process = observer::inspect(next.key.pid)?;
    update_flow::keep_helper_files(state, &attempt, out)?;
    steps.push(json!({
        "name": "update",
        "attempt": attempt,
        "helper": helper,
        "n_reports": n_reports,
        "reached": reached,
        "offline_before": offline_before,
        "offline_after": offline_after,
        "next": next,
        "next_process": next_process,
    }));
    let offline_ack = fixture::receipt(2, &format!("emit {OFFLINE_LINES}"));
    let receipt_offline = window_receipt(
        state,
        Wait::Line {
            text: offline_ack.clone(),
        },
    )?;
    receipts.push(Named {
        name: "offline",
        receipt: receipt_offline.clone(),
    });

    // Живой вывод при открытом окне N+1.
    let live_command = format!("emit {LIVE_LINES}");
    control(&record)?.input(3, &live_command)?;
    let live_ack = fixture::receipt(3, &live_command);
    let receipt_live = window_receipt(
        state,
        Wait::Line {
            text: live_ack.clone(),
        },
    )?;
    receipts.push(Named {
        name: "live",
        receipt: receipt_live.clone(),
    });

    // Доступность между двумя квитанциями.
    let ax = window::ax_text(next.key.pid);
    let receipt_after_ax = window_receipt(state, Wait::Now)?;
    receipts.push(Named {
        name: "after-ax",
        receipt: receipt_after_ax.clone(),
    });
    steps.push(json!({ "name": "ax", "read": ax }));
    if !ax.trusted || ax.windows_error.is_some() || ax.text_areas.is_empty() {
        return Err(format!(
            "стенд: доступность окна N+1 не прочитана: trusted {}, AXWindows {:?}, AXTextArea {}",
            ax.trusted,
            ax.windows_error,
            ax.text_areas.len()
        ));
    }

    // Синтетический ввод.
    let before_input = window::read_text(&receipt_after_ax.text).last_number();
    window::type_text(next.key.pid, TYPED)?;
    let receipt_input = window_receipt(state, Wait::SentAcked { count: 1 })?;
    receipts.push(Named {
        name: "input",
        receipt: receipt_input.clone(),
    });
    let independent = receipt_input.sent.first().map(|sent| {
        let hello = attach::hello(Purpose::Attach, ATTACH_VERSIONS, 0, Winsize::cells(0, 0));
        match attach::connect(Path::new(&record.socket), &hello, update_flow::STEP) {
            Ok(Connected::Welcome(client)) => {
                let mut tail = Attached::new(client, Some(1));
                let found = tail
                    .pump_until(Instant::now() + update_flow::STEP, |transcript| {
                        transcript.receipt(sent.seq, &sent.command).is_some()
                    })
                    .is_ok();
                let _ = tail.detach(Instant::now() + update_flow::STEP);
                (found, tail.transcript.problems.clone())
            }
            Ok(Connected::Refused(reason)) => (false, vec![reason]),
            Err(reason) => (false, vec![reason]),
        }
    });
    let journal = stand.store.read().map_err(|err| err.to_string())?;
    let same = update_flow::same_processes(&record)?;
    steps.push(json!({
        "name": "input",
        "before_input": before_input,
        "independent": independent,
        "journal": journal,
        "h1_same_processes": same,
    }));

    let gen1_runtime = crate::runtime::ready_dir(state, 1);
    let health_ack = next
        .update
        .as_ref()
        .map(|confirmed| fixture::receipt(confirmed.health.input_seq, crate::health::COMMAND));
    let cells = vec![
        part(
            &LAUNCH,
            launch_verdict(&LaunchFacts {
                stand: &stand,
                owner_n: &owner_n,
                n_ppid: n_process.as_ref().map(|info| info.ppid),
                n_reports: &n_reports,
                next: &next,
                next_ppid: next_process.as_ref().map(|info| info.ppid),
                attempt: &attempt,
                journal: journal.as_ref(),
                host_path: &host_path,
                runtime: &gen1_runtime,
                same,
            }),
        ),
        part(
            &TAIL,
            tail_verdict(
                &receipt_offline,
                &receipt_live,
                (&offline_ack, &live_ack, health_ack.as_deref()),
                &record,
            ),
        ),
        part(&AX, ax_verdict(&receipt_live, &receipt_after_ax, &ax)),
        part(
            &INPUT,
            input_verdict(&receipt_input, before_input, independent.as_ref()),
        ),
    ];

    let finished = update_flow::finish(&stand, guard, next.key)?;
    steps.push(json!({ "name": "finish", "facts": finished }));
    Ok(cells)
}

/// Факты части `launch-services`.
pub struct LaunchFacts<'a> {
    pub stand: &'a Stand,
    pub owner_n: &'a OwnerInfo,
    pub n_ppid: Option<i32>,
    pub n_reports: &'a [AppReport],
    pub next: &'a OwnerInfo,
    pub next_ppid: Option<i32>,
    pub attempt: &'a str,
    pub journal: Option<&'a crate::journal::Journal>,
    pub host_path: &'a str,
    pub runtime: &'a Path,
    pub same: bool,
}

pub fn launch_verdict(facts: &LaunchFacts) -> Verdict {
    let mut violations = Vec::new();
    let installed = update_flow::canonical(&facts.stand.app_bin);
    for (who, info, ppid, generation) in [
        ("GUI N", facts.owner_n, facts.n_ppid, 1),
        ("GUI N+1", facts.next, facts.next_ppid, 2),
    ] {
        violation(&mut violations, ppid == Some(1), || {
            format!("{who}: ppid {ppid:?}, а не launchd")
        });
        violation(&mut violations, info.window, || format!("{who} без окна"));
        violation(&mut violations, info.generation == generation, || {
            format!("{who}: поколение {}", info.generation)
        });
        violation(
            &mut violations,
            update_flow::canonical(Path::new(&info.executable)) == installed,
            || format!("{who}: исполняемый файл {}", info.executable),
        );
    }
    violation(
        &mut violations,
        facts
            .n_reports
            .iter()
            .any(|report| matches!(report, AppReport::Updated { attempt, .. } if attempt == facts.attempt)),
        || "GUI N не сообщил updated этой попытки".to_owned(),
    );
    violation(
        &mut violations,
        facts.journal.is_some_and(|journal| {
            journal.attempt == facts.attempt
                && journal.state == State::Idle
                && journal.outcome == Some(Outcome::Committed)
        }),
        || format!("журнал не idle «подтверждена»: {:?}", facts.journal),
    );
    violation(
        &mut violations,
        update_flow::canonical(Path::new(facts.host_path))
            .starts_with(update_flow::canonical(facts.runtime)),
        || format!("хост h1 не из runtime/1/: {}", facts.host_path),
    );
    violation(&mut violations, facts.same, || {
        "процессы h1 сменились".to_owned()
    });
    Verdict::of(
        violations,
        format!(
            "GUI N {} и N+1 {} запущены LaunchServices с окном, попытка {} подтверждена, h1 из runtime/1/",
            facts.owner_n.key.pid, facts.next.key.pid, facts.attempt
        ),
    )
}

/// Номер строки с текстом `text` в прочитанном тексте окна.
fn line_of(text: &str, wanted: &str) -> Option<u64> {
    window::read_text(text)
        .lines
        .iter()
        .find(|line| line.text == wanted)
        .map(|line| line.number)
}

pub fn tail_verdict(
    offline: &Receipt,
    live: &Receipt,
    (offline_ack, live_ack, health_ack): (&str, &str, Option<&str>),
    record: &SessionRecord,
) -> Verdict {
    let mut violations = Vec::new();
    for (name, receipt) in [
        ("после вывода без клиента", offline),
        ("после живого", live),
    ] {
        let read = window::read_text(&receipt.text);
        violation(&mut violations, receipt.satisfied, || {
            format!("{name}: ожидаемой строки нет к сроку")
        });
        violation(&mut violations, read.problems.is_empty(), || {
            format!("{name}: {:?}", read.problems)
        });
        violation(
            &mut violations,
            receipt.attachment.as_ref().is_some_and(|attachment| {
                attachment.session == record.session
                    && attachment.version == 1
                    && attachment.start == 0
            }),
            || format!("{name}: окно подключено как {:?}", receipt.attachment),
        );
    }
    let read = window::read_text(&live.text);
    let emitted = |seq: u64, count: u64| {
        (1..=count).all(|index| read.find(&format!("emit {seq} {index}/{count}")).is_some())
    };
    violation(&mut violations, emitted(2, OFFLINE_LINES), || {
        "не все строки вывода без клиента в окне".to_owned()
    });
    violation(&mut violations, emitted(3, LIVE_LINES), || {
        "не все строки живого вывода в окне".to_owned()
    });
    violation(
        &mut violations,
        health_ack.is_some_and(|ack| read.find(ack).is_some()),
        || format!("квитанции health {health_ack:?} в окне нет"),
    );
    let (offline_at, live_at) = (
        line_of(&live.text, offline_ack),
        line_of(&live.text, live_ack),
    );
    violation(
        &mut violations,
        matches!((offline_at, live_at), (Some(a), Some(b)) if a < b),
        || format!("порядок квитанций: без клиента {offline_at:?}, живой {live_at:?}"),
    );
    Verdict::of(
        violations,
        format!(
            "окно N+1: строки 1..{} подряд, вывод без клиента до строки {offline_at:?}, живой до {live_at:?}",
            read.last_number().unwrap_or(0)
        ),
    )
}

pub fn ax_verdict(before: &Receipt, after: &Receipt, ax: &AxRead) -> Verdict {
    let mut violations = Vec::new();
    violation(&mut violations, before.text == after.text, || {
        "текст окна менялся во время чтения доступности".to_owned()
    });
    violation(&mut violations, ax.text_areas.len() == 1, || {
        format!("текстовых областей {}", ax.text_areas.len())
    });
    if let Some(text) = ax.text_areas.first()
        && let Err(reason) =
            window::agree(&window::read_text(&after.text), &window::read_text(text))
    {
        violations.push(format!("доступность и квитанция: {reason}"));
    }
    Verdict::of(
        violations,
        format!(
            "AXValue совпал с квитанцией окна: {} строк",
            window::read_text(&after.text).lines.len()
        ),
    )
}

pub fn input_verdict(
    receipt: &Receipt,
    before: Option<u64>,
    independent: Option<&(bool, Vec<String>)>,
) -> Verdict {
    let mut violations = Vec::new();
    let read = window::read_text(&receipt.text);
    violation(&mut violations, receipt.satisfied, || {
        format!(
            "квитанции ввода окна нет к сроку; поле «{}», чужих нажатий {}",
            receipt.field, receipt.foreign_keys
        )
    });
    violation(
        &mut violations,
        receipt.sent.len() == 1 && receipt.sent[0].command == TYPED,
        || format!("окно отправило {:?}", receipt.sent),
    );
    violation(&mut violations, read.problems.is_empty(), || {
        format!("{:?}", read.problems)
    });
    if let Some(sent) = receipt.sent.first() {
        let ack = read
            .receipt(sent.seq, &sent.command)
            .map(|line| line.number);
        violation(
            &mut violations,
            matches!((ack, before), (Some(ack), Some(before)) if ack > before),
            || format!("квитанция ввода в строке {ack:?}, до ввода последняя {before:?}"),
        );
        violation(
            &mut violations,
            independent.is_some_and(|(found, problems)| *found && problems.is_empty()),
            || format!("независимый attach: {independent:?}"),
        );
    }
    Verdict::of(
        violations,
        format!(
            "ввод {:?} дошёл до нагрузки, квитанция в окне и в хвосте attach",
            receipt.sent.first()
        ),
    )
}
