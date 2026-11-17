//! Стенд сценариев матрицы на zmx: роль GUI, сессии с фикстурой, attach-клиенты с приёмником и
//! общие шаги оракулов. Его используют рубеж A (`lifecycle`) и headless-матрица задачи 9.
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
//! сторож видит любой оставшийся процесс кандидата. Комбинация `zmx-prev` (G1, G2) работает в том
//! же `ZMX_DIR`: клиент одной комбинации подключается к демону другой.

use std::fs;
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus};
use std::time::Instant;

use serde::Serialize;
use serde_json::json;

use crate::backend::rust::{self as rust_host, RustHost};
use crate::backend::zmx::{self, ZmxSet};
use crate::backend::{
    self as backend, Backend, HostRecord, Lead, Liveness, Role, SessionId, Workload, signal,
    wait_child,
};
use crate::fixture::channel::{Command, Event, Link, Listener, Ready, hex};
use crate::fixture::{self, Mode};
use crate::launch;
use crate::matrix::Candidate;
use crate::matrix::evidence::Evidence;
use crate::matrix::util;
use crate::observer::{self, Identity, ProcessKey};
use crate::oracle::model::{Outcome, State, compare};
use crate::oracle::vt::{
    Capture, Config, Receiver, ReceiverProfile, Reference, Route, ThroughCandidate,
};
use crate::pty::{self, PtyChild, Spawn, Winsize};
use crate::results::{self, Environment};
use std::time::Duration;

pub const COLS: u16 = 80;
pub const ROWS: u16 = 24;
/// Срок одного ожидания квитанции, роли или выхода процесса.
pub const WAIT: Duration = Duration::from_secs(15);
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
pub const STOP_GRACE: Duration = Duration::from_secs(5);
/// `Ctrl-\`: клавиша отключения клиента zmx.
pub const DETACH_KEY: &[u8] = b"\x1c";

// --- стенд ---

pub struct Stand {
    pub nonce: String,
    pub out: PathBuf,
    pub root: PathBuf,
    pub listener: Option<Listener>,
    pub fixture: PathBuf,
    pub zmx: zmx::Zmx,
    /// Комбинация `zmx-prev` в том же `ZMX_DIR` и с тем же токеном: G1 и G2.
    pub prev: zmx::Zmx,
    /// Кандидат процесса сценария: его сессии создают `ensure` и `client`.
    pub candidate: Candidate,
    /// Rust-хост в каталоге `/tmp/pss-<nonce>/r`, журналы — в `<out>/rust/logs`.
    pub rust: Option<RustHost>,
    pub config: Config,
    pub size: Winsize,
    pub sessions: Vec<SessionId>,
}

impl Stand {
    pub fn new(out: &Path, environment: &Environment) -> Result<Stand, String> {
        Stand::for_candidate(out, environment, Candidate::Zmx)
    }

    /// Стенд кандидата `candidate`; zmx готовится всегда: на нём идут G1 и G2.
    pub fn for_candidate(
        out: &Path,
        environment: &Environment,
        candidate: Candidate,
    ) -> Result<Stand, String> {
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
        let prev_binary = ZmxSet::Prev.binary(&sets_dir);
        if !prev_binary.is_file() {
            return Err(format!("предусловие: нет {}", prev_binary.display()));
        }
        let zmx_config = zmx::Config {
            set: ZmxSet::Current,
            binary,
            dir: link,
            cwd: root.clone(),
            env,
            token: format!("pss-{nonce}"),
            timeout: COMMAND_TIMEOUT,
        };
        let prev = zmx::Zmx::new(zmx::Config {
            set: ZmxSet::Prev,
            binary: prev_binary,
            ..zmx_config.clone()
        })?;
        let rust = match candidate {
            Candidate::Zmx => None,
            Candidate::Rust => {
                let bin_dir = program.parent().ok_or("каталог раннера")?;
                let dir = root.join("r");
                fs::create_dir(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
                Some(RustHost::new(rust_host::Config {
                    host: bin_dir.join(crate::host::HOST_NAME),
                    attach: bin_dir.join(crate::host::attach::ATTACH_NAME),
                    dir,
                    logs: out.join("rust").join("logs"),
                    cwd: root.clone(),
                    env: zmx_config.env.clone(),
                    token: zmx_config.token.clone(),
                    timeout: COMMAND_TIMEOUT,
                    break_socket: None,
                    versions: crate::proto::SUPPORTED,
                })?)
            }
        };
        let zmx = zmx::Zmx::new(zmx_config)?;
        let config = Config::new(COLS, ROWS);
        let size = config.winsize(COLS, ROWS);
        let stand = Stand {
            nonce,
            out: out.to_path_buf(),
            root,
            listener: Some(listener),
            fixture,
            zmx,
            prev,
            candidate,
            rust,
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

    pub fn listener(&self) -> Result<&Listener, String> {
        self.listener
            .as_ref()
            .ok_or_else(|| "сокет квитанций закрыт".to_owned())
    }

    pub fn session(&self, label: &str) -> Result<SessionId, String> {
        SessionId::new(&format!("s{}-{label}", self.nonce))
    }

    pub fn workload(&self, mode: Mode, fixture_nonce: &str) -> Result<Workload, String> {
        Ok(Workload {
            program: self.fixture.clone(),
            args: fixture::command_args(mode, self.listener()?.path(), fixture_nonce, None),
        })
    }

    /// Сессия с фикстурой и её готовность. Размер PTY нагрузки — размер окна стенда: иначе
    /// attach менял бы размер и эталону понадобился бы ресайз.
    pub fn ensure(&mut self, id: &SessionId, mode: Mode) -> Result<Session, String> {
        self.ensure_via(None, id, mode)
    }

    /// Кандидат процесса сценария.
    pub fn main(&self) -> &(dyn Backend + Sync) {
        match &self.rust {
            Some(rust) => rust,
            None => &self.zmx,
        }
    }

    fn pick(&self, set: Option<ZmxSet>) -> &(dyn Backend + Sync) {
        match set {
            Some(set) => self.backend(set),
            None => self.main(),
        }
    }

    /// Комбинация zmx стенда.
    pub fn backend(&self, set: ZmxSet) -> &zmx::Zmx {
        match set {
            ZmxSet::Current => &self.zmx,
            ZmxSet::Prev => &self.prev,
        }
    }

    /// Сессия на демоне комбинации `set`.
    pub fn ensure_on(
        &mut self,
        set: ZmxSet,
        id: &SessionId,
        mode: Mode,
    ) -> Result<Session, String> {
        self.ensure_via(Some(set), id, mode)
    }

    fn ensure_via(
        &mut self,
        set: Option<ZmxSet>,
        id: &SessionId,
        mode: Mode,
    ) -> Result<Session, String> {
        self.sessions.push(id.clone());
        self.ensure_by(self.pick(set), id, mode)
    }

    /// Сессия на адаптере вне стенда: другое поколение Rust-хоста (G1, G2). Уборку его сессии
    /// стенд делает через основной адаптер, если тот её знает, иначе — по nonce запуска.
    pub fn ensure_with(
        &mut self,
        backend: &dyn Backend,
        id: &SessionId,
        mode: Mode,
    ) -> Result<Session, String> {
        self.sessions.push(id.clone());
        self.ensure_by(backend, id, mode)
    }

    fn ensure_by(
        &self,
        backend: &dyn Backend,
        id: &SessionId,
        mode: Mode,
    ) -> Result<Session, String> {
        let fixture_nonce = format!("{id}-w");
        let workload = self.workload(mode, &fixture_nonce)?;
        let ensured = backend
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

    pub fn accept(&self, nonce: Option<&str>) -> Result<(Link, Ready), String> {
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

    pub fn client(&self, id: &SessionId, label: &str) -> Result<Client, String> {
        self.client_on(self.main(), id, label, Lead::IfFree, self.size)
    }

    /// Клиент комбинации `set` с правилом лидерства `lead` в окне размера `size`.
    pub fn client_with(
        &self,
        set: ZmxSet,
        id: &SessionId,
        label: &str,
        lead: Lead,
        size: Winsize,
    ) -> Result<Client, String> {
        self.client_on(self.backend(set), id, label, lead, size)
    }

    pub fn client_on(
        &self,
        backend: &dyn Backend,
        id: &SessionId,
        label: &str,
        lead: Lead,
        size: Winsize,
    ) -> Result<Client, String> {
        let command = backend
            .attach_command(id, lead)
            .map_err(|err| format!("команда attach: {err}"))?;
        let mut profile = ReceiverProfile::for_client(label);
        if let Some(prefix) = backend.role_prefix() {
            profile = profile.with_role_prefix(&prefix);
        }
        let config = Config {
            cols: size.cols,
            rows: size.rows,
            ..self.config.clone()
        };
        let receiver = Receiver::new(label, &config, profile)
            .map_err(|err| format!("приёмник {label}: {err}"))?;
        let PtyChild { master, child, .. } = pty::spawn(&Spawn {
            program: &command.program,
            args: &command.args,
            env: &command.env,
            size,
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
    pub fn discard(&self, id: &SessionId) -> Vec<String> {
        let mut notes = Vec::new();
        let main = self.main();
        let Some(record) = main.record(id) else {
            return notes;
        };
        let processes = backend::session_processes(&record).unwrap_or_else(|_| vec![record.host]);
        if main
            .liveness(id)
            .is_ok_and(|state| matches!(state, Liveness::Live { .. }))
        {
            let _ = main.stop(id, STOP_GRACE);
        }
        let survivors = backend::alive(&processes).unwrap_or_default();
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
    pub fn teardown(&mut self) -> Vec<String> {
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

    pub fn artifact(&self, name: &str, value: &impl Serialize) -> Result<String, String> {
        results::write_json(&self.out, name, value)?;
        Ok(name.to_owned())
    }

    pub fn log_name(&self, id: &SessionId) -> String {
        match self.candidate {
            Candidate::Zmx => format!("zmx/logs/{id}.log"),
            Candidate::Rust => format!("rust/logs/{id}.log"),
        }
    }

    pub fn daemon_log(&self, id: &SessionId) -> String {
        let path = match &self.rust {
            Some(rust) => rust.session_log(id),
            None => self.zmx.session_log(id),
        };
        fs::read_to_string(path).unwrap_or_default()
    }
}

/// Точка покоя Rust-хоста для протокола разреза: наблюдатель разбора подключён, всё прочитанное
/// хостом разобрано, `base` — разобрано байт к этой точке.
pub struct ParsedCut {
    pub observer: rust_host::Observer,
    pub base: u64,
}

impl Stand {
    /// Rust-хост стенда; у zmx протокол разреза идёт через ревизию `zmx screen`.
    pub fn rust(&self) -> Result<&RustHost, String> {
        self.rust
            .as_ref()
            .ok_or_else(|| "стенд не на Rust-хосте".to_owned())
    }

    /// Наблюдатель разбора и точка покоя сессии `id`: прочитанное хостом равно разобранному.
    pub fn parsed_cut(&self, id: &SessionId) -> Result<ParsedCut, String> {
        let rust = self.rust()?;
        let observer = rust
            .observe(id)
            .map_err(|err| format!("наблюдатель разбора: {err}"))?;
        let deadline = Instant::now() + WAIT;
        loop {
            let status = rust
                .status(id)
                .map_err(|err| format!("состояние хоста: {err}"))?;
            if status.read_bytes == status.parsed_bytes {
                return Ok(ParsedCut {
                    observer,
                    base: status.parsed_bytes,
                });
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "хост не пришёл в покой за {} с: прочитано {}, разобрано {}",
                    WAIT.as_secs(),
                    status.read_bytes,
                    status.parsed_bytes
                ));
            }
        }
    }
}

impl ParsedCut {
    /// Ждёт разбора префикса длиной `len`; итог — заметка для доказательств и граница разреза.
    pub fn end(
        &self,
        len: usize,
        violations: &[String],
    ) -> (serde_json::Value, Result<(), String>) {
        let target = self.base + len as u64;
        let reached = self.observer.wait_offset(target, WAIT);
        let events: Vec<rust_host::Parsed> = self
            .observer
            .events()
            .into_iter()
            .filter(|event| event.offset > self.base)
            .collect();
        let pieces: Vec<(u64, u64)> = events.iter().map(|e| (e.read, e.offset)).collect();
        let verdict = match reached {
            None => Err(format!(
                "хост не разобрал префикс {len} байт за {} с",
                WAIT.as_secs()
            )),
            Some(_) => util::parsed_boundary(&pieces, self.base, len as u64, violations),
        };
        (json!({ "base": self.base, "parsed": events }), verdict)
    }
}

/// Поколение лидерства из последнего заголовка роли клиента.
pub fn generation(client: &Client) -> Result<u32, String> {
    client
        .route()?
        .with(|rx| rx.last_role().and_then(backend::parse_role))
        .map(|report| report.generation)
        .ok_or_else(|| format!("клиент {} не сообщил поколение", client.label))
}

pub struct Session {
    pub id: SessionId,
    pub fixture_nonce: String,
    pub record: HostRecord,
    pub link: Link,
}

// --- attach-клиент ---

/// Attach-клиент кандидата в PTY стенда. Мастер держат стенд (для `tcgetpgrp`) и маршрут; гибель
/// GUI — закрытие обоих.
pub struct Client {
    pub label: String,
    pub process: Child,
    pub master: Option<OwnedFd>,
    pub route: Option<Route>,
}

impl Client {
    pub fn pid(&self) -> i32 {
        self.process.id() as i32
    }

    pub fn route(&self) -> Result<&Route, String> {
        self.route
            .as_ref()
            .ok_or_else(|| format!("маршрут клиента {} закрыт", self.label))
    }

    /// Ждёт сообщения «лидер» от управляемого клиента: `Init` обработан и клиент ведёт сессию.
    pub fn wait_leader(&self) -> Result<String, String> {
        let route = self.route()?;
        route
            .wait_until(WAIT, |rx, _| {
                rx.last_role()
                    .and_then(backend::parse_role)
                    .is_some_and(|report| report.role == Role::Leader)
            })
            .map_err(|err| format!("клиент {} не стал лидером: {err}", self.label))?;
        Ok(route
            .with(|rx| rx.last_role().map(str::to_owned))
            .unwrap_or_default())
    }

    pub fn input(&self, bytes: &[u8]) -> Result<(), String> {
        self.route()?.input(bytes);
        Ok(())
    }

    pub fn capture(&self) -> Result<State, String> {
        self.route()?
            .with(|rx| rx.capture(Capture::default()))
            .map_err(|err| format!("состояние приёмника {}: {err}", self.label))
    }

    /// Группа переднего плана PTY клиента.
    pub fn foreground(&self) -> Result<i32, String> {
        let master = self.master.as_ref().ok_or("мастер PTY клиента закрыт")?;
        pty::foreground_pgid(master.as_fd()).map_err(|err| format!("tcgetpgrp: {err}"))
    }

    pub fn wait_exit(&mut self) -> Result<ExitStatus, String> {
        wait_child(&mut self.process, WAIT).map_err(|err| err.to_string())
    }

    /// `SIGKILL` клиенту.
    pub fn kill(&mut self, backend: &dyn Backend) -> Result<ExitStatus, String> {
        backend
            .kill_client(&mut self.process, WAIT)
            .map_err(|err| err.to_string())
    }

    /// Гибель GUI: закрываются все дескрипторы мастера PTY клиента, ядро шлёт ему `SIGHUP`.
    pub fn hangup(&mut self) -> Result<ExitStatus, String> {
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

pub fn barrier(link: &mut Link, client: &Client) -> Result<(), String> {
    ThroughCandidate {
        link,
        route: client.route()?,
        timeout: WAIT,
    }
    .barrier()
}

/// Запись фикстуры с подтверждением и та же запись в эталон.
pub fn emit(link: &mut Link, reference: &mut Reference, bytes: &[u8]) -> Result<(), String> {
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

pub fn identity(key: ProcessKey) -> Result<Identity, String> {
    observer::identify(key)
}

pub fn same_alive(evidence: &mut Evidence, name: &str, key: ProcessKey) -> Result<bool, String> {
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
pub fn compare_states(
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
