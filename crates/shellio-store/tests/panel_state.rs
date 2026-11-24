//! Данные и статусы панелей на настоящем `state.db`: запуск с `init` и `migrate`, строки, которые
//! ядро не перезаписывает, и отпечаток реестра плагинов. Базу готовит настоящее ядро с одним
//! реестром, строки `panel_states` правятся прямым SQL, следующий запуск идёт с другим реестром.

use std::collections::BTreeMap;
use std::error::Error;
use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension};
use shellio_core::test_support::{
    FakeRootResolver, ManualClock, TestInit, TestMigrate, TestPlugin, registry, registry_of,
};
use shellio_core::{
    Command, CommandValue, Core, CoreHandle, EventsSince, Loaded, PanelData, PanelId, PanelStatus,
    Persist, ReplyValue, Revision, Snapshot, SystemIds, UnixMillis, fingerprint,
};
use shellio_layout::Viewport;
use shellio_plugin_api::{Cardinality, Registry};
use shellio_store::{Database, Profile, SqlitePersist};
use tempfile::TempDir;
use tokio::runtime::Handle;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

const NOW: UnixMillis = UnixMillis(1_795_000_000_000);
const REPO: &str = "/src/repo";
const REPO_KEY: &str = "/src/repo/.git";
const FIXTURE_V2: &str = include_str!("fixtures/v2.sql");

/// Строка `panel_states` как её хранит SQLite: тип и значение версии, тип данных и их байты.
type Row = (String, String, String, String);

fn hex(text: &str) -> String {
    text.bytes().map(|byte| format!("{byte:02X}")).collect()
}

fn text_row(version: i64, data: &str) -> Option<Row> {
    Some((
        "integer".to_owned(),
        version.to_string(),
        "text".to_owned(),
        hex(data),
    ))
}

fn text(version: u32, raw: &str) -> PanelData {
    PanelData::Text {
        version,
        raw: raw.to_owned(),
    }
}

struct Store {
    _dir: TempDir,
    profile: Profile,
}

impl Store {
    fn new() -> TestResult<Self> {
        let dir = TempDir::new()?;
        let profile = Profile::isolated(dir.path().join("state"));
        Ok(Self { _dir: dir, profile })
    }

    /// База v2 из образца, ещё не мигрированная.
    fn fixture_v2() -> TestResult<Self> {
        let store = Self::new()?;
        store.profile.ensure_state_dir()?;
        Connection::open(store.profile.state_db())?.execute_batch(FIXTURE_V2)?;
        Ok(store)
    }

    fn persist(&self) -> TestResult<SqlitePersist> {
        Ok(SqlitePersist::new(Database::open(
            &self.profile,
            &ManualClock::new(NOW),
        )?))
    }

    /// Загрузка отдельным соединением, как при следующем запуске.
    fn load(&self) -> TestResult<Loaded> {
        self.persist()?.load()
    }

    fn raw(&self) -> TestResult<Connection> {
        Ok(Connection::open(self.profile.state_db())?)
    }

    fn revision(&self) -> TestResult<Revision> {
        let value: i64 =
            self.raw()?
                .query_row("SELECT value FROM meta WHERE key = 'revision'", [], |row| {
                    row.get(0)
                })?;
        Ok(Revision(u64::try_from(value)?))
    }

    fn count(&self, table: &str) -> TestResult<i64> {
        Ok(self
            .raw()?
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })?)
    }

    fn start(&self, registry: Arc<Registry>) -> TestResult<CoreHandle> {
        let resolver = FakeRootResolver::new().with_git(REPO, REPO_KEY);
        let core = Core::start(
            &Handle::current(),
            Box::new(self.persist()?),
            Box::new(ManualClock::new(NOW)),
            Box::new(SystemIds),
            Arc::new(resolver),
            registry,
        )
        .map_err(|error| error.to_string())?;
        // Команде `layout.split` нужен размер окна.
        core.set_viewport(Some(Viewport {
            width: 2400,
            height: 1400,
        }));
        Ok(core)
    }

    /// Строка данных панели побайтно. `None` — строки нет.
    fn row(&self, panel: PanelId) -> TestResult<Option<Row>> {
        Ok(self
            .raw()?
            .query_row(
                "SELECT typeof(version), quote(version), typeof(data), hex(CAST(data AS BLOB)) \
                 FROM panel_states WHERE panel = ?1",
                [panel.as_bytes().as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?)
    }

    fn rows(&self, panels: &BTreeMap<&'static str, PanelId>) -> TestResult<Vec<Option<Row>>> {
        panels.values().map(|panel| self.row(*panel)).collect()
    }

    fn rowid(&self, panel: PanelId) -> TestResult<i64> {
        Ok(self.raw()?.query_row(
            "SELECT rowid FROM panel_states WHERE panel = ?1",
            [panel.as_bytes().as_slice()],
            |row| row.get(0),
        )?)
    }

    /// Правка строки данных панели в обход ядра. `sql` получает id панели параметром `?1`.
    fn edit(&self, sql: &str, panel: PanelId) -> TestResult {
        let changed = self.raw()?.execute(sql, [panel.as_bytes().as_slice()])?;
        if changed == 1 {
            Ok(())
        } else {
            Err(format!("правка «{sql}» изменила строк: {changed}").into())
        }
    }
}

async fn snapshot(core: &CoreHandle) -> TestResult<Snapshot> {
    Ok(core.subscribe().await?.0)
}

async fn created(core: &CoreHandle, command: Command) -> TestResult<CommandValue> {
    match core.request(command).await?.value {
        ReplyValue::Command(value) => Ok(value),
        other => Err(format!("ожидался ответ команды, получено {other:?}").into()),
    }
}

/// Новая вкладка плагина `plugin` рядом с панелью `beside`.
async fn open(core: &CoreHandle, beside: PanelId, plugin: &str) -> TestResult<PanelId> {
    let command = Command::LayoutSplit {
        panel: beside,
        open: Some(plugin.to_owned()),
    };
    match created(core, command).await? {
        CommandValue::Panel { panel } => Ok(panel),
        other => Err(format!("ожидалась панель, получено {other:?}").into()),
    }
}

/// Проект с рабочей областью на каждую пару `(ключ, плагин)`: в области — навигатор, терминал и
/// панель плагина. Возвращает панели плагинов по ключам.
async fn build(
    core: &CoreHandle,
    panels: &[(&'static str, &'static str)],
) -> TestResult<BTreeMap<&'static str, PanelId>> {
    let workspace = snapshot(core)
        .await?
        .state
        .window
        .active_workspace
        .ok_or("нет активного воркспейса")?;
    let command = Command::ProjectAdd {
        workspace,
        path: REPO.into(),
        title: None,
    };
    let CommandValue::Project { project, .. } = created(core, command).await? else {
        return Err("ожидался проект".into());
    };
    let mut built = BTreeMap::new();
    for (key, plugin) in panels {
        let command = Command::WorkbenchCreate {
            project,
            title: (*key).into(),
        };
        let CommandValue::Workbench { workbench } = created(core, command).await? else {
            return Err("ожидалась рабочая область".into());
        };
        let terminal = snapshot(core).await?.state.layouts[&workbench]
            .focused()
            .ok_or("нет панели с фокусом")?;
        built.insert(*key, open(core, terminal, plugin).await?);
    }
    Ok(built)
}

fn statuses(
    panels: &BTreeMap<&'static str, PanelId>,
    expected: &[(&'static str, PanelStatus)],
) -> BTreeMap<PanelId, PanelStatus> {
    expected
        .iter()
        .map(|(key, status)| (panels[key], *status))
        .collect()
}

#[tokio::test]
async fn statuses_keep_row_bytes_through_writes_and_restart() -> TestResult {
    const PLUGINS: [&str; 8] = [
        "gone", "newer", "corrupt", "migfail", "initfail", "big", "mig", "ready",
    ];
    let store = Store::new()?;
    let core = store.start(registry_of(
        PLUGINS.iter().map(|id| TestPlugin::new(id)).collect(),
    ))?;
    let pairs: Vec<_> = PLUGINS.iter().map(|id| (*id, *id)).collect();
    let panels = build(&core, &pairs).await?;
    core.shutdown().await;

    let set = "UPDATE panel_states SET";
    store.edit(
        &format!("{set} version = 5 WHERE panel = ?1"),
        panels["newer"],
    )?;
    store.edit(
        &format!("{set} data = '{{\"count\":10}}' WHERE panel = ?1"),
        panels["corrupt"],
    )?;
    store.edit(
        &format!("{set} data = '{{\"n\":3}}' WHERE panel = ?1"),
        panels["migfail"],
    )?;
    store.edit(
        &format!("{set} data = '{{\"n\":7}}' WHERE panel = ?1"),
        panels["mig"],
    )?;
    for key in ["initfail", "big"] {
        store.edit("DELETE FROM panel_states WHERE panel = ?1", panels[key])?;
    }
    let mut kept = panels.clone();
    kept.remove("mig");
    let before = store.rows(&kept)?;
    assert_eq!(store.row(panels["gone"])?, text_row(1, r#"{"count":0}"#));
    assert_eq!(store.row(panels["newer"])?, text_row(5, r#"{"count":0}"#));
    assert_eq!(
        store.row(panels["corrupt"])?,
        text_row(1, r#"{"count":10}"#)
    );
    assert_eq!(store.row(panels["migfail"])?, text_row(1, r#"{"n":3}"#));
    assert_eq!(store.row(panels["initfail"])?, None);
    assert_eq!(store.row(panels["big"])?, None);

    // Другой реестр над тем же профилем: плагина `gone` нет, у двух плагинов новая версия данных.
    let other = || {
        registry_of(vec![
            TestPlugin::new("newer"),
            TestPlugin::new("corrupt"),
            TestPlugin::new("migfail")
                .state_version(2)
                .migrate(TestMigrate::Refuse),
            TestPlugin::new("initfail").init(TestInit::Refuse),
            TestPlugin::new("big").init(TestInit::Oversize),
            TestPlugin::new("mig").state_version(2),
            TestPlugin::new("ready"),
        ])
    };
    let expected = statuses(
        &panels,
        &[
            ("gone", PanelStatus::PluginMissing),
            ("newer", PanelStatus::StateNewer),
            ("corrupt", PanelStatus::StateCorrupt),
            ("migfail", PanelStatus::MigrateFailed),
            ("initfail", PanelStatus::InitFailed),
            ("big", PanelStatus::InitFailed),
        ],
    );
    let loaded = store.revision()?;
    let core = store.start(other())?;
    let (initial, mut events) = core.subscribe().await?;
    assert_eq!(initial.revision, loaded.next());
    assert_eq!(initial.state.diagnostics, Vec::new());
    assert_eq!(initial.state.panel_status, expected);
    // Запись запуска с новым отпечатком в буфер не попала: прежний курсор получает снимок.
    assert_eq!(
        core.events_since(loaded).await?,
        EventsSince::Reset(initial.clone())
    );
    // `migrate` с меньшей версии записан той же записью.
    assert_eq!(store.row(panels["mig"])?, text_row(2, r#"{"count":7}"#));
    assert_eq!(
        initial.state.panel_data[&panels["mig"]],
        text(2, r#"{"count":7}"#)
    );
    assert_eq!(store.rows(&kept)?, before, "запуск не тронул строки");

    // Запись других панелей: одна с данными, одна со статусом. Снимок и события дают состояние
    // актора вместе с данными и статусами.
    let mut mirror = (*initial.state).clone();
    let fresh = open(&core, panels["ready"], "ready").await?;
    let failed = open(&core, panels["initfail"], "initfail").await?;
    for _ in 0..2 {
        mirror.apply_all(&events.try_recv()?.changes);
    }
    let after = snapshot(&core).await?;
    assert_eq!(after.revision, Revision(loaded.0 + 3));
    assert_eq!(*after.state, mirror, "зеркало разошлось с актором");
    assert_eq!(mirror.panel_data[&fresh], text(1, r#"{"count":0}"#));
    assert_eq!(mirror.panel_data.get(&failed), None);
    let mut expected = expected;
    expected.insert(failed, PanelStatus::InitFailed);
    assert_eq!(mirror.panel_status, expected);
    assert_eq!(store.rows(&kept)?, before, "запись другой панели");
    core.shutdown().await;

    // Новый запуск с тем же реестром ничего не пишет: статусы прежние, `migrate` не повторяется.
    let core = store.start(other())?;
    let again = snapshot(&core).await?;
    assert_eq!(again, after);
    core.shutdown().await;
    assert_eq!(store.revision()?, after.revision);
    assert_eq!(store.rows(&kept)?, before, "перезапуск");
    assert_eq!(store.row(panels["mig"])?, text_row(2, r#"{"count":7}"#));
    Ok(())
}

#[tokio::test]
async fn missing_null_and_broken_text_are_three_different_inputs() -> TestResult {
    let plugins = || registry_of(vec![TestPlugin::new("p")]);
    let store = Store::new()?;
    let core = store.start(plugins())?;
    let panels = build(&core, &[("absent", "p"), ("null", "p"), ("brace", "p")]).await?;
    core.shutdown().await;
    store.edit(
        "DELETE FROM panel_states WHERE panel = ?1",
        panels["absent"],
    )?;
    store.edit(
        "UPDATE panel_states SET data = 'null' WHERE panel = ?1",
        panels["null"],
    )?;
    store.edit(
        "UPDATE panel_states SET data = '{' WHERE panel = ?1",
        panels["brace"],
    )?;

    let loaded = store.load()?;
    assert_eq!(loaded.state.diagnostics, Vec::new());
    assert_eq!(loaded.state.panel_data.get(&panels["absent"]), None);
    assert_eq!(loaded.state.panel_data[&panels["null"]], text(1, "null"));
    assert_eq!(loaded.state.panel_data[&panels["brace"]], text(1, "{"));

    // Строки нет — `init`; `null` и `{` — данные, которые плагин не разобрал: они остаются.
    let core = store.start(plugins())?;
    let started = snapshot(&core).await?;
    core.shutdown().await;
    assert_eq!(started.revision, loaded.revision.next());
    assert_eq!(
        started.state.panel_status,
        statuses(
            &panels,
            &[
                ("null", PanelStatus::StateCorrupt),
                ("brace", PanelStatus::StateCorrupt),
            ]
        )
    );
    assert_eq!(
        started.state.panel_data[&panels["absent"]],
        text(1, r#"{"count":0}"#)
    );
    assert_eq!(store.row(panels["absent"])?, text_row(1, r#"{"count":0}"#));
    assert_eq!(store.row(panels["null"])?, text_row(1, "null"));
    assert_eq!(store.row(panels["brace"])?, text_row(1, "{"));

    let core = store.start(plugins())?;
    assert_eq!(snapshot(&core).await?, started);
    core.shutdown().await;
    assert_eq!(store.revision()?, started.revision);
    Ok(())
}

#[tokio::test]
async fn unreadable_rows_are_state_corrupt_and_never_rewritten() -> TestResult {
    let plugins = || registry_of(vec![TestPlugin::new("p")]);
    let store = Store::new()?;
    let core = store.start(plugins())?;
    let mut panels = build(
        &core,
        &[("neg", "p"), ("huge", "p"), ("bytes", "p"), ("other", "p")],
    )
    .await?;
    core.shutdown().await;
    let other = panels.remove("other").ok_or("нет панели other")?;
    store.edit(
        "UPDATE panel_states SET version = -1 WHERE panel = ?1",
        panels["neg"],
    )?;
    store.edit(
        "UPDATE panel_states SET version = 4294967296 WHERE panel = ?1",
        panels["huge"],
    )?;
    store.edit(
        "UPDATE panel_states SET data = CAST(X'FF' AS TEXT) WHERE panel = ?1",
        panels["bytes"],
    )?;
    let before = store.rows(&panels)?;
    assert_eq!(store.row(panels["neg"])?, text_row(-1, r#"{"count":0}"#));
    assert_eq!(
        store.row(panels["huge"])?,
        text_row(4_294_967_296, r#"{"count":0}"#)
    );
    assert_eq!(
        store.row(panels["bytes"])?,
        Some((
            "integer".to_owned(),
            "1".to_owned(),
            "text".to_owned(),
            "FF".to_owned()
        ))
    );

    let loaded = store.load()?;
    let mut findings: Vec<_> = loaded
        .state
        .diagnostics
        .iter()
        .map(|finding| {
            (
                finding.table.as_str(),
                finding.rowid,
                finding.reason.as_str(),
            )
        })
        .collect();
    findings.sort_unstable();
    let mut expected_findings = Vec::new();
    for panel in panels.values() {
        assert_eq!(loaded.state.panel_data[panel], PanelData::Unreadable {});
        expected_findings.push(("panel_states", store.rowid(*panel)?, "invalid_value"));
    }
    expected_findings.sort_unstable();
    assert_eq!(findings, expected_findings);

    let corrupt: BTreeMap<_, _> = panels
        .values()
        .map(|panel| (*panel, PanelStatus::StateCorrupt))
        .collect();
    let core = store.start(plugins())?;
    let started = snapshot(&core).await?;
    assert_eq!(started.state.panel_status, corrupt);
    assert_eq!(store.rows(&panels)?, before, "запуск не тронул строки");
    open(&core, other, "p").await?;
    assert_eq!(store.rows(&panels)?, before, "запись другой панели");
    core.shutdown().await;

    let core = store.start(plugins())?;
    assert_eq!(snapshot(&core).await?.state.panel_status, corrupt);
    core.shutdown().await;
    assert_eq!(store.rows(&panels)?, before, "перезапуск");
    Ok(())
}

#[tokio::test]
async fn fingerprint_change_writes_startup_record_and_marks_duplicate() -> TestResult {
    let multi = || registry_of(vec![TestPlugin::new("solo")]);
    let singleton = || {
        registry_of(vec![
            TestPlugin::new("solo").cardinality(Cardinality::PerProject),
        ])
    };
    assert_ne!(fingerprint(&multi()), fingerprint(&singleton()));

    let store = Store::new()?;
    let core = store.start(multi())?;
    let panels = build(&core, &[("one", "solo"), ("two", "solo")]).await?;
    let built = snapshot(&core).await?;
    core.shutdown().await;
    assert_eq!(store.load()?.fingerprint, Some(fingerprint(&multi())));

    // Обычный повторный запуск без изменений записи не порождает.
    let core = store.start(multi())?;
    assert_eq!(snapshot(&core).await?, built);
    core.shutdown().await;
    assert_eq!(store.revision()?, built.revision);
    let before = store.rows(&panels)?;

    // Тот же id и `state_version`, новая кардинальность: строки панелей не менялись, но запись
    // запуска есть, и клиент с курсором прежнего владельца получает снимок.
    let core = store.start(singleton())?;
    let started = snapshot(&core).await?;
    assert_eq!(started.revision, built.revision.next());
    assert_eq!(
        core.events_since(built.revision).await?,
        EventsSince::Reset(started.clone())
    );
    let age = |panel: &PanelId| (started.state.panels[panel].created_at, *panel);
    let duplicate = panels
        .values()
        .max_by_key(|panel| age(panel))
        .ok_or("нет панелей")?;
    assert_eq!(
        started.state.panel_status,
        BTreeMap::from([(*duplicate, PanelStatus::Duplicate)])
    );
    core.shutdown().await;
    assert_eq!(store.load()?.fingerprint, Some(fingerprint(&singleton())));
    assert_eq!(store.rows(&panels)?, before);

    // Третий запуск с тем же реестром ревизию не меняет.
    let core = store.start(singleton())?;
    assert_eq!(snapshot(&core).await?, started);
    core.shutdown().await;
    assert_eq!(store.revision()?, started.revision);
    Ok(())
}

#[tokio::test]
async fn fixture_v2_gets_init_on_start_and_second_start_writes_nothing() -> TestResult {
    let store = Store::fixture_v2()?;
    let core = store.start(registry())?;
    let started = snapshot(&core).await?;
    assert_eq!(started.revision, Revision(21));
    let state = &started.state;
    assert_eq!(state.diagnostics, Vec::new());
    assert_eq!(state.check_invariants(), Vec::new());

    // Панели зарегистрированных плагинов получили `init` записью запуска; панель плагина,
    // которого в реестре нет, осталась без строки данных.
    let notes: Vec<_> = state
        .panels
        .values()
        .filter(|panel| panel.plugin.as_str() == "notes")
        .map(|panel| panel.id)
        .collect();
    assert_eq!(notes.len(), 1);
    assert_eq!(state.panels.len(), 7);
    assert_eq!(state.panel_data.len(), 6);
    assert!(
        state
            .panel_data
            .values()
            .all(|data| *data == text(1, "null"))
    );
    assert_eq!(state.panel_data.get(&notes[0]), None);
    assert_eq!(
        state.panel_status,
        BTreeMap::from([(notes[0], PanelStatus::PluginMissing)])
    );
    assert_eq!(
        core.events_since(Revision(20)).await?,
        EventsSince::Reset(started.clone())
    );
    core.shutdown().await;
    assert_eq!(store.count("panel_states")?, 6);
    assert_eq!(store.load()?.fingerprint, Some(fingerprint(&registry())));

    let core = store.start(registry())?;
    assert_eq!(snapshot(&core).await?, started);
    core.shutdown().await;
    assert_eq!(store.revision()?, Revision(21));
    Ok(())
}
