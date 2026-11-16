//! Рубеж A — жизненный цикл сессии (план P0.4, «Рубежи и клетки»), клетки A1–A7. Задача 7 гонит
//! их на zmx текущей комбинации.
//!
//! Стенд играет роль GUI: attach-клиент кандидата сидит в PTY стенда, вывод клиента идёт в
//! приёмник, ответы приёмника — обратно в PTY клиента (`oracle::vt::Route`). Нагрузка — фикстура
//! в PTY кандидата, её квитанции идут по сокету стенда. Синхронизация — квитанции, сообщения о роли
//! клиента и выход процессов по `kqueue`, без `sleep`.
//!
//! Пути сокетов. Каталог процесса сценария длиннее `sun_path`, поэтому `ZMX_DIR` — короткая ссылка
//! `/tmp/pss-<nonce>/z` на `<out>/zmx`: сокеты и журналы демонов лежат в каталоге процесса, а путь
//! сокета помещается. Сокет квитанций фикстур — `/tmp/pss-<nonce>/c.sock`: в `ZMX_DIR` его принял
//! бы за сессию `zmx list`. Имена сессий и nonce фикстур несут nonce запуска, поэтому внешний
//! сторож видит любой оставшийся процесс кандидата.

use std::fs;
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;

use crate::args::Target;
use crate::backend::zmx::{self, Role, ZmxSet};
use crate::backend::{
    Backend, HostRecord, InputPath, Lead, Liveness, SessionId, Workload, signal, wait_child,
};
use crate::fixture::channel::{Command, Event, JobChange, Link, Listener, Ready, hex};
use crate::fixture::{self, Mode, flood_line};
use crate::launch;
use crate::matrix::Candidate;
use crate::matrix::evidence::{Evidence, Spec, cell};
use crate::observer::{self, Identity, ProcessKey};
use crate::oracle::model::{Outcome, State, compare};
use crate::oracle::vt::{
    Capture, Config, Receiver, ReceiverProfile, Reference, Route, ThroughCandidate,
};
use crate::pty::{self, PtyChild, Spawn, Winsize};
use crate::results::{self, Cell, DefectOrigin, Environment, Report};

const COLS: u16 = 80;
const ROWS: u16 = 24;
/// Срок одного ожидания квитанции, роли или выхода процесса.
const WAIT: Duration = Duration::from_secs(15);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const STOP_GRACE: Duration = Duration::from_secs(5);
const FLOOD_LINES: u64 = 4000;
/// Одновременных `ensure` одной сессии в A4.
const RACERS: usize = 8;
/// `Ctrl-\`: клавиша отключения клиента zmx.
const DETACH_KEY: &[u8] = b"\x1c";
/// Причины выхода фикстуры при потере PTY.
const PTY_LOSS: [&str; 2] = ["hangup", "pty-closed"];

pub fn run(target: &Target, environment: &Environment) -> Result<Report, String> {
    match target.backend {
        Some(Candidate::Zmx) => {}
        other => return Err(format!("lifecycle идёт на zmx, а не на {other:?}")),
    }
    let mut stand = Stand::new(&target.out, environment)?;
    let outcome = cells(&mut stand);
    let cleanup = stand.teardown();
    let mut report = outcome?;
    report.diagnostics.notes.extend(cleanup);
    Ok(report)
}

fn cells(stand: &mut Stand) -> Result<Report, String> {
    let mut report = Report::default();
    report.cells.extend(a1(stand)?);
    report.cells.push(a2(stand)?);
    report.cells.push(a3(stand)?);
    report.cells.extend(a4(stand)?);
    report.cells.extend(a5(stand)?);
    report.cells.extend(a6(stand)?);
    report.cells.push(a7(stand)?);
    Ok(report)
}

// --- стенд ---

struct Stand {
    nonce: String,
    out: PathBuf,
    root: PathBuf,
    listener: Option<Listener>,
    fixture: PathBuf,
    zmx: zmx::Zmx,
    config: Config,
    size: Winsize,
    sessions: Vec<SessionId>,
}

impl Stand {
    fn new(out: &Path, environment: &Environment) -> Result<Stand, String> {
        let nonce = environment
            .process
            .nonce
            .clone()
            .ok_or("предусловие: нет nonce запуска")?;
        let program = std::env::current_exe().map_err(|err| format!("путь раннера: {err}"))?;
        let fixture = program
            .parent()
            .map(|dir| dir.join(fixture::FIXTURE_NAME))
            .filter(|path| path.is_file())
            .ok_or_else(|| {
                format!(
                    "предусловие: нет {} рядом с раннером",
                    fixture::FIXTURE_NAME
                )
            })?;
        let entry = &environment.process.env;
        let sets_dir = entry
            .get(launch::SETS_DIR)
            .map(PathBuf::from)
            .ok_or_else(|| format!("предусловие: нет {}", launch::SETS_DIR))?;
        let binary = ZmxSet::Current.binary(&sets_dir);
        if !binary.is_file() {
            return Err(format!("предусловие: нет {}", binary.display()));
        }
        let real_dir = out.join("zmx");
        fs::create_dir(&real_dir).map_err(|err| format!("{}: {err}", real_dir.display()))?;
        let root = PathBuf::from(format!("/tmp/pss-{nonce}"));
        fs::create_dir(&root).map_err(|err| format!("{}: {err}", root.display()))?;
        let link = root.join("z");
        std::os::unix::fs::symlink(&real_dir, &link)
            .map_err(|err| format!("{}: {err}", link.display()))?;
        let listener = Listener::bind(&root.join("c.sock"))?;
        let env = launch::INHERITED
            .iter()
            .chain([launch::LANG].iter())
            .filter_map(|name| {
                entry
                    .get(*name)
                    .map(|value| ((*name).to_owned(), value.clone()))
            })
            .collect();
        let zmx = zmx::Zmx::new(zmx::Config {
            set: ZmxSet::Current,
            binary,
            dir: link,
            cwd: root.clone(),
            env,
            token: format!("pss-{nonce}"),
            timeout: COMMAND_TIMEOUT,
        })?;
        let config = Config::new(COLS, ROWS);
        let size = config.winsize(COLS, ROWS);
        let stand = Stand {
            nonce,
            out: out.to_path_buf(),
            root,
            listener: Some(listener),
            fixture,
            zmx,
            config,
            size,
            sessions: Vec::new(),
        };
        let version = stand
            .zmx
            .version()
            .map_err(|err| format!("zmx version: {err}"))?;
        results::write_json(&stand.out, "zmx-version.json", &version)?;
        Ok(stand)
    }

    fn listener(&self) -> Result<&Listener, String> {
        self.listener
            .as_ref()
            .ok_or_else(|| "сокет квитанций закрыт".to_owned())
    }

    fn session(&self, label: &str) -> Result<SessionId, String> {
        SessionId::new(&format!("s{}-{label}", self.nonce))
    }

    fn workload(&self, mode: Mode, fixture_nonce: &str) -> Result<Workload, String> {
        Ok(Workload {
            program: self.fixture.clone(),
            args: fixture::command_args(mode, self.listener()?.path(), fixture_nonce, None),
        })
    }

    /// Сессия с фикстурой и её готовность. Размер PTY нагрузки — размер окна стенда: иначе
    /// attach менял бы размер и эталону понадобился бы ресайз.
    fn ensure(&mut self, id: &SessionId, mode: Mode) -> Result<Session, String> {
        let fixture_nonce = format!("{id}-w");
        let workload = self.workload(mode, &fixture_nonce)?;
        self.sessions.push(id.clone());
        let ensured = self
            .zmx
            .ensure(id, &workload, self.size)
            .map_err(|err| format!("ensure {id}: {err}"))?;
        let (link, ready) = self.accept(Some(&fixture_nonce))?;
        if ready.pid != ensured.record.workload.pid {
            return Err(format!(
                "предусловие: фикстура {} готова с PID {}, а нагрузка сессии — {}",
                fixture_nonce, ready.pid, ensured.record.workload.pid
            ));
        }
        if ready.size != Some(self.size) {
            return Err(format!(
                "предусловие: размер PTY нагрузки {id} {:?}, ожидался {:?}",
                ready.size, self.size
            ));
        }
        if !ensured.created {
            return Err(format!("предусловие: ensure {id} не создал новую нагрузку"));
        }
        Ok(Session {
            id: id.clone(),
            fixture_nonce,
            record: ensured.record,
            link,
        })
    }

    fn accept(&self, nonce: Option<&str>) -> Result<(Link, Ready), String> {
        let mut link = self
            .listener()?
            .accept(WAIT)
            .map_err(|err| format!("фикстура не подключилась: {err}"))?;
        let ready = link
            .wait_for(WAIT, |receipt| match &receipt.event {
                Event::Ready(ready) => Some(ready.clone()),
                _ => None,
            })
            .map_err(|err| format!("готовность фикстуры: {err}"))?;
        if let Some(nonce) = nonce
            && ready.nonce != nonce
        {
            return Err(format!(
                "подключилась фикстура {}, ожидалась {nonce}",
                ready.nonce
            ));
        }
        Ok((link, ready))
    }

    fn client(&self, id: &SessionId, label: &str) -> Result<Client, String> {
        let command = self
            .zmx
            .attach_command(id, Lead::IfFree)
            .map_err(|err| format!("команда attach: {err}"))?;
        let mut profile = ReceiverProfile::for_client(label);
        if let Some(prefix) = self.zmx.role_prefix() {
            profile = profile.with_role_prefix(&prefix);
        }
        let receiver = Receiver::new(label, &self.config, profile)
            .map_err(|err| format!("приёмник {label}: {err}"))?;
        let PtyChild { master, child, .. } = pty::spawn(&Spawn {
            program: &command.program,
            args: &command.args,
            env: &command.env,
            size: self.size,
        })
        .map_err(|err| format!("attach-клиент {label}: {err}"))?;
        let mut client = Client {
            label: label.to_owned(),
            process: child,
            master: None,
            route: None,
        };
        let pump = master
            .try_clone()
            .map_err(|err| format!("PTY клиента {label}: {err}"))?;
        client.master = Some(master);
        client.route =
            Some(Route::start(pump, receiver).map_err(|err| format!("маршрут {label}: {err}"))?);
        Ok(client)
    }

    /// Останавливает сессию и добивает её процессы: уборка стенда, а не вывод клетки.
    fn discard(&self, id: &SessionId) -> Vec<String> {
        let mut notes = Vec::new();
        let Some(record) = self.zmx.record(id) else {
            return notes;
        };
        let processes = zmx::session_processes(&record).unwrap_or_else(|_| vec![record.host]);
        if self
            .zmx
            .liveness(id)
            .is_ok_and(|state| matches!(state, Liveness::Live { .. }))
        {
            let _ = self.zmx.stop(id, STOP_GRACE);
        }
        let survivors = zmx::alive(&processes).unwrap_or_default();
        for key in &survivors {
            let _ = signal(key.pid, libc::SIGKILL);
            notes.push(format!("уборка {id}: SIGKILL PID {}", key.pid));
        }
        let pids: Vec<i32> = survivors.iter().map(|key| key.pid).collect();
        launch::wait_exit(&pids, STOP_GRACE);
        notes
    }

    /// Уборка в конце процесса: сессии стенда, затем любые процессы с nonce запуска, затем
    /// короткий каталог. Что пришлось добить, попадает в диагностику.
    fn teardown(&mut self) -> Vec<String> {
        let mut notes = Vec::new();
        for id in self.sessions.clone() {
            notes.extend(self.discard(&id));
        }
        let leftovers = launch::find_by_token(&self.nonce);
        for process in &leftovers {
            let _ = signal(process.pid, libc::SIGKILL);
            notes.push(format!(
                "уборка: оставшийся процесс {} {}",
                process.pid, process.command
            ));
        }
        let pids: Vec<i32> = leftovers.iter().map(|process| process.pid).collect();
        launch::wait_exit(&pids, STOP_GRACE);
        self.listener = None;
        if let Err(err) = fs::remove_dir_all(&self.root) {
            notes.push(format!("уборка: {}: {err}", self.root.display()));
        }
        notes
    }

    fn artifact(&self, name: &str, evidence: &Evidence) -> Result<String, String> {
        results::write_json(&self.out, name, evidence)?;
        Ok(name.to_owned())
    }

    fn log_name(&self, id: &SessionId) -> String {
        format!("zmx/logs/{id}.log")
    }

    fn daemon_log(&self, id: &SessionId) -> String {
        fs::read_to_string(self.zmx.session_log(id)).unwrap_or_default()
    }
}

struct Session {
    id: SessionId,
    fixture_nonce: String,
    record: HostRecord,
    link: Link,
}

// --- attach-клиент ---

/// Attach-клиент кандидата в PTY стенда. Мастер держат стенд (для `tcgetpgrp`) и маршрут; гибель
/// GUI — закрытие обоих.
struct Client {
    label: String,
    process: Child,
    master: Option<OwnedFd>,
    route: Option<Route>,
}

impl Client {
    fn pid(&self) -> i32 {
        self.process.id() as i32
    }

    fn route(&self) -> Result<&Route, String> {
        self.route
            .as_ref()
            .ok_or_else(|| format!("маршрут клиента {} закрыт", self.label))
    }

    /// Ждёт сообщения «лидер» от управляемого клиента: `Init` обработан и клиент ведёт сессию.
    fn wait_leader(&self) -> Result<String, String> {
        let route = self.route()?;
        route
            .wait_until(WAIT, |rx, _| {
                rx.last_role()
                    .and_then(zmx::parse_role)
                    .is_some_and(|report| report.role == Role::Leader)
            })
            .map_err(|err| format!("клиент {} не стал лидером: {err}", self.label))?;
        Ok(route
            .with(|rx| rx.last_role().map(str::to_owned))
            .unwrap_or_default())
    }

    fn input(&self, bytes: &[u8]) -> Result<(), String> {
        self.route()?.input(bytes);
        Ok(())
    }

    fn capture(&self) -> Result<State, String> {
        self.route()?
            .with(|rx| rx.capture(Capture::default()))
            .map_err(|err| format!("состояние приёмника {}: {err}", self.label))
    }

    /// Группа переднего плана PTY клиента.
    fn foreground(&self) -> Result<i32, String> {
        let master = self.master.as_ref().ok_or("мастер PTY клиента закрыт")?;
        pty::foreground_pgid(master.as_fd()).map_err(|err| format!("tcgetpgrp: {err}"))
    }

    fn wait_exit(&mut self) -> Result<ExitStatus, String> {
        wait_child(&mut self.process, WAIT).map_err(|err| err.to_string())
    }

    /// `SIGKILL` клиенту.
    fn kill(&mut self, backend: &dyn Backend) -> Result<ExitStatus, String> {
        backend
            .kill_client(&mut self.process, WAIT)
            .map_err(|err| err.to_string())
    }

    /// Гибель GUI: закрываются все дескрипторы мастера PTY клиента, ядро шлёт ему `SIGHUP`.
    fn hangup(&mut self) -> Result<ExitStatus, String> {
        self.route = None;
        self.master = None;
        self.wait_exit()
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.route = None;
        if self.process.try_wait().ok().flatten().is_none() {
            let _ = self.process.kill();
            let _ = self.process.wait();
        }
    }
}

fn barrier(link: &mut Link, client: &Client) -> Result<(), String> {
    ThroughCandidate {
        link,
        route: client.route()?,
        timeout: WAIT,
    }
    .barrier()
}

/// Запись фикстуры с подтверждением и та же запись в эталон.
fn emit(link: &mut Link, reference: &mut Reference, bytes: &[u8]) -> Result<(), String> {
    let written = link
        .request(Command::Write { hex: hex(bytes) }, WAIT)
        .map_err(|err| format!("запись фикстуры: {err}"))?;
    if written != bytes.len() as u64 {
        return Err(format!(
            "фикстура записала {written} байт из {}",
            bytes.len()
        ));
    }
    reference.feed(bytes);
    Ok(())
}

fn identity(key: ProcessKey) -> Result<Identity, String> {
    observer::identify(key)
}

fn same_alive(evidence: &mut Evidence, name: &str, key: ProcessKey) -> Result<bool, String> {
    let found = identity(key)?;
    let ok = found.is_alive();
    evidence.note(name, &found);
    Ok(evidence.check(
        name,
        ok,
        format!("PID {} старт {}: {found:?}", key.pid, key.start_us),
    ))
}

/// Сравнение эталона с приёмником клиента; `Incomplete` — условие не доказано.
fn compare_states(
    evidence: &mut Evidence,
    name: &str,
    reference: &Reference,
    client: &Client,
) -> Result<Outcome, String> {
    let expected = reference
        .capture(Capture::default())
        .map_err(|err| format!("состояние эталона: {err}"))?;
    let actual = client.capture()?;
    let comparison = compare(&expected, &actual);
    let outcome = comparison.outcome();
    evidence.note(&format!("{name}.reference_text"), expected.text());
    evidence.note(&format!("{name}.receiver_text"), actual.text());
    evidence.note(&format!("{name}.comparison"), &comparison);
    match outcome {
        Outcome::Equal => {
            evidence.check(name, true, comparison.to_string());
        }
        Outcome::Differs => {
            evidence.check(name, false, comparison.to_string());
        }
        Outcome::Incomplete => {
            evidence.not_proven = Some(format!("{name}: сравнение неполное: {comparison}"));
        }
    }
    Ok(outcome)
}

// --- A1 ---

fn a1(stand: &mut Stand) -> Result<Vec<Cell>, String> {
    let mut cells = Vec::new();
    for (part, label, stimulus) in [
        (
            "client-sigkill",
            "a1k",
            "SIGKILL attach-клиента, который ведёт сессию",
        ),
        (
            "gui-hangup",
            "a1g",
            "гибель GUI: стенд закрывает все дескрипторы мастера PTY клиента, ядро шлёт клиенту \
             SIGHUP; настоящий SIGKILL процесса GUI — клетка C1",
        ),
    ] {
        let id = stand.session(label)?;
        let mut session = stand.ensure(&id, Mode::Emitter)?;
        let mut evidence = Evidence::default();
        evidence.note("record", &session.record);
        a1_part(stand, &mut session, part, &mut evidence)?;
        let notes = stand.discard(&id);
        evidence.note("cleanup", notes);
        let artifact = stand.artifact(&format!("a1-{part}.json"), &evidence)?;
        cells.push(cell(
            Spec {
                id: "A1",
                part: Some(part),
                stimulus: format!("{stimulus}; затем новый клиент подключается к той же сессии"),
                oracle: "наблюдатель процессов: PID и время старта нагрузки и хоста до и после; \
                         список сессий; барьер DSR через нового клиента",
                completion: "клиент вышел; нагрузка и хост — те же живые процессы; новый клиент \
                             стал лидером и передал ответ барьера",
                origin: DefectOrigin::Host,
            },
            &evidence,
            vec![artifact, stand.log_name(&id)],
        ));
    }
    Ok(cells)
}

fn a1_part(
    stand: &Stand,
    session: &mut Session,
    part: &str,
    evidence: &mut Evidence,
) -> Result<(), String> {
    let mut first = stand.client(&session.id, "c1")?;
    let Some(role) = evidence.require("c1 лидер", first.wait_leader()) else {
        return Ok(());
    };
    evidence.note("c1.role", role);
    if evidence
        .require("барьер через c1", barrier(&mut session.link, &first))
        .is_none()
    {
        return Ok(());
    }
    let exit = if part == "client-sigkill" {
        first.kill(&stand.zmx)?
    } else {
        first.hangup()?
    };
    evidence.note("c1.exit", format!("{exit:?}"));
    same_alive(evidence, "нагрузка та же и жива", session.record.workload)?;
    same_alive(evidence, "хост тот же и жив", session.record.host)?;
    let liveness = stand
        .zmx
        .liveness(&session.id)
        .map_err(|err| err.to_string())?;
    evidence.check(
        "сессия в списке",
        matches!(liveness, Liveness::Live { workload_pid, .. } if workload_pid == session.record.workload.pid),
        format!("{liveness:?}"),
    );
    let second = stand.client(&session.id, "c2")?;
    if evidence.require("c2 лидер", second.wait_leader()).is_some() {
        evidence.require("барьер через c2", barrier(&mut session.link, &second));
    }
    same_alive(
        evidence,
        "нагрузка та же после нового клиента",
        session.record.workload,
    )?;
    Ok(())
}

// --- A2 ---

fn a2(stand: &mut Stand) -> Result<Cell, String> {
    let id = stand.session("a2")?;
    let mut session = stand.ensure(&id, Mode::Flood)?;
    let mut evidence = Evidence::default();
    evidence.note("record", &session.record);
    a2_body(stand, &mut session, &mut evidence)?;
    let artifact = stand.artifact("a2.json", &evidence)?;
    stand.discard(&id);
    Ok(cell(
        Spec {
            id: "A2",
            part: None,
            stimulus: format!(
                "клиент c1 подключился и отключился клавишей Ctrl-\\, затем без клиентов фикстура \
                 пишет {FLOOD_LINES} строк по {COLS} колонок и ED 3 (очистка истории: перенос \
                 истории — клетка B1), затем подключается c2; сессия без единого клиента — клетка A7"
            ),
            oracle: "квитанции фикстуры: поток закончен, записи и задержки записи; ревизия \
                     обработки хоста до и после; после attach — эталон и приёмник c2",
            completion: "квитанция конца потока; барьер DSR через c2; сравнение эталона с \
                         приёмником",
            origin: DefectOrigin::Unknown,
        },
        &evidence,
        vec![artifact, stand.log_name(&id)],
    ))
}

fn a2_body(stand: &Stand, session: &mut Session, evidence: &mut Evidence) -> Result<(), String> {
    let mut reference = Reference::new(&stand.config).map_err(|err| format!("эталон: {err}"))?;
    let mut first = stand.client(&session.id, "c1")?;
    if evidence.require("c1 лидер", first.wait_leader()).is_none()
        || evidence
            .require("барьер через c1", barrier(&mut session.link, &first))
            .is_none()
    {
        return Ok(());
    }
    first.input(DETACH_KEY)?;
    let Some(exit) = evidence.require("c1 отключился клавишей", first.wait_exit())
    else {
        return Ok(());
    };
    evidence.note("c1.exit", format!("{exit:?}"));
    let liveness = stand
        .zmx
        .liveness(&session.id)
        .map_err(|err| err.to_string())?;
    evidence.check(
        "без клиентов",
        matches!(liveness, Liveness::Live { clients: 0, .. }),
        format!("{liveness:?}"),
    );
    let before = stand
        .zmx
        .read_boundary(&session.id)
        .map_err(|err| err.to_string())?;
    session
        .link
        .request(
            Command::Flood {
                lines: FLOOD_LINES,
                width: COLS,
                interval_us: 0,
                report_every: FLOOD_LINES / 4,
            },
            WAIT,
        )
        .map_err(|err| format!("команда потока: {err}"))?;
    let started = Instant::now();
    let done = session
        .link
        .wait_for(WAIT * 4, |receipt| match &receipt.event {
            Event::FloodDone {
                lines,
                bytes,
                cancelled,
                writes,
                ..
            } => Some((*lines, *bytes, *cancelled, *writes)),
            _ => None,
        });
    let Some((lines, bytes, cancelled, writes)) =
        evidence.require("поток закончен без клиентов", done)
    else {
        return Ok(());
    };
    evidence.note("flood.elapsed_ms", started.elapsed().as_millis() as u64);
    evidence.note("flood.writes", writes);
    evidence.check(
        "все строки записаны",
        lines == FLOOD_LINES && !cancelled,
        format!(
            "строк {lines}, байт {bytes}, отменён {cancelled}, задержек {} на {} мс",
            writes.stalls,
            writes.stalled_ns / 1_000_000
        ),
    );
    for index in 1..=FLOOD_LINES {
        reference.feed(&flood_line(&session.fixture_nonce, index, COLS));
    }
    emit(&mut session.link, &mut reference, b"\x1b[3J")?;
    let after = stand
        .zmx
        .read_boundary(&session.id)
        .map_err(|err| err.to_string())?;
    evidence.note("revision", json!({ "before": before, "after": after }));
    evidence.check(
        "хост обработал поток без клиентов",
        after > before,
        format!("ревизия {before} → {after}"),
    );
    let second = stand.client(&session.id, "c2")?;
    if evidence.require("c2 лидер", second.wait_leader()).is_none()
        || evidence
            .require("барьер через c2", barrier(&mut session.link, &second))
            .is_none()
    {
        return Ok(());
    }
    compare_states(evidence, "конец потока у c2", &reference, &second)?;
    Ok(())
}

// --- A3 ---

fn a3(stand: &mut Stand) -> Result<Cell, String> {
    let id = stand.session("a3")?;
    let mut session = stand.ensure(&id, Mode::Shell)?;
    let mut evidence = Evidence::default();
    evidence.note("record", &session.record);
    a3_body(stand, &mut session, &mut evidence)?;
    let artifact = stand.artifact("a3.json", &evidence)?;
    stand.discard(&id);
    Ok(cell(
        Spec {
            id: "A3",
            part: None,
            stimulus:
                "через attach-клиент, ведущий сессию: run /bin/sleep 600, Ctrl-Z, bg 1, fg 1, \
                       Ctrl-C"
                    .to_owned(),
            oracle: "квитанции заданий и сигналов фикстуры shell с tcgetpgrp внутреннего PTY; \
                     группа переднего плана PTY клиента (tcgetpgrp мастера)",
            completion: "квитанция каждого шага; клиент жив и остаётся группой переднего плана \
                         своего PTY",
            origin: DefectOrigin::Host,
        },
        &evidence,
        vec![artifact, stand.log_name(&id)],
    ))
}

fn job(link: &mut Link) -> Result<(i32, JobChange, Option<i32>), String> {
    link.wait_for(WAIT, |receipt| match &receipt.event {
        Event::Job {
            pid,
            change,
            foreground_pgid,
            ..
        } => Some((*pid, change.clone(), *foreground_pgid)),
        _ => None,
    })
    .map_err(|err| err.to_string())
}

fn shell_done(link: &mut Link, line: &str) -> Result<Option<i32>, String> {
    let (error, foreground) = link
        .wait_for(WAIT, |receipt| match &receipt.event {
            Event::Shell {
                line: done,
                error,
                foreground_pgid,
                ..
            } if done == line => Some((error.clone(), *foreground_pgid)),
            _ => None,
        })
        .map_err(|err| err.to_string())?;
    match error {
        None => Ok(foreground),
        Some(error) => Err(format!("{line}: {error}")),
    }
}

fn a3_body(stand: &Stand, session: &mut Session, evidence: &mut Evidence) -> Result<(), String> {
    let shell = session.record.workload.pid;
    let client = stand.client(&session.id, "c1")?;
    if evidence.require("c1 лидер", client.wait_leader()).is_none() {
        return Ok(());
    }
    let outer = client.pid();
    let outer_ok = |evidence: &mut Evidence, step: &str| -> Result<(), String> {
        let foreground = client.foreground()?;
        evidence.check(
            &format!("{step}: PTY клиента"),
            foreground == outer,
            format!("tcgetpgrp {foreground}, клиент {outer}"),
        );
        Ok(())
    };
    client.input(b"run /bin/sleep 600\n")?;
    let Some((job_pid, change, foreground)) = evidence.require("run", job(&mut session.link))
    else {
        return Ok(());
    };
    evidence.check(
        "run: задание на переднем плане",
        change == JobChange::Started { foreground: true } && foreground == Some(job_pid),
        format!("{change:?}, tcgetpgrp {foreground:?}, задание {job_pid}"),
    );
    let tpgid = observer::inspect(shell)?.and_then(|info| info.tpgid);
    evidence.check(
        "run: tpgid shell",
        tpgid == Some(job_pid),
        format!("{tpgid:?}"),
    );
    outer_ok(evidence, "run")?;

    client.input(b"\x1a")?;
    let Some((_, change, foreground)) = evidence.require("Ctrl-Z", job(&mut session.link)) else {
        return Ok(());
    };
    evidence.check(
        "Ctrl-Z: задание остановлено, терминал у shell",
        change
            == JobChange::Stopped {
                signal: "SIGTSTP".to_owned(),
            }
            && foreground == Some(shell),
        format!("{change:?}, tcgetpgrp {foreground:?}"),
    );
    outer_ok(evidence, "Ctrl-Z")?;

    client.input(b"bg 1\n")?;
    if evidence
        .require("bg 1", shell_done(&mut session.link, "bg 1"))
        .is_none()
    {
        return Ok(());
    }
    let Some((_, change, _)) = evidence.require("bg 1: квитанция", job(&mut session.link))
    else {
        return Ok(());
    };
    evidence.check(
        "bg 1: задание продолжено в фоне",
        change == JobChange::Continued { foreground: false },
        format!("{change:?}"),
    );

    client.input(b"fg 1\n")?;
    let Some(foreground) = evidence.require("fg 1", shell_done(&mut session.link, "fg 1")) else {
        return Ok(());
    };
    evidence.check(
        "fg 1: терминал у задания",
        foreground == Some(job_pid),
        format!("tcgetpgrp {foreground:?}"),
    );
    let Some((_, change, _)) = evidence.require("fg 1: квитанция", job(&mut session.link))
    else {
        return Ok(());
    };
    evidence.check(
        "fg 1: задание на переднем плане",
        change == JobChange::Continued { foreground: true },
        format!("{change:?}"),
    );

    client.input(b"\x03")?;
    let Some((_, change, foreground)) = evidence.require("Ctrl-C", job(&mut session.link)) else {
        return Ok(());
    };
    evidence.check(
        "Ctrl-C: задание прервано, терминал у shell",
        change
            == JobChange::Signaled {
                signal: "SIGINT".to_owned(),
            }
            && foreground == Some(shell),
        format!("{change:?}, tcgetpgrp {foreground:?}"),
    );
    outer_ok(evidence, "Ctrl-C")?;
    let alive = observer::inspect(outer)?.is_some_and(|info| info.is_alive());
    evidence.check(
        "клиент жив после Ctrl-Z и Ctrl-C",
        alive,
        format!("PID {outer}"),
    );
    Ok(())
}

// --- A4 ---

fn a4(stand: &mut Stand) -> Result<Vec<Cell>, String> {
    let id = stand.session("a4r")?;
    let mut session = stand.ensure(&id, Mode::Emitter)?;
    let mut evidence = Evidence::default();
    evidence.note("record", &session.record);
    a4_reattach(stand, &mut session, &mut evidence)?;
    let artifact = stand.artifact("a4-reattach.json", &evidence)?;
    stand.discard(&id);
    let reattach = cell(
        Spec {
            id: "A4",
            part: Some("reattach"),
            stimulus: "attach, отключение Ctrl-\\, повторный attach и повторный ensure той же \
                       сессии"
                .to_owned(),
            oracle: "наблюдатель процессов: ключ нагрузки; процессы с nonce нагрузки в аргументах; \
                     список сессий",
            completion: "второй клиент стал лидером; повторный ensure вернулся",
            origin: DefectOrigin::Host,
        },
        &evidence,
        vec![artifact, stand.log_name(&id)],
    );

    let id = stand.session("a4c")?;
    stand.sessions.push(id.clone());
    let mut evidence = Evidence::default();
    a4_concurrent(stand, &id, &mut evidence)?;
    let artifact = stand.artifact("a4-concurrent.json", &evidence)?;
    stand.discard(&id);
    let concurrent = cell(
        Spec {
            id: "A4",
            part: Some("concurrent-ensure"),
            stimulus: format!(
                "{RACERS} одновременных ensure одной сессии, у каждого своя нагрузка"
            ),
            oracle: "наблюдатель процессов: процессы с общим префиксом nonce нагрузок; сокеты в \
                     ZMX_DIR и каталоге подготовки; список сессий; готовность фикстуры",
            completion: "все ensure вернулись",
            origin: DefectOrigin::Host,
        },
        &evidence,
        vec![artifact, stand.log_name(&id)],
    );
    Ok(vec![reattach, concurrent])
}

/// Процессы с `token` в аргументах: ключи по PID.
fn processes_with(token: &str) -> Result<Vec<ProcessKey>, String> {
    let mut keys = Vec::new();
    for process in launch::find_by_token(token) {
        if let Some(info) = observer::inspect(process.pid)?
            && info.is_alive()
        {
            keys.push(info.key);
        }
    }
    keys.sort();
    Ok(keys)
}

fn a4_reattach(
    stand: &Stand,
    session: &mut Session,
    evidence: &mut Evidence,
) -> Result<(), String> {
    let workload = session.record.workload;
    let mut first = stand.client(&session.id, "c1")?;
    if evidence.require("c1 лидер", first.wait_leader()).is_none() {
        return Ok(());
    }
    first.input(DETACH_KEY)?;
    if evidence
        .require("c1 отключился", first.wait_exit())
        .is_none()
    {
        return Ok(());
    }
    let second = stand.client(&session.id, "c2")?;
    if evidence.require("c2 лидер", second.wait_leader()).is_none()
        || evidence
            .require("барьер через c2", barrier(&mut session.link, &second))
            .is_none()
    {
        return Ok(());
    }
    let workload_args = stand.workload(Mode::Emitter, &session.fixture_nonce)?;
    let again = stand
        .zmx
        .ensure(&session.id, &workload_args, stand.size)
        .map_err(|err| format!("повторный ensure: {err}"))?;
    evidence.note("ensure_again", &again);
    evidence.check(
        "повторный ensure не создал нагрузку",
        !again.created && again.record.workload == workload,
        format!(
            "created {}, нагрузка {:?}",
            again.created, again.record.workload
        ),
    );
    same_alive(evidence, "нагрузка та же", workload)?;
    let found = processes_with(&session.fixture_nonce)?;
    let mut expected = vec![session.record.host, workload];
    expected.sort();
    evidence.note("processes", &found);
    evidence.check(
        "одна нагрузка и один хост",
        found == expected,
        format!("{found:?}, ожидалось {expected:?}"),
    );
    let listed = stand.zmx.list().map_err(|err| err.to_string())?;
    let entries: Vec<_> = listed
        .iter()
        .filter(|entry| entry.name == session.id.as_str())
        .collect();
    evidence.check(
        "одна сессия в списке",
        entries.len() == 1 && entries[0].workload_pid == Some(workload.pid),
        format!("{entries:?}"),
    );
    let log = stand.daemon_log(&session.id);
    evidence.note("leaders", zmx::leadership_events(&log));
    Ok(())
}

fn a4_concurrent(stand: &Stand, id: &SessionId, evidence: &mut Evidence) -> Result<(), String> {
    let prefix = format!("{id}-w");
    let workloads: Vec<(String, Workload)> = (0..RACERS)
        .map(|index| {
            let nonce = format!("{prefix}{index}");
            stand
                .workload(Mode::Emitter, &nonce)
                .map(|workload| (nonce, workload))
        })
        .collect::<Result<_, _>>()?;
    let (zmx, size) = (&stand.zmx, stand.size);
    let results: Vec<_> = thread::scope(|scope| {
        let handles: Vec<_> = workloads
            .iter()
            .map(|(_, workload)| scope.spawn(move || zmx.ensure(id, workload, size)))
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().map_err(|_| "поток ensure упал".to_owned()))
            .collect()
    });
    let mut ensured = Vec::new();
    for (index, result) in results.into_iter().enumerate() {
        match result? {
            Ok(value) => ensured.push((index, value)),
            Err(err) => {
                evidence.check(&format!("ensure {index}"), false, err);
            }
        }
    }
    evidence.note(
        "ensured",
        ensured
            .iter()
            .map(|(index, value)| json!({ "index": index, "created": value.created, "record": value.record, "command": value.command }))
            .collect::<Vec<_>>(),
    );
    if ensured.len() != RACERS {
        return Ok(());
    }
    let keys: Vec<ProcessKey> = ensured
        .iter()
        .map(|(_, value)| value.record.workload)
        .collect();
    evidence.check(
        "все ensure вернули одну нагрузку",
        keys.windows(2).all(|pair| pair[0] == pair[1]),
        format!("{keys:?}"),
    );
    let winners: Vec<usize> = ensured
        .iter()
        .filter(|(_, value)| value.created)
        .map(|(index, _)| *index)
        .collect();
    evidence.check(
        "нагрузку запустил ровно один ensure",
        winners.len() == 1,
        format!("{winners:?}"),
    );
    let host = ensured[0].1.record.host;
    let found = processes_with(&prefix)?;
    let mut expected = vec![host, keys[0]];
    expected.sort();
    evidence.note("processes", &found);
    evidence.check(
        "один хост и одна нагрузка",
        found == expected,
        format!("{found:?}, ожидалось {expected:?}"),
    );
    let sockets = stand
        .zmx
        .socket_entries()
        .map_err(|err| format!("сокеты zmx: {err}"))?;
    evidence.note("sockets", &sockets);
    evidence.check(
        "один сокет и пустой каталог подготовки",
        sockets
            .sessions
            .iter()
            .filter(|name| *name == id.as_str())
            .count()
            == 1
            && sockets.staged.is_empty(),
        format!("{sockets:?}"),
    );
    let listed = stand.zmx.list().map_err(|err| err.to_string())?;
    let entries = listed
        .iter()
        .filter(|entry| entry.name == id.as_str())
        .count();
    evidence.check("одна сессия в списке", entries == 1, format!("{entries}"));
    if let [winner] = winners.as_slice() {
        let (link, ready) = stand.accept(None)?;
        evidence.note("ready", &ready);
        evidence.check(
            "готова нагрузка победителя",
            ready.nonce == workloads[*winner].0 && ready.pid == keys[0].pid,
            format!("{} PID {}", ready.nonce, ready.pid),
        );
        drop(link);
    }
    Ok(())
}

// --- A5 ---

fn a5(stand: &mut Stand) -> Result<Vec<Cell>, String> {
    let mut cells = Vec::new();
    for (part, label, stimulus, completion) in [
        (
            "workload-exit",
            "a5x",
            "нагрузка выходит сама (команда Exit фикстуре) при подключённом клиенте",
            "выход нагрузки и хоста пойман kqueue; клиент вышел",
        ),
        (
            "stop",
            "a5s",
            "stop сессии (zmx kill) при подключённом клиенте",
            "stop вернулся после ожидания гибели процессов; клиент вышел",
        ),
    ] {
        let id = stand.session(label)?;
        let mut session = stand.ensure(&id, Mode::Emitter)?;
        let mut evidence = Evidence::default();
        evidence.note("record", &session.record);
        a5_part(stand, &mut session, part, &mut evidence)?;
        let artifact = stand.artifact(&format!("a5-{part}.json"), &evidence)?;
        stand.discard(&id);
        cells.push(cell(
            Spec {
                id: "A5",
                part: Some(part),
                stimulus: stimulus.to_owned(),
                oracle: "наблюдатель процессов по PID и времени старта: хост, нагрузка и её \
                         потомки; сокет — только сопутствующее наблюдение",
                completion,
                origin: DefectOrigin::Host,
            },
            &evidence,
            vec![artifact, stand.log_name(&id)],
        ));
    }
    Ok(cells)
}

fn a5_part(
    stand: &Stand,
    session: &mut Session,
    part: &str,
    evidence: &mut Evidence,
) -> Result<(), String> {
    let mut client = stand.client(&session.id, "c1")?;
    if evidence.require("c1 лидер", client.wait_leader()).is_none() {
        return Ok(());
    }
    let record = session.record.clone();
    if part == "workload-exit" {
        session
            .link
            .request(Command::Exit { code: 0 }, WAIT)
            .map_err(|err| format!("команда выхода: {err}"))?;
        let survivors = launch::wait_exit(&[record.workload.pid, record.host.pid], WAIT);
        evidence.check(
            "нагрузка и хост вышли",
            survivors.is_empty(),
            format!("живы: {survivors:?}"),
        );
    } else {
        let outcome = stand
            .zmx
            .stop(&session.id, STOP_GRACE)
            .map_err(|err| format!("stop: {err}"))?;
        evidence.note("stop", &outcome);
        evidence.check(
            "команда остановки",
            outcome.command.code == Some(0),
            format!("{:?}", outcome.command),
        );
        evidence.check(
            "stop подтверждён гибелью процессов",
            outcome.confirmed(),
            format!(
                "процессов {}, живы после команды {}, выжили {:?}, ждали {} мс",
                outcome.processes.len(),
                outcome.alive_after_command.len(),
                outcome.survivors,
                outcome.waited_ms
            ),
        );
    }
    for (name, key) in [("нагрузка", record.workload), ("хост", record.host)] {
        let found = identity(key)?;
        evidence.check(
            &format!("{name} не исполняется"),
            !found.is_alive(),
            format!("{found:?}"),
        );
    }
    let exit = client.wait_exit();
    evidence.require("клиент вышел", exit);
    let liveness = stand
        .zmx
        .liveness(&session.id)
        .map_err(|err| err.to_string())?;
    evidence.note("liveness", &liveness);
    evidence.note(
        "socket_present",
        stand.zmx.socket_path(&session.id).exists(),
    );
    Ok(())
}

// --- A6 ---

fn a6(stand: &mut Stand) -> Result<Vec<Cell>, String> {
    let mut cells = Vec::new();
    for (part, label, mode, stimulus) in [
        (
            "shell",
            "a6s",
            Mode::Shell,
            "SIGKILL хосту сессии с фикстурой shell",
        ),
        (
            "agent",
            "a6a",
            Mode::Agent,
            "SIGKILL хосту сессии с фикстурой agent и её потомком nohup-child, игнорирующим HUP",
        ),
        (
            "background",
            "a6b",
            Mode::Shell,
            "SIGKILL хосту сессии с фикстурой shell и фоновым заданием nohup-child, \
             игнорирующим HUP (запущено через zmx send)",
        ),
    ] {
        let id = stand.session(label)?;
        let mut session = stand.ensure(&id, mode)?;
        let mut evidence = Evidence::default();
        evidence.note("record", &session.record);
        a6_part(stand, &mut session, part, &mut evidence)?;
        let artifact = stand.artifact(&format!("a6-{part}.json"), &evidence)?;
        stand.discard(&id);
        cells.push(cell(
            Spec {
                id: "A6",
                part: Some(part),
                stimulus: stimulus.to_owned(),
                oracle: "наблюдатель процессов по записи, сохранённой при ensure и перед сбоем \
                         (PID и время старта хоста, нагрузки и потомков); живые процессы с nonce \
                         нагрузки против записи; квитанции нагрузки и процесса, игнорирующего \
                         HUP; состояние сессии адаптера",
                completion: "выход хоста и нагрузки пойман kqueue; квитанция выхода нагрузки; \
                             каждый сохранённый процесс классифицирован, найденный отвечает на \
                             команду",
                origin: DefectOrigin::Host,
            },
            &evidence,
            vec![artifact, stand.log_name(&id)],
        ));
    }
    Ok(cells)
}

fn a6_part(
    stand: &Stand,
    session: &mut Session,
    part: &str,
    evidence: &mut Evidence,
) -> Result<(), String> {
    // Потомок, игнорирующий HUP: его канал и ключ.
    let mut hup_child: Option<(Link, ProcessKey)> = None;
    match part {
        "agent" => {
            let spawned = session
                .link
                .wait_for(WAIT, |receipt| match &receipt.event {
                    Event::Spawned { pid, mode } => Some((*pid, *mode)),
                    _ => None,
                })
                .map_err(|err| format!("потомок agent: {err}"))?;
            let (link, ready) = stand.accept(Some(&format!("{}-child", session.fixture_nonce)))?;
            if ready.pid != spawned.0 {
                return Err(format!("потомок agent: PID {} и {}", ready.pid, spawned.0));
            }
            let key = observer::inspect(ready.pid)?
                .ok_or("потомок agent вышел до сбоя")?
                .key;
            hup_child = Some((link, key));
        }
        "background" => {
            let nonce = format!("{}-bg", session.fixture_nonce);
            let args =
                fixture::command_args(Mode::NohupChild, stand.listener()?.path(), &nonce, None);
            let line = format!("run {} {} &\n", stand.fixture.display(), args.join(" "));
            stand
                .zmx
                .input(&session.id, line.as_bytes(), InputPath::Raw)
                .map_err(|err| format!("zmx send: {err}"))?;
            let (_, change, _) = job(&mut session.link)?;
            if change != (JobChange::Started { foreground: false }) {
                return Err(format!("фоновое задание: {change:?}"));
            }
            let (link, ready) = stand.accept(Some(&nonce))?;
            let key = observer::inspect(ready.pid)?
                .ok_or("фоновое задание вышло до сбоя")?
                .key;
            hup_child = Some((link, key));
        }
        _ => {}
    }
    let saved = zmx::session_processes(&session.record).map_err(|err| err.to_string())?;
    evidence.note("saved", &saved);
    let host = stand
        .zmx
        .kill_host(&session.id, WAIT)
        .map_err(|err| format!("kill_host: {err}"))?;
    evidence.note("killed_host", host);
    // Выход нагрузки доказывают её квитанция и kqueue; наблюдатель после kqueue видит процесс в
    // `exit()`, поэтому смена родителя потомков — наблюдение, а не проверка.
    let exit = session.link.wait_for(WAIT, |receipt| match &receipt.event {
        Event::Exit { reason, .. } => Some(reason.clone()),
        _ => None,
    });
    let left = launch::wait_exit(&[session.record.workload.pid], WAIT);
    let exit = exit.map_err(|err| err.to_string());
    evidence.note("workload_exit", &exit);
    evidence.check(
        "нагрузка вышла по потере PTY",
        left.is_empty()
            && exit
                .as_deref()
                .is_ok_and(|reason| PTY_LOSS.contains(&reason)),
        format!(
            "квитанция {exit:?}, жива после ожидания: {}",
            !left.is_empty()
        ),
    );
    let sockets = stand
        .zmx
        .socket_entries()
        .map_err(|err| format!("сокеты zmx: {err}"))?;
    evidence.note("sockets_before_liveness", &sockets);
    let liveness = stand
        .zmx
        .liveness(&session.id)
        .map_err(|err| err.to_string())?;
    evidence.check(
        "состояние «хост потерян»",
        liveness == Liveness::Lost,
        format!("{liveness:?}"),
    );
    let mut found = Vec::new();
    for key in &saved {
        let identity = identity(*key)?;
        let ppid = match &identity {
            Identity::Same(info) => Some(info.ppid),
            _ => None,
        };
        found.push(json!({
            "key": key,
            "alive": identity.is_alive(),
            "identity": identity,
            "ppid": ppid,
        }));
    }
    evidence.note("found", found);
    let survivors = zmx::alive(&saved).map_err(|err| err.to_string())?;
    let running = processes_with(&session.fixture_nonce)?;
    evidence.note("survivors", &survivors);
    evidence.note("running_with_nonce", &running);
    let unsaved: Vec<&ProcessKey> = running.iter().filter(|key| !saved.contains(key)).collect();
    evidence.check(
        "оставшиеся процессы сессии найдены по записи",
        unsaved.is_empty(),
        format!("живы по записи {survivors:?}, вне записи {unsaved:?}"),
    );
    if let Some((mut link, key)) = hup_child {
        evidence.check(
            "процесс, игнорирующий HUP, среди сохранённых",
            saved.contains(&key),
            format!("PID {}", key.pid),
        );
        if survivors.contains(&key) {
            // Квитанция команды — доказательство, что найденный процесс исполняется; затем уборка.
            evidence.require(
                "процесс, игнорирующий HUP, отвечает",
                link.request(Command::Exit { code: 0 }, WAIT),
            );
        } else {
            evidence.not_proven = Some(format!(
                "процесс, игнорирующий HUP (PID {}), не пережил сбой: стенд не создал условие \
                 клетки",
                key.pid
            ));
        }
        let cleaned = launch::wait_exit(&[key.pid], WAIT).is_empty();
        evidence.note("cleanup_hup_child_exited", cleaned);
    }
    Ok(())
}

// --- A7 ---

/// Долгая работа без клиента: заголовок, перо, позиционирование, режимы и много строк.
fn a7_steps() -> Vec<Vec<u8>> {
    let mut steps = vec![
        b"\x1b]2;pss-a7\x07".to_vec(),
        b"\x1b[1;31mred\x1b[0m plain\r\n".to_vec(),
    ];
    for line in 0..60 {
        steps.push(format!("headless line {line:03}\r\n").into_bytes());
    }
    steps.extend([
        b"\x1b[5;10H\x1b[4mpositioned\x1b[0m".to_vec(),
        b"\x1b[?2004h\x1b[?1h".to_vec(),
        b"\x1b[12;3Hcursor here".to_vec(),
    ]);
    steps
}

fn a7(stand: &mut Stand) -> Result<Cell, String> {
    let id = stand.session("a7")?;
    let mut session = stand.ensure(&id, Mode::Emitter)?;
    let mut evidence = Evidence::default();
    evidence.note("record", &session.record);
    a7_body(stand, &mut session, &mut evidence)?;
    let artifact = stand.artifact("a7.json", &evidence)?;
    stand.discard(&id);
    Ok(cell(
        Spec {
            id: "A7",
            part: None,
            stimulus:
                "сессия создана ensure и ни разу не имела клиента; фикстура пишет заголовок, \
                       перо, 62 строки, позиционирование и режимы; повторный ensure; затем первый \
                       attach"
                    .to_owned(),
            oracle: "эталон и приёмник первого клиента; журнал демона: назначения лидера и \
                     отправка снимка; ревизия обработки без клиента",
            completion: "c1 стал лидером, барьер DSR через c1 пройден, состояния сняты",
            origin: DefectOrigin::Host,
        },
        &evidence,
        vec![artifact, stand.log_name(&id)],
    ))
}

fn a7_body(stand: &Stand, session: &mut Session, evidence: &mut Evidence) -> Result<(), String> {
    let mut reference = Reference::new(&stand.config).map_err(|err| format!("эталон: {err}"))?;
    for step in a7_steps() {
        emit(&mut session.link, &mut reference, &step)?;
    }
    let snapshot = stand
        .zmx
        .snapshot(&session.id)
        .map_err(|err| err.to_string())?;
    evidence.note("headless_snapshot", &snapshot);
    evidence.check(
        "хост обработал вывод без клиента",
        snapshot.text.contains("cursor here"),
        format!("ревизия {}", snapshot.revision),
    );
    let again = stand
        .zmx
        .ensure(
            &session.id,
            &stand.workload(Mode::Emitter, &session.fixture_nonce)?,
            stand.size,
        )
        .map_err(|err| format!("повторный ensure: {err}"))?;
    evidence.note("ensure_again", &again);
    let log = stand.daemon_log(&session.id);
    let leaders = zmx::leadership_events(&log);
    evidence.check(
        "ensure без скрытого attach",
        leaders == 0 && !again.created,
        format!("назначений лидера до attach: {leaders}"),
    );
    let client = stand.client(&session.id, "c1")?;
    if evidence.require("c1 лидер", client.wait_leader()).is_none()
        || evidence
            .require("барьер через c1", barrier(&mut session.link, &client))
            .is_none()
    {
        return Ok(());
    }
    compare_states(evidence, "первый attach", &reference, &client)?;
    let log = stand.daemon_log(&session.id);
    evidence.note(
        "daemon_log",
        json!({
            "leaders": zmx::leadership_events(&log),
            "snapshots": zmx::snapshot_events(&log),
        }),
    );
    Ok(())
}
