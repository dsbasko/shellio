//! Сценарий `bridge` в поверхности (план P0.4, задача 15). Процесс сценария — водитель без
//! поверхности; обе GUI — дочерние процессы `probe-session-surface --gui`, первую стенд снимает
//! настоящим `SIGKILL`, вторая открывает поверхность к той же сессии. Клетки и части —
//! `matrix::bridge`.

use std::fs;
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as Process, Stdio};
use std::time::{Duration, Instant};

use crate::args::Target;
use crate::backend::rust::{self as rust_host, RustHost};
use crate::backend::zmx::{self, ZmxSet};
use crate::backend::{self as backend, Backend, HostRecord, Lead, SessionId, Workload, signal};
use crate::fixture::channel::{Command, Event, Link, Listener, hex};
use crate::fixture::{self, Mode};
use crate::host::HOST_NAME;
use crate::host::attach::ATTACH_NAME;
use crate::launch;
use crate::matrix::bridge::{self, GuiCommand, Hello, Reply};
use crate::matrix::evidence::{self, Evidence, Spec};
use crate::matrix::{Candidate, report_policy};
use crate::pty::Winsize;
use crate::results::{Cell, DefectOrigin, Environment, Report};
use crate::surface::gui::CONFIG_FILE;

const COLS: u16 = 80;
const ROWS: u16 = 24;
const WAIT: Duration = Duration::from_secs(15);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const STOP_GRACE: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(5);

pub fn run(target: &Target, environment: &Environment) -> Result<Report, String> {
    let candidate = target
        .backend
        .ok_or("предусловие: сценарий bridge идёт на кандидате (--backend)")?;
    let mut driver = Driver::new(&target.out, environment, candidate)?;
    let outcome = driver.cells();
    let notes = driver.teardown();
    let mut report = Report {
        cells: outcome?,
        ..Report::default()
    };
    report.diagnostics.notes.extend(notes);
    Ok(report)
}

/// Водитель: кандидат, сокет квитанций фикстуры и сокет управления GUI.
struct Driver {
    nonce: String,
    out: PathBuf,
    root: PathBuf,
    candidate: Candidate,
    backend: Box<dyn Backend + Sync>,
    fixture: PathBuf,
    program: PathBuf,
    listener: Option<Listener>,
    control: UnixListener,
    size: Winsize,
    sessions: Vec<SessionId>,
    guis: u32,
}

struct Gui {
    label: String,
    child: Child,
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    hello: Hello,
}

impl Gui {
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

    fn text(&mut self) -> Result<String, String> {
        Ok(self
            .command(&GuiCommand::Text { surface: 1 })?
            .text
            .unwrap_or_default())
    }
}

struct Session {
    id: SessionId,
    record: HostRecord,
    link: Link,
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
        let backend: Box<dyn Backend + Sync> = match candidate {
            Candidate::Zmx => {
                let real_dir = out.join("zmx");
                fs::create_dir(&real_dir)
                    .map_err(|err| format!("{}: {err}", real_dir.display()))?;
                let link = root.join("z");
                std::os::unix::fs::symlink(&real_dir, &link)
                    .map_err(|err| format!("{}: {err}", link.display()))?;
                Box::new(zmx::Zmx::new(zmx::Config {
                    set: ZmxSet::Current,
                    binary,
                    dir: link,
                    cwd: root.clone(),
                    env,
                    token,
                    timeout: COMMAND_TIMEOUT,
                })?)
            }
            Candidate::Rust => {
                let dir = root.join("r");
                fs::create_dir(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
                Box::new(RustHost::new(rust_host::Config {
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
                })?)
            }
        };
        Ok(Driver {
            nonce,
            out: out.to_path_buf(),
            root,
            candidate,
            backend,
            fixture,
            program,
            listener: Some(listener),
            control,
            size: Winsize::cells(ROWS, COLS),
            sessions: Vec::new(),
            guis: 0,
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
        })
    }

    /// Новый GUI-процесс с поверхностью, команда которой — attach-клиент кандидата.
    fn gui(
        &mut self,
        session: &SessionId,
        lead: Lead,
        cols: u16,
        rows: u16,
    ) -> Result<Gui, String> {
        self.guis += 1;
        let label = format!("gui{}", self.guis);
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
        let stream = self.accept_gui(&label)?;
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
        let mut gui = Gui {
            label,
            child,
            reader,
            writer: stream,
            hello,
        };
        let attach = self
            .backend
            .attach_command(session, lead)
            .map_err(|err| format!("команда attach: {err}"))?;
        gui.command(&GuiCommand::Open {
            surface: 1,
            command: surface_command(&attach.program, &attach.args, &attach.env),
            cols,
            rows,
        })?;
        Ok(gui)
    }

    fn accept_gui(&self, label: &str) -> Result<UnixStream, String> {
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

    fn cells(&mut self) -> Result<Vec<Cell>, String> {
        let mut cells = vec![self.c1()?];
        cells.extend(
            bridge::pending("до задачи 15: клетка моста ещё не построена")
                .into_iter()
                .filter(|cell| cell.id != "C1"),
        );
        Ok(cells)
    }

    /// C1 на простой сцене: строки текста и маркер курсора после настоящего `SIGKILL` GUI.
    fn c1(&mut self) -> Result<Cell, String> {
        let mut session = self.ensure("c1", Mode::Emitter)?;
        let mut evidence = Evidence::default();
        let mut first = self.gui(&session.id, Lead::IfFree, COLS, ROWS)?;
        self.wait_clients(&session.id, 1)?;
        let lines: Vec<String> = (1..=5).map(|n| format!("bridge-c1-line-{n}")).collect();
        let scene = lines.join("\r\n");
        request(
            &mut session.link,
            Command::Write {
                hex: hex(scene.as_bytes()),
            },
        )?;
        let before = wait_text(&mut first, |text| text.contains(&lines[4]))?;
        evidence.check("gui1-saw-scene", before.is_some(), "GUI №1 показала сцену");
        signal(first.hello.pid, libc::SIGKILL)
            .map_err(|err| format!("SIGKILL {}: {err}", first.label))?;
        let status = backend::wait_child(&mut first.child, WAIT)
            .map_err(|err| format!("{}: {err}", first.label))?;
        evidence.check(
            "gui1-killed",
            status.signal() == Some(libc::SIGKILL),
            format!("GUI №1 завершилась {status:?}"),
        );
        self.wait_clients(&session.id, 0)?;
        let alive = backend::alive(&[session.record.workload, session.record.host])
            .map_err(|err| format!("{err}"))?;
        evidence.check(
            "workload-and-host-alive",
            alive.len() == 2,
            format!("живы после SIGKILL GUI: {alive:?}"),
        );
        let mut second = self.gui(&session.id, Lead::IfFree, COLS, ROWS)?;
        self.wait_clients(&session.id, 1)?;
        let restored = wait_text(&mut second, |text| {
            lines.iter().all(|line| text.contains(line))
        })?;
        evidence.check(
            "restored-text",
            restored.is_some(),
            format!(
                "текст GUI №2: {:?}",
                restored.as_deref().map(report_policy::text_rows)
            ),
        );
        request(&mut session.link, Command::Write { hex: hex(b"@") })?;
        let marker = format!("{}@", lines[4]);
        let cursor = wait_text(&mut second, |text| text.contains(&marker))?;
        evidence.check(
            "cursor-marker",
            cursor.is_some(),
            "маркер «@» после последней строки: курсор восстановлен",
        );
        let _ = second.command(&GuiCommand::Quit);
        let _ = backend::wait_child(&mut second.child, WAIT);
        Ok(evidence::cell(
            Spec {
                id: "C1",
                part: None,
                stimulus: format!(
                    "{}: пять строк сцены через GUI №1, SIGKILL GUI №1, новая GUI №2, маркер «@»",
                    self.candidate
                ),
                oracle: "ghostty_surface_read_text, фикстура без перерисовки",
                completion: "строки сцены и маркер в read_text GUI №2",
                origin: DefectOrigin::Host,
            },
            &evidence,
            Vec::new(),
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

/// Текст вида, когда `done` выполнено; `None` — не дождались за срок (вывод клетки, а не ошибка).
fn wait_text(gui: &mut Gui, done: impl Fn(&str) -> bool) -> Result<Option<String>, String> {
    let deadline = Instant::now() + WAIT;
    loop {
        let text = gui.text()?;
        if done(&text) {
            return Ok(Some(text));
        }
        if Instant::now() >= deadline {
            return Ok(None);
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
