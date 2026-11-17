//! Прогон `report-policy` в поверхности (план P0.4, задача 10, часть B).
//!
//! Каждая часть — сцена × доставка (`matrix::report_policy`): новая поверхность, её команда —
//! фикстура `replay` с файлом сцены, фиктивный attach. Стенд закрепляет фокус, видимость и схему
//! до восстановления, задаёт сетку 80×24 и только после квитанции `SIGWINCH` с этой сеткой просит
//! фикстуру отдать префикс attach и сцену.
//!
//! Граница каждой стадии — вставка маркера через `ghostty_surface_text`: она идёт в почту потока
//! ввода-вывода поверхности после отчётов, поставленных раньше. Отчёты разбора потока
//! (`messageWriter`) идут в ту же почту, поэтому после байтов из PTY стадия сначала ждёт ответа на
//! DSR позиции курсора, затем проход приложения, затем маркер. Ресайз поверхность сливает
//! таймером, поэтому стадия ресайза ждёт квитанцию `SIGWINCH` с новой сеткой. Ответ на DSR —
//! единственная последовательность от поверхности, которую вызывает сам стенд; он же даёт курсор.
//!
//! Ввод от поверхности — квитанции прочитанных байтов фикстуры. Записанное фикстурой — её
//! квитанции `emitted`, их стенд проверяет независимо от фильтра.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use objc2::MainThreadMarker;
use serde::Serialize;

use super::ghostty::{self, App, Config, GridSize, KEY_UP, Spec, Surface};
use super::view::{self, Shell};
use crate::args::Target;
use crate::fixture::channel::{Command, Event, Link, LinkError, Listener, Receipt, hex, unhex};
use crate::fixture::query::{Query, ResponseKind};
use crate::fixture::{self, Mode};
use crate::matrix::evidence::{self, Evidence};
use crate::matrix::report_policy::{
    self as policy, CURSOR_MARK, Cause, Delivery, KEY_UP_APPLICATION, MANIFEST, Manifest,
    RESIZE_COLS, RESIZE_ROWS, SCENES_DIR, SceneEntry, Stage,
};
use crate::oracle::model::PROFILE_VERSION;
use crate::results::{Cell, DefectOrigin, Environment, Report};

/// Срок одного ожидания квитанции или подключения.
const WAIT: Duration = Duration::from_secs(10);
/// Один проход главного цикла между проверками квитанций.
const PUMP: Duration = Duration::from_millis(5);
const POLL: Duration = Duration::from_millis(1);
const FIXTURE_LIFETIME: Duration = Duration::from_secs(120);
/// Конфигурация libghostty, которую `run.sh` пишет в `--out`.
pub const CONFIG_FILE: &str = "ghostty.conf";
const ARTIFACTS: &str = "report-policy";
/// Нажатие «вверх» без DECCKM.
const KEY_UP_NORMAL: &[u8] = b"\x1b[A";

pub fn run(target: &Target, environment: &Environment) -> Result<Report, String> {
    let mtm = MainThreadMarker::new().ok_or("сценарий не на главном потоке")?;
    let nonce = environment
        .process
        .nonce
        .clone()
        .ok_or("предусловие: нет nonce запуска")?;
    let out = target.out.clone();
    let manifest = read_manifest(&out)?;
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
    // До NSApplication и рабочих потоков пробы.
    ghostty::init_once(crate::PROBE_NAME)?;
    let config = Config::load(&out.join(CONFIG_FILE))?;
    let config_diagnostics = config.diagnostics();
    let app = App::new(config)?;
    let shell = Shell::new(mtm, "probe-session-surface");
    let root = PathBuf::from(format!("/tmp/pss-{nonce}"));
    fs::create_dir(&root).map_err(|err| format!("{}: {err}", root.display()))?;
    let artifacts = out.join(ARTIFACTS);
    fs::create_dir(&artifacts).map_err(|err| format!("{}: {err}", artifacts.display()))?;
    let listener = Listener::bind(&root.join("c.sock"));
    let outcome = listener.and_then(|listener| {
        let stand = Stand {
            app: &app,
            shell: &shell,
            listener,
            fixture,
            nonce,
            out: out.clone(),
            root: root.clone(),
            manifest: &manifest,
        };
        stand.parts()
    });
    shell.close();
    let _ = fs::remove_dir_all(&root);
    let records = outcome?;
    let (wakeups, actions) = app.counts();
    drop(app);
    let mut report = Report::default();
    for record in &records {
        let path = format!(
            "{ARTIFACTS}/{}-{}.json",
            record.delivery.name(),
            record.scene
        );
        let text = serde_json::to_string_pretty(record).map_err(|err| format!("{path}: {err}"))?;
        fs::write(out.join(&path), text).map_err(|err| format!("{path}: {err}"))?;
    }
    for scene in &manifest.scenes {
        report.cells.extend(judge_scene(scene, &records));
    }
    let diagnostics = &mut report.diagnostics.notes;
    diagnostics.push(format!(
        "сцены: профиль v{}, штамп VT {}",
        manifest.profile,
        manifest.vt_stamp.as_deref().unwrap_or("?")
    ));
    diagnostics.push(format!("libghostty: wakeup {wakeups}, действий {actions}"));
    for message in config_diagnostics {
        diagnostics.push(format!("конфигурация ghostty: {message}"));
    }
    if let Some(record) = records.first() {
        diagnostics.push(format!(
            "до восстановления в поверхности: {:?}",
            record.before_replay
        ));
    }
    Ok(report)
}

fn read_manifest(out: &Path) -> Result<Manifest, String> {
    let path = out.join(SCENES_DIR).join(MANIFEST);
    let text = fs::read_to_string(&path)
        .map_err(|err| format!("предусловие: сцены {}: {err}", path.display()))?;
    let manifest: Manifest = serde_json::from_str(&text)
        .map_err(|err| format!("предусловие: сцены {}: {err}", path.display()))?;
    if manifest.profile != PROFILE_VERSION {
        return Err(format!(
            "предусловие: сцены построены под профилем v{}, текущий v{PROFILE_VERSION}",
            manifest.profile
        ));
    }
    if (manifest.cols, manifest.rows) != (policy::COLS, policy::ROWS) {
        return Err(format!(
            "предусловие: сцены {}×{}, стенд {}×{}",
            manifest.cols,
            manifest.rows,
            policy::COLS,
            policy::ROWS
        ));
    }
    Ok(manifest)
}

// --- записи части ---

#[derive(Clone, Debug, Serialize)]
struct StageRecord {
    stage: Stage,
    /// Байты от поверхности до маркера.
    before_hex: String,
    bracketed: bool,
    /// Ожидаемые последовательности, снятые с байтов стадии.
    allowed_hex: Vec<String>,
    unexpected_hex: String,
    reports: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
struct EmittedRecord {
    source: String,
    input: u64,
    hex: String,
    removed: u64,
}

#[derive(Clone, Debug, Serialize)]
struct PartRecord {
    scene: String,
    delivery: Delivery,
    grid: GridSize,
    before_replay: Option<String>,
    replay_rows: Vec<String>,
    live_rows: Vec<String>,
    /// CPR после восстановления: строка и столбец с единицы.
    cpr: Option<(u16, u16)>,
    key_handled: (bool, bool),
    key_hex: String,
    resized: Option<(u16, u16)>,
    stages: Vec<StageRecord>,
    emitted: Vec<EmittedRecord>,
    receipts: usize,
}

impl PartRecord {
    fn stage(&self, stage: Stage) -> Option<&StageRecord> {
        self.stages.iter().find(|record| record.stage == stage)
    }

    /// Всё, что фикстура записала в PTY поверхности, одной строкой.
    fn emitted_bytes(&self) -> Vec<u8> {
        self.emitted
            .iter()
            .filter_map(|record| unhex(&record.hex))
            .flatten()
            .collect()
    }
}

// --- стенд ---

struct Stand<'a> {
    app: &'a App,
    shell: &'a Shell,
    listener: Listener,
    fixture: PathBuf,
    nonce: String,
    out: PathBuf,
    root: PathBuf,
    manifest: &'a Manifest,
}

impl Stand<'_> {
    fn parts(&self) -> Result<Vec<PartRecord>, String> {
        let mut records = Vec::new();
        for scene in &self.manifest.scenes {
            for delivery in Delivery::ALL {
                let record = self
                    .part(scene, delivery)
                    .map_err(|reason| format!("{}-{}: {reason}", delivery.name(), scene.name))?;
                records.push(record);
            }
        }
        Ok(records)
    }

    /// Проход главного цикла и, если libghostty просила, проход приложения.
    fn pass(&self) {
        view::pump(PUMP);
        if self.app.wants_tick() {
            self.app.tick();
        }
    }

    fn accept(&self) -> Result<Link, String> {
        let deadline = Instant::now() + WAIT;
        loop {
            self.pass();
            match self.listener.accept(POLL) {
                Ok(link) => return Ok(link),
                Err(LinkError::Timeout) if Instant::now() < deadline => {}
                Err(err) => return Err(format!("фикстура не подключилась: {err}")),
            }
        }
    }

    /// Первая квитанция, для которой `pick` даёт значение; главный цикл крутится всё ожидание.
    fn wait<T>(
        &self,
        link: &mut Link,
        what: &str,
        mut pick: impl FnMut(&Receipt) -> Option<T>,
    ) -> Result<T, String> {
        let deadline = Instant::now() + WAIT;
        loop {
            self.pass();
            match link.wait_for(POLL, &mut pick) {
                Ok(value) => return Ok(value),
                Err(LinkError::Timeout) if Instant::now() < deadline => {}
                Err(err) => return Err(format!("{what}: {err}")),
            }
        }
    }

    fn request(&self, link: &mut Link, command: Command, what: &str) -> Result<u64, String> {
        let id = link.send(command).map_err(|err| format!("{what}: {err}"))?;
        let (bytes, error) = self.wait(link, what, |receipt| match &receipt.event {
            Event::Ack {
                id: acked,
                bytes,
                error,
            } if *acked == id => Some((*bytes, error.clone())),
            _ => None,
        })?;
        match error {
            None => Ok(bytes),
            Some(error) => Err(format!("{what}: фикстура отказала: {error}")),
        }
    }

    /// Ждёт маркер стадии в байтах от поверхности после `consumed`.
    fn wait_marker(
        &self,
        link: &mut Link,
        consumed: usize,
        stage: Stage,
    ) -> Result<policy::Fence, String> {
        let marker = stage.marker();
        let deadline = Instant::now() + WAIT;
        loop {
            self.pass();
            match link.wait_for(POLL, |_| None::<()>) {
                Err(LinkError::Timeout) => {}
                Err(err) => return Err(format!("маркер {marker}: {err}")),
                Ok(()) => {}
            }
            let input = input_of(link.log());
            if let Some(fence) = input
                .get(consumed..)
                .and_then(|rest| policy::split_at_marker(rest, &marker))
            {
                return Ok(fence);
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "маркер {marker} не пришёл за {WAIT:?}; после границы: {}",
                    hex(input.get(consumed..).unwrap_or_default())
                ));
            }
        }
    }

    /// Граница по запросу в потоке: DSR позиции курсора через фикстуру и ответ поверхности.
    fn dsr(&self, link: &mut Link, seen: usize) -> Result<Vec<u8>, String> {
        self.request(
            link,
            Command::Query {
                query: Query::CursorPosition,
                split: None,
            },
            "DSR",
        )?;
        self.wait(link, "ответ на DSR", |receipt| {
            match &receipt.event {
                Event::Response {
                    index,
                    response: ResponseKind::CursorPosition,
                    hex,
                } if *index as usize > seen => unhex(hex),
                _ => None,
            }
        })
    }

    fn part(&self, scene: &SceneEntry, delivery: Delivery) -> Result<PartRecord, String> {
        let encoding = policy::encoding_name(delivery.encoding());
        let file = scene
            .files
            .get(encoding)
            .ok_or_else(|| format!("в манифесте нет сцены {encoding}"))?;
        let scene_path = self.out.join(SCENES_DIR).join(file);
        let fixture_nonce = format!("{}-{}-{}", self.nonce, delivery.name(), scene.name);
        let mut args = fixture::command_args(
            Mode::Replay,
            self.listener.path(),
            &fixture_nonce,
            Some(FIXTURE_LIFETIME),
        );
        args.push("--scene".to_owned());
        args.push(scene_path.display().to_string());
        if delivery.filtered() {
            args.push("--filter".to_owned());
        }
        let program = self.fixture.display().to_string();
        let spec = Spec {
            command: policy::shell_command(&program, &args),
            working_directory: self.root.display().to_string(),
            scale: self.shell.scale(),
        };
        let view = self.shell.view(640.0, 400.0);
        let surface = Surface::new(self.app, &view, &spec)?;
        surface.set_content_scale(spec.scale);
        // Каждая стадия — настоящий переход: колбэки фокуса, видимости и схемы без перемены
        // состояния ничего не делают.
        surface.set_focus(true);
        surface.set_visible(true);
        surface.set_dark(false);
        let cell = surface.size();
        if cell.cell_width_px == 0 || cell.cell_height_px == 0 {
            return Err(format!("нулевая клетка поверхности: {cell:?}"));
        }
        let grid_px = |cols: u16, rows: u16| {
            (
                u32::from(cols) * cell.cell_width_px,
                u32::from(rows) * cell.cell_height_px,
            )
        };
        let (width, height) = grid_px(policy::COLS, policy::ROWS);
        surface.set_size(width, height);
        let grid = surface.size();
        if (grid.columns, grid.rows) != (policy::COLS, policy::ROWS) {
            return Err(format!(
                "сетка поверхности {}×{} вместо {}×{}: {grid:?}",
                grid.columns,
                grid.rows,
                policy::COLS,
                policy::ROWS
            ));
        }
        let mut link = self.accept()?;
        let ready = self.wait(
            &mut link,
            "готовность фикстуры",
            |receipt| match &receipt.event {
                Event::Ready(ready) => Some(ready.clone()),
                _ => None,
            },
        )?;
        if ready.nonce != fixture_nonce {
            return Err(format!("подключилась чужая фикстура {}", ready.nonce));
        }
        let at_grid = |size: Option<crate::pty::Winsize>| {
            size.is_some_and(|size| (size.cols, size.rows) == (policy::COLS, policy::ROWS))
        };
        if !at_grid(ready.size) {
            self.wait(
                &mut link,
                "SIGWINCH с сеткой 80×24",
                |receipt| match &receipt.event {
                    Event::Winch { size } if at_grid(*size) => Some(()),
                    _ => None,
                },
            )?;
        }
        let before_replay = surface.viewport_text();

        let mut record = PartRecord {
            scene: scene.name.clone(),
            delivery,
            grid,
            before_replay,
            replay_rows: Vec::new(),
            live_rows: Vec::new(),
            cpr: None,
            key_handled: (false, false),
            key_hex: String::new(),
            resized: None,
            stages: Vec::new(),
            emitted: Vec::new(),
            receipts: 0,
        };
        let mut consumed = 0usize;
        let close = |record: &mut PartRecord,
                     link: &mut Link,
                     stage: Stage,
                     allowed: Vec<Vec<u8>>,
                     consumed: &mut usize|
         -> Result<policy::Fence, String> {
            self.app.tick();
            surface.paste(&stage.marker());
            let fence = self.wait_marker(link, *consumed, stage)?;
            *consumed += fence.end;
            let rest = policy::unexpected(&fence.before, &allowed);
            record.stages.push(StageRecord {
                stage,
                before_hex: hex(&fence.before),
                bracketed: fence.bracketed,
                allowed_hex: allowed.iter().map(|token| hex(token)).collect(),
                unexpected_hex: hex(&rest),
                reports: policy::report_kinds(&rest),
            });
            Ok(fence)
        };

        // Восстановление: префикс attach и сцена одной записью, затем граница по DSR.
        let prefix = unhex(&self.manifest.prefix_hex).ok_or("префикс attach не разобран")?;
        self.request(
            &mut link,
            Command::Replay {
                prefix: hex(&prefix),
            },
            "восстановление",
        )?;
        let seen = count_responses(link.log());
        let cpr = self.dsr(&mut link, seen)?;
        record.cpr = policy::parse_cpr(&cpr);
        close(
            &mut record,
            &mut link,
            Stage::Replay,
            vec![cpr],
            &mut consumed,
        )?;
        record.replay_rows = policy::text_rows(&surface.viewport_text().unwrap_or_default());

        // Режим вне группы по кодированию клавиши.
        record.key_handled = surface.tap(KEY_UP);
        let fence = close(
            &mut record,
            &mut link,
            Stage::Key,
            vec![KEY_UP_APPLICATION.to_vec(), KEY_UP_NORMAL.to_vec()],
            &mut consumed,
        )?;
        record.key_hex = hex(&fence.before);

        // Живой поток нагрузки, затем граница по DSR.
        let live = unhex(&self.manifest.live_hex).ok_or("живой поток не разобран")?;
        self.request(&mut link, Command::Write { hex: hex(&live) }, "живой поток")?;
        let seen = count_responses(link.log());
        let cpr = self.dsr(&mut link, seen)?;
        close(
            &mut record,
            &mut link,
            Stage::Live,
            vec![cpr],
            &mut consumed,
        )?;
        record.live_rows = policy::text_rows(&surface.viewport_text().unwrap_or_default());

        // Стимулы окна — явные вызовы, которые вид делал бы на событиях окна.
        surface.set_focus(false);
        close(
            &mut record,
            &mut link,
            Stage::FocusOut,
            Vec::new(),
            &mut consumed,
        )?;
        surface.set_focus(true);
        close(
            &mut record,
            &mut link,
            Stage::FocusIn,
            Vec::new(),
            &mut consumed,
        )?;
        surface.set_visible(false);
        close(
            &mut record,
            &mut link,
            Stage::Hidden,
            Vec::new(),
            &mut consumed,
        )?;
        surface.set_visible(true);
        close(
            &mut record,
            &mut link,
            Stage::Visible,
            Vec::new(),
            &mut consumed,
        )?;
        let (width, height) = grid_px(RESIZE_COLS, RESIZE_ROWS);
        surface.set_size(width, height);
        let resized = |size: Option<crate::pty::Winsize>| {
            size.is_some_and(|size| (size.cols, size.rows) == (RESIZE_COLS, RESIZE_ROWS))
        };
        self.wait(
            &mut link,
            "SIGWINCH после ресайза",
            |receipt| match &receipt.event {
                Event::Winch { size } if resized(*size) => Some(()),
                _ => None,
            },
        )?;
        record.resized = Some((RESIZE_COLS, RESIZE_ROWS));
        close(
            &mut record,
            &mut link,
            Stage::Resize,
            Vec::new(),
            &mut consumed,
        )?;
        surface.set_dark(true);
        close(
            &mut record,
            &mut link,
            Stage::Scheme,
            Vec::new(),
            &mut consumed,
        )?;

        link.send(Command::Exit { code: 0 })
            .map_err(|err| format!("выход фикстуры: {err}"))?;
        self.wait(
            &mut link,
            "выход фикстуры",
            |receipt| match &receipt.event {
                Event::Exit { .. } => Some(()),
                _ => None,
            },
        )?;
        record.emitted = link
            .log()
            .iter()
            .filter_map(|receipt| match &receipt.event {
                Event::Emitted {
                    source,
                    input,
                    hex,
                    removed,
                    ..
                } => Some(EmittedRecord {
                    source: source.clone(),
                    input: *input,
                    hex: hex.clone(),
                    removed: *removed,
                }),
                _ => None,
            })
            .collect();
        record.receipts = link.log().len();
        drop(surface);
        drop(view);
        Ok(record)
    }
}

fn input_of(log: &[Receipt]) -> Vec<u8> {
    log.iter()
        .filter_map(|receipt| match &receipt.event {
            Event::Read { hex, .. } => unhex(hex),
            _ => None,
        })
        .flatten()
        .collect()
}

fn count_responses(log: &[Receipt]) -> usize {
    log.iter()
        .filter(|receipt| matches!(receipt.event, Event::Response { .. }))
        .count()
}

// --- клетки ---

const STAND_ORIGIN: DefectOrigin = DefectOrigin::Unknown;

fn artifact(record: &PartRecord) -> Vec<String> {
    vec![format!(
        "{ARTIFACTS}/{}-{}.json",
        record.delivery.name(),
        record.scene
    )]
}

/// Ожидаемые виды отчёта контроля по стадии; пусто — достаточно любого отчёта.
fn control_kinds(stage: Stage) -> &'static [&'static str] {
    match stage {
        Stage::FocusOut => &["focus-out"],
        Stage::FocusIn => &["focus-in"],
        Stage::Hidden | Stage::Visible => &["visibility"],
        Stage::Resize => &["size"],
        Stage::Scheme => &["scheme"],
        Stage::Replay | Stage::Key | Stage::Live => &[],
    }
}

fn judge_scene(scene: &SceneEntry, records: &[PartRecord]) -> Vec<Cell> {
    let find = |delivery: Delivery| {
        records
            .iter()
            .find(|record| record.scene == scene.name && record.delivery == delivery)
    };
    let mut cells = Vec::new();
    let control = find(Delivery::Control);
    let mut unproven: Vec<Stage> = Vec::new();
    if let Some(control) = control {
        let mut evidence = Evidence::default();
        for stage in Stage::ALL.into_iter().filter(|s| s.reports_in_control()) {
            let Some(record) = control.stage(stage) else {
                evidence.check(stage.name(), false, "стадии нет в записи");
                unproven.push(stage);
                continue;
            };
            let expected = control_kinds(stage);
            let seen = !record.reports.is_empty()
                && expected
                    .iter()
                    .all(|kind| record.reports.iter().any(|seen| seen == kind));
            if !seen {
                unproven.push(stage);
            }
            evidence.check(
                &format!("{}: отчёт поверхности виден", stage.name()),
                seen,
                format!("{:?}", record.reports),
            );
        }
        evidence.note("reports", stage_reports(control));
        cells.push(evidence::cell(
            evidence::Spec {
                id: "surface-control",
                part: Some(&scene.name),
                stimulus: format!(
                    "сцена {} с режимами отчётов и живой поток без фильтра, фокус, видимость, \
                     ресайз и схема явными вызовами",
                    scene.name
                ),
                oracle: "квитанции прочитанных байтов фикстуры: отчёт поверхности на каждой \
                         стадии",
                completion: "маркер-вставка каждой стадии прочитан фикстурой",
                origin: STAND_ORIGIN,
            },
            &evidence,
            artifact(control),
        ));
    }
    for delivery in Delivery::ALL {
        let Some(record) = find(delivery) else {
            continue;
        };
        let part = format!("{}-{}", delivery.name(), scene.name);
        if delivery.filtered() {
            cells.push(judge_c2(scene, record, &part, control.is_some(), &unproven));
        }
        cells.push(judge_c1(scene, record, &part));
        cells.push(judge_c3(record, &part));
    }
    cells
}

fn stage_reports(record: &PartRecord) -> Vec<(String, Vec<String>)> {
    record
        .stages
        .iter()
        .map(|stage| (stage.stage.name().to_owned(), stage.reports.clone()))
        .collect()
}

fn judge_c2(
    scene: &SceneEntry,
    record: &PartRecord,
    part: &str,
    has_control: bool,
    unproven: &[Stage],
) -> Cell {
    let mut evidence = Evidence::default();
    let emitted = record.emitted_bytes();
    let leaked = policy::leaked_report_modes(&emitted);
    evidence.check(
        "записанные байты без режимов отчётов (независимый разбор)",
        leaked.is_empty(),
        format!("{leaked:?}"),
    );
    let mut failed = !leaked.is_empty();
    for stage in Stage::ALL {
        let Some(stage_record) = record.stage(stage) else {
            evidence.check(stage.name(), false, "стадии нет в записи");
            failed = true;
            continue;
        };
        let clean = stage_record.unexpected_hex.is_empty();
        failed |= !clean;
        evidence.check(
            &format!("{}: ни одного байта от поверхности", stage.name()),
            clean,
            if clean {
                "нет".to_owned()
            } else {
                format!(
                    "{} ({:?})",
                    stage_record.unexpected_hex, stage_record.reports
                )
            },
        );
    }
    evidence.note("reports", stage_reports(record));
    evidence.note(
        "removed",
        record.emitted.iter().map(|e| e.removed).sum::<u64>(),
    );
    if failed {
        let cause = if leaked.is_empty() {
            policy::report_cause(&emitted)
        } else {
            Cause::Filter
        };
        evidence.note("cause", cause.name());
    } else if !has_control {
        evidence.not_proven = Some("контрольной части нет".to_owned());
    } else if !unproven.is_empty() {
        let names: Vec<&str> = unproven.iter().map(|stage| stage.name()).collect();
        evidence.not_proven = Some(format!(
            "контроль не показал отчёт на стадиях: {}",
            names.join(", ")
        ));
    }
    evidence::cell(
        evidence::Spec {
            id: "surface-C2",
            part: Some(part),
            stimulus: format!(
                "сцена {} ({}), живой поток со смешанными CSI через фильтр, фокус, видимость, \
                 ресайз и схема после восстановления",
                scene.name,
                record.delivery.name()
            ),
            oracle: "квитанции прочитанных байтов фикстуры за вычетом ответа на DSR стенда и \
                     кодирования клавиши; записанные байты — квитанции emitted",
            completion: "маркер-вставка каждой стадии прочитан фикстурой",
            origin: STAND_ORIGIN,
        },
        &evidence,
        artifact(record),
    )
}

fn judge_c1(scene: &SceneEntry, record: &PartRecord, part: &str) -> Cell {
    let mut evidence = Evidence::default();
    let rows = policy::normalize_rows(&scene.rows);
    evidence.check(
        "текст после восстановления по read_text",
        record.replay_rows == rows,
        format!("поверхность {:?}, ожидалось {rows:?}", record.replay_rows),
    );
    let expected_cpr = (scene.cursor_y + 1, scene.cursor_x + 1);
    evidence.check(
        "курсор после восстановления по CPR",
        record.cpr == Some(expected_cpr),
        format!("{:?}, ожидалось {expected_cpr:?}", record.cpr),
    );
    let marked = policy::with_mark(&rows, scene.cursor_x, scene.cursor_y, CURSOR_MARK);
    evidence.check(
        "символ живого потока в позиции курсора по read_text",
        record.live_rows == marked,
        format!("поверхность {:?}, ожидалось {marked:?}", record.live_rows),
    );
    if evidence.checks.iter().any(|check| !check.ok) {
        evidence.note("cause", Cause::Bytes.name());
    }
    evidence.note("inexpressible", &scene.inexpressible);
    evidence::cell(
        evidence::Spec {
            id: "surface-C1",
            part: Some(part),
            stimulus: format!(
                "RIS и байты кодировщика сцены {} ({}) в новой поверхности, затем «{CURSOR_MARK}» \
                 живым потоком",
                scene.name,
                policy::encoding_name(record.delivery.encoding())
            ),
            oracle: "ghostty_surface_read_text видимой области против строк VT-источника; CPR",
            completion: "граница по DSR и маркер-вставка прочитаны фикстурой",
            origin: STAND_ORIGIN,
        },
        &evidence,
        artifact(record),
    )
}

fn judge_c3(record: &PartRecord, part: &str) -> Cell {
    let mut evidence = Evidence::default();
    let emitted = record.emitted_bytes();
    let bracketed = |stage: Stage| record.stage(stage).is_some_and(|s| s.bracketed);
    let mut causes = Vec::new();
    for (stage, what) in [
        (Stage::Replay, "после восстановления"),
        (Stage::Live, "после живого потока"),
    ] {
        let ok = bracketed(stage);
        if !ok {
            causes.push(policy::kept_cause(&emitted, 2004));
        }
        evidence.check(
            &format!("2004 {what}: вставка в CSI 200~ … 201~"),
            ok,
            if ok {
                "да"
            } else {
                "голый маркер"
            },
        );
    }
    let key = unhex(&record.key_hex).unwrap_or_default();
    let application = key == KEY_UP_APPLICATION;
    if !application {
        causes.push(policy::kept_cause(&emitted, 1));
    }
    evidence.check(
        "DECCKM: «вверх» кодируется CSI O A",
        application,
        format!("{} (обработано {:?})", record.key_hex, record.key_handled),
    );
    if let Some(cause) = causes.first() {
        evidence.note("cause", cause.name());
    }
    evidence::cell(
        evidence::Spec {
            id: "surface-C3",
            part: Some(part),
            stimulus: "вставка маркера через ghostty_surface_text и нажатие «вверх» через \
                       ghostty_surface_key после восстановления и живого потока"
                .to_owned(),
            oracle: "квитанции прочитанных байтов фикстуры: обёртка вставки и кодирование клавиши",
            completion: "маркер-вставка прочитан фикстурой",
            origin: STAND_ORIGIN,
        },
        &evidence,
        artifact(record),
    )
}
