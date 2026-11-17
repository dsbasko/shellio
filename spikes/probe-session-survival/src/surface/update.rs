//! Сценарий `update` (план P0.4, задача 16, клетка G3): замена подписанного бандла пробы при
//! живом хосте. Водитель — процесс из установленного бандла поколения N. Хост запускается из
//! `runtime/N/`, GUI №1 поколения N снимается `SIGKILL`, нагрузка пишет без клиента, бандл
//! заменяется подписанным поколением N+1 атомарным обменом, новое поколение готовит
//! `runtime/N+1/`, находит сессию своими бинарями и открывает её GUI №2 из того же пути.
//!
//! Ошибка стенда (подпись, поколения, раскладка, GUI) — ошибка исполнения. Недоступность живого
//! хоста для нового поколения — `fail` клетки.

use std::ffi::OsStr;
use std::fs;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::time::Instant;

use super::bridge::{
    Binaries, Driver, Mark, Session, WAIT, Window, barrier, drain_with, fence, read_scenes,
    request, step,
};
use crate::args::Target;
use crate::backend::{Backend, Lead, SessionId};
use crate::fixture::Mode;
use crate::fixture::channel::{Command, unhex};
use crate::matrix::bridge::{self as scenes, SceneSpec};
use crate::matrix::evidence::{self, Evidence, Spec};
use crate::matrix::update::{
    self, ARTIFACT, BUNDLE_ID, CELL, Helper, MANIFEST_FILE, Manifest, Runtime, SURFACE_NAME,
};
use crate::results::{Cell, DefectOrigin, Environment, Report};
use crate::sys;

const MARKER_GUI: &str = "pss-g3-gui";
const MARKER_CONTROL: &str = "pss-g3-control";

pub fn run(target: &Target, environment: &Environment) -> Result<Report, String> {
    let candidate = target
        .backend
        .ok_or("предусловие: сценарий update идёт на кандидате (--backend)")?;
    let scenes = read_scenes(&target.out)?;
    let scene = scenes.scene(update::SCENE)?.clone();
    let path = target.out.join(MANIFEST_FILE);
    let manifest = Manifest::parse(
        &fs::read_to_string(&path)
            .map_err(|err| format!("предусловие: {}: {err}", path.display()))?,
    )?;
    let mut stand = Stand::check(&manifest)?;
    let runtime = Runtime::new(&manifest.runtime);
    let prepared = runtime
        .prepare(stand.generation, &stand.installed, |file, helper| {
            stand.verify_helper(file, helper)
        })
        .map_err(|err| format!("предусловие: runtime/{}: {err}", stand.generation))?;
    stand.prepared.push(prepared);
    let generation = update::select(stand.generation, &runtime.prepared()?)
        .map_err(|err| format!("предусловие: {err}"))?;
    let mut driver = Driver::with_binaries(
        &target.out,
        environment,
        candidate,
        &binaries(&runtime, generation),
    )?;
    if !same_file(
        &driver.program,
        &update::bundled(&stand.installed, SURFACE_NAME),
    ) {
        let _ = driver.teardown();
        return Err(format!(
            "предусловие: водитель {} не из установленного бандла {}",
            driver.program.display(),
            stand.installed.display()
        ));
    }
    let outcome = g3(&mut driver, &mut stand, &runtime, &scene);
    let notes = driver.teardown();
    let mut report = Report {
        cells: vec![outcome?],
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

fn binaries(runtime: &Runtime, generation: u32) -> Binaries {
    Binaries {
        host: runtime.helper(generation, Helper::Host),
        attach: runtime.helper(generation, Helper::Attach),
        zmx: runtime.helper(generation, Helper::Zmx),
    }
}

/// Поколения бандла и их подписи.
struct Stand {
    installed: PathBuf,
    generation: u32,
    next: PathBuf,
    leaf: String,
    requirement: String,
    cdhash: String,
    next_cdhash: String,
    prepared: Vec<update::Prepared>,
}

impl Stand {
    /// Установленный бандл — поколение N, в манифесте есть N+1; оба проходят `codesign --verify`
    /// с требованием установленного поколения, их CDHash различны.
    fn check(manifest: &Manifest) -> Result<Stand, String> {
        let installed = manifest.installed.clone();
        let generation = generation_of(&installed)?;
        let next = manifest.bundle(generation + 1)?.to_path_buf();
        let next_generation = generation_of(&next)?;
        if next_generation != generation + 1 {
            return Err(format!(
                "предусловие: {} — поколение {next_generation}, ожидалось {}",
                next.display(),
                generation + 1
            ));
        }
        let designated = codesign(&["-d", "-r-"], &installed)?.1;
        let leaf = update::parse_leaf(&designated).ok_or_else(|| {
            format!(
                "предусловие: в требовании {} нет листового сертификата",
                installed.display()
            )
        })?;
        let requirement = update::requirement(BUNDLE_ID, &leaf);
        verify(&installed, &requirement, true)?;
        verify(&next, &requirement, true)?;
        let cdhash = cdhash_of(&installed)?;
        let next_cdhash = cdhash_of(&next)?;
        if cdhash == next_cdhash {
            return Err(format!(
                "предусловие: у поколений {generation} и {next_generation} один CDHash {cdhash}"
            ));
        }
        Ok(Stand {
            installed,
            generation,
            next,
            leaf,
            requirement,
            cdhash,
            next_cdhash,
            prepared: Vec::new(),
        })
    }

    fn verify_helper(&self, file: &Path, helper: Helper) -> Result<(), String> {
        verify(
            file,
            &update::requirement(&helper.identifier(), &self.leaf),
            false,
        )
    }

    /// Установка поколения N+1 на место установленного: копия рядом с ним, проверка подписи и
    /// поколения копии, атомарный обмен, затем проверка установленного.
    fn install_next(&self) -> Result<serde_json::Value, String> {
        let parent = self
            .installed
            .parent()
            .ok_or("предусловие: у установленного бандла нет каталога")?;
        let staging = parent.join(format!(".next-{}", std::process::id()));
        if staging.exists() {
            fs::remove_dir_all(&staging).map_err(|err| format!("{}: {err}", staging.display()))?;
        }
        fs::create_dir(&staging).map_err(|err| format!("{}: {err}", staging.display()))?;
        let staged = staging.join(
            self.installed
                .file_name()
                .ok_or("предусловие: у установленного бандла нет имени")?,
        );
        tool(
            "/usr/bin/ditto",
            &[
                OsStr::new("--norsrc"),
                OsStr::new("--noextattr"),
                OsStr::new("--noacl"),
                self.next.as_os_str(),
                staged.as_os_str(),
            ],
        )
        .and_then(|(ok, output)| ok.then_some(()).ok_or(output))
        .map_err(|err| format!("копия поколения {}: {err}", self.generation + 1))?;
        verify(&staged, &self.requirement, true)?;
        if generation_of(&staged)? != self.generation + 1 || cdhash_of(&staged)? != self.next_cdhash
        {
            return Err("копия поколения N+1 не совпала с подписанным поколением".to_owned());
        }
        let started = Instant::now();
        update::install(&self.installed, &staged)?;
        let swap_us = started.elapsed().as_micros();
        fs::remove_dir(&staging).map_err(|err| format!("{}: {err}", staging.display()))?;
        let installed_generation = generation_of(&self.installed)?;
        verify(&self.installed, &self.requirement, true)?;
        let installed_cdhash = cdhash_of(&self.installed)?;
        if installed_generation != self.generation + 1 || installed_cdhash != self.next_cdhash {
            return Err(format!(
                "после обмена установлено поколение {installed_generation} с CDHash \
                 {installed_cdhash}, ожидалось {} с {}",
                self.generation + 1,
                self.next_cdhash
            ));
        }
        Ok(serde_json::json!({
            "staged": staged,
            "swap_us": swap_us,
            "installed_generation": installed_generation,
            "installed_cdhash": installed_cdhash,
        }))
    }
}

/// Клетка G3.
fn g3(
    driver: &mut Driver,
    stand: &mut Stand,
    runtime: &Runtime,
    scene: &SceneSpec,
) -> Result<Cell, String> {
    let candidate = driver.candidate;
    let generation = stand.generation;
    let restored_step = step(scene, "restored")?;
    let headless_step = step(scene, "headless")?;
    let live_step = step(scene, "live")?;
    let source = unhex(&scene.source_hex).ok_or("сцена update: hex исходника")?;
    let headless = unhex(&headless_step.suffix_hex).ok_or("сцена update: hex шага headless")?;
    let live = unhex(&live_step.suffix_hex).ok_or("сцена update: hex шага live")?;
    let mut evidence = Evidence::default();
    let mut session = driver.ensure("g3", Mode::Emitter)?;
    let host_binary = runtime.host_binary(candidate, generation);
    let host_path = sys::executable_path(session.record.host.pid);
    evidence.check(
        "host-from-runtime",
        host_path
            .as_deref()
            .is_some_and(|path| same_file(path, &host_binary)),
        format!(
            "исполняемый файл хоста {:?}, ожидался {}",
            host_path,
            host_binary.display()
        ),
    );
    let mut record = serde_json::json!({
        "candidate": candidate,
        "session": session.id.as_str(),
        "workload": session.record.workload,
        "host": session.record.host,
        "host_path": host_path,
        "generations": {
            "installed": stand.installed,
            "current": generation,
            "next": stand.next,
            "requirement": stand.requirement,
            "cdhash": stand.cdhash,
            "next_cdhash": stand.next_cdhash,
        },
        "expected": {
            "restored": restored_step.view,
            "headless": headless_step.view,
            "live": live_step.view,
        },
    });
    let attach = driver
        .backend
        .attach_command(&session.id, Lead::IfFree)
        .map_err(|err| format!("команда attach поколения {generation}: {err}"))?;
    let mut first = driver.open_command(&attach, scene.cols, scene.rows)?;
    gui_generation(driver, &first, &stand.cdhash, generation)?;
    driver.wait_clients(&session.id, 1)?;
    if !barrier(&mut session.link, &source)? {
        return Err("предусловие G3: GUI №1 не ответила на барьер".to_owned());
    }
    let control = first.lines()?;
    record["gui1"] = serde_json::json!({ "pid": first.hello.pid, "lines": control });
    if let Some(diff) = scenes::first_difference(&restored_step.view.lines, &control) {
        evidence.not_proven = Some(format!(
            "контроль: GUI №1 без восстановления не совпала с эталоном, {diff}"
        ));
        first.quit();
        return finish(driver, &evidence, &record, stand);
    }
    let status = driver.kill(&mut first)?;
    evidence.check(
        "gui1-killed",
        status.signal() == Some(libc::SIGKILL),
        format!("GUI №1 завершилась {status:?}"),
    );
    driver.wait_clients(&session.id, 0)?;
    request(
        &mut session.link,
        Command::Write {
            hex: headless_step.suffix_hex.clone(),
        },
    )?;
    record["headless_bytes"] = serde_json::json!(headless.len());
    record["swap"] = stand.install_next()?;
    evidence.check(
        "alive-after-swap",
        driver.alive(&session)?,
        "нагрузка и хост с прежними PID и временем старта после замены бандла",
    );
    let next_generation = generation + 1;
    let prepared = runtime
        .prepare(next_generation, &stand.installed, |file, helper| {
            stand.verify_helper(file, helper)
        })
        .map_err(|err| format!("предусловие: runtime/{next_generation}: {err}"))?;
    stand.prepared.push(prepared);
    let selected = update::select(next_generation, &runtime.prepared()?)
        .map_err(|err| format!("предусловие: {err}"))?;
    let (next_backend, _) = driver.adapter(&binaries(runtime, selected))?;
    let found = find(&*next_backend, &session);
    evidence.check(
        "new-generation-lists",
        found.is_ok(),
        match &found {
            Ok(detail) | Err(detail) => detail.clone(),
        },
    );
    let ensured = driver.ensure_with(&*next_backend, &session, Mode::Emitter);
    evidence.check(
        "new-generation-ensure-keeps-workload",
        ensured.as_ref().is_ok_and(|ensured| {
            ensured.record.workload == session.record.workload
                && ensured.record.host == session.record.host
        }),
        match &ensured {
            Ok(ensured) => format!(
                "нагрузка {:?} и хост {:?}, были {:?} и {:?}",
                ensured.record.workload,
                ensured.record.host,
                session.record.workload,
                session.record.host
            ),
            Err(err) => err.clone(),
        },
    );
    record["new_generation"] = serde_json::json!({
        "generation": next_generation,
        "list": found,
        "ensure": ensured.as_ref().map(|ensured| &ensured.record),
    });
    let attach = match next_backend.attach_command(&session.id, Lead::IfFree) {
        Ok(attach) => attach,
        Err(err) => {
            evidence.check(
                "new-generation-attach",
                false,
                format!("команда attach: {err}"),
            );
            return finish(driver, &evidence, &record, stand);
        }
    };
    let mut second = driver.open_command(&attach, scene.cols, scene.rows)?;
    gui_generation(driver, &second, &stand.next_cdhash, next_generation)?;
    let attached = wait_listed(&*next_backend, &session.id, 1)?;
    evidence.check(
        "new-generation-attach",
        attached,
        "клиент нового поколения в списке сессии",
    );
    let outcome = if attached {
        views_and_input(
            &mut evidence,
            &*next_backend,
            &mut second,
            &mut session,
            headless_step,
            &live,
            live_step,
        )
    } else {
        Ok(serde_json::Value::Null)
    };
    record["gui2"] = serde_json::json!({ "pid": second.hello.pid, "observed": outcome });
    second.quit();
    evidence.check(
        "workload-and-host-unchanged",
        driver.alive(&session)?,
        "нагрузка и хост с прежними PID и временем старта в конце клетки",
    );
    let host_after = sys::executable_path(session.record.host.pid);
    evidence.check(
        "host-still-from-runtime",
        host_after
            .as_deref()
            .is_some_and(|path| same_file(path, &host_binary)),
        format!("исполняемый файл хоста в конце {host_after:?}"),
    );
    finish(driver, &evidence, &record, stand)
}

/// Вид GUI №2 после вывода без клиента и после живого вывода, ввод вставкой через поверхность
/// и управляющим соединением нового поколения.
fn views_and_input(
    evidence: &mut Evidence,
    backend: &dyn Backend,
    window: &mut Window,
    session: &mut Session,
    headless_step: &scenes::Step,
    live: &[u8],
    live_step: &scenes::Step,
) -> Result<serde_json::Value, String> {
    let restored = barrier(&mut session.link, b"")?
        .then(|| window.lines())
        .transpose()?;
    evidence.check(
        "view-after-headless-output",
        restored.as_ref() == Some(&headless_step.view.lines),
        view_detail(&headless_step.view.lines, restored.as_ref()),
    );
    let after_live = barrier(&mut session.link, live)?
        .then(|| window.lines())
        .transpose()?;
    evidence.check(
        "view-after-live-output",
        after_live.as_ref() == Some(&live_step.view.lines),
        view_detail(&live_step.view.lines, after_live.as_ref()),
    );
    let mut consumed = Mark::at(session.link.log());
    let pasted = fence(window, &mut session.link, &mut consumed, MARKER_GUI)?;
    evidence.check(
        "input-via-new-generation",
        pasted.is_some(),
        if pasted.is_some() {
            format!("маркер {MARKER_GUI} вставкой через GUI №2 дошёл до нагрузки")
        } else {
            format!("маркер {MARKER_GUI} не дошёл до нагрузки")
        },
    );
    let controlled = drain_with(backend, session, MARKER_CONTROL);
    evidence.check(
        "control-input-new-generation",
        matches!(controlled, Ok(Some(_))),
        match &controlled {
            Ok(Some(_)) => format!("маркер {MARKER_CONTROL} дошёл с квитанцией фикстуры"),
            Ok(None) => format!("маркер {MARKER_CONTROL} не дошёл"),
            Err(err) => err.clone(),
        },
    );
    Ok(serde_json::json!({
        "restored": restored,
        "after_live": after_live,
        "paste_before_marker_hex": pasted.map(|bytes| crate::fixture::channel::hex(&bytes)),
        "control": controlled.is_ok_and(|mark| mark.is_some()),
    }))
}

fn view_detail(expected: &[String], actual: Option<&Vec<String>>) -> String {
    match actual {
        Some(lines) => scenes::first_difference(expected, lines)
            .unwrap_or_else(|| "строки равны эталону".to_owned()),
        None => "GUI №2 не ответила на барьер".to_owned(),
    }
}

/// Сессия в `list` поколения: имя, ответ хоста и PID нагрузки по записи.
fn find(backend: &dyn Backend, session: &Session) -> Result<String, String> {
    let listed = backend.list().map_err(|err| format!("list: {err}"))?;
    let entry = listed
        .iter()
        .find(|entry| entry.name == session.id.as_str())
        .ok_or_else(|| format!("сессии {} нет в list", session.id))?;
    if let Some(error) = &entry.error {
        return Err(format!("хост {} не ответил: {error}", session.id));
    }
    if entry.workload_pid != Some(session.record.workload.pid) {
        return Err(format!(
            "PID нагрузки в list {:?}, по записи {}",
            entry.workload_pid, session.record.workload.pid
        ));
    }
    Ok(format!(
        "сессия {} с нагрузкой PID {} и {} клиентами",
        session.id,
        session.record.workload.pid,
        entry.clients.unwrap_or(0)
    ))
}

/// Ждёт `want` клиентов по `list` поколения; `false` — не дождались за срок.
fn wait_listed(backend: &dyn Backend, id: &SessionId, want: u32) -> Result<bool, String> {
    let deadline = Instant::now() + WAIT;
    loop {
        let clients = backend.list().ok().and_then(|listed| {
            listed
                .iter()
                .find(|entry| entry.name == id.as_str())
                .and_then(|entry| entry.clients)
        });
        if clients == Some(want) {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::park_timeout(std::time::Duration::from_millis(20));
    }
}

/// GUI запущена из установленного бандла поколения `generation`: путь исполняемого файла
/// процесса и CDHash процесса по `codesign`.
fn gui_generation(
    driver: &Driver,
    window: &Window,
    cdhash: &str,
    generation: u32,
) -> Result<(), String> {
    let path = sys::executable_path(window.hello.pid);
    if !path
        .as_deref()
        .is_some_and(|path| same_file(path, &driver.program))
    {
        return Err(format!(
            "предусловие: {} запущена из {path:?}, а не из {}",
            window.label,
            driver.program.display()
        ));
    }
    let running = codesign(
        &["-d", "--verbose=2"],
        Path::new(&window.hello.pid.to_string()),
    )
    .ok()
    .and_then(|(_, details)| update::parse_cdhash(&details));
    if running.as_deref() != Some(cdhash) {
        return Err(format!(
            "предусловие: CDHash {} {running:?}, у поколения {generation} {cdhash}",
            window.label
        ));
    }
    Ok(())
}

fn finish(
    driver: &Driver,
    evidence: &Evidence,
    record: &serde_json::Value,
    stand: &Stand,
) -> Result<Cell, String> {
    let mut record = record.clone();
    record["runtime"] = serde_json::json!(stand.prepared);
    record["evidence"] = serde_json::json!(evidence);
    let artifact = driver.artifact(ARTIFACT, &record)?;
    Ok(evidence::cell(
        Spec {
            id: CELL,
            part: None,
            stimulus: format!(
                "{}: хост из runtime/{}, GUI №1 поколения {}, SIGKILL GUI №1, вывод без клиента, \
                 замена бандла поколением {} атомарным обменом, runtime/{}, list и ensure нового \
                 поколения, GUI №2 из того же пути, живой вывод, ввод вставкой и управляющим \
                 соединением",
                driver.candidate,
                stand.generation,
                stand.generation,
                stand.generation + 1,
                stand.generation + 1
            ),
            oracle: "наблюдатель процессов (PID и время старта нагрузки и хоста, исполняемый файл \
                     хоста), codesign --verify обоих поколений, read_text против эталона из \
                     --scenes, квитанции ввода фикстуры",
            completion: "ответ поверхности на барьер CSI 5 n и маркеры ввода у фикстуры",
            origin: DefectOrigin::Host,
        },
        evidence,
        vec![artifact],
    ))
}

fn same_file(first: &Path, second: &Path) -> bool {
    match (first.canonicalize(), second.canonicalize()) {
        (Ok(first), Ok(second)) => first == second,
        _ => false,
    }
}

/// Код успеха и объединённый вывод инструмента.
fn tool(program: &str, args: &[&OsStr]) -> Result<(bool, String), String> {
    let output = Process::new(program)
        .args(args)
        .output()
        .map_err(|err| format!("{program}: {err}"))?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    Ok((output.status.success(), text))
}

fn codesign(args: &[&str], target: &Path) -> Result<(bool, String), String> {
    let mut all: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
    all.push(target.as_os_str());
    let (ok, output) = tool("/usr/bin/codesign", &all)?;
    if ok {
        Ok((ok, output))
    } else {
        Err(format!("codesign {args:?} {}: {output}", target.display()))
    }
}

/// `codesign --verify --strict` с требованием; у бандла ещё и вложенный код.
fn verify(target: &Path, requirement: &str, bundle: bool) -> Result<(), String> {
    let requirement = format!("-R={requirement}");
    let mut args = vec!["--verify", "--strict"];
    if bundle {
        args.push("--deep");
    }
    args.push(&requirement);
    codesign(&args, target)
        .map(drop)
        .map_err(|err| format!("предусловие: подпись: {err}"))
}

fn cdhash_of(target: &Path) -> Result<String, String> {
    let details = codesign(&["-d", "--verbose=2"], target)?.1;
    update::parse_cdhash(&details)
        .ok_or_else(|| format!("предусловие: нет CDHash у {}", target.display()))
}

/// Поколение бандла: `CFBundleVersion`.
fn generation_of(app: &Path) -> Result<u32, String> {
    let plist = update::info_plist(app);
    let (ok, output) = tool(
        "/usr/bin/plutil",
        &[
            OsStr::new("-extract"),
            OsStr::new("CFBundleVersion"),
            OsStr::new("raw"),
            OsStr::new("-o"),
            OsStr::new("-"),
            plist.as_os_str(),
        ],
    )?;
    ok.then(|| update::parse_generation(&output))
        .flatten()
        .ok_or_else(|| {
            format!(
                "предусловие: поколение {}: «{}»",
                plist.display(),
                output.trim()
            )
        })
}
