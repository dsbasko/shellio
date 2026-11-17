//! Сценарий `bridge` в поверхности (план P0.4, задача 15). Процесс сценария — водитель без
//! поверхности; GUI — дочерние процессы `probe-session-surface --gui`, первую стенд снимает
//! настоящим `SIGKILL`, следующая открывает поверхность к той же сессии. Клетки, протокол и
//! ожидаемые виды — `matrix::bridge`.
//!
//! Границы:
//! - барьер — DSR состояния `CSI 5 n` от фикстуры: поверхность отвечает на него после всех байтов
//!   перед ним, поэтому после ответа `read_text` показывает их результат;
//! - граница ввода — маркер-вставка через поверхность: байты от поверхности до маркера — то, что
//!   она послала до него; маркер, не дошедший до фикстуры, — `not_run`, а не пустой итог.

use std::fs;
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as Process, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::args::Target;
use crate::backend::rust::{self as rust_host, RustHost};
use crate::backend::zmx::{self, ZmxSet};
use crate::backend::{
    self as backend, Backend, HostRecord, InputPath, Lead, SessionId, Workload, signal,
};
use crate::fixture::channel::{Command, Event, Link, LinkError, Listener, Receipt, hex, unhex};
use crate::fixture::query::{Query, ResponseKind};
use crate::fixture::{self, Mode};
use crate::host::HOST_NAME;
use crate::host::attach::ATTACH_NAME;
use crate::launch;
use crate::matrix::Candidate;
use crate::matrix::bridge::{
    self, BARRIER, BARRIER_REPLY, CLICK, Effect, GuiCommand, Hello, KEYCODE_A, KEYCODE_UP,
    PASTE_TEXT, Reply, SceneSpec, Scenes, Step,
};
use crate::matrix::evidence::{self, Evidence, Spec};
use crate::matrix::report_policy::{self, SCENES_DIR};
use crate::matrix::util::{D1_QUERIES, base64, osc52_payload};
use crate::oracle::model::PROFILE_VERSION;
use crate::pty::Winsize;
use crate::results::{Cell, DefectOrigin, Environment, Report};
use crate::surface::gui::CONFIG_FILE;

const COLS: u16 = 80;
const ROWS: u16 = 24;
const WAIT: Duration = Duration::from_secs(15);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const STOP_GRACE: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(5);
/// Срок ответа нагрузке на чтение буфера обмена после гибели GUI: срок ожидания GUI хостом
/// (1 с) с запасом.
const CLIPBOARD_SETTLE: Duration = Duration::from_secs(3);
const CLIPBOARD_TEXT: &str = "bridge-d4";

pub fn run(target: &Target, environment: &Environment) -> Result<Report, String> {
    let candidate = target
        .backend
        .ok_or("предусловие: сценарий bridge идёт на кандидате (--backend)")?;
    let scenes = read_scenes(&target.out)?;
    let mut driver = Driver::new(&target.out, environment, candidate)?;
    let outcome = driver.cells(&scenes);
    let notes = driver.teardown();
    let mut report = Report {
        cells: outcome?,
        ..Report::default()
    };
    report.diagnostics.notes.push(format!(
        "сцены: профиль v{}, штамп VT {}",
        scenes.profile,
        scenes.vt_stamp.as_deref().unwrap_or("?")
    ));
    report.diagnostics.notes.extend(notes);
    Ok(report)
}

fn read_scenes(out: &Path) -> Result<Scenes, String> {
    let path = out.join(SCENES_DIR).join(bridge::SCENES_FILE);
    let text = fs::read_to_string(&path)
        .map_err(|err| format!("предусловие: сцены {}: {err}", path.display()))?;
    let scenes: Scenes = serde_json::from_str(&text)
        .map_err(|err| format!("предусловие: сцены {}: {err}", path.display()))?;
    if scenes.profile != PROFILE_VERSION {
        return Err(format!(
            "предусловие: сцены построены под профилем v{}, текущий v{PROFILE_VERSION}",
            scenes.profile
        ));
    }
    for scene in &scenes.scenes {
        if (scene.cols, scene.rows) != (COLS, ROWS) {
            return Err(format!(
                "предусловие: сцена {} {}×{}, стенд {COLS}×{ROWS}",
                scene.name, scene.cols, scene.rows
            ));
        }
    }
    Ok(scenes)
}

fn step<'a>(scene: &'a SceneSpec, name: &str) -> Result<&'a Step, String> {
    scene
        .steps
        .iter()
        .find(|step| step.name == name)
        .ok_or_else(|| format!("предусловие: у сцены {} нет шага {name}", scene.name))
}

/// Водитель: кандидат, сокет квитанций фикстуры и сокет управления GUI.
struct Driver {
    nonce: String,
    out: PathBuf,
    root: PathBuf,
    candidate: Candidate,
    backend: Box<dyn Backend + Sync>,
    /// Второй экземпляр адаптера Rust-хоста для соединений роли GUI и состояния.
    rust: Option<RustHost>,
    fixture: PathBuf,
    program: PathBuf,
    listener: Option<Listener>,
    control: UnixListener,
    size: Winsize,
    sessions: Vec<SessionId>,
    windows: u32,
}

/// GUI-процесс с одной поверхностью.
struct Window {
    label: String,
    child: Child,
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    hello: Hello,
    opened: Reply,
}

impl Window {
    fn command(&mut self, command: &GuiCommand) -> Result<Reply, String> {
        let mut line = serde_json::to_vec(command).map_err(|err| format!("{err}"))?;
        line.push(b'\n');
        self.writer
            .write_all(&line)
            .map_err(|err| format!("{}: команда: {err}", self.label))?;
        let mut answer = String::new();
        self.reader
            .read_line(&mut answer)
            .map_err(|err| format!("{}: ответ: {err}", self.label))?;
        let reply: Reply = serde_json::from_str(&answer)
            .map_err(|err| format!("{}: ответ «{answer}»: {err}", self.label))?;
        match &reply.error {
            Some(error) => Err(format!("{}: {command:?}: {error}", self.label)),
            None => Ok(reply),
        }
    }

    /// Строки видимой области по `read_text`.
    fn lines(&mut self) -> Result<Vec<String>, String> {
        let text = self
            .command(&GuiCommand::Text { surface: 1 })?
            .text
            .ok_or_else(|| format!("{}: read_text не прочитал вид", self.label))?;
        Ok(report_policy::text_rows(&text))
    }

    fn paste(&mut self, text: &str) -> Result<(), String> {
        self.command(&GuiCommand::Paste {
            surface: 1,
            text: text.to_owned(),
        })
        .map(drop)
    }

    fn quit(mut self) {
        let _ = self.command(&GuiCommand::Quit);
        let _ = backend::wait_child(&mut self.child, WAIT);
    }
}

struct Session {
    id: SessionId,
    record: HostRecord,
    link: Link,
    /// Размер нагрузки по квитанции готовности.
    ready: Option<Winsize>,
}

impl Driver {
    fn new(out: &Path, environment: &Environment, candidate: Candidate) -> Result<Driver, String> {
        let nonce = environment
            .process
            .nonce
            .clone()
            .ok_or("предусловие: нет nonce запуска")?;
        let entry = &environment.process.env;
        let host_dir = required_dir(entry.get(launch::HOST_DIR), launch::HOST_DIR)?;
        for name in [HOST_NAME, ATTACH_NAME] {
            required_file(&host_dir.join(name))?;
        }
        let sets_dir = required_dir(entry.get(launch::SETS_DIR), launch::SETS_DIR)?;
        let binary = ZmxSet::Current.binary(&sets_dir);
        required_file(&binary)?;
        let program = std::env::current_exe().map_err(|err| format!("путь процесса: {err}"))?;
        let fixture = program
            .parent()
            .map(|dir| dir.join(fixture::FIXTURE_NAME))
            .filter(|path| path.is_file())
            .ok_or_else(|| {
                format!(
                    "предусловие: нет {} рядом с процессом",
                    fixture::FIXTURE_NAME
                )
            })?;
        let root = PathBuf::from(format!("/tmp/pss-{nonce}"));
        fs::create_dir(&root).map_err(|err| format!("{}: {err}", root.display()))?;
        let listener = Listener::bind(&root.join("c.sock"))?;
        let control = UnixListener::bind(root.join("g.sock"))
            .map_err(|err| format!("сокет управления GUI: {err}"))?;
        control
            .set_nonblocking(true)
            .map_err(|err| format!("сокет управления GUI: {err}"))?;
        let env: Vec<(String, String)> = launch::INHERITED
            .iter()
            .chain([launch::LANG].iter())
            .filter_map(|name| {
                entry
                    .get(*name)
                    .map(|value| ((*name).to_owned(), value.clone()))
            })
            .collect();
        let token = format!("pss-{nonce}");
        let (backend, rust): (Box<dyn Backend + Sync>, Option<RustHost>) = match candidate {
            Candidate::Zmx => {
                let real_dir = out.join("zmx");
                fs::create_dir(&real_dir)
                    .map_err(|err| format!("{}: {err}", real_dir.display()))?;
                let link = root.join("z");
                std::os::unix::fs::symlink(&real_dir, &link)
                    .map_err(|err| format!("{}: {err}", link.display()))?;
                let zmx = zmx::Zmx::new(zmx::Config {
                    set: ZmxSet::Current,
                    binary,
                    dir: link,
                    cwd: root.clone(),
                    env,
                    token,
                    timeout: COMMAND_TIMEOUT,
                })?;
                (Box::new(zmx), None)
            }
            Candidate::Rust => {
                let dir = root.join("r");
                fs::create_dir(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
                let config = rust_host::Config {
                    host: host_dir.join(HOST_NAME),
                    attach: host_dir.join(ATTACH_NAME),
                    dir,
                    logs: out.join("rust").join("logs"),
                    cwd: root.clone(),
                    env,
                    token,
                    timeout: COMMAND_TIMEOUT,
                    break_socket: None,
                    versions: crate::proto::SUPPORTED,
                };
                let side = RustHost::new(config.clone())?;
                (Box::new(RustHost::new(config)?), Some(side))
            }
        };
        Ok(Driver {
            nonce,
            out: out.to_path_buf(),
            root,
            candidate,
            backend,
            rust,
            fixture,
            program,
            listener: Some(listener),
            control,
            size: Winsize::cells(ROWS, COLS),
            sessions: Vec::new(),
            windows: 0,
        })
    }

    fn listener(&self) -> Result<&Listener, String> {
        self.listener
            .as_ref()
            .ok_or_else(|| "сокет квитанций закрыт".to_owned())
    }

    fn ensure(&mut self, label: &str, mode: Mode) -> Result<Session, String> {
        let id = SessionId::new(&format!("s{}-{label}", self.nonce))?;
        let fixture_nonce = format!("{id}-w");
        let workload = Workload {
            program: self.fixture.clone(),
            args: fixture::command_args(mode, self.listener()?.path(), &fixture_nonce, None),
        };
        self.sessions.push(id.clone());
        let ensured = self
            .backend
            .ensure(&id, &workload, self.size)
            .map_err(|err| format!("ensure {id}: {err}"))?;
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
        if ready.nonce != fixture_nonce || ready.pid != ensured.record.workload.pid {
            return Err(format!(
                "предусловие: подключилась фикстура {} PID {}, ожидалась {fixture_nonce} PID {}",
                ready.nonce, ready.pid, ensured.record.workload.pid
            ));
        }
        Ok(Session {
            id,
            record: ensured.record,
            link,
            ready: ready.size,
        })
    }

    /// Новый GUI-процесс с поверхностью, команда которой — attach-клиент кандидата.
    fn open(
        &mut self,
        session: &SessionId,
        lead: Lead,
        cols: u16,
        rows: u16,
    ) -> Result<Window, String> {
        self.windows += 1;
        let label = format!("gui{}", self.windows);
        let dir = self.out.join(&label);
        fs::create_dir(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
        fs::copy(self.out.join(CONFIG_FILE), dir.join(CONFIG_FILE))
            .map_err(|err| format!("{label}: конфигурация ghostty: {err}"))?;
        let file = |name: &str| {
            fs::File::create(dir.join(name)).map_err(|err| format!("{label}: {name}: {err}"))
        };
        let gui_nonce = format!("{}-{label}", self.nonce);
        let child = Process::new(&self.program)
            .arg("--gui")
            .arg(self.root.join("g.sock"))
            .arg("--out")
            .arg(&dir)
            .arg("--nonce")
            .arg(&gui_nonce)
            .stdin(Stdio::null())
            .stdout(file("stdout.txt")?)
            .stderr(file("stderr.txt")?)
            .spawn()
            .map_err(|err| format!("{label}: запуск: {err}"))?;
        let stream = self.accept_window(&label)?;
        let mut reader = BufReader::new(
            stream
                .try_clone()
                .map_err(|err| format!("{label}: сокет: {err}"))?,
        );
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .map_err(|err| format!("{label}: приветствие: {err}"))?;
        let hello: Hello = serde_json::from_str(&line)
            .map_err(|err| format!("{label}: приветствие «{line}»: {err}"))?;
        if hello.nonce != gui_nonce || hello.pid != child.id().cast_signed() {
            return Err(format!("{label}: чужое приветствие {hello:?}"));
        }
        let mut window = Window {
            label,
            child,
            reader,
            writer: stream,
            hello,
            opened: Reply::default(),
        };
        let attach = self
            .backend
            .attach_command(session, lead)
            .map_err(|err| format!("команда attach: {err}"))?;
        window.opened = window.command(&GuiCommand::Open {
            surface: 1,
            command: surface_command(&attach.program, &attach.args, &attach.env),
            cols,
            rows,
        })?;
        Ok(window)
    }

    fn accept_window(&self, label: &str) -> Result<UnixStream, String> {
        let deadline = Instant::now() + WAIT;
        loop {
            match self.control.accept() {
                Ok((stream, _)) => {
                    stream
                        .set_nonblocking(false)
                        .and_then(|()| stream.set_read_timeout(Some(WAIT)))
                        .map_err(|err| format!("{label}: сокет: {err}"))?;
                    return Ok(stream);
                }
                Err(err) if err.kind() == ErrorKind::WouldBlock && Instant::now() < deadline => {
                    std::thread::park_timeout(POLL);
                }
                Err(err) => return Err(format!("{label} не подключилась: {err}")),
            }
        }
    }

    /// Число клиентов сессии по `list` кандидата.
    fn clients(&self, id: &SessionId) -> Result<u32, String> {
        let listed = self.backend.list().map_err(|err| format!("list: {err}"))?;
        let entry = listed
            .iter()
            .find(|entry| entry.name == id.as_str())
            .ok_or_else(|| format!("сессии {id} нет в list"))?;
        Ok(entry.clients.unwrap_or(0))
    }

    fn wait_clients(&self, id: &SessionId, want: u32) -> Result<(), String> {
        let deadline = Instant::now() + WAIT;
        loop {
            let clients = self.clients(id)?;
            if clients == want {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!("клиентов {id}: {clients}, ожидалось {want}"));
            }
            std::thread::park_timeout(POLL);
        }
    }

    /// Настоящий `SIGKILL` GUI-процесса.
    fn kill(&self, window: &mut Window) -> Result<ExitStatus, String> {
        signal(window.hello.pid, libc::SIGKILL)
            .map_err(|err| format!("SIGKILL {}: {err}", window.label))?;
        backend::wait_child(&mut window.child, WAIT)
            .map_err(|err| format!("{}: {err}", window.label))
    }

    /// Нагрузка и хост живы.
    fn alive(&self, session: &Session) -> Result<bool, String> {
        let keys = [session.record.workload, session.record.host];
        let alive = backend::alive(&keys).map_err(|err| format!("{err}"))?;
        Ok(alive.len() == keys.len())
    }

    /// Журнал эффектов GUI; файла нет — ошибка стенда, а не ноль событий.
    fn effects(&self, window: &str) -> Result<Vec<Effect>, String> {
        let path = self.out.join(window).join(bridge::EFFECTS_FILE);
        let journal = fs::read_to_string(&path)
            .map_err(|err| format!("журнал эффектов {}: {err}", path.display()))?;
        bridge::parse_effects(&journal)
    }

    fn clipboard_writes(&self, window: &str, text: &str) -> Result<usize, String> {
        Ok(self
            .effects(window)?
            .iter()
            .filter(|effect| {
                matches!(effect, Effect::ClipboardWrite { text: written, .. } if written == text)
            })
            .count())
    }

    fn clipboard_reads(&self, window: &str) -> Result<Vec<u64>, String> {
        Ok(self
            .effects(window)?
            .iter()
            .filter_map(|effect| match effect {
                Effect::ClipboardRead { request, .. } => Some(*request),
                Effect::ClipboardWrite { .. } => None,
            })
            .collect())
    }

    /// Номер запроса `count`-го чтения буфера обмена в журнале GUI; `None` — не пришёл за срок.
    fn wait_clipboard_read(&self, window: &str, count: usize) -> Result<Option<u64>, String> {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(request) = self.clipboard_reads(window)?.get(count - 1) {
                return Ok(Some(*request));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::park_timeout(POLL);
        }
    }

    /// Ввод `marker` через кандидата без клиента и позиция во вводе фикстуры сразу после него;
    /// `None` — маркер не дошёл за срок.
    fn drain(&self, session: &mut Session, marker: &str) -> Result<Option<Mark>, String> {
        let start = Mark::at(session.link.log());
        self.backend
            .input(&session.id, marker.as_bytes(), InputPath::Raw)
            .map_err(|err| format!("ввод {marker} через кандидата: {err}"))?;
        let mut end = None;
        pull_until(&mut session.link, WAIT, |log| {
            let input = bridge::input_since(log, 0);
            end = input
                .get(start.bytes..)
                .and_then(|rest| report_policy::split_at_marker(rest, marker))
                .map(|fence| start.bytes + fence.end);
            end.is_some()
        })?;
        Ok(end.map(|bytes| Mark {
            bytes,
            barriers: bridge::barrier_replies(session.link.log()),
        }))
    }

    /// Соединение роли GUI от имени текущего лидера (Rust-хост).
    fn host_gui(&self, id: &SessionId) -> Result<Option<rust_host::Gui>, String> {
        let Some(rust) = &self.rust else {
            return Ok(None);
        };
        let generation = rust
            .status(id)
            .map_err(|err| format!("состояние {id}: {err}"))?
            .generation;
        rust.gui(id, generation, None)
            .map(Some)
            .map_err(|err| format!("GUI поколения {generation}: {err}"))
    }

    fn artifact(&self, name: &str, value: &impl Serialize) -> Result<String, String> {
        crate::results::write_json(&self.out, name, value)?;
        Ok(name.to_owned())
    }

    fn cells(&mut self, scenes: &Scenes) -> Result<Vec<Cell>, String> {
        let mut cells = vec![self.c1(scenes.scene("text")?)?];
        cells.extend(self.c2_c3(scenes.scene("modes")?)?);
        cells.push(self.c4()?);
        cells.push(self.d1()?);
        cells.push(self.d3()?);
        cells.push(self.d4()?);
        cells.push(self.d5()?);
        cells.push(self.f2()?);
        Ok(cells)
    }

    fn vt_origin(&self) -> DefectOrigin {
        match self.candidate {
            Candidate::Zmx => DefectOrigin::Vt,
            Candidate::Rust => DefectOrigin::Host,
        }
    }

    /// C1: история, мягкий перенос и курсор после настоящего `SIGKILL` GUI против эталона.
    fn c1(&mut self, scene: &SceneSpec) -> Result<Cell, String> {
        let restored_step = step(scene, "restored")?;
        let marker_step = step(scene, "marker")?;
        let source = unhex(&scene.source_hex).ok_or("сцена text: hex исходника")?;
        let suffix = unhex(&marker_step.suffix_hex).ok_or("сцена text: hex суффикса")?;
        let mut session = self.ensure("c1", Mode::Emitter)?;
        let mut evidence = Evidence::default();
        let mut first = self.open(&session.id, Lead::IfFree, scene.cols, scene.rows)?;
        self.wait_clients(&session.id, 1)?;
        if !barrier(&mut session.link, &source)? {
            return Err("предусловие C1: GUI №1 не ответила на барьер".to_owned());
        }
        let live = first.lines()?;
        let mut record = serde_json::json!({
            "session": session.id.as_str(),
            "workload": session.record.workload,
            "host": session.record.host,
            "expected": { "restored": restored_step.view, "marker": marker_step.view },
            "gui1": { "pid": first.hello.pid, "lines": live },
        });
        if let Some(diff) = bridge::first_difference(&restored_step.view.lines, &live) {
            evidence.not_proven = Some(format!(
                "контроль: GUI №1 без восстановления не совпала с эталоном, {diff}"
            ));
            first.quit();
            return self.c1_cell(&evidence, &record);
        }
        let kill_mark = session.link.log().len();
        let status = self.kill(&mut first)?;
        evidence.check(
            "gui1-killed",
            status.signal() == Some(libc::SIGKILL),
            format!("GUI №1 завершилась {status:?}"),
        );
        self.wait_clients(&session.id, 0)?;
        evidence.check(
            "workload-and-host-alive",
            self.alive(&session)?,
            "нагрузка и хост после SIGKILL GUI №1",
        );
        let mut second = self.open(&session.id, Lead::IfFree, scene.cols, scene.rows)?;
        self.wait_clients(&session.id, 1)?;
        if !barrier(&mut session.link, b"")? {
            evidence.not_proven = Some("GUI №2 не ответила на барьер".to_owned());
            second.quit();
            return self.c1_cell(&evidence, &record);
        }
        let restored = second.lines()?;
        let writes = bridge::writes_since(session.link.log(), kill_mark);
        evidence.check(
            "no-redraw",
            writes == [BARRIER.len() as u64],
            format!("записи фикстуры после SIGKILL до чтения: {writes:?} (только барьер)"),
        );
        evidence.check(
            "restored-lines",
            restored == restored_step.view.lines,
            bridge::first_difference(&restored_step.view.lines, &restored)
                .unwrap_or_else(|| "строки равны эталону".to_owned()),
        );
        let marked = if barrier(&mut session.link, &suffix)? {
            Some(second.lines()?)
        } else {
            None
        };
        evidence.check(
            "cursor-marker",
            marked.as_ref() == Some(&marker_step.view.lines),
            match &marked {
                Some(lines) => bridge::first_difference(&marker_step.view.lines, lines)
                    .unwrap_or_else(|| "«@» в позиции курсора эталона".to_owned()),
                None => "GUI №2 не ответила на барьер после маркера".to_owned(),
            },
        );
        record["gui1"]["status"] = serde_json::json!(format!("{status:?}"));
        record["gui2"] = serde_json::json!({
            "pid": second.hello.pid,
            "restored": restored,
            "marked": marked,
        });
        record["receipts_after_kill"] = serde_json::json!(&session.link.log()[kill_mark..]);
        second.quit();
        self.c1_cell(&evidence, &record)
    }

    fn c1_cell(&self, evidence: &Evidence, record: &serde_json::Value) -> Result<Cell, String> {
        let artifact = self.artifact("bridge-c1.json", record)?;
        Ok(evidence::cell(
            Spec {
                id: "C1",
                part: None,
                stimulus: format!(
                    "{}: сцена text (история, SGR, гиперссылка, мягкий и жёсткий перенос) через \
                     GUI №1, SIGKILL GUI №1, новая GUI №2, «@» живым потоком",
                    self.candidate
                ),
                oracle: "ghostty_surface_read_text против строк эталона из --scenes; фикстура \
                         без перерисовки",
                completion: "ответ поверхности на барьер CSI 5 n",
                origin: self.vt_origin(),
            },
            evidence,
            vec![artifact],
        ))
    }

    /// C2 и C3 на сцене `modes`: восстановление режимов без лишнего ввода и кодирование ввода по
    /// шагам эталона. Контроль — та же сцена в GUI №1 живым потоком, без восстановления.
    fn c2_c3(&mut self, scene: &SceneSpec) -> Result<Vec<Cell>, String> {
        let source = unhex(&scene.source_hex).ok_or("сцена modes: hex исходника")?;
        let restored_step = step(scene, "restored")?;
        let mut session = self.ensure("c23", Mode::Emitter)?;
        let mut c2 = Evidence::default();
        let mut c3 = Evidence::default();
        let mut first = self.open(&session.id, Lead::IfFree, scene.cols, scene.rows)?;
        self.wait_clients(&session.id, 1)?;
        if !barrier(&mut session.link, &source)? {
            return Err("предусловие C2/C3: GUI №1 не ответила на барьер".to_owned());
        }
        let mut consumed = Mark::at(session.link.log());
        let live_lines = first.lines()?;
        let live_input = probe(
            &mut first,
            &mut session.link,
            &mut consumed,
            "pss-c3-control",
        )?;
        let expected_input = probes_of(restored_step)?;
        let mut record = serde_json::json!({
            "session": session.id.as_str(),
            "control": {
                "lines": live_lines,
                "input_hex": live_input.as_deref().map(hex),
                "expected_input_hex": hex(&expected_input),
            },
        });
        let control =
            if let Some(diff) = bridge::first_difference(&restored_step.view.lines, &live_lines) {
                Some(format!("вид {diff}"))
            } else if live_input.as_deref() != Some(expected_input.as_slice()) {
                Some(format!(
                    "ввод {:?} против эталона {}",
                    live_input.as_deref().map(hex),
                    hex(&expected_input)
                ))
            } else {
                None
            };
        if let Some(reason) = control {
            let reason =
                format!("контроль: GUI №1 без восстановления не совпала с эталоном, {reason}");
            c2.not_proven = Some(reason.clone());
            c3.not_proven = Some(reason);
            first.quit();
            return self.c2_c3_cells(&c2, &c3, &record);
        }
        let kill_mark = session.link.log().len();
        self.kill(&mut first)?;
        self.wait_clients(&session.id, 0)?;
        // Окно C2 начинается после всего, что вызвала гибель GUI №1: ввод через кандидата идёт
        // в PTY нагрузки после его собственных байтов отключения.
        let drained = self.drain(&mut session, "pss-c2-drain")?;
        c2.note("drain", drained);
        let Some(after_drain) = drained else {
            let reason = "ввод через кандидата перед GUI №2 не дошёл до фикстуры".to_owned();
            c2.not_proven = Some(reason.clone());
            c3.not_proven = Some(reason);
            return self.c2_c3_cells(&c2, &c3, &record);
        };
        consumed = after_drain;
        let mut second = self.open(&session.id, Lead::IfFree, scene.cols, scene.rows)?;
        self.wait_clients(&session.id, 1)?;
        if !barrier(&mut session.link, b"")? {
            let reason = "GUI №2 не ответила на барьер".to_owned();
            c2.not_proven = Some(reason.clone());
            c3.not_proven = Some(reason);
            second.quit();
            return self.c2_c3_cells(&c2, &c3, &record);
        }
        let winches = bridge::winches_since(session.link.log(), kill_mark);
        c2.note("winches", &winches);
        let transient: Vec<&Winsize> = winches
            .iter()
            .filter(|size| (size.cols, size.rows) != (scene.cols, scene.rows))
            .collect();
        match fence(
            &mut second,
            &mut session.link,
            &mut consumed,
            "pss-c2-restore",
        )? {
            Some(before) if transient.is_empty() => {
                c2.check(
                    "restore-quiet",
                    before.is_empty(),
                    if before.is_empty() {
                        "ни одного байта от поверхности и хоста до маркера".to_owned()
                    } else {
                        format!(
                            "{} ({:?})",
                            hex(&before),
                            report_policy::report_kinds(&before)
                        )
                    },
                );
                record["c2_before_hex"] = serde_json::json!(hex(&before));
            }
            Some(_) => {
                c2.not_proven = Some(format!("переходный размер нагрузки: {transient:?}"));
            }
            None => c2.not_proven = Some("маркер восстановления не дошёл до фикстуры".to_owned()),
        }
        let mut steps = Vec::new();
        for (index, step) in scene.steps.iter().enumerate() {
            let suffix = unhex(&step.suffix_hex).ok_or("сцена modes: hex суффикса")?;
            if index > 0 && !barrier(&mut session.link, &suffix)? {
                c3.not_proven = Some(format!("{}: GUI №2 не ответила на барьер", step.name));
                break;
            }
            let lines = second.lines()?;
            c3.check(
                &format!("{}: вид", step.name),
                lines == step.view.lines,
                bridge::first_difference(&step.view.lines, &lines)
                    .unwrap_or_else(|| "равен эталону".to_owned()),
            );
            let expected = probes_of(step)?;
            let marker = format!("pss-c3-{}", step.name);
            let Some(got) = probe(&mut second, &mut session.link, &mut consumed, &marker)? else {
                c3.not_proven = Some(format!("{}: маркер ввода не дошёл", step.name));
                break;
            };
            c3.check(
                &format!("{}: ввод", step.name),
                got == expected,
                format!("{} против эталона {}", hex(&got), hex(&expected)),
            );
            steps.push(serde_json::json!({
                "step": step.name,
                "lines": lines,
                "input_hex": hex(&got),
            }));
        }
        record["gui2"] = serde_json::json!({ "pid": second.hello.pid, "steps": steps });
        record["receipts_after_kill"] = serde_json::json!(&session.link.log()[kill_mark..]);
        second.quit();
        self.c2_c3_cells(&c2, &c3, &record)
    }

    fn c2_c3_cells(
        &self,
        c2: &Evidence,
        c3: &Evidence,
        record: &serde_json::Value,
    ) -> Result<Vec<Cell>, String> {
        let artifact = self.artifact("bridge-c23.json", record)?;
        Ok(vec![
            evidence::cell(
                Spec {
                    id: "C2",
                    part: None,
                    stimulus: format!(
                        "{}: сцена modes (1004, 2048, 2004, DECCKM, мышь, Kitty, alternate) через \
                         GUI №1, SIGKILL, восстановление в GUI №2",
                        self.candidate
                    ),
                    oracle: "квитанции прочитанных байтов фикстуры до маркера-вставки GUI №2",
                    completion: "маркер-вставка прочитан фикстурой",
                    origin: DefectOrigin::Host,
                },
                c2,
                vec![artifact.clone()],
            ),
            evidence::cell(
                Spec {
                    id: "C3",
                    part: None,
                    stimulus: format!(
                        "{}: после восстановления «a», «вверх», щелчок, вставка и суффиксы \
                         pop Kitty, pop Kitty, DECRC, выход из alternate",
                        self.candidate
                    ),
                    oracle: "байты от поверхности против кодировщиков эталона; read_text против \
                             строк эталона",
                    completion: "барьер CSI 5 n после суффикса и маркер-вставка после проб",
                    origin: self.vt_origin(),
                },
                c3,
                vec![artifact],
            ),
        ])
    }

    /// C4: два клиента разного размера, затем закрытие одного.
    fn c4(&mut self) -> Result<Cell, String> {
        const B_COLS: u16 = 100;
        const B_ROWS: u16 = 30;
        let mut session = self.ensure("c4", Mode::Emitter)?;
        let mut evidence = Evidence::default();
        let mut a = self.open(&session.id, Lead::IfFree, COLS, ROWS)?;
        self.wait_clients(&session.id, 1)?;
        let mut b = self.open(&session.id, Lead::IfFree, B_COLS, B_ROWS)?;
        self.wait_clients(&session.id, 2)?;
        if !barrier(&mut session.link, b"c4-both\r\n")? {
            return Err("предусловие C4: GUI A не ответила на барьер".to_owned());
        }
        let both = vec!["c4-both".to_owned()];
        let a_lines = a.lines()?;
        let (b_ok, b_lines) = wait_lines(&mut b, |lines| *lines == both)?;
        evidence.check(
            "a-shows-output",
            a_lines == both,
            format!("A {}×{}: {a_lines:?}", COLS, ROWS),
        );
        evidence.check(
            "b-shows-output",
            b_ok,
            format!("B {B_COLS}×{B_ROWS}: {b_lines:?}"),
        );
        let winches = bridge::winches_since(session.link.log(), 0);
        let current = winches.last().copied().or(session.ready);
        evidence.check(
            "size-from-leader",
            !winches
                .iter()
                .any(|size| (size.cols, size.rows) == (B_COLS, B_ROWS))
                && current.is_some_and(|size| (size.cols, size.rows) == (COLS, ROWS)),
            format!("SIGWINCH нагрузки {winches:?}, размер {current:?}"),
        );
        let closed = session.link.log().len();
        a.command(&GuiCommand::Close { surface: 1 })?;
        self.wait_clients(&session.id, 1)?;
        evidence.check(
            "workload-and-host-alive",
            self.alive(&session)?,
            "нагрузка и хост после закрытия A",
        );
        request(
            &mut session.link,
            Command::Write {
                hex: hex(b"c4-after\r\n"),
            },
        )?;
        let (after_ok, after_lines) =
            wait_lines(&mut b, |lines| lines.iter().any(|line| line == "c4-after"))?;
        evidence.check(
            "b-live-after-close",
            after_ok,
            format!("B после закрытия A: {after_lines:?}"),
        );
        let after = bridge::winches_since(session.link.log(), closed);
        evidence.note("winches_after_close", &after);
        let record = serde_json::json!({
            "session": session.id.as_str(),
            "a": { "pid": a.hello.pid, "grid": a.opened.grid, "lines": a_lines },
            "b": {
                "pid": b.hello.pid,
                "grid": b.opened.grid,
                "lines": b_lines,
                "after_close": after_lines,
            },
            "ready": session.ready,
            "receipts": session.link.log(),
        });
        a.quit();
        b.quit();
        let artifact = self.artifact("bridge-c4.json", &record)?;
        Ok(evidence::cell(
            Spec {
                id: "C4",
                part: None,
                stimulus: format!(
                    "{}: GUI A {COLS}×{ROWS} лидер, GUI B {B_COLS}×{B_ROWS} вторым клиентом, \
                     вывод нагрузки, закрытие поверхности A",
                    self.candidate
                ),
                oracle: "read_text обеих GUI, квитанции SIGWINCH нагрузки, list кандидата",
                completion: "барьер через A; строка в B за срок",
                origin: DefectOrigin::Host,
            },
            &evidence,
            vec![artifact],
        ))
    }

    /// D1 в поверхности: запросы профиля D1 при одной и при двух GUI, ровно один ответ на каждый.
    fn d1(&mut self) -> Result<Cell, String> {
        let mut session = self.ensure("d1", Mode::Emitter)?;
        let mut evidence = Evidence::default();
        let expected: Vec<ResponseKind> = D1_QUERIES.iter().map(|(_, q)| q.response()).collect();
        let a = self.open(&session.id, Lead::IfFree, COLS, ROWS)?;
        self.wait_clients(&session.id, 1)?;
        if !barrier(&mut session.link, b"")? {
            return Err("предусловие D1: GUI не ответила на барьер".to_owned());
        }
        let one = d1_round(&mut session.link)?;
        let b = self.open(&session.id, Lead::IfFree, COLS, ROWS)?;
        self.wait_clients(&session.id, 2)?;
        let two = if barrier(&mut session.link, b"")? {
            d1_round(&mut session.link)?
        } else {
            None
        };
        for (name, got) in [("одна GUI", &one), ("две GUI", &two)] {
            match got {
                Some(got) => {
                    let kinds: Vec<ResponseKind> = got.iter().map(|(kind, _)| *kind).collect();
                    evidence.check(
                        &format!("{name}: ровно один ответ на каждый запрос по порядку"),
                        kinds == expected,
                        format!("{got:?}"),
                    );
                }
                None => {
                    evidence.not_proven = Some(format!("{name}: барьер не пришёл"));
                }
            }
        }
        let record = serde_json::json!({
            "session": session.id.as_str(),
            "queries": D1_QUERIES.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
            "one": one,
            "two": two,
        });
        a.quit();
        b.quit();
        let artifact = self.artifact("bridge-d1.json", &record)?;
        Ok(evidence::cell(
            Spec {
                id: "D1",
                part: Some(bridge::SURFACE_PART),
                stimulus: format!(
                    "{}: DA1, DA2, CPR, DECRQM, OSC 10/11/4, XTVERSION от нагрузки при одной GUI и \
                     при двух",
                    self.candidate
                ),
                oracle: "счётчик ответов фикстуры",
                completion: "ответ поверхности на барьер CSI 5 n после запросов",
                origin: DefectOrigin::Host,
            },
            &evidence,
            vec![artifact],
        ))
    }

    /// D3 в поверхности: OSC 52 записи даёт эффект один раз при живом клиенте и не повторяется
    /// при восстановлении в новой GUI после `SIGKILL` прежней.
    fn d3(&mut self) -> Result<Cell, String> {
        const TEXT: &str = "bridge-d3";
        let mut session = self.ensure("d3", Mode::Emitter)?;
        let mut evidence = Evidence::default();
        let mut first = self.open(&session.id, Lead::IfFree, COLS, ROWS)?;
        self.wait_clients(&session.id, 1)?;
        let payload = format!("\x1b]52;c;{}\x07d3-after", base64(TEXT.as_bytes()));
        if !barrier(&mut session.link, payload.as_bytes())? {
            return Err("предусловие D3: GUI №1 не ответила на барьер".to_owned());
        }
        let live = self.clipboard_writes(&first.label, TEXT)?;
        evidence.check("live-once", live == 1, format!("эффектов у GUI №1: {live}"));
        self.kill(&mut first)?;
        self.wait_clients(&session.id, 0)?;
        let mut second = self.open(&session.id, Lead::IfFree, COLS, ROWS)?;
        self.wait_clients(&session.id, 1)?;
        if !barrier(&mut session.link, b"\r\nd3-end")? {
            return Err("предусловие D3: GUI №2 не ответила на барьер".to_owned());
        }
        let lines = second.lines()?;
        if !lines.iter().any(|line| line.contains("d3-after"))
            || !lines.iter().any(|line| line == "d3-end")
        {
            evidence.not_proven = Some(format!(
                "GUI №2 не показала восстановленный текст и живой поток: {lines:?}"
            ));
        }
        let replayed = self.clipboard_writes(&second.label, TEXT)?;
        let first_after = self.clipboard_writes(&first.label, TEXT)?;
        evidence.check(
            "no-replay",
            replayed == 0,
            format!("эффектов у GUI №2 после восстановления: {replayed}"),
        );
        evidence.check(
            "journal-survived-kill",
            first_after == live,
            format!("журнал GUI №1 после SIGKILL: {first_after}"),
        );
        let record = serde_json::json!({
            "session": session.id.as_str(),
            "gui1": { "label": first.label, "writes": live, "after_kill": first_after },
            "gui2": { "label": second.label, "writes": replayed, "lines": lines },
        });
        let journal = format!("{}/{}", first.label, bridge::EFFECTS_FILE);
        second.quit();
        let artifact = self.artifact("bridge-d3.json", &record)?;
        Ok(evidence::cell(
            Spec {
                id: "D3",
                part: Some(bridge::SURFACE_PART),
                stimulus: format!(
                    "{}: OSC 52 записи через GUI №1, SIGKILL, восстановление в GUI №2",
                    self.candidate
                ),
                oracle: "журнал эффектов поверхности (write_clipboard_cb)",
                completion: "барьер CSI 5 n после OSC 52 и после живого потока",
                origin: DefectOrigin::Host,
            },
            &evidence,
            vec![artifact, journal],
        ))
    }

    /// D4 в поверхности: чтение буфера обмена с асинхронным ответом GUI и чтение, ожидающее
    /// ответа, когда GUI снимают `SIGKILL`.
    fn d4(&mut self) -> Result<Cell, String> {
        let mut session = self.ensure("d4", Mode::Emitter)?;
        let mut evidence = Evidence::default();
        let mut first = self.open(&session.id, Lead::IfFree, COLS, ROWS)?;
        self.wait_clients(&session.id, 1)?;
        if !barrier(&mut session.link, b"")? {
            return Err("предусловие D4: GUI №1 не ответила на барьер".to_owned());
        }
        let mut host_gui = self.host_gui(&session.id)?;
        let mark = session.link.log().len();
        request(
            &mut session.link,
            Command::Query {
                query: Query::ClipboardRead,
                split: None,
            },
        )?;
        let asked = match &host_gui {
            None => match self.wait_clipboard_read(&first.label, 1)? {
                Some(id) => {
                    first.command(&GuiCommand::ClipboardReply {
                        request: id,
                        text: CLIPBOARD_TEXT.to_owned(),
                    })?;
                    format!("запрос {id} в журнале GUI №1, ответ после него")
                }
                None => {
                    evidence.check(
                        "live: запрос дошёл до колбэка поверхности",
                        false,
                        "нет ClipboardRead в журнале GUI №1",
                    );
                    String::new()
                }
            },
            Some(gui) => match gui.wait_ask(1, WAIT) {
                Ok(ask) => {
                    gui.answer(
                        ask,
                        ask.generation,
                        Some(CLIPBOARD_TEXT.as_bytes().to_vec()),
                    )
                    .map_err(|err| format!("ответ GUI: {err}"))?;
                    format!("запрос хоста {ask:?}, ответ соединением GUI")
                }
                Err(err) => {
                    evidence.check("live: хост спросил GUI лидера", false, err);
                    String::new()
                }
            },
        };
        evidence.note("live.asked", &asked);
        if !barrier(&mut session.link, b"")? {
            evidence.not_proven = Some("барьер после ответа не пришёл".to_owned());
        }
        let live = bridge::responses_since(session.link.log(), mark);
        let payloads = clipboard_payloads(&live);
        evidence.check(
            "live: ровно один ответ с данными GUI",
            payloads == [Some(base64(CLIPBOARD_TEXT.as_bytes()))],
            format!("{live:?}"),
        );
        if host_gui.is_some() {
            let reads = self.clipboard_reads(&first.label)?;
            evidence.check(
                "live: поверхность запроса не получила",
                reads.is_empty(),
                format!("ClipboardRead в журнале GUI №1: {reads:?}"),
            );
        }
        let mark = session.link.log().len();
        request(
            &mut session.link,
            Command::Query {
                query: Query::ClipboardRead,
                split: None,
            },
        )?;
        let pending = match &host_gui {
            None => self
                .wait_clipboard_read(&first.label, 1 + usize::from(!asked.is_empty()))?
                .map(|id| format!("запрос {id} в журнале GUI №1 без ответа")),
            Some(gui) => gui
                .wait_ask(2, WAIT)
                .ok()
                .map(|ask| format!("запрос хоста {ask:?} без ответа")),
        };
        evidence.note("pending", &pending);
        // Соединение роли GUI стоит за GUI №1 и гибнет вместе с ней.
        drop(host_gui.take());
        self.kill(&mut first)?;
        self.wait_clients(&session.id, 0)?;
        let second = self.open(&session.id, Lead::IfFree, COLS, ROWS)?;
        self.wait_clients(&session.id, 1)?;
        pull_until(&mut session.link, CLIPBOARD_SETTLE, |log| {
            bridge::count(&bridge::responses_since(log, mark), ResponseKind::Clipboard) > 0
        })?;
        if !barrier(&mut session.link, b"")? {
            evidence.not_proven = Some("барьер GUI №2 не пришёл".to_owned());
        }
        let after = bridge::responses_since(session.link.log(), mark);
        let payloads = clipboard_payloads(&after);
        evidence.check(
            "после SIGKILL: не больше одного ответа и без данных",
            payloads.len() <= 1
                && payloads
                    .iter()
                    .all(|payload| payload.as_deref().is_none_or(str::is_empty)),
            format!("{after:?}"),
        );
        let replayed = self.clipboard_reads(&second.label)?;
        evidence.check(
            "после SIGKILL: GUI №2 запроса не получила",
            replayed.is_empty(),
            format!("ClipboardRead в журнале GUI №2: {replayed:?}"),
        );
        let record = serde_json::json!({
            "session": session.id.as_str(),
            "live": { "asked": asked, "responses": live },
            "after_kill": { "pending": pending, "responses": after, "gui2_reads": replayed },
            "gui1": first.label,
            "gui2": second.label,
        });
        let journal = format!("{}/{}", first.label, bridge::EFFECTS_FILE);
        second.quit();
        let artifact = self.artifact("bridge-d4.json", &record)?;
        Ok(evidence::cell(
            Spec {
                id: "D4",
                part: Some(bridge::SURFACE_PART),
                stimulus: format!(
                    "{}: OSC 52 чтения при живой GUI с асинхронным ответом, затем чтение без \
                     ответа и SIGKILL GUI №1, новая GUI №2",
                    self.candidate
                ),
                oracle: "счётчик ответов фикстуры; журналы эффектов GUI (read_clipboard_cb)",
                completion: "барьер CSI 5 n; после SIGKILL — ответ или срок 3 с и барьер",
                origin: DefectOrigin::Host,
            },
            &evidence,
            vec![artifact, journal],
        ))
    }

    /// D5 в поверхности: отчёты фокуса и размера при включении, фокусе, ресайзе окна и при
    /// новой GUI после `SIGKILL` прежней.
    fn d5(&mut self) -> Result<Cell, String> {
        const R_COLS: u16 = 90;
        const R_ROWS: u16 = 28;
        let mut session = self.ensure("d5", Mode::Emitter)?;
        let mut evidence = Evidence::default();
        let mut first = self.open(&session.id, Lead::IfFree, COLS, ROWS)?;
        self.wait_clients(&session.id, 1)?;
        if !barrier(&mut session.link, b"")? {
            return Err("предусловие D5: GUI №1 не ответила на барьер".to_owned());
        }
        let mut host_gui = self.host_gui(&session.id)?;
        let mut stages = serde_json::Map::new();
        let mark = session.link.log().len();
        if !barrier(&mut session.link, b"\x1b[?1004h\x1b[?2048h")? {
            return Err("предусловие D5: барьер после включения не пришёл".to_owned());
        }
        let got = bridge::responses_since(session.link.log(), mark);
        evidence.check(
            "включение 2048: один отчёт размера с сеткой окна",
            sizes(&got).len() == 1
                && sizes(&got)
                    .iter()
                    .all(|raw| bridge::is_size_report(raw, COLS, ROWS)),
            format!("{got:?}"),
        );
        stages.insert("enable".to_owned(), serde_json::json!(got));
        for (focused, kind, name) in [
            (false, ResponseKind::FocusOut, "focus-out"),
            (true, ResponseKind::FocusIn, "focus-in"),
        ] {
            let mark = session.link.log().len();
            first.command(&GuiCommand::Focus {
                surface: 1,
                focused,
            })?;
            if let Some(gui) = &host_gui
                && let Some(reason) = gui
                    .focus(focused)
                    .map_err(|err| format!("фокус через GUI: {err}"))?
            {
                evidence.check(&format!("{name}: хост принял фокус GUI"), false, reason);
            }
            if !barrier(&mut session.link, b"")? {
                evidence.not_proven = Some(format!("{name}: барьер не пришёл"));
            }
            let got = bridge::responses_since(session.link.log(), mark);
            evidence.check(
                &format!("{name}: ровно один отчёт"),
                bridge::count(&got, kind) == 1 && got.len() == 1,
                format!("{got:?}"),
            );
            stages.insert(name.to_owned(), serde_json::json!(got));
        }
        let mark = session.link.log().len();
        first.command(&GuiCommand::Size {
            surface: 1,
            cols: R_COLS,
            rows: R_ROWS,
        })?;
        let resized = pull_until(&mut session.link, WAIT, |log| {
            bridge::winches_since(log, mark)
                .iter()
                .any(|size| (size.cols, size.rows) == (R_COLS, R_ROWS))
        })?;
        if !barrier(&mut session.link, b"")? {
            evidence.not_proven = Some("ресайз: барьер не пришёл".to_owned());
        }
        let got = bridge::responses_since(session.link.log(), mark);
        evidence.check(
            "ресайз окна: SIGWINCH и ровно один отчёт размера с новой сеткой",
            resized
                && sizes(&got).len() == 1
                && sizes(&got)
                    .iter()
                    .all(|raw| bridge::is_size_report(raw, R_COLS, R_ROWS)),
            format!("SIGWINCH {R_COLS}×{R_ROWS}: {resized}; {got:?}"),
        );
        stages.insert("resize".to_owned(), serde_json::json!(got));
        let mark = session.link.log().len();
        drop(host_gui.take());
        self.kill(&mut first)?;
        self.wait_clients(&session.id, 0)?;
        let second = self.open(&session.id, Lead::IfFree, COLS, ROWS)?;
        self.wait_clients(&session.id, 1)?;
        host_gui = self.host_gui(&session.id)?;
        if let Some(gui) = &host_gui
            && let Some(reason) = gui
                .focus(true)
                .map_err(|err| format!("фокус через GUI №2: {err}"))?
        {
            evidence.check("reconnect: хост принял фокус GUI №2", false, reason);
        }
        if !barrier(&mut session.link, b"")? {
            evidence.not_proven = Some("reconnect: барьер GUI №2 не пришёл".to_owned());
        }
        let got = bridge::responses_since(session.link.log(), mark);
        evidence.check(
            "новая GUI: один FocusIn, FocusOut не больше одного",
            bridge::count(&got, ResponseKind::FocusIn) == 1
                && bridge::count(&got, ResponseKind::FocusOut) <= 1,
            format!("{got:?}"),
        );
        evidence.check(
            "новая GUI: не больше одного отчёта размера и с её сеткой",
            sizes(&got).len() <= 1
                && sizes(&got)
                    .iter()
                    .all(|raw| bridge::is_size_report(raw, COLS, ROWS)),
            format!("{:?}", sizes(&got)),
        );
        stages.insert("reconnect".to_owned(), serde_json::json!(got));
        drop(host_gui);
        let record = serde_json::json!({
            "session": session.id.as_str(),
            "stages": stages,
            "winches": bridge::winches_since(session.link.log(), 0),
        });
        second.quit();
        let artifact = self.artifact("bridge-d5.json", &record)?;
        Ok(evidence::cell(
            Spec {
                id: "D5",
                part: Some(bridge::SURFACE_PART),
                stimulus: format!(
                    "{}: включение 1004 и 2048, фокус окна, ресайз окна {R_COLS}×{R_ROWS}, \
                     SIGKILL GUI №1 и новая GUI №2; у Rust-хоста фокус идёт соединением GUI \
                     поколения лидера",
                    self.candidate
                ),
                oracle: "счётчик ответов фикстуры",
                completion: "барьер CSI 5 n после каждого стимула",
                origin: DefectOrigin::Host,
            },
            &evidence,
            vec![artifact],
        ))
    }

    /// F2: строки, колонки и пиксели у нагрузки (`TIOCGWINSZ` по квитанции `SIGWINCH`) против
    /// `ghostty_surface_size`, масштаб содержимого против окна и смена масштаба.
    fn f2(&mut self) -> Result<Cell, String> {
        const F2_COLS: u16 = 100;
        const F2_ROWS: u16 = 30;
        let mut session = self.ensure("f2", Mode::Emitter)?;
        let mut evidence = Evidence::default();
        let mut gui = self.open(&session.id, Lead::IfFree, F2_COLS, F2_ROWS)?;
        let reply = gui.command(&GuiCommand::Size {
            surface: 1,
            cols: F2_COLS,
            rows: F2_ROWS,
        })?;
        let grid = reply.grid.ok_or("F2: нет сетки поверхности")?;
        let scale = reply.scale.ok_or("F2: нет масштаба поверхности")?;
        if (grid.cols, grid.rows) != (F2_COLS, F2_ROWS) {
            return Err(format!("предусловие F2: сетка поверхности {grid:?}"));
        }
        let size = wait_winch(&mut session.link, 0, |size| {
            (size.cols, size.rows) == (F2_COLS, F2_ROWS)
        })?;
        evidence.check(
            "cells",
            size.is_some(),
            format!("SIGWINCH нагрузки с {F2_COLS}×{F2_ROWS}: {size:?}"),
        );
        let pixels = size.map(|size| (u32::from(size.xpixel), u32::from(size.ypixel)));
        evidence.check(
            "pixels",
            pixels == Some((grid.width_px, grid.height_px)),
            format!(
                "пиксели нагрузки {pixels:?}, поверхности {}×{} (клетка {}×{})",
                grid.width_px, grid.height_px, grid.cell_width_px, grid.cell_height_px
            ),
        );
        evidence.check(
            "content-scale",
            (scale.content - scale.window).abs() < f64::EPSILON && scale.window > 0.0,
            format!(
                "масштаб содержимого {} при backingScaleFactor окна {}",
                scale.content, scale.window
            ),
        );
        let target = if scale.window >= 2.0 { 1.0 } else { 2.0 };
        let mark = session.link.log().len();
        gui.command(&GuiCommand::Scale {
            surface: 1,
            percent: (target * 100.0_f64).round() as u32,
        })?;
        let rescaled = wait_winch(&mut session.link, mark, |size| {
            (size.cols, size.rows) != (F2_COLS, F2_ROWS)
        })?;
        let after = gui.command(&GuiCommand::Grid { surface: 1 })?;
        let grid2 = after.grid.ok_or("F2: нет сетки после смены масштаба")?;
        evidence.check(
            "rescaled-winsize",
            rescaled.is_some_and(|size| {
                (size.cols, size.rows) == (grid2.cols, grid2.rows)
                    && (u32::from(size.xpixel), u32::from(size.ypixel))
                        == (grid2.width_px, grid2.height_px)
            }),
            format!("масштаб {target}: SIGWINCH нагрузки {rescaled:?}, поверхность {grid2:?}"),
        );
        let factor = scale.content / target;
        let scaled =
            |small: u32, large: u32| (f64::from(small) * factor - f64::from(large)).abs() <= factor;
        evidence.check(
            "cell-follows-scale",
            scaled(grid2.cell_width_px, grid.cell_width_px)
                && scaled(grid2.cell_height_px, grid.cell_height_px),
            format!(
                "клетка {}×{} при масштабе {}, {}×{} при {target}",
                grid.cell_width_px,
                grid.cell_height_px,
                scale.content,
                grid2.cell_width_px,
                grid2.cell_height_px
            ),
        );
        gui.quit();
        let record = serde_json::json!({
            "session": session.id.as_str(),
            "grid": grid,
            "scale": scale,
            "winsize": size,
            "rescaled": { "target": target, "grid": grid2, "winsize": rescaled },
            "receipts": session.link.log(),
        });
        let artifact = self.artifact("bridge-f2.json", &record)?;
        Ok(evidence::cell(
            Spec {
                id: "F2",
                part: None,
                stimulus: format!(
                    "{}: поверхность {F2_COLS}×{F2_ROWS} через attach-клиента, затем масштаб \
                     содержимого {target}",
                    self.candidate
                ),
                oracle: "TIOCGWINSZ нагрузки против ghostty_surface_size; масштаб против \
                         backingScaleFactor окна",
                completion: "квитанция SIGWINCH с сеткой поверхности или срок",
                origin: DefectOrigin::Attach,
            },
            &evidence,
            vec![artifact],
        ))
    }

    fn teardown(&mut self) -> Vec<String> {
        let mut notes = Vec::new();
        for id in self.sessions.clone() {
            let Some(record) = self.backend.record(&id) else {
                continue;
            };
            let processes =
                backend::session_processes(&record).unwrap_or_else(|_| vec![record.host]);
            let _ = self.backend.stop(&id, STOP_GRACE);
            for key in backend::alive(&processes).unwrap_or_default() {
                let _ = signal(key.pid, libc::SIGKILL);
                notes.push(format!("уборка {id}: SIGKILL PID {}", key.pid));
            }
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
}

/// Команда поверхности: RIS стирает приветствие `login(1)` до attach, затем attach-клиент с
/// окружением кандидата, собранным с нуля, как у attach-клиента стенда.
fn surface_command(program: &Path, args: &[String], env: &[(String, String)]) -> String {
    let mut inner = vec!["/usr/bin/env".to_owned(), "-i".to_owned()];
    inner.extend(env.iter().map(|(name, value)| format!("{name}={value}")));
    inner.push(program.display().to_string());
    inner.extend(args.iter().cloned());
    let attach = report_policy::shell_command(&inner[0], &inner[1..]);
    report_policy::shell_command(
        "/bin/sh",
        &["-c".to_owned(), format!("printf '\\033c'; exec {attach}")],
    )
}

fn request(link: &mut Link, command: Command) -> Result<(), String> {
    let id = link
        .send(command)
        .map_err(|err| format!("команда фикстуре: {err}"))?;
    let error = link
        .wait_for(WAIT, |receipt| match &receipt.event {
            Event::Ack {
                id: acked, error, ..
            } if *acked == id => Some(error.clone()),
            _ => None,
        })
        .map_err(|err| format!("квитанция фикстуры: {err}"))?;
    match error {
        None => Ok(()),
        Some(error) => Err(format!("фикстура отказала: {error}")),
    }
}

/// Квитанции, пока `done` над журналом не выполнено; `false` — срок вышел.
fn pull_until(
    link: &mut Link,
    timeout: Duration,
    mut done: impl FnMut(&[Receipt]) -> bool,
) -> Result<bool, String> {
    let deadline = Instant::now() + timeout;
    loop {
        if done(link.log()) {
            return Ok(true);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(false);
        }
        match link.wait_for(left.min(POLL), |_| None::<()>) {
            Ok(()) | Err(LinkError::Timeout) => {}
            Err(err) => return Err(format!("квитанции фикстуры: {err}")),
        }
    }
}

/// Фикстура пишет `bytes` и барьер одной записью; `true` — поверхность ответила на барьер.
fn barrier(link: &mut Link, bytes: &[u8]) -> Result<bool, String> {
    let before = bridge::barrier_replies(link.log());
    let mut out = bytes.to_vec();
    out.extend_from_slice(BARRIER);
    request(link, Command::Write { hex: hex(&out) })?;
    pull_until(link, WAIT, |log| bridge::barrier_replies(log) > before)
}

/// Позиция во вводе фикстуры: прочитанные байты и ответы на барьеры стенда до неё.
#[derive(Clone, Copy, Debug, Default, Serialize)]
struct Mark {
    bytes: usize,
    barriers: usize,
}

impl Mark {
    fn at(log: &[Receipt]) -> Mark {
        Mark {
            bytes: bridge::input_since(log, 0).len(),
            barriers: bridge::barrier_replies(log),
        }
    }
}

/// Маркер-вставка через поверхность и байты от неё до маркера после `consumed` без ответов на
/// барьеры стенда за это время; `None` — маркер не дошёл до фикстуры за срок.
fn fence(
    window: &mut Window,
    link: &mut Link,
    consumed: &mut Mark,
    marker: &str,
) -> Result<Option<Vec<u8>>, String> {
    window.paste(marker)?;
    let mut found = None;
    let start = *consumed;
    pull_until(link, WAIT, |log| {
        let input = bridge::input_since(log, 0);
        found = input
            .get(start.bytes..)
            .and_then(|rest| report_policy::split_at_marker(rest, marker));
        found.is_some()
    })?;
    Ok(found.map(|fence| {
        let barriers = bridge::barrier_replies(link.log());
        let replies = vec![BARRIER_REPLY.to_vec(); barriers.saturating_sub(start.barriers)];
        *consumed = Mark {
            bytes: start.bytes + fence.end,
            barriers,
        };
        report_policy::unexpected(&fence.before, &replies)
    }))
}

/// Пробы ввода в порядке эталона: «a», «вверх», щелчок, вставка; затем маркер-граница.
fn probe(
    window: &mut Window,
    link: &mut Link,
    consumed: &mut Mark,
    marker: &str,
) -> Result<Option<Vec<u8>>, String> {
    window.command(&GuiCommand::Key {
        surface: 1,
        keycode: KEYCODE_A,
        text: Some("a".to_owned()),
    })?;
    window.command(&GuiCommand::Key {
        surface: 1,
        keycode: KEYCODE_UP,
        text: None,
    })?;
    window.command(&GuiCommand::Click {
        surface: 1,
        col: CLICK.0,
        row: CLICK.1,
    })?;
    window.paste(PASTE_TEXT)?;
    fence(window, link, consumed, marker)
}

fn probes_of(step: &Step) -> Result<Vec<u8>, String> {
    step.probes
        .as_ref()
        .and_then(bridge::Probes::bytes)
        .ok_or_else(|| format!("предусловие: у шага {} нет проб", step.name))
}

/// Запросы профиля D1 и барьер; `None` — барьер не пришёл.
fn d1_round(link: &mut Link) -> Result<Option<Vec<(ResponseKind, String)>>, String> {
    let mark = link.log().len();
    for (_, query) in D1_QUERIES {
        request(link, Command::Query { query, split: None })?;
    }
    if !barrier(link, b"")? {
        return Ok(None);
    }
    Ok(Some(bridge::responses_since(link.log(), mark)))
}

fn clipboard_payloads(responses: &[(ResponseKind, String)]) -> Vec<Option<String>> {
    responses
        .iter()
        .filter(|(kind, _)| *kind == ResponseKind::Clipboard)
        .map(|(_, raw)| osc52_payload(raw))
        .collect()
}

fn sizes(responses: &[(ResponseKind, String)]) -> Vec<&String> {
    responses
        .iter()
        .filter(|(kind, _)| *kind == ResponseKind::SizeReport)
        .map(|(_, raw)| raw)
        .collect()
}

/// Первая квитанция `SIGWINCH` после `mark`, размер которой удовлетворяет `want`.
fn wait_winch(
    link: &mut Link,
    mark: usize,
    want: impl Fn(&Winsize) -> bool,
) -> Result<Option<Winsize>, String> {
    pull_until(link, WAIT, |log| {
        bridge::winches_since(log, mark).iter().any(&want)
    })?;
    Ok(bridge::winches_since(link.log(), mark)
        .into_iter()
        .find(|size| want(size)))
}

/// Строки вида, когда `done` выполнено; `false` — не дождались за срок (вывод клетки).
fn wait_lines(
    window: &mut Window,
    done: impl Fn(&Vec<String>) -> bool,
) -> Result<(bool, Vec<String>), String> {
    let deadline = Instant::now() + WAIT;
    loop {
        let lines = window.lines()?;
        if done(&lines) {
            return Ok((true, lines));
        }
        if Instant::now() >= deadline {
            return Ok((false, lines));
        }
        std::thread::park_timeout(POLL);
    }
}

fn required_dir(value: Option<&String>, name: &str) -> Result<PathBuf, String> {
    let dir = value
        .map(PathBuf::from)
        .ok_or_else(|| format!("предусловие: нет {name}"))?;
    if dir.is_dir() {
        Ok(dir)
    } else {
        Err(format!(
            "предусловие: {name}={}: каталога нет",
            dir.display()
        ))
    }
}

fn required_file(path: &Path) -> Result<(), String> {
    if path.is_file() {
        Ok(())
    } else {
        Err(format!("предусловие: нет {}", path.display()))
    }
}
