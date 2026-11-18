//! Сценарий стенда `host` (задача 3): минимальный хост, attach-клиент и нагрузка на настоящих
//! процессах и PTY. Части по порядку:
//!
//! 1. `create` — хост вне дерева запустившего, нагрузка — лидер своей сессии с терминалом, запись
//!    сессии и сокет на месте;
//! 2. `attach-input` — `attach` получает хвост с начала, становится лидером и задаёт размер, ввод
//!    получает ответ хоста и квитанцию нагрузки в нумерованном выводе;
//! 3. `refused` — `probe` и `attach` с несовместимым диапазоном получают `Refused`, лидерство,
//!    клиенты и размер не меняются;
//! 4. `probe` — совместимый `probe` получает общую наибольшую версию без лидерства и ресайза;
//! 5. `detach` — после `Detach` клиентов нет, лидера нет;
//! 6. `output-without-client` — ввод через управляющее соединение и вывод без клиента: хост
//!    читает PTY, барьер — `AwaitLines` на хосте, а не `sleep`;
//! 7. `reattach` — отдельный процесс `probe-update-attach` продолжает с номера байта: хвост без
//!    пропуска и повтора по номерам, затем живой вывод и квитанция ввода;
//! 8. `processes` — PID и время старта хоста и нагрузки прежние;
//! 9. `stop` — `Stop`, выход хоста и нагрузки по ключам процессов через `kqueue`, сокет и запись
//!    убраны.
//!
//! Это клетка стенда: невыполненный оракул — сломанный стенд, то есть ошибка исполнения, а не
//! `fail`. Шаги и факты пишутся в `host/steps.json` и при ошибке. Оставшиеся процессы и файлы
//! сессии убирает [`Guard`], иначе запуск отметил бы их отдельной ошибкой.

use std::fs::{self, File};
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::json;

use crate::attach::{self, Attached, CliReport, Connected};
use crate::fixture;
use crate::host::{self, SessionRecord};
use crate::launch;
use crate::observer::{self, Identity};
use crate::proto::{self, ATTACH_VERSIONS, Ack, HOST_VERSIONS, Purpose, Status, Versions};
use crate::pty::Winsize;
use crate::results::{self, Cell, Diagnostics, Environment, HOST_CELL, Report, Requirement};

use super::sibling_binary;

/// Срок процесса сценария.
pub const DEADLINE: Duration = Duration::from_secs(60);
pub const DIR: &str = "host";
pub const STEPS_ARTIFACT: &str = "host/steps.json";
pub const HOST_STDERR: &str = "host/host.stderr";
const SESSION: &str = "h1";
/// Срок одного шага.
const STEP: Duration = Duration::from_secs(10);
const HOST_SIZE: Winsize = Winsize {
    rows: 24,
    cols: 80,
    xpixel: 0,
    ypixel: 0,
};
const ATTACH_SIZE: Winsize = Winsize {
    rows: 30,
    cols: 100,
    xpixel: 0,
    ypixel: 0,
};
/// Строк `emit` при подключённом клиенте и без клиента.
const EMIT_ATTACHED: u64 = 3;
const EMIT_DETACHED: u64 = 50;

/// Бинари пробы рядом с раннером.
struct Bins {
    host: PathBuf,
    attach: PathBuf,
    fixture: PathBuf,
}

#[derive(Debug, Default, Serialize)]
struct Evidence {
    steps: Vec<Step>,
}

#[derive(Debug, Serialize)]
struct Step {
    name: &'static str,
    facts: serde_json::Value,
}

impl Evidence {
    fn step(&mut self, name: &'static str, facts: serde_json::Value) {
        self.steps.push(Step { name, facts });
    }
}

/// Пройденная часть клетки стенда.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Part {
    pub name: &'static str,
    pub stimulus: &'static str,
    pub oracle: &'static str,
    pub reason: String,
}

/// Убирает процессы и файлы сессии, если сценарий не дошёл до проверенной остановки.
struct Guard {
    sessions: PathBuf,
    record: Option<SessionRecord>,
    armed: bool,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(record) = &self.record {
            let mut pids = Vec::new();
            for key in [record.host, record.workload] {
                if observer::identify(key).is_ok_and(|identity| identity.is_alive()) {
                    // SAFETY: сигнал процессу, узнанному по PID и времени старта.
                    unsafe { libc::kill(key.pid, libc::SIGKILL) };
                    pids.push(key.pid);
                }
            }
            launch::wait_exit(&pids, STEP);
        }
        let _ = fs::remove_dir_all(&self.sessions);
    }
}

fn check(ok: bool, what: impl FnOnce() -> String) -> Result<(), String> {
    if ok { Ok(()) } else { Err(what()) }
}

/// Лидерство, клиенты и размер — то, что не меняют отказ и `probe`.
fn leadership(status: &Status) -> (u32, u32, bool, Winsize) {
    (status.clients, status.lead, status.leader, status.size)
}

/// Лидерство для причин отчёта.
fn describe_lead(status: &Status) -> String {
    format!(
        "клиентов {}, номер лидера {}{}, размер {}x{}",
        status.clients,
        status.lead,
        if status.leader {
            " (подключён)"
        } else {
            ""
        },
        status.size.rows,
        status.size.cols
    )
}

fn welcome_of(connected: Connected, what: &str) -> Result<attach::Client, String> {
    match connected {
        Connected::Welcome(client) => Ok(client),
        Connected::Refused(reason) => Err(format!("{what}: хост отказал: {reason}")),
    }
}

fn run_cli(binary: &Path, args: &[String]) -> Result<CliReport, String> {
    let output = Command::new(binary)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|err| format!("запуск {}: {err}", binary.display()))?;
    if !output.status.success() {
        return Err(format!(
            "{} {}: {}; stderr: {}",
            binary.display(),
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    serde_json::from_slice(&output.stdout).map_err(|err| {
        format!(
            "итог {}: {err}; stdout: {}",
            binary.display(),
            String::from_utf8_lossy(&output.stdout).trim()
        )
    })
}

fn texts(lines: &[attach::Line]) -> Vec<&str> {
    lines.iter().map(|line| line.text.as_str()).collect()
}

pub fn run(out: &Path, environment: &Environment) -> Result<Report, String> {
    let nonce = environment
        .process
        .nonce
        .as_deref()
        .ok_or("предусловие: нет nonce")?;
    let state_dir = environment
        .process
        .state_dir
        .as_deref()
        .map(PathBuf::from)
        .ok_or_else(|| format!("предусловие: нет {}", launch::STATE_DIR))?;
    let bins = Bins {
        host: sibling_binary(host::HOST_NAME)?,
        attach: sibling_binary(attach::ATTACH_NAME)?,
        fixture: sibling_binary(fixture::FIXTURE_NAME)?,
    };
    let dir = out.join(DIR);
    fs::create_dir(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    let stderr = File::create(out.join(HOST_STDERR))
        .map_err(|err| format!("{}: {err}", out.join(HOST_STDERR).display()))?;
    let sessions = state_dir.join(host::SESSIONS_DIR);
    fs::create_dir(&sessions).map_err(|err| format!("{}: {err}", sessions.display()))?;
    let mut guard = Guard {
        sessions: sessions.clone(),
        record: None,
        armed: true,
    };
    let mut evidence = Evidence::default();
    let walked = walk(&bins, &sessions, nonce, stderr, &mut evidence, &mut guard);
    drop(guard);
    if let Err(reason) = &walked {
        evidence.step("error", json!({ "reason": reason }));
    }
    let written = results::write_json(&dir, "steps.json", &evidence);
    let parts = walked?;
    written?;
    Ok(report(&parts))
}

/// Части клетки стенда `host` с общими артефактами.
pub fn report(parts: &[Part]) -> Report {
    let cells = parts
        .iter()
        .map(|part| Cell {
            id: HOST_CELL.to_owned(),
            part: Some(part.name.to_owned()),
            requirement: Requirement::Required,
            status: results::Status::Pass,
            stimulus: part.stimulus.to_owned(),
            oracle: part.oracle.to_owned(),
            completion: "ответ хоста или выход процесса в срок шага; иначе ошибка исполнения"
                .to_owned(),
            reason: part.reason.clone(),
            artifacts: vec![STEPS_ARTIFACT.to_owned(), HOST_STDERR.to_owned()],
        })
        .collect();
    Report {
        cells,
        diagnostics: Diagnostics {
            notes: vec![format!(
                "протокол хоста {HOST_VERSIONS}, attach {ATTACH_VERSIONS}, поколение {}",
                crate::GENERATION
            )],
        },
    }
}

fn walk(
    bins: &Bins,
    sessions: &Path,
    nonce: &str,
    stderr: File,
    evidence: &mut Evidence,
    guard: &mut Guard,
) -> Result<Vec<Part>, String> {
    let mut parts = Vec::new();
    let socket = host::socket_path(sessions, SESSION);
    host::check_socket_path(&socket).map_err(|err| format!("предусловие: {err}"))?;
    let socket_arg = socket.display().to_string();

    // 1. Запуск.
    let options = host::Options {
        dir: sessions.to_path_buf(),
        session: SESSION.to_owned(),
        size: HOST_SIZE,
        tag: nonce.to_owned(),
        program: bins.fixture.clone(),
        args: vec!["--tag".to_owned(), nonce.to_owned()],
    };
    let record = host::launch(&bins.host, &options, stderr, false)?;
    guard.record = Some(record.clone());
    let host_info = observer::inspect(record.host.pid)?;
    let workload_info = observer::inspect(record.workload.pid)?;
    let on_disk =
        host::read_record(sessions, SESSION).map_err(|err| format!("запись сессии: {err}"))?;
    let socket_kind = fs::symlink_metadata(&socket).map(|meta| meta.file_type().is_socket());
    evidence.step(
        "create",
        json!({
            "record": record,
            "host": host_info,
            "workload": workload_info,
            "record_on_disk": on_disk,
            "socket": socket_arg,
            "socket_is_socket": socket_kind.as_ref().ok(),
        }),
    );
    let host_info = host_info.ok_or("хост вышел сразу после запуска")?;
    let workload_info = workload_info.ok_or("нагрузка вышла сразу после запуска")?;
    let (host_pid, workload_pid) = (record.host.pid, record.workload.pid);
    check(host_info.key == record.host, || {
        format!("ключ хоста {:?}, в записи {:?}", host_info.key, record.host)
    })?;
    check(host_info.ppid == 1, || {
        format!("хост в дереве запустившего: PPID {}", host_info.ppid)
    })?;
    check(
        host_info.sid != Some(host_pid) && host_info.tty.is_none(),
        || {
            format!(
                "хост — лидер сессии или с терминалом: сессия {:?}, терминал {:?}",
                host_info.sid, host_info.tty
            )
        },
    )?;
    check(
        workload_info.key == record.workload
            && workload_info.ppid == host_pid
            && workload_info.sid == Some(workload_pid)
            && workload_info.pgid == workload_pid
            && workload_info.tpgid == Some(workload_pid)
            && workload_info.tty.is_some(),
        || format!("нагрузка не лидер своей сессии с терминалом: {workload_info:?}"),
    )?;
    check(on_disk.as_ref() == Some(&record), || {
        format!("запись сессии на диске {on_disk:?} не совпадает со строкой запуска")
    })?;
    check(matches!(socket_kind, Ok(true)), || {
        format!("{socket_arg}: не сокет ({socket_kind:?})")
    })?;
    check(
        record.versions == HOST_VERSIONS && record.generation == crate::GENERATION,
        || {
            format!(
                "запись: протокол {}, поколение {}; сборка {HOST_VERSIONS}, {}",
                record.versions,
                record.generation,
                crate::GENERATION
            )
        },
    )?;
    parts.push(Part {
        name: "create",
        stimulus: "probe-update-host serve с нагрузкой probe-update-fixture",
        oracle: "хост вне дерева (PPID 1), не лидер сессии и без терминала; нагрузка — его потомок, лидер своей сессии и группы переднего плана с PTY; запись сессии равна строке запуска; сокет на месте",
        reason: format!(
            "хост {host_pid} (PPID 1, сессия {}, без терминала), нагрузка {workload_pid} — лидер сессии на {}; протокол {}, поколение {}",
            host_info.sid.map_or_else(|| "нет".to_owned(), |sid| sid.to_string()),
            record.tty,
            record.versions,
            record.generation
        ),
    });

    // 2. Attach и ввод.
    let mut control = welcome_of(
        attach::connect(
            &socket,
            &attach::hello(Purpose::Control, ATTACH_VERSIONS, 0, Winsize::cells(0, 0)),
            STEP,
        )?,
        "control",
    )?;
    let client = welcome_of(
        attach::connect(
            &socket,
            &attach::hello(Purpose::Attach, ATTACH_VERSIONS, 0, ATTACH_SIZE),
            STEP,
        )?,
        "attach",
    )?;
    let mut first = Attached::new(client, Some(1));
    let welcome = first.welcome().clone();
    let deadline = Instant::now() + STEP;
    first.pump_until(deadline, |transcript| !transcript.lines.is_empty())?;
    let attached = control.status()?;
    let emit = format!("emit {EMIT_ATTACHED}");
    first.exchange(1, &emit, deadline)?;
    evidence.step(
        "attach-input",
        json!({
            "welcome": welcome,
            "status": attached,
            "transcript": first.transcript,
            "acks": first.acks,
        }),
    );
    let version = proto::negotiate(HOST_VERSIONS, ATTACH_VERSIONS)?;
    check(
        welcome.version == version && welcome.start == 0 && welcome.lead == 1,
        || format!("Welcome {welcome:?}: ожидались версия {version}, начало 0, лидер 1"),
    )?;
    check(leadership(&attached) == (1, 1, true, ATTACH_SIZE), || {
        format!("после attach: {:?}", leadership(&attached))
    })?;
    let mut expected = vec![fixture::ready_text(workload_pid as u32, crate::GENERATION)];
    expected.extend((1..=EMIT_ATTACHED).map(|index| format!("emit 1 {index}/{EMIT_ATTACHED}")));
    expected.push(fixture::receipt(1, &emit));
    let numbers: Vec<u64> = first
        .transcript
        .lines
        .iter()
        .map(|line| line.number)
        .collect();
    check(
        texts(&first.transcript.lines) == expected
            && numbers == (1..=expected.len() as u64).collect::<Vec<_>>()
            && first.transcript.problems.is_empty(),
        || format!("вывод при attach: {:?}", first.transcript),
    )?;
    check(first.acks == [(1, Ack::Accepted)], || {
        format!("ответы хоста на ввод: {:?}", first.acks)
    })?;
    parts.push(Part {
        name: "attach-input",
        stimulus: "attach с начала с размером 30x100, затем ввод «1 emit 3»",
        oracle: "Welcome с общей наибольшей версией, началом 0 и лидером 1; клиент один, лидер, размер 30x100; строки 1–5: ready, три emit, квитанция «ack 1 emit 3»; ответ хоста Accepted",
        reason: format!(
            "версия {}, строки {:?}, квитанция в строке {}",
            welcome.version,
            numbers,
            numbers.last().copied().unwrap_or(0)
        ),
    });

    // 3. Отказ без изменения сессии.
    let before = control.status()?;
    let beyond = HOST_VERSIONS.max + 1;
    let incompatible = Versions::new(beyond, beyond);
    let cli_refused = run_cli(
        &bins.attach,
        &[
            "probe".to_owned(),
            "--socket".to_owned(),
            socket_arg.clone(),
            "--tag".to_owned(),
            nonce.to_owned(),
            "--versions".to_owned(),
            incompatible.to_string(),
        ],
    )?;
    let refused_attach = attach::connect(
        &socket,
        &attach::hello(Purpose::Attach, incompatible, 0, Winsize::cells(50, 200)),
        STEP,
    )?;
    let refused_attach = match refused_attach {
        Connected::Refused(reason) => Some(reason),
        Connected::Welcome(_) => None,
    };
    let after_refused = control.status()?;
    evidence.step(
        "refused",
        json!({
            "before": before,
            "probe": cli_refused,
            "attach_refused": refused_attach,
            "after": after_refused,
        }),
    );
    check(
        cli_refused.outcome == "refused"
            && cli_refused
                .refused
                .as_deref()
                .is_some_and(|reason| reason.contains(&incompatible.to_string())),
        || format!("probe {incompatible}: {cli_refused:?}"),
    )?;
    let refused_reason = refused_attach.ok_or_else(|| format!("attach {incompatible} принят"))?;
    check(leadership(&after_refused) == leadership(&before), || {
        format!(
            "отказ изменил сессию: до {:?}, после {:?}",
            leadership(&before),
            leadership(&after_refused)
        )
    })?;
    parts.push(Part {
        name: "refused",
        stimulus: "probe (отдельный процесс probe-update-attach) и attach с размером 50x200 с диапазоном за пределом хоста",
        oracle: "оба получают Refused с причиной, называющей диапазон клиента; клиенты, номер лидера, лидер и размер прежние",
        reason: format!(
            "диапазон {incompatible} против {HOST_VERSIONS}: «{refused_reason}»; сессия: {}",
            describe_lead(&after_refused)
        ),
    });

    // 4. Совместимый probe.
    let wide = Versions::new(1, beyond);
    let cli_probe = run_cli(
        &bins.attach,
        &[
            "probe".to_owned(),
            "--socket".to_owned(),
            socket_arg.clone(),
            "--tag".to_owned(),
            nonce.to_owned(),
            "--versions".to_owned(),
            wide.to_string(),
        ],
    )?;
    let after_probe = control.status()?;
    evidence.step("probe", json!({ "probe": cli_probe, "after": after_probe }));
    let probe_welcome = cli_probe
        .welcome
        .as_ref()
        .ok_or_else(|| format!("probe {wide}: {cli_probe:?}"))?;
    check(
        probe_welcome.version == HOST_VERSIONS.max && probe_welcome.lead == before.lead,
        || format!("probe {wide}: {probe_welcome:?}"),
    )?;
    check(leadership(&after_probe) == leadership(&before), || {
        format!(
            "probe изменил сессию: до {:?}, после {:?}",
            leadership(&before),
            leadership(&after_probe)
        )
    })?;
    parts.push(Part {
        name: "probe",
        stimulus: "probe (отдельный процесс probe-update-attach) с диапазоном шире хоста",
        oracle: "Welcome с наибольшей версией хоста; номер лидера, клиенты и размер прежние",
        reason: format!(
            "диапазон {wide} против {HOST_VERSIONS}: версия {}, сессия: {}",
            probe_welcome.version,
            describe_lead(&after_probe)
        ),
    });

    // 5. Отрыв.
    first.detach(Instant::now() + STEP)?;
    let detached = control.status()?;
    let resume = first.transcript.resume_offset();
    let next_number = first
        .transcript
        .next_number()
        .ok_or("после attach нет ни одной строки")?;
    evidence.step(
        "detach",
        json!({ "status": detached, "resume": resume, "next_number": next_number }),
    );
    check(
        (detached.clients, detached.leader, detached.lead) == (0, false, 1)
            && first.transcript.problems.is_empty(),
        || {
            format!(
                "после Detach: {:?}, проблемы {:?}",
                leadership(&detached),
                first.transcript.problems
            )
        },
    )?;
    parts.push(Part {
        name: "detach",
        stimulus: "Detach первого attach",
        oracle: "Done от хоста; клиентов 0, лидера нет, номер лидера прежний",
        reason: format!(
            "клиентов {}, продолжение с байта {resume} и строки {next_number}",
            detached.clients
        ),
    });

    // 6. Вывод без клиента.
    let emit_detached = format!("emit {EMIT_DETACHED}");
    let ack = control.input(2, &emit_detached)?;
    let target = detached.lines + EMIT_DETACHED + 1;
    let waited = control.await_lines(target, STEP)?;
    evidence.step(
        "output-without-client",
        json!({ "ack": ack, "target_lines": target, "status": waited }),
    );
    check(ack == Ack::Accepted, || {
        format!("ввод без клиента: {ack:?}")
    })?;
    check(
        waited.lines >= target && waited.clients == 0 && waited.end > resume,
        || format!("хост не дочитал вывод без клиента: {waited:?}, ожидалось строк {target}"),
    )?;
    parts.push(Part {
        name: "output-without-client",
        stimulus: "ввод «2 emit 50» через управляющее соединение без attach-клиентов",
        oracle: "ответ хоста Accepted; AwaitLines на хосте дожидается 51 новой строки при нуле клиентов",
        reason: format!(
            "строк {} → {}, байтов {resume} → {}, клиентов {}",
            detached.lines, waited.lines, waited.end, waited.clients
        ),
    });

    // 7. Повторный attach отдельным процессом.
    let cli_attach = run_cli(
        &bins.attach,
        &[
            "attach".to_owned(),
            "--socket".to_owned(),
            socket_arg.clone(),
            "--tag".to_owned(),
            nonce.to_owned(),
            "--from".to_owned(),
            resume.to_string(),
            "--expect".to_owned(),
            next_number.to_string(),
            "--seq".to_owned(),
            "3".to_owned(),
            "--input".to_owned(),
            "ping".to_owned(),
            "--size".to_owned(),
            format!("{}x{}", ATTACH_SIZE.rows, ATTACH_SIZE.cols),
        ],
    )?;
    let after_reattach = control.status()?;
    evidence.step(
        "reattach",
        json!({ "attach": cli_attach, "after": after_reattach }),
    );
    let rewelcome = cli_attach
        .welcome
        .as_ref()
        .ok_or_else(|| format!("повторный attach: {cli_attach:?}"))?;
    let transcript = cli_attach
        .transcript
        .as_ref()
        .ok_or("повторный attach без стенограммы")?;
    check(
        rewelcome.start == resume && rewelcome.lead == 2 && rewelcome.end == waited.end,
        || {
            format!(
                "Welcome повторного attach {rewelcome:?}: ожидались начало {resume}, лидер 2, конец {}",
                waited.end
            )
        },
    )?;
    let mut expected: Vec<String> = (1..=EMIT_DETACHED)
        .map(|index| format!("emit 2 {index}/{EMIT_DETACHED}"))
        .collect();
    expected.push(fixture::receipt(2, &emit_detached));
    expected.push(fixture::receipt(3, "ping"));
    check(
        transcript.problems.is_empty()
            && texts(&transcript.lines) == expected
            && transcript.lines.first().map(|line| line.number) == Some(next_number),
        || format!("хвост повторного attach: {transcript:?}"),
    )?;
    let tail_receipt = transcript
        .receipt(2, &emit_detached)
        .ok_or("нет квитанции 2")?;
    let live_receipt = transcript.receipt(3, "ping").ok_or("нет квитанции 3")?;
    check(
        tail_receipt.offset < rewelcome.end && live_receipt.offset >= rewelcome.end,
        || {
            format!(
                "квитанция 2 в байте {} должна быть в хвосте, квитанция 3 в байте {} — в живом выводе (конец хвоста {})",
                tail_receipt.offset, live_receipt.offset, rewelcome.end
            )
        },
    )?;
    check(cli_attach.acks == [(3, Ack::Accepted)], || {
        format!("ответы хоста повторному attach: {:?}", cli_attach.acks)
    })?;
    check(
        (after_reattach.clients, after_reattach.lead) == (0, 2),
        || format!("после повторного attach: {:?}", leadership(&after_reattach)),
    )?;
    parts.push(Part {
        name: "reattach",
        stimulus: "отдельный процесс probe-update-attach attach --from <байт> --expect <строка> --input ping",
        oracle: "хвост с запрошенного байта: строки с ожидаемого номера без пропуска и повтора, 50 emit и квитанция 2 в хвосте; квитанция «ack 3 ping» в живом выводе; ответ хоста Accepted",
        reason: format!(
            "строки {}–{}, квитанция 2 в байте {} < {} ≤ квитанция 3 в байте {}",
            next_number,
            transcript.last_number().unwrap_or(0),
            tail_receipt.offset,
            rewelcome.end,
            live_receipt.offset
        ),
    });

    // 8. Процессы прежние.
    let host_now = observer::identify(record.host)?;
    let workload_now = observer::identify(record.workload)?;
    evidence.step(
        "processes",
        json!({ "host": host_now, "workload": workload_now }),
    );
    let same_workload = matches!(
        &workload_now,
        Identity::Same(info) if info.is_alive()
            && info.pgid == record.workload_pgid
            && info.sid == record.workload_sid
    );
    check(host_now.is_alive() && same_workload, || {
        format!("процессы сменились: хост {host_now:?}, нагрузка {workload_now:?}")
    })?;
    parts.push(Part {
        name: "processes",
        stimulus: "сверка ключей процессов после отрыва, вывода без клиента и повторного attach",
        oracle: "хост и нагрузка — те же процессы по PID и времени старта, группа и сессия нагрузки прежние",
        reason: format!(
            "хост {} (старт {} мкс), нагрузка {} (старт {} мкс) — те же",
            record.host.pid, record.host.start_us, record.workload.pid, record.workload.start_us
        ),
    });

    // 9. Остановка.
    control.stop()?;
    let alive = launch::wait_exit(&[host_pid, workload_pid], STEP);
    let host_after = observer::identify(record.host)?;
    let workload_after = observer::identify(record.workload)?;
    let leftovers: Vec<String> = fs::read_dir(sessions)
        .map_err(|err| format!("{}: {err}", sessions.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    evidence.step(
        "stop",
        json!({
            "alive": alive,
            "host": host_after,
            "workload": workload_after,
            "leftovers": leftovers,
        }),
    );
    check(
        alive.is_empty() && !host_after.is_alive() && !workload_after.is_alive(),
        || format!("после Stop живы: {alive:?}; хост {host_after:?}, нагрузка {workload_after:?}"),
    )?;
    check(leftovers.is_empty(), || {
        format!("хост не убрал за собой: {leftovers:?}")
    })?;
    fs::remove_dir(sessions).map_err(|err| format!("{}: {err}", sessions.display()))?;
    guard.armed = false;
    parts.push(Part {
        name: "stop",
        stimulus: "Stop через управляющее соединение",
        oracle: "хост и нагрузка вышли по ключам процессов (kqueue NOTE_EXIT), сокет и запись сессии убраны хостом",
        reason: format!(
            "хост: {}, нагрузка: {}; каталог сессий пуст",
            identity_name(&host_after),
            identity_name(&workload_after)
        ),
    });
    Ok(parts)
}

fn identity_name(identity: &Identity) -> &'static str {
    match identity {
        Identity::Same(_) => "зомби до прибора",
        Identity::Reused { .. } => "PID занят другим процессом",
        Identity::Gone => "процесса нет",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parts_become_valid_stand_cells() {
        let parts = vec![
            Part {
                name: "create",
                stimulus: "s",
                oracle: "o",
                reason: "r".to_owned(),
            },
            Part {
                name: "stop",
                stimulus: "s",
                oracle: "o",
                reason: "r".to_owned(),
            },
        ];
        let report = report(&parts);
        assert_eq!(report.cells.len(), 2);
        for cell in &report.cells {
            assert_eq!(cell.id, HOST_CELL);
            assert!(cell.problems().is_empty(), "{:?}", cell.problems());
        }
        assert_eq!(report.cells[1].label(), "host/stop");
    }

    #[test]
    fn leadership_ignores_output_counters() {
        let status = |lines: u64, size: Winsize| Status {
            session: SESSION.to_owned(),
            host: observer::ProcessKey {
                pid: 1,
                start_us: 1,
            },
            workload: observer::ProcessKey {
                pid: 2,
                start_us: 2,
            },
            workload_pgid: 2,
            workload_sid: Some(2),
            versions: HOST_VERSIONS,
            generation: 1,
            clients: 1,
            lead: 1,
            leader: true,
            size,
            start: 0,
            end: lines * 10,
            lines,
            exited: false,
        };
        assert_eq!(
            leadership(&status(5, HOST_SIZE)),
            leadership(&status(9, HOST_SIZE))
        );
        assert_ne!(
            leadership(&status(5, HOST_SIZE)),
            leadership(&status(5, ATTACH_SIZE))
        );
    }

    #[test]
    fn socket_path_fits_under_a_launch_state_dir() {
        // Канонический $TMPDIR macOS — около 57 байт; запас под иную длину — ещё 16.
        let state = PathBuf::from(format!(
            "/private/var/folders/{}/T/{}",
            "x".repeat(48),
            launch::state_dir_name("0123456789abcdef")
        ));
        let socket = host::socket_path(&state.join(host::SESSIONS_DIR), SESSION);
        assert!(
            host::check_socket_path(&socket).is_ok(),
            "{}",
            socket.display()
        );
    }
}
