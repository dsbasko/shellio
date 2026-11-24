//! Фикстуры SQL сценария `scripts/e2e/plugins.sh`: каждая загружается системным `sqlite3` во
//! временную базу и несёт нужную версию схемы и строки своего пункта матрицы.

use std::path::{Path, PathBuf};
use std::process::Command;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const SQLITE: &str = "/usr/bin/sqlite3";

fn repo(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(path)
}

fn sqlite(db: &Path, sql: &str) -> Result<String> {
    let output = Command::new(SQLITE).arg(db).arg(sql).env_clear().output()?;
    if !output.status.success() {
        return Err(format!("sqlite3: {}", String::from_utf8_lossy(&output.stderr)).into());
    }
    Ok(String::from_utf8(output.stdout)?.trim_end().to_owned())
}

/// База во временном каталоге, заполненная файлом SQL так же, как это делает сценарий: файл
/// идёт на стандартный ввод `sqlite3`.
fn load(dir: &Path, name: &str, file: &Path) -> Result<PathBuf> {
    let db = dir.join(name);
    let output = Command::new(SQLITE)
        .arg(&db)
        .stdin(std::fs::File::open(file)?)
        .env_clear()
        .output()?;
    if !output.status.success() || !output.stderr.is_empty() {
        return Err(format!(
            "sqlite3 < {}: {}",
            file.display(),
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(db)
}

fn fixture(dir: &Path, name: &str) -> Result<PathBuf> {
    load(
        dir,
        &format!("{name}.db"),
        &repo("scripts/e2e/plugins-fixtures").join(format!("{name}.sql")),
    )
}

/// Таблицы базы: имя, признак STRICT, столбцы с типом, `NOT NULL` и ключом, внешние ключи.
fn shape(db: &Path) -> Result<String> {
    sqlite(
        db,
        "SELECT m.name, l.strict, \
           (SELECT group_concat(name || ' ' || type || ' ' || \"notnull\" || ' ' || pk, ', ') \
              FROM pragma_table_info(m.name)), \
           (SELECT group_concat(\"from\" || '>' || \"table\" || '.' || \"to\" || ' ' || on_delete, ', ') \
              FROM (SELECT * FROM pragma_foreign_key_list(m.name) ORDER BY \"from\")) \
         FROM sqlite_master m JOIN pragma_table_list l ON l.name = m.name \
         WHERE m.type = 'table' ORDER BY m.name;",
    )
}

fn id(tail: &str) -> String {
    format!("019A8F2E00007000800000000000{tail}")
}

/// Фикстуры схемы v3 повторяют образец свежей схемы хранилища таблица в таблицу.
#[test]
fn v3_fixtures_have_the_schema_of_the_store() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let want = shape(&load(
        dir.path(),
        "schema.db",
        &repo("crates/shellio-store/tests/schema_v3.sql"),
    )?)?;
    assert!(want.contains("panel_states|0|"), "{want}");
    for name in ["v3-missing-plugin", "v3-status"] {
        let db = fixture(dir.path(), name)?;
        assert_eq!(sqlite(&db, "PRAGMA user_version;")?, "3", "{name}");
        assert_eq!(shape(&db)?, want, "{name}");
        assert_eq!(sqlite(&db, "PRAGMA foreign_key_check;")?, "", "{name}");
        assert_eq!(
            sqlite(&db, "SELECT count(*) FROM plugin_registry;")?,
            "0",
            "{name}"
        );
    }
    Ok(())
}

/// `missing-plugin`: открытая рабочая область с панелью незарегистрированного плагина и её данными.
#[test]
fn missing_plugin_fixture_holds_the_gone_plugin_panel_with_its_bytes() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = fixture(dir.path(), "v3-missing-plugin")?;
    assert_eq!(
        sqlite(
            &db,
            "SELECT hex(id), plugin, title, hex(workbench) FROM panels ORDER BY id;"
        )?,
        format!(
            "{}|navigator|Навигатор|{bench}\n{}|terminal|Терминал|{bench}\n{}|gone-plugin|gone-plugin|{bench}",
            id("E201"),
            id("E202"),
            id("E203"),
            bench = id("C201"),
        )
    );
    // Данные {"x":1} побайтно.
    assert_eq!(
        sqlite(
            &db,
            "SELECT hex(panel), version, typeof(data), hex(CAST(data AS BLOB)) FROM panel_states \
             ORDER BY panel;"
        )?,
        format!(
            "{}|1|text|6E756C6C\n{}|1|text|6E756C6C\n{}|1|text|7B2278223A317D",
            id("E201"),
            id("E202"),
            id("E203"),
        )
    );
    assert_eq!(
        sqlite(
            &db,
            "SELECT hex(route_workbench_id), \
               (SELECT hex(last_workbench_id) FROM workspaces), \
               (SELECT hex(workbench) FROM workbench_layouts) FROM window_state;"
        )?,
        [id("C201"), id("C201"), id("C201")].join("|")
    );
    let layout = sqlite(&db, "SELECT layout FROM workbench_layouts;")?;
    let layout: serde_json::Value = serde_json::from_str(&layout)?;
    let zones = &layout["layout"]["zones"];
    assert_eq!(
        layout["layout"]["navigator"],
        "019a8f2e-0000-7000-8000-00000000e201"
    );
    assert_eq!(
        zones["center"]["parts"][0]["tabs"],
        serde_json::json!(["019a8f2e-0000-7000-8000-00000000e202"])
    );
    assert_eq!(
        zones["right"]["parts"][0]["tabs"],
        serde_json::json!(["019a8f2e-0000-7000-8000-00000000e203"])
    );
    Ok(())
}

/// `status-stubs`: по одной панели каждого демо, список с версией новее, поле с неразбираемым текстом.
#[test]
fn status_fixture_holds_a_newer_list_and_a_corrupt_field() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = fixture(dir.path(), "v3-status")?;
    assert_eq!(
        sqlite(
            &db,
            "SELECT p.plugin, p.title, s.version, s.data FROM panels p \
             JOIN panel_states s ON s.panel = p.id ORDER BY p.id;"
        )?,
        "navigator|Навигатор|1|null\n\
         terminal|Терминал|1|null\n\
         demo-list|Демо: список|2|{\"key\":\"a\",\"history\":[],\"details\":false}\n\
         demo-native|Демо: поле|1|{"
    );
    // Обе панели демо канонические: по одной на плагин во всей базе.
    assert_eq!(
        sqlite(
            &db,
            "SELECT plugin, count(*) FROM panels WHERE plugin LIKE 'demo-%' GROUP BY plugin;"
        )?,
        "demo-list|1\ndemo-native|1"
    );
    let layout = sqlite(&db, "SELECT layout FROM workbench_layouts;")?;
    let layout: serde_json::Value = serde_json::from_str(&layout)?;
    let zones = &layout["layout"]["zones"];
    // Обе заглушки показаны: поле — активная вкладка центра, список — в правой зоне.
    let center = &zones["center"]["parts"][0];
    assert_eq!(
        center["tabs"][center["active"].as_u64().ok_or("active")? as usize],
        "019a8f2e-0000-7000-8000-00000000e304"
    );
    assert_eq!(
        zones["right"]["parts"][0]["tabs"],
        serde_json::json!(["019a8f2e-0000-7000-8000-00000000e303"])
    );
    Ok(())
}

/// `legacy-duplicate`: схема v2 без таблиц v3, две панели `demo-native` в разных рабочих областях,
/// старшая — в открытой A.
#[test]
fn duplicate_fixture_is_a_v2_base_with_two_native_panels() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = fixture(dir.path(), "v2-duplicate-demo-native")?;
    assert_eq!(sqlite(&db, "PRAGMA user_version;")?, "2");
    assert_eq!(sqlite(&db, "PRAGMA foreign_key_check;")?, "");
    assert_eq!(
        sqlite(
            &db,
            "SELECT count(*) FROM sqlite_master WHERE name IN ('panel_states', 'plugin_registry');"
        )?,
        "0"
    );
    assert_eq!(
        sqlite(
            &db,
            "SELECT count(*) FROM pragma_table_info('panels') WHERE name = 'opened_from';"
        )?,
        "0"
    );
    assert_eq!(
        sqlite(
            &db,
            "SELECT hex(p.id), w.title, p.title FROM panels p JOIN workbenches w ON w.id = p.workbench \
             WHERE p.plugin = 'demo-native' ORDER BY p.created_at, p.id;"
        )?,
        format!("{}|A|Демо: поле\n{}|B|Демо: поле", id("E403"), id("E406"))
    );
    assert_eq!(
        sqlite(
            &db,
            "SELECT w.title FROM window_state s JOIN workbenches w ON w.id = s.route_workbench_id;"
        )?,
        "A"
    );
    assert_eq!(sqlite(&db, "SELECT count(*) FROM workbench_layouts;")?, "2");
    Ok(())
}
