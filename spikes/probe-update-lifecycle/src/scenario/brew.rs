//! Сценарий `brew` (задача 15): клетка-наблюдение G «Карантин через brew». Вопрос — снимает ли
//! поддерживаемый путь установки, cask с `preflight_steps`, карантин так, что бандл запускается.
//!
//! Стенд:
//! - изолированный Homebrew заводится заново в каждом прогоне в `target/probes/update-lifecycle/
//!   brew/prefix/`: локальный `git clone --no-hardlinks` из `/opt/homebrew` только по локальному
//!   пути, копия `vendor/portable-ruby/<версия>` через `cp -cR`; удаляется в конце сценария;
//! - окружение `brew` собирается для каждого его запуска: `HOMEBREW_NO_AUTO_UPDATE`,
//!   `HOMEBREW_NO_INSTALL_FROM_API`, `HOMEBREW_NO_ANALYTICS`, `HOMEBREW_NO_ENV_HINTS`,
//!   `HOMEBREW_CACHE`, `HOMEBREW_LOGS`, `HOMEBREW_TEMP` и `XDG_CONFIG_HOME` в `<state>/brew/`,
//!   системный `PATH`. Процесс сценария переменных `HOMEBREW_*` не получает;
//! - локальный tap `shellio-probe/probe` с cask `probe-update` (`preflight_steps` снимают
//!   карантин) и контролем `probe-update-control` (без шагов) из шаблонов `brew/Casks/`, архив
//!   `file://` копии поколения 1, `--appdir` во временных каталогах.
//!
//! Части — наблюдения клетки G: `staged-quarantine`, `installed-clean`, `launch`, `control`.
//! Ошибка исполнения — только сломанный стенд: шаг не вышел в срок, шаг `preflight_steps` не
//! записал наблюдение, бандл не там, где сказал `brew`, изменился настоящий Homebrew, окно
//! Gatekeeper осталось на экране. Отказ изоляции или установки — `not_run` частей с выводом `brew`.

use std::fs::{self, File};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Value, json};

use crate::brew::{self, Gatekeeper, Install, RealBrew, Scan};
use crate::generation::{self, APP_BIN, APP_NAME, BundleEntry, Layout, Run, StubReport};
use crate::keychain::sha256_hex;
use crate::kq::{Event, Queue};
use crate::launch;
use crate::results::{Cell, Diagnostics, Environment, ProcessRecord, Report, Requirement, Status};
use crate::spawn;
use crate::window::{self, AxDialog};

use super::reject::install;
use super::smoke::{WindowEntry, window_list};
use super::{PartSpec, bundles, stand_inputs};

pub const DEADLINE: Duration = Duration::from_secs(600);
pub const CELL: &str = "G";
pub const DIR: &str = "brew";
pub const STEPS_ARTIFACT: &str = "brew/steps.json";
pub const REAL_ARTIFACT: &str = "brew/real-homebrew.json";
pub const REAL_PREFIX: &str = "/opt/homebrew";
/// Срок одного вызова `brew`, `git` и прочих команд стенда.
pub const COMMAND_DEADLINE: Duration = Duration::from_secs(180);
/// Срок `open -W` бандла без карантина.
pub const OPEN_DEADLINE: Duration = Duration::from_secs(30);
/// Ожидание `open -W` контроля до поиска окна Gatekeeper.
pub const GATEKEEPER_WAIT: Duration = Duration::from_secs(10);
/// Ожидание выхода `open -W` и процесса бандла после «Готово» или `SIGKILL`.
pub const CLOSE_WAIT: Duration = Duration::from_secs(10);
/// Срок `log show`.
pub const LOG_DEADLINE: Duration = Duration::from_secs(60);
/// Владелец окна Gatekeeper.
pub const DIALOG_OWNER: &str = "CoreServicesUIAgent";
const GIT: &str = "/usr/bin/git";
const OPEN: &str = "/usr/bin/open";
const XATTR: &str = "/usr/bin/xattr";
const DITTO: &str = "/usr/bin/ditto";
const CP: &str = "/bin/cp";
const LOG: &str = "/usr/bin/log";
const SPCTL: &str = "/usr/sbin/spctl";
const SYSTEM_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";
const EXECUTABLE: &str = "Contents/MacOS/probe-update-app";
const ZIP_NAME: &str = "probe-update.zip";
const COMPLETION: &str = "каждый вызов brew и git в срок 180 с; open без карантина в срок 30 с; open контроля ждётся 10 с, затем окно Gatekeeper по списку окон, AXPress «Готово» и выход в срок 10 с";
/// Окружение процесса сценария, которое получает `brew`.
const BREW_INHERITED: [&str; 5] = ["HOME", "USER", "LOGNAME", "TMPDIR", "LANG"];

pub const STAGED: PartSpec = PartSpec {
    name: "staged-quarantine",
    stimulus: "brew install --cask probe-update из локального tap изолированного префикса: шаг preflight_steps записывает xattr -lr staged-копии в Caskroom до снятия карантина",
    oracle: "в записи шага до снятия com.apple.quarantine есть на корне бандла и на Contents/MacOS/probe-update-app",
};
pub const INSTALLED: PartSpec = PartSpec {
    name: "installed-clean",
    stimulus: "xattr -lr установленного бандла в --appdir после brew install; запись шага после xattr -dr",
    oracle: "com.apple.quarantine нет ни на одном файле установленного бандла и в записи шага после снятия; в --appdir бандл поколения 1 (CDHash из bundles.json)",
};
pub const LAUNCH: PartSpec = PartSpec {
    name: "launch",
    stimulus: "open -n -W установленного бандла с --args --report --tag <метка>",
    oracle: "open вышел в срок, квитанция probe-update-app поколения 1 с исполняемым файлом установленного бандла, окна Gatekeeper нет",
};
pub const CONTROL: PartSpec = PartSpec {
    name: "control",
    stimulus: "brew install --cask probe-update-control (тот же архив без снятия), open -n -W контроля с --report; окно Gatekeeper закрывается AXPress кнопки «Готово»",
    oracle: "карантин на корне контроля, квитанции нет, окно CoreServicesUIAgent закрыто «Готово» стендом, контроль остался в --appdir, syspolicyd: Prompt shown или denial breadcrumb с идентификатором пробы и Terminating process due to Gatekeeper rejection с PID процесса контроля",
};

fn cell(spec: &PartSpec, status: Status, reason: String, artifacts: &[String]) -> Cell {
    Cell {
        id: CELL.to_owned(),
        part: Some(spec.name.to_owned()),
        requirement: Requirement::Observation,
        status,
        stimulus: spec.stimulus.to_owned(),
        oracle: spec.oracle.to_owned(),
        completion: COMPLETION.to_owned(),
        reason,
        artifacts: artifacts.to_vec(),
    }
}

// --- окна Gatekeeper ---

pub fn dialogs(windows: &[WindowEntry]) -> Vec<WindowEntry> {
    windows
        .iter()
        .filter(|window| window.owner == DIALOG_OWNER)
        .cloned()
        .collect()
}

fn dialogs_now() -> Vec<WindowEntry> {
    dialogs(&window_list())
}

fn describe_windows(windows: &[WindowEntry]) -> String {
    windows
        .iter()
        .map(|window| format!("{} pid {} окно {}", window.owner, window.pid, window.number))
        .collect::<Vec<_>>()
        .join(", ")
}

// --- запуск с ограниченным сроком ---

/// Запуск с выводом в файлы и ожиданием выхода до `deadline`; `None` — процесс ещё жив.
fn spawn_bounded(
    program: &Path,
    args: &[String],
    env: Option<&[(String, String)]>,
    files: (&Path, &Path),
    setsid: bool,
    deadline: Instant,
) -> Result<(i32, Option<spawn::Exit>), String> {
    let out = File::create(files.0).map_err(|err| format!("{}: {err}", files.0.display()))?;
    let err = File::create(files.1).map_err(|err| format!("{}: {err}", files.1.display()))?;
    let pid = spawn::spawn(spawn::Spec {
        program,
        args,
        env,
        stdio: [None, Some(OwnedFd::from(out)), Some(OwnedFd::from(err))],
        setsid,
    })
    .map_err(|err| format!("запуск {}: {err}", program.display()))?;
    let queue = Queue::new()?;
    if queue.watch_exit(pid)? {
        loop {
            match queue.wait(deadline)? {
                Event::Exited(exited) if exited == pid => break,
                Event::TimedOut => return Ok((pid, None)),
                _ => {}
            }
        }
    }
    let exit = spawn::wait(pid).map_err(|err| format!("waitpid {pid}: {err}"))?;
    Ok((pid, Some(exit)))
}

/// `SIGKILL` процессу или его группе и ожидание выхода.
fn kill_and_reap(pid: i32, group: bool) -> Option<spawn::Exit> {
    let target = if group { -pid } else { pid };
    // SAFETY: сигнал собственному потомку или его группе по PID, памяти вызов не касается.
    unsafe { libc::kill(target, libc::SIGKILL) };
    spawn::wait(pid).ok()
}

fn kill_tagged(tag: &str) -> Vec<ProcessRecord> {
    let found = launch::find_by_token(tag);
    for process in &found {
        // SAFETY: сигнал процессу стенда, найденному по метке в аргументах.
        unsafe { libc::kill(process.pid, libc::SIGKILL) };
    }
    let pids: Vec<i32> = found.iter().map(|process| process.pid).collect();
    let _ = launch::wait_exit(&pids, CLOSE_WAIT);
    found
}

// --- стенд ---

/// Запись вызова внешней команды; полный вывод — в файлах каталога процесса.
#[derive(Clone, Debug, Serialize)]
struct Command {
    command: Vec<String>,
    exit: String,
    elapsed_ms: u64,
    stdout: String,
    stderr: String,
}

struct Stand {
    out: PathBuf,
    /// `<state>/brew`: кэш, журналы, temp, конфигурация, архив и `--appdir`.
    work: PathBuf,
    /// `target/probes/update-lifecycle/brew`.
    root: PathBuf,
    prefix: PathBuf,
    nonce: String,
    base_env: Vec<(String, String)>,
    commands: Vec<Command>,
    facts: serde_json::Map<String, Value>,
}

impl Stand {
    fn fact(&mut self, key: &str, value: impl Serialize) {
        self.facts.insert(
            key.to_owned(),
            serde_json::to_value(value).unwrap_or(Value::Null),
        );
    }

    fn cmd_files(&self, name: &str) -> (PathBuf, PathBuf, String) {
        let stem = format!("{:02}-{name}", self.commands.len() + 1);
        let dir = self.out.join(DIR).join("cmd");
        (
            dir.join(format!("{stem}.stdout")),
            dir.join(format!("{stem}.stderr")),
            format!("{DIR}/cmd/{stem}"),
        )
    }

    /// Команда с ограниченным сроком: не вышла — ошибка исполнения стенда.
    fn exec(
        &mut self,
        name: &str,
        program: &Path,
        args: &[String],
        env: Option<&[(String, String)]>,
        group: bool,
    ) -> Result<Run, String> {
        let (stdout, stderr, stem) = self.cmd_files(name);
        let started = Instant::now();
        let (pid, exit) = spawn_bounded(
            program,
            args,
            env,
            (&stdout, &stderr),
            group,
            started + COMMAND_DEADLINE,
        )?;
        let mut command = vec![program.display().to_string()];
        command.extend(args.iter().cloned());
        let exit = match exit {
            Some(exit) => exit,
            None => {
                let exit = kill_and_reap(pid, group);
                self.commands.push(Command {
                    command: command.clone(),
                    exit: exit.map_or("не вышел".to_owned(), |exit| exit.to_string()),
                    elapsed_ms: started.elapsed().as_millis() as u64,
                    stdout: format!("{stem}.stdout"),
                    stderr: format!("{stem}.stderr"),
                });
                return Err(format!(
                    "стенд: {} не вышел за {} с",
                    command.join(" "),
                    COMMAND_DEADLINE.as_secs()
                ));
            }
        };
        self.commands.push(Command {
            command: command.clone(),
            exit: exit.to_string(),
            elapsed_ms: started.elapsed().as_millis() as u64,
            stdout: format!("{stem}.stdout"),
            stderr: format!("{stem}.stderr"),
        });
        let read = |path: &Path| {
            fs::read(path)
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .unwrap_or_default()
        };
        Ok(Run {
            command,
            code: exit.code,
            stdout: read(&stdout),
            stderr: read(&stderr),
        })
    }

    fn tool(&mut self, name: &str, program: &str, args: &[String]) -> Result<Run, String> {
        self.exec(name, Path::new(program), args, None, false)
    }

    fn brew_env(&self) -> Vec<(String, String)> {
        let mut env = self.base_env.clone();
        env.push(("PATH".to_owned(), SYSTEM_PATH.to_owned()));
        for (name, value) in [
            ("HOMEBREW_NO_AUTO_UPDATE", "1"),
            ("HOMEBREW_NO_INSTALL_FROM_API", "1"),
            ("HOMEBREW_NO_ANALYTICS", "1"),
            ("HOMEBREW_NO_ENV_HINTS", "1"),
        ] {
            env.push((name.to_owned(), value.to_owned()));
        }
        for (name, sub) in [
            ("HOMEBREW_CACHE", "cache"),
            ("HOMEBREW_LOGS", "logs"),
            ("HOMEBREW_TEMP", "temp"),
            ("XDG_CONFIG_HOME", "config"),
        ] {
            env.push((name.to_owned(), self.work.join(sub).display().to_string()));
        }
        env
    }

    /// `brew` изолированного префикса в своей сессии: по сроку снимается вся группа.
    fn brew(&mut self, name: &str, args: &[String]) -> Result<Run, String> {
        let program = self.prefix.join("bin/brew");
        let env = self.brew_env();
        self.exec(name, &program, args, Some(&env), true)
    }
}

fn strings(list: &[&str]) -> Vec<String> {
    list.iter().map(|item| (*item).to_owned()).collect()
}

fn path_arg(path: &Path) -> String {
    path.display().to_string()
}

// --- настоящий Homebrew ---

fn list_children(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn two_levels(dir: &Path) -> Vec<String> {
    let mut all = Vec::new();
    for first in list_children(dir) {
        let sub = dir.join(&first);
        if sub.is_dir() {
            all.extend(
                list_children(&sub)
                    .into_iter()
                    .map(|second| format!("{first}/{second}")),
            );
        } else {
            all.push(first);
        }
    }
    all
}

/// HEAD и `git status` без необязательных блокировок, списки tap и Caskroom чтением каталогов,
/// SHA-256 `~/.homebrew/trust.json`. Настоящий `brew` не вызывается.
fn real_snapshot(stand: &mut Stand, home: &Path, when: &'static str) -> Result<RealBrew, String> {
    let real = Path::new(REAL_PREFIX);
    let (head, status) = if real.join(".git").exists() {
        let git = |sub: &[&str]| {
            let mut args = strings(&["-C", REAL_PREFIX, "--no-optional-locks"]);
            args.extend(strings(sub));
            args
        };
        let head = stand.tool("real-head", GIT, &git(&["rev-parse", "HEAD"]))?;
        let status = stand.tool("real-status", GIT, &git(&["status", "--porcelain"]))?;
        if status.code != Some(0) {
            return Err(format!(
                "стенд: git status {REAL_PREFIX}: код {:?}; {}",
                status.code,
                status.stderr.trim()
            ));
        }
        (
            (head.code == Some(0)).then(|| head.stdout.trim().to_owned()),
            status.stdout.lines().map(str::to_owned).collect(),
        )
    } else {
        (None, Vec::new())
    };
    let trust = home.join(".homebrew/trust.json");
    Ok(RealBrew {
        when,
        head,
        status,
        taps: two_levels(&real.join("Library/Taps")),
        caskroom: two_levels(&real.join("Caskroom")),
        trust_sha256: fs::read(&trust).ok().and_then(|bytes| sha256_hex(&bytes)),
    })
}

// --- изоляция ---

#[derive(Clone, Debug, Default, Serialize)]
struct Isolation {
    ok: bool,
    reason: Option<String>,
    prefix: String,
    version: Option<String>,
    portable_ruby: Option<String>,
    elapsed_ms: u64,
}

fn failed(run: &Run) -> String {
    let text = format!("{}{}", run.stderr, run.stdout);
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    format!(
        "{}: код {:?}; {}",
        run.command.join(" "),
        run.code,
        lines[lines.len().saturating_sub(3)..].join(" | ")
    )
}

/// Ранняя проверка изоляции по порядку раздела «Brew» плана: префикс из локального клона и
/// копии portable-ruby, затем `brew --version`, `--prefix` и `--repository`.
fn isolate(stand: &mut Stand) -> Result<Isolation, String> {
    let started = Instant::now();
    let mut isolation = Isolation {
        prefix: path_arg(&stand.prefix),
        ..Isolation::default()
    };
    let done = |mut isolation: Isolation, reason: Option<String>| {
        isolation.ok = reason.is_none();
        isolation.reason = reason;
        isolation.elapsed_ms = started.elapsed().as_millis() as u64;
        Ok(isolation)
    };
    let real = Path::new(REAL_PREFIX);
    if !real.join(".git").is_dir() {
        return done(
            isolation,
            Some(format!("нет git-репозитория {REAL_PREFIX}")),
        );
    }
    if stand.root.exists() {
        fs::remove_dir_all(&stand.root)
            .map_err(|err| format!("стенд: {}: {err}", stand.root.display()))?;
    }
    fs::create_dir_all(&stand.root).map_err(|err| format!("{}: {err}", stand.root.display()))?;
    let clone = stand.tool(
        "clone",
        GIT,
        &[
            strings(&[
                "-c",
                "protocol.allow=never",
                "-c",
                "protocol.file.allow=always",
                "clone",
                "--quiet",
                "--no-hardlinks",
                REAL_PREFIX,
            ]),
            vec![path_arg(&stand.prefix)],
        ]
        .concat(),
    )?;
    if clone.code != Some(0) {
        return done(isolation, Some(failed(&clone)));
    }
    let vendor = "Library/Homebrew/vendor";
    let version = fs::read_to_string(stand.prefix.join(vendor).join("portable-ruby-version"))
        .map(|text| text.trim().to_owned())
        .unwrap_or_default();
    let source = real.join(vendor).join("portable-ruby").join(&version);
    if version.is_empty() || !source.is_dir() {
        return done(
            isolation,
            Some(format!(
                "нет portable-ruby «{version}» в {}",
                real.join(vendor).display()
            )),
        );
    }
    isolation.portable_ruby = Some(version.clone());
    let dest = stand.prefix.join(vendor).join("portable-ruby");
    fs::create_dir_all(&dest).map_err(|err| format!("{}: {err}", dest.display()))?;
    let copy = stand.tool(
        "portable-ruby",
        CP,
        &[
            "-cR".to_owned(),
            path_arg(&source),
            format!("{}/", dest.display()),
        ],
    )?;
    if copy.code != Some(0) {
        return done(isolation, Some(failed(&copy)));
    }
    std::os::unix::fs::symlink(&version, dest.join("current"))
        .map_err(|err| format!("стенд: ссылка portable-ruby/current: {err}"))?;
    let run = stand.brew("brew-version", &strings(&["--version"]))?;
    isolation.version = brew::parse_version(&run.stdout);
    if run.code != Some(0) || isolation.version.is_none() {
        return done(isolation, Some(failed(&run)));
    }
    let canonical = fs::canonicalize(&stand.prefix).unwrap_or_else(|_| stand.prefix.clone());
    for flag in ["--prefix", "--repository"] {
        let run = stand.brew(&format!("brew{flag}"), &strings(&[flag]))?;
        let named = fs::canonicalize(run.stdout.trim()).ok();
        if run.code != Some(0) || named.as_ref() != Some(&canonical) {
            return done(
                isolation,
                Some(format!(
                    "brew {flag} назвал «{}», а не изолированный префикс: {}",
                    run.stdout.trim(),
                    failed(&run)
                )),
            );
        }
    }
    done(isolation, None)
}

// --- архив, tap и установка ---

fn prepare_casks(stand: &mut Stand, gen1: &BundleEntry) -> Result<(), String> {
    let source = stand.work.join("src").join(APP_NAME);
    install(&gen1.app, &source)?;
    let zip = stand.work.join("zip").join(ZIP_NAME);
    let packed = stand.tool(
        "zip",
        DITTO,
        &[
            strings(&[
                "-c",
                "-k",
                "--norsrc",
                "--noextattr",
                "--noacl",
                "--keepParent",
            ]),
            vec![path_arg(&source), path_arg(&zip)],
        ]
        .concat(),
    )?;
    if packed.code != Some(0) {
        return Err(format!("стенд: {}", failed(&packed)));
    }
    let attrs = stand.tool("zip-xattr", XATTR, &["-l".to_owned(), path_arg(&zip)])?;
    if brew::parse_xattr(&attrs.stdout)
        .iter()
        .any(|attr| attr.name == brew::QUARANTINE)
    {
        return Err(format!(
            "стенд: карантин на архиве {} до brew",
            zip.display()
        ));
    }
    let bytes = fs::read(&zip).map_err(|err| format!("{}: {err}", zip.display()))?;
    let sha256 = sha256_hex(&bytes).ok_or("стенд: SHA-256 архива не посчитан")?;
    let url = format!("file://{}", zip.display());
    if url.contains(' ') {
        return Err(format!("стенд: пробел в пути архива {url}"));
    }
    let casks = stand.prefix.join(brew::TAP_DIR).join("Casks");
    fs::create_dir_all(&casks).map_err(|err| format!("{}: {err}", casks.display()))?;
    let copies = stand.out.join(DIR).join("casks");
    for (token, template) in [
        (brew::CASK, brew::CASK_TEMPLATE),
        (brew::CONTROL_CASK, brew::CONTROL_TEMPLATE),
    ] {
        let text =
            brew::fill_cask(template, &sha256, &url).map_err(|err| format!("стенд: {err}"))?;
        for dir in [&casks, &copies] {
            let path = dir.join(format!("{token}.rb"));
            fs::write(&path, &text).map_err(|err| format!("{}: {err}", path.display()))?;
        }
    }
    stand.fact(
        "archive",
        json!({ "zip": path_arg(&zip), "sha256": sha256, "bytes": bytes.len(), "url": url }),
    );
    Ok(())
}

/// Установка cask; `Err` — сломанный стенд, отказ `brew` — причина во втором поле.
fn install_cask(
    stand: &mut Stand,
    token: &str,
    appdir: &Path,
) -> Result<(Install, Option<String>), String> {
    let run = stand.brew(
        &format!("install-{token}"),
        &[
            "install".to_owned(),
            "--cask".to_owned(),
            format!("--appdir={}", appdir.display()),
            format!("{}/{token}", brew::TAP),
        ],
    )?;
    let install = brew::parse_install(run.code, &run.stdout, &run.stderr, token);
    let refusal =
        (!install.installed).then(|| brew::refusal(run.code, &install, &run.stdout, &run.stderr));
    stand.fact(
        &format!("install-{token}"),
        json!({ "install": install, "refusal": refusal }),
    );
    let app = appdir.join(APP_NAME);
    if install.installed {
        if !app.is_dir() {
            return Err(format!(
                "стенд: brew сообщил об установке {token}, а {} нет",
                app.display()
            ));
        }
        if !install
            .moved
            .iter()
            .any(|(_, target)| Path::new(target) == app)
        {
            return Err(format!(
                "стенд: brew переместил {token} не в {}: {:?}",
                app.display(),
                install.moved
            ));
        }
    }
    Ok((install, refusal))
}

fn xattr_scan(stand: &mut Stand, name: &str, app: &Path) -> Result<(Scan, String), String> {
    let run = stand.tool(name, XATTR, &["-lr".to_owned(), path_arg(app)])?;
    if run.code != Some(0) {
        return Err(format!("стенд: {}", failed(&run)));
    }
    let artifact = format!("{DIR}/{name}.txt");
    let path = stand.out.join(&artifact);
    fs::write(&path, &run.stdout).map_err(|err| format!("{}: {err}", path.display()))?;
    Ok((
        brew::scan(&brew::parse_xattr(&run.stdout), APP_NAME, EXECUTABLE),
        artifact,
    ))
}

/// Запись шага `preflight_steps` из каталога cask в Caskroom изолированного префикса.
fn step_record(stand: &Stand, file: &str) -> Result<(Scan, String), String> {
    let path = stand.prefix.join("Caskroom").join(brew::CASK).join(file);
    let text = fs::read_to_string(&path).map_err(|err| {
        format!(
            "стенд: шаг preflight_steps не записал {}: {err}",
            path.display()
        )
    })?;
    let artifact = format!("{DIR}/step-{file}");
    let copy = stand.out.join(&artifact);
    fs::write(&copy, &text).map_err(|err| format!("{}: {err}", copy.display()))?;
    Ok((
        brew::scan(&brew::parse_xattr(&text), APP_NAME, EXECUTABLE),
        artifact,
    ))
}

// --- запуск бандла ---

/// Запуск бандла через `open -n -W` с квитанцией и, если `open` не вышел за `wait`, окно
/// Gatekeeper и его закрытие кнопкой «Готово».
#[derive(Clone, Debug, Serialize)]
pub struct OpenRun {
    pub name: String,
    pub app: String,
    pub tag: String,
    pub open_exit: Option<String>,
    /// `open` вышел до срока ожидания.
    pub exited_in_wait: bool,
    pub receipt: Option<StubReport>,
    pub receipt_error: Option<String>,
    /// Процессы бандла с меткой в момент поиска окна.
    pub app_pids: Vec<i32>,
    pub dialogs: Vec<WindowEntry>,
    pub ax: Option<AxDialog>,
    pub pressed: Option<String>,
    pub press_error: Option<String>,
    /// `open` вышел после нажатия в срок.
    pub closed_by_press: bool,
    pub killed: Vec<ProcessRecord>,
    pub after_close: Vec<WindowEntry>,
    pub elapsed_ms: u64,
}

fn open_app(stand: &mut Stand, name: &str, app: &Path, wait: Duration) -> Result<OpenRun, String> {
    let started = Instant::now();
    let tag = format!("{}.brew-{name}", stand.nonce);
    let dir = stand.out.join(DIR);
    let receipt_path = dir.join(format!("{name}.app.stdout"));
    File::create(&receipt_path).map_err(|err| format!("{}: {err}", receipt_path.display()))?;
    let mut args = strings(&["-n", "-W", "--stdout"]);
    args.push(path_arg(&receipt_path));
    args.push("--stderr".to_owned());
    args.push(path_arg(&dir.join(format!("{name}.app.stderr"))));
    args.push(path_arg(app));
    args.extend(strings(&["--args", "--report", "--tag"]));
    args.push(tag.clone());
    let (pid, exit) = spawn_bounded(
        Path::new(OPEN),
        &args,
        None,
        (
            &dir.join(format!("{name}.open.stdout")),
            &dir.join(format!("{name}.open.stderr")),
        ),
        false,
        started + wait,
    )?;
    let mut run = OpenRun {
        name: name.to_owned(),
        app: path_arg(app),
        tag: tag.clone(),
        open_exit: None,
        exited_in_wait: exit.is_some(),
        receipt: None,
        receipt_error: None,
        app_pids: Vec::new(),
        dialogs: Vec::new(),
        ax: None,
        pressed: None,
        press_error: None,
        closed_by_press: false,
        killed: Vec::new(),
        after_close: Vec::new(),
        elapsed_ms: 0,
    };
    let exit = match exit {
        Some(exit) => Some(exit),
        None => {
            run.dialogs = dialogs_now();
            run.app_pids = launch::find_by_token(&tag)
                .iter()
                .map(|process| process.pid)
                .collect();
            if let Some(dialog) = run.dialogs.first() {
                let ax = window::ax_dialog(dialog.pid as i32);
                match brew::dismiss_button(&ax.buttons) {
                    Ok(button) => {
                        let identifier = button.identifier.clone().unwrap_or_default();
                        match window::ax_press(dialog.pid as i32, &identifier) {
                            Ok(()) => run.pressed = Some(identifier),
                            Err(err) => run.press_error = Some(err),
                        }
                    }
                    Err(err) => run.press_error = Some(err),
                }
                run.ax = Some(ax);
            }
            let mut alive = !launch::wait_exit(&[pid], CLOSE_WAIT).is_empty();
            run.closed_by_press = run.pressed.is_some() && !alive;
            if alive || !launch::find_by_token(&tag).is_empty() {
                run.killed = kill_tagged(&tag);
                alive = !launch::wait_exit(&[pid], CLOSE_WAIT).is_empty();
            }
            let exit = if alive {
                kill_and_reap(pid, false)
            } else {
                spawn::wait(pid).ok()
            };
            run.after_close = dialogs_now();
            exit
        }
    };
    run.open_exit = exit.map(|exit| exit.to_string());
    let text = fs::read_to_string(&receipt_path).unwrap_or_default();
    if !text.trim().is_empty() {
        match serde_json::from_str::<StubReport>(text.trim()) {
            Ok(receipt) => run.receipt = Some(receipt),
            Err(err) => run.receipt_error = Some(format!("квитанция не разобрана: {err}")),
        }
    }
    run.elapsed_ms = started.elapsed().as_millis() as u64;
    stand.fact(&format!("open-{name}"), &run);
    Ok(run)
}

// --- выводы частей ---

pub fn staged_verdict(before: &Scan) -> (Status, String) {
    if !before.root {
        return (
            Status::NotRun,
            format!(
                "карантин не поставлен: на корне staged-копии его нет, с карантином {} из {} путей",
                before.quarantined.len(),
                before.listed
            ),
        );
    }
    if !before.executable {
        return (
            Status::NotRun,
            format!(
                "карантин не поставлен на {EXECUTABLE}, только на {:?}",
                before.quarantined
            ),
        );
    }
    (
        Status::Pass,
        format!(
            "до снятия в шаге com.apple.quarantine «{}» на {} из {} путей staged-копии, корень и {EXECUTABLE} среди них",
            before.value.as_deref().unwrap_or(""),
            before.quarantined.len(),
            before.listed
        ),
    )
}

pub fn installed_verdict(
    staged: Status,
    after_step: &Scan,
    installed: &Scan,
    identity: &Result<String, String>,
) -> (Status, String) {
    if staged != Status::Pass {
        return (
            Status::NotRun,
            "карантин не поставлен на staged-копию: снимать нечего".to_owned(),
        );
    }
    let mut problems = Vec::new();
    if !after_step.quarantined.is_empty() {
        problems.push(format!(
            "в записи шага после xattr -dr карантин остался на {:?}",
            after_step.quarantined
        ));
    }
    if !installed.quarantined.is_empty() {
        problems.push(format!(
            "на установленном бандле карантин на {:?}",
            installed.quarantined
        ));
    }
    if let Err(err) = identity {
        problems.push(err.clone());
    }
    if problems.is_empty() {
        (
            Status::Pass,
            format!(
                "карантина нет ни на одном из {} путей с атрибутами установленного бандла и в записи шага после снятия; {}",
                installed.listed,
                identity.as_deref().unwrap_or_default()
            ),
        )
    } else {
        (Status::Fail, problems.join("; "))
    }
}

fn canonical(path: &str) -> Option<PathBuf> {
    fs::canonicalize(path).ok()
}

pub fn launch_verdict(run: &OpenRun, installed: &Path) -> (Status, String) {
    let mut problems = Vec::new();
    if !run.exited_in_wait {
        problems.push(format!(
            "open не вышел за срок, окна Gatekeeper: {}",
            if run.dialogs.is_empty() {
                "нет".to_owned()
            } else {
                describe_windows(&run.dialogs)
            }
        ));
    }
    match &run.receipt {
        None => problems.push(format!(
            "квитанции нет{}",
            run.receipt_error
                .as_deref()
                .map(|err| format!(" ({err})"))
                .unwrap_or_default()
        )),
        Some(receipt) => {
            let expected = fs::canonicalize(installed.join(EXECUTABLE)).ok();
            if receipt.binary != APP_BIN || receipt.generation != 1 {
                problems.push(format!(
                    "квитанция {} поколения {}, ожидается {APP_BIN} поколения 1",
                    receipt.binary, receipt.generation
                ));
            }
            if canonical(&receipt.executable) != expected || expected.is_none() {
                problems.push(format!(
                    "исполняемый файл {}, ожидается {}",
                    receipt.executable,
                    installed.join(EXECUTABLE).display()
                ));
            }
        }
    }
    if problems.is_empty() {
        (
            Status::Pass,
            format!(
                "open вышел ({}) за {} мс, квитанция {APP_BIN} поколения 1 из {}, окна Gatekeeper нет",
                run.open_exit.as_deref().unwrap_or("?"),
                run.elapsed_ms,
                run.receipt
                    .as_ref()
                    .map(|receipt| receipt.executable.as_str())
                    .unwrap_or_default()
            ),
        )
    } else {
        (Status::Fail, problems.join("; "))
    }
}

/// Факты части `control`.
#[derive(Clone, Debug, Serialize)]
pub struct ControlFacts {
    pub quarantined: bool,
    pub quarantine_value: Option<String>,
    /// Запуск контроля; без карантина контроль не запускается.
    pub open: Option<OpenRun>,
    /// Контроль остался в `--appdir` после закрытия окна.
    pub kept: bool,
    pub gatekeeper: Gatekeeper,
    /// Что стенд сделал с контролем в Корзине, если он туда попал.
    pub trash: Option<String>,
}

pub fn control_verdict(facts: &ControlFacts) -> (Status, String) {
    let Some(open) = facts.open.as_ref().filter(|_| facts.quarantined) else {
        return (
            Status::NotRun,
            "карантин не поставлен на контроль: проверять нечего".to_owned(),
        );
    };
    if let Some(receipt) = &open.receipt {
        return (
            Status::Fail,
            format!(
                "бандл с карантином запустился: квитанция {} поколения {} из {}",
                receipt.binary, receipt.generation, receipt.executable
            ),
        );
    }
    if open.exited_in_wait {
        return (
            Status::NotRun,
            format!(
                "open вышел ({}) до поиска окна: окно Gatekeeper стенд не наблюдал и не закрывал",
                open.open_exit.as_deref().unwrap_or("?")
            ),
        );
    }
    if open.dialogs.is_empty() {
        return (
            Status::NotRun,
            format!(
                "open не вышел за {} с, а окна {DIALOG_OWNER} нет",
                GATEKEEPER_WAIT.as_secs()
            ),
        );
    }
    if !open.closed_by_press || !open.after_close.is_empty() {
        return (
            Status::NotRun,
            format!(
                "окно Gatekeeper не закрыто нажатием «Готово»: нажата {:?}, ошибка {:?}, после закрытия окна {}",
                open.pressed,
                open.press_error,
                describe_windows(&open.after_close)
            ),
        );
    }
    if !facts.kept {
        return (
            Status::NotRun,
            format!(
                "контроль пропал из --appdir после закрытия окна (syspolicyd moveItemToTrash: {}); {}",
                facts.gatekeeper.trash,
                facts.trash.as_deref().unwrap_or("в Корзине не найден")
            ),
        );
    }
    let gk = &facts.gatekeeper;
    let terminated = if open.app_pids.is_empty() {
        !gk.terminated.is_empty()
    } else {
        gk.terminated.iter().any(|pid| open.app_pids.contains(pid))
    };
    if !(gk.prompt || gk.breadcrumb) || !terminated {
        return (
            Status::NotRun,
            format!(
                "записи syspolicyd нет: Prompt shown {}, denial breadcrumb {}, Terminating {:?} при процессах контроля {:?}",
                gk.prompt, gk.breadcrumb, gk.terminated, open.app_pids
            ),
        );
    }
    let titles: Vec<&str> = open
        .ax
        .iter()
        .flat_map(|ax| ax.buttons.iter())
        .filter(|button| button.identifier == open.pressed)
        .filter_map(|button| button.title.as_deref())
        .collect();
    (
        Status::Pass,
        format!(
            "карантин «{}» на контроле, квитанции нет; окно {DIALOG_OWNER} закрыто кнопкой {} «{}», контроль на месте; syspolicyd: Prompt shown {}, denial breadcrumb {}, Terminating {:?} (процессы контроля {:?})",
            facts.quarantine_value.as_deref().unwrap_or(""),
            open.pressed.as_deref().unwrap_or(""),
            titles.join(""),
            gk.prompt,
            gk.breadcrumb,
            gk.terminated,
            open.app_pids
        ),
    )
}

// --- сценарий ---

pub fn run(out: &Path, environment: &Environment) -> Result<Report, String> {
    let (state, nonce, layout) = stand_inputs(environment)?;
    let (bundles_dir, _) = bundles(environment)?;
    if bundles_dir.file_name().and_then(|name| name.to_str()) != Some("bundle") {
        return Err(format!(
            "предусловие: каталог бандлов {} не bundle/ рядом с каталогами прогонов",
            bundles_dir.display()
        ));
    }
    let root = bundles_dir
        .parent()
        .ok_or("предусловие: у каталога бандлов нет родителя")?
        .join(DIR);
    let home = environment
        .process
        .env
        .get("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .ok_or("предусловие: нет HOME")?;
    let dir = out.join(DIR);
    for sub in [dir.clone(), dir.join("cmd"), dir.join("casks")] {
        fs::create_dir_all(&sub).map_err(|err| format!("{}: {err}", sub.display()))?;
    }
    let work = state.join(DIR);
    for sub in [
        "cache",
        "logs",
        "temp",
        "config",
        "apps",
        "apps-control",
        "zip",
        "src",
    ] {
        let path = work.join(sub);
        fs::create_dir_all(&path).map_err(|err| format!("{}: {err}", path.display()))?;
    }
    let base_env = BREW_INHERITED
        .iter()
        .filter_map(|name| {
            environment
                .process
                .env
                .get(*name)
                .map(|value| ((*name).to_owned(), value.clone()))
        })
        .collect();
    let mut stand = Stand {
        out: out.to_path_buf(),
        work,
        prefix: root.join("prefix"),
        root,
        nonce,
        base_env,
        commands: Vec::new(),
        facts: serde_json::Map::new(),
    };
    let before = dialogs_now();
    if !before.is_empty() {
        return Err(format!(
            "предусловие: окно {DIALOG_OWNER} на экране до клетки: {}",
            describe_windows(&before)
        ));
    }
    let mut reals = vec![real_snapshot(&mut stand, &home, "до клетки")?];
    let walked = walk(&mut stand, &layout, &home);
    let mut removed = Vec::new();
    for path in [stand.root.clone(), stand.work.clone()] {
        if path.exists()
            && let Err(err) = fs::remove_dir_all(&path)
        {
            removed.push(format!("{}: {err}", path.display()));
        }
    }
    let after = real_snapshot(&mut stand, &home, "после клетки");
    if let Ok(snapshot) = &after {
        reals.push(snapshot.clone());
    }
    let lingering = dialogs_now();
    let changes = brew::real_changes(&reals);
    let steps = json!({
        "facts": stand.facts,
        "commands": stand.commands,
        "removed_errors": removed,
        "lingering_dialogs": lingering,
        "walk_error": walked.as_ref().err(),
    });
    let written = crate::results::write_json(&dir, "steps.json", &steps).and_then(|_| {
        crate::results::write_json(
            &dir,
            "real-homebrew.json",
            &json!({ "snapshots": reals, "changes": changes }),
        )
    });
    let (cells, notes) = walked?;
    written?;
    after?;
    if !removed.is_empty() {
        return Err(format!("стенд: не удалено: {}", removed.join("; ")));
    }
    if !changes.is_empty() {
        return Err(format!(
            "стенд: изменился настоящий Homebrew: {}",
            changes.join("; ")
        ));
    }
    if !lingering.is_empty() {
        return Err(format!(
            "стенд: окно {DIALOG_OWNER} осталось на экране в конце сценария: {}",
            describe_windows(&lingering)
        ));
    }
    Ok(Report {
        cells,
        diagnostics: Diagnostics { notes },
    })
}

type Walked = (Vec<Cell>, Vec<String>);

fn with_steps(extra: &[&str]) -> Vec<String> {
    let mut all = vec![STEPS_ARTIFACT.to_owned(), REAL_ARTIFACT.to_owned()];
    all.extend(extra.iter().map(|item| (*item).to_owned()));
    all
}

fn walk(stand: &mut Stand, layout: &Layout, home: &Path) -> Result<Walked, String> {
    let gen1 = layout.get("gen-1")?;
    let started = Instant::now();
    let isolation = isolate(stand)?;
    stand.fact("isolation", &isolation);
    let mut notes = vec![format!(
        "изоляция: {} за {} мс; префикс {}, Homebrew {}, portable-ruby {}",
        if isolation.ok {
            "удалась"
        } else {
            "не удалась"
        },
        isolation.elapsed_ms,
        isolation.prefix,
        isolation.version.as_deref().unwrap_or("?"),
        isolation.portable_ruby.as_deref().unwrap_or("?")
    )];
    if let Some(reason) = &isolation.reason {
        let reason = format!("изоляция не удалась: {reason}");
        notes.push(reason.clone());
        let artifacts = with_steps(&[]);
        return Ok((
            [&STAGED, &INSTALLED, &LAUNCH, &CONTROL]
                .into_iter()
                .map(|spec| cell(spec, Status::NotRun, reason.clone(), &artifacts))
                .collect(),
            notes,
        ));
    }
    prepare_casks(stand, gen1)?;
    let mut cells = Vec::new();

    let apps = stand.work.join("apps");
    let (main, main_refusal) = install_cask(stand, brew::CASK, &apps)?;
    if main.installed {
        let (before, before_artifact) = step_record(stand, "xattr-before.txt")?;
        let (after_step, after_artifact) = step_record(stand, "xattr-after.txt")?;
        let app = apps.join(APP_NAME);
        let (installed, installed_artifact) = xattr_scan(stand, "xattr-installed", &app)?;
        let mut runs = Vec::new();
        let identity = match generation::details(&app, &mut runs)? {
            Ok(details)
                if details.cdhash == gen1.cdhash && details.identifier == gen1.identifier =>
            {
                Ok(format!(
                    "в --appdir бандл поколения 1 (CDHash {})",
                    details.cdhash
                ))
            }
            Ok(details) => Err(format!(
                "в --appdir {} с CDHash {}, ожидается {} с {}",
                details.identifier, details.cdhash, gen1.identifier, gen1.cdhash
            )),
            Err(err) => Err(format!("подпись установленного бандла не прочитана: {err}")),
        };
        stand.fact("installed-codesign", &runs);
        let (staged_status, staged_reason) = staged_verdict(&before);
        let (installed_status, installed_reason) =
            installed_verdict(staged_status, &after_step, &installed, &identity);
        cells.push(cell(
            &STAGED,
            staged_status,
            staged_reason,
            &with_steps(&[&before_artifact]),
        ));
        cells.push(cell(
            &INSTALLED,
            installed_status,
            installed_reason,
            &with_steps(&[&after_artifact, &installed_artifact]),
        ));
        let spctl = stand.tool(
            "spctl-installed",
            SPCTL,
            &strings(&["--assess", "--type", "execute", "-vv"])
                .into_iter()
                .chain([path_arg(&app)])
                .collect::<Vec<_>>(),
        )?;
        notes.push(format!(
            "spctl --assess установленного бандла без карантина: код {:?}, {}",
            spctl.code,
            spctl.stderr.trim().replace('\n', " | ")
        ));
        let dialogs = dialogs_now();
        if !dialogs.is_empty() {
            return Err(format!(
                "стенд: окно {DIALOG_OWNER} на экране до запуска: {}",
                describe_windows(&dialogs)
            ));
        }
        let launched = open_app(stand, "launch", &app, OPEN_DEADLINE)?;
        let (status, reason) = launch_verdict(&launched, &app);
        cells.push(cell(
            &LAUNCH,
            status,
            reason,
            &with_steps(&["brew/launch.app.stdout"]),
        ));
        if !launched.after_close.is_empty() || !dialogs_now().is_empty() {
            return Err(format!(
                "стенд: окно {DIALOG_OWNER} осталось после запуска бандла без карантина: {}",
                describe_windows(&dialogs_now())
            ));
        }
    } else {
        let reason = format!(
            "brew отказался ставить cask {}: {}",
            brew::CASK,
            main_refusal.as_deref().unwrap_or("?")
        );
        for spec in [&STAGED, &INSTALLED, &LAUNCH] {
            cells.push(cell(spec, Status::NotRun, reason.clone(), &with_steps(&[])));
        }
    }

    let control_apps = stand.work.join("apps-control");
    let (control, control_refusal) = install_cask(stand, brew::CONTROL_CASK, &control_apps)?;
    if control.installed {
        let app = control_apps.join(APP_NAME);
        let (scan, scan_artifact) = xattr_scan(stand, "xattr-control", &app)?;
        let spctl = stand.tool(
            "spctl-control",
            SPCTL,
            &strings(&["--assess", "--type", "execute", "-vv"])
                .into_iter()
                .chain([path_arg(&app)])
                .collect::<Vec<_>>(),
        )?;
        notes.push(format!(
            "spctl --assess контроля с карантином: код {:?}, {}",
            spctl.code,
            spctl.stderr.trim().replace('\n', " | ")
        ));
        let facts = if scan.root {
            let dialogs = dialogs_now();
            if !dialogs.is_empty() {
                return Err(format!(
                    "стенд: окно {DIALOG_OWNER} на экране до запуска контроля: {}",
                    describe_windows(&dialogs)
                ));
            }
            let opened = Instant::now();
            let open = open_app(stand, "control", &app, GATEKEEPER_WAIT)?;
            let kept = app.is_dir();
            let trash = (!kept).then(|| trash_cleanup(stand, home, gen1));
            let last = opened.elapsed().as_secs() + 5;
            let log = stand.tool(
                "syspolicyd",
                LOG,
                &[
                    "show".to_owned(),
                    "--style".to_owned(),
                    "compact".to_owned(),
                    "--last".to_owned(),
                    format!("{last}s"),
                    "--predicate".to_owned(),
                    "process == \"syspolicyd\"".to_owned(),
                ],
            )?;
            let gatekeeper = brew::gatekeeper(&log.stdout, &gen1.identifier);
            let lines = stand.out.join(DIR).join("syspolicyd.txt");
            fs::write(&lines, gatekeeper.lines.join("\n"))
                .map_err(|err| format!("{}: {err}", lines.display()))?;
            ControlFacts {
                quarantined: true,
                quarantine_value: scan.value.clone(),
                open: Some(open),
                kept,
                gatekeeper,
                trash,
            }
        } else {
            ControlFacts {
                quarantined: false,
                quarantine_value: None,
                open: None,
                kept: true,
                gatekeeper: Gatekeeper::default(),
                trash: None,
            }
        };
        let (status, reason) = control_verdict(&facts);
        stand.fact("control", &facts);
        let mut artifacts = with_steps(&[&scan_artifact]);
        if facts.quarantined {
            artifacts.push(format!("{DIR}/syspolicyd.txt"));
        }
        cells.push(cell(&CONTROL, status, reason, &artifacts));
    } else {
        let reason = format!(
            "brew отказался ставить cask {}: {}",
            brew::CONTROL_CASK,
            control_refusal.as_deref().unwrap_or("?")
        );
        cells.push(cell(&CONTROL, Status::NotRun, reason, &with_steps(&[])));
    }
    notes.push(format!(
        "от начала изоляции до последней части {} мс; вызовов команд {}",
        started.elapsed().as_millis(),
        stand.commands.len()
    ));
    Ok((cells, notes))
}

/// Контроль пропал из `--appdir`: если в Корзине лежит `probe-update.app` с подписью поколения 1
/// пробы, стенд удаляет его по точному пути; чужое не трогается.
fn trash_cleanup(stand: &mut Stand, home: &Path, gen1: &BundleEntry) -> String {
    let candidate = home.join(".Trash").join(APP_NAME);
    if !candidate.is_dir() {
        return format!("{} нет", candidate.display());
    }
    let mut runs = Vec::new();
    let verdict = match generation::details(&candidate, &mut runs) {
        Ok(Ok(details))
            if details.identifier == gen1.identifier && details.cdhash == gen1.cdhash =>
        {
            match fs::remove_dir_all(&candidate) {
                Ok(()) => format!(
                    "{} с подписью пробы поколения 1 удалён из Корзины",
                    candidate.display()
                ),
                Err(err) => format!("{} не удалён: {err}", candidate.display()),
            }
        }
        Ok(Ok(details)) => format!(
            "{} чужой ({} {}), не тронут",
            candidate.display(),
            details.identifier,
            details.cdhash
        ),
        Ok(Err(err)) | Err(err) => format!("подпись {} не прочитана: {err}", candidate.display()),
    };
    stand.fact("trash-codesign", &runs);
    verdict
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::window::AxButton;

    fn scan(quarantined: &[&str], listed: usize) -> Scan {
        let quarantined: Vec<String> = quarantined.iter().map(|path| (*path).to_owned()).collect();
        Scan {
            listed,
            root: quarantined.iter().any(String::is_empty),
            executable: quarantined.iter().any(|path| path == EXECUTABLE),
            value: (!quarantined.is_empty()).then(|| "0181;6afd9982;;UUID".to_owned()),
            quarantined,
        }
    }

    #[test]
    fn staged_part_needs_quarantine_on_root_and_executable() {
        assert_eq!(staged_verdict(&scan(&["", EXECUTABLE], 9)).0, Status::Pass);
        assert_eq!(staged_verdict(&scan(&[], 9)).0, Status::NotRun);
        assert!(
            staged_verdict(&scan(&[], 9))
                .1
                .starts_with("карантин не поставлен")
        );
        assert_eq!(staged_verdict(&scan(&[""], 9)).0, Status::NotRun);
    }

    #[test]
    fn installed_part_checks_step_bundle_and_identity() {
        let clean = scan(&[], 10);
        let ok = Ok("поколение 1".to_owned());
        assert_eq!(
            installed_verdict(Status::Pass, &clean, &clean, &ok).0,
            Status::Pass
        );
        assert_eq!(
            installed_verdict(Status::NotRun, &clean, &clean, &ok).0,
            Status::NotRun
        );
        let left = scan(&["Contents/MacOS"], 10);
        let (status, reason) = installed_verdict(Status::Pass, &clean, &left, &ok);
        assert_eq!(status, Status::Fail);
        assert!(reason.contains("на установленном бандле"));
        let (status, reason) = installed_verdict(Status::Pass, &left, &clean, &ok);
        assert_eq!(status, Status::Fail);
        assert!(reason.contains("в записи шага"));
        let wrong = Err("в --appdir чужой код".to_owned());
        assert_eq!(
            installed_verdict(Status::Pass, &clean, &clean, &wrong).0,
            Status::Fail
        );
    }

    fn open_run() -> OpenRun {
        OpenRun {
            name: "control".to_owned(),
            app: "/t/apps-control/probe-update.app".to_owned(),
            tag: "n.brew-control".to_owned(),
            open_exit: Some("код 0".to_owned()),
            exited_in_wait: false,
            receipt: None,
            receipt_error: None,
            app_pids: vec![34373],
            dialogs: vec![WindowEntry {
                number: 7,
                pid: 83446,
                layer: 8,
                owner: DIALOG_OWNER.to_owned(),
                bounds: [0.0; 4],
            }],
            ax: Some(AxDialog {
                trusted: true,
                windows: 1,
                subroles: vec!["AXSystemDialog".to_owned()],
                texts: Vec::new(),
                buttons: vec![
                    AxButton {
                        identifier: Some("action-button--992".to_owned()),
                        title: Some("Переместить в Корзину".to_owned()),
                        default: true,
                    },
                    AxButton {
                        identifier: Some("action-button--997".to_owned()),
                        title: Some("Готово".to_owned()),
                        default: false,
                    },
                ],
                windows_error: None,
            }),
            pressed: Some("action-button--997".to_owned()),
            press_error: None,
            closed_by_press: true,
            killed: Vec::new(),
            after_close: Vec::new(),
            elapsed_ms: 10500,
        }
    }

    fn control() -> ControlFacts {
        ControlFacts {
            quarantined: true,
            quarantine_value: Some("0181;6afd9982;;UUID".to_owned()),
            open: Some(open_run()),
            kept: true,
            gatekeeper: Gatekeeper {
                prompt: true,
                breadcrumb: true,
                trash: false,
                terminated: vec![34373],
                lines: Vec::new(),
            },
            trash: None,
        }
    }

    fn receipt(executable: &str) -> StubReport {
        serde_json::from_value(json!({
            "binary": APP_BIN,
            "generation": 1,
            "host_versions": {"min": 1, "max": 1},
            "attach_versions": {"min": 1, "max": 1},
            "executable": executable,
        }))
        .unwrap()
    }

    #[test]
    fn control_part_outcomes() {
        let (status, reason) = control_verdict(&control());
        assert_eq!(status, Status::Pass, "{reason}");
        assert!(reason.contains("«Готово»"));

        let mut launched = control();
        launched.open.as_mut().unwrap().receipt = Some(receipt("/t/x"));
        assert_eq!(control_verdict(&launched).0, Status::Fail);

        let mut clean = control();
        clean.quarantined = false;
        assert_eq!(control_verdict(&clean).0, Status::NotRun);
        let mut not_opened = control();
        not_opened.open = None;
        assert_eq!(control_verdict(&not_opened).0, Status::NotRun);

        let mut early = control();
        early.open.as_mut().unwrap().exited_in_wait = true;
        assert!(control_verdict(&early).1.contains("до поиска окна"));

        let mut no_window = control();
        no_window.open.as_mut().unwrap().dialogs.clear();
        assert_eq!(control_verdict(&no_window).0, Status::NotRun);

        let mut stuck = control();
        let open = stuck.open.as_mut().unwrap();
        open.closed_by_press = false;
        open.press_error = Some("AXPress: ошибка AX -25204".to_owned());
        assert!(control_verdict(&stuck).1.contains("не закрыто нажатием"));

        let mut trashed = control();
        trashed.kept = false;
        trashed.gatekeeper.trash = true;
        assert!(control_verdict(&trashed).1.contains("пропал из --appdir"));

        let mut silent = control();
        silent.gatekeeper.terminated = vec![1];
        assert!(
            control_verdict(&silent)
                .1
                .starts_with("записи syspolicyd нет")
        );
        let mut no_prompt = control();
        no_prompt.gatekeeper.prompt = false;
        no_prompt.gatekeeper.breadcrumb = false;
        assert_eq!(control_verdict(&no_prompt).0, Status::NotRun);
        let mut unknown_pid = control();
        unknown_pid.open.as_mut().unwrap().app_pids.clear();
        assert_eq!(control_verdict(&unknown_pid).0, Status::Pass);
    }

    #[test]
    fn launch_part_needs_receipt_from_installed_bundle() {
        let dir = std::env::temp_dir().join(format!(
            "probe-update-brew-{}-{}",
            std::process::id(),
            crate::sys::uptime_ns()
        ));
        let app = dir.join(APP_NAME);
        fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        fs::write(app.join(EXECUTABLE), "x").unwrap();
        let mut run = open_run();
        run.name = "launch".to_owned();
        run.exited_in_wait = true;
        run.dialogs.clear();
        run.receipt = Some(receipt(&app.join(EXECUTABLE).display().to_string()));
        assert_eq!(launch_verdict(&run, &app).0, Status::Pass);
        let mut elsewhere = run.clone();
        elsewhere.receipt = Some(receipt("/usr/bin/true"));
        assert!(
            launch_verdict(&elsewhere, &app)
                .1
                .contains("исполняемый файл")
        );
        let mut none = run.clone();
        none.receipt = None;
        assert!(launch_verdict(&none, &app).1.contains("квитанции нет"));
        let mut blocked = run.clone();
        blocked.exited_in_wait = false;
        blocked.dialogs = open_run().dialogs;
        assert!(launch_verdict(&blocked, &app).1.contains(DIALOG_OWNER));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn only_gatekeeper_windows_count() {
        let window = |owner: &str| WindowEntry {
            number: 1,
            pid: 2,
            layer: 8,
            owner: owner.to_owned(),
            bounds: [0.0; 4],
        };
        let found = dialogs(&[
            window("Finder"),
            window(DIALOG_OWNER),
            window("SecurityAgent"),
        ]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].owner, DIALOG_OWNER);
    }
}
