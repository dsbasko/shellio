//! Минимум «Демо: список» в командах раскладки: ядро берёт `min_size` панели из манифеста её плагина
//! в продуктовом реестре, поэтому отказы `split_no_room` и `resize_out_of_range` называют 260×160, а
//! не минимум по умолчанию.

// allow-unwrap-in-tests не распространяется на вспомогательные функции интеграционного теста.
#![allow(clippy::unwrap_used)]

use std::error::Error;
use std::sync::Arc;

use serde_json::{Value, json};
use shellio::plugins;
use shellio_core::test_support::{FakeRootResolver, ManualClock, MemoryPersist, SequentialIds};
use shellio_core::{
    Command, CommandValue, Core, CoreError, CoreHandle, ErrorCode, OpenOutcome, PanelId, ProjectId,
    ReplyValue, UnixMillis, WorkbenchId,
};
use shellio_layout::{MinSize, Viewport, Zone};
use shellio_plugin_api::PluginId;
use tokio::runtime::Handle;

type TestResult<T> = Result<T, Box<dyn Error>>;

const NOW: UnixMillis = UnixMillis(1_795_000_000_000);
const DEMO: &str = "demo-list";

/// Ядро в памяти с продуктовым реестром, окном 1180×720 и двумя рабочими областями одного проекта.
struct World {
    core: CoreHandle,
    first: WorkbenchId,
    second: WorkbenchId,
}

async fn value(core: &CoreHandle, command: Command) -> TestResult<CommandValue> {
    match core.request(command).await?.value {
        ReplyValue::Command(value) => Ok(value),
        other => Err(format!("ожидался ответ команды, получено {other:?}").into()),
    }
}

async fn workbench(core: &CoreHandle, project: ProjectId, title: &str) -> TestResult<WorkbenchId> {
    let create = Command::WorkbenchCreate {
        project,
        title: title.into(),
    };
    match value(core, create).await? {
        CommandValue::Workbench { workbench } => Ok(workbench),
        other => Err(format!("ожидалась рабочая область, получено {other:?}").into()),
    }
}

async fn world() -> TestResult<World> {
    let core = Core::start(
        &Handle::current(),
        Box::new(MemoryPersist::new()),
        Box::new(ManualClock::new(NOW)),
        Box::new(SequentialIds::new()),
        Arc::new(FakeRootResolver::new()),
        plugins::shared_registry()?,
    )?;
    let workspace = core
        .subscribe()
        .await?
        .0
        .state
        .window
        .active_workspace
        .ok_or("нет активного воркспейса")?;
    let add = Command::ProjectAdd {
        workspace,
        path: "/src/home".into(),
        title: None,
    };
    let CommandValue::Project { project, .. } = value(&core, add).await? else {
        return Err("ожидался проект".into());
    };
    let first = workbench(&core, project, "Первая").await?;
    let second = workbench(&core, project, "Вторая").await?;
    core.set_viewport(Some(Viewport {
        width: 1180,
        height: 720,
    }));
    Ok(World {
        core,
        first,
        second,
    })
}

/// Открывает «Демо: список» без зоны: панель встаёт в правую зону манифеста.
async fn open_demo(core: &CoreHandle, workbench: WorkbenchId) -> TestResult<PanelId> {
    let open = Command::PanelOpen {
        plugin: DEMO.into(),
        workbench: Some(workbench),
        zone: None,
        args: None,
    };
    match value(core, open).await? {
        CommandValue::Opened {
            panel,
            outcome: OpenOutcome::Created,
            ..
        } => Ok(panel),
        other => Err(format!("ожидалась новая панель, получено {other:?}").into()),
    }
}

fn refused(error: &CoreError, reason: &str, field: &str) {
    assert_eq!(error.code(), ErrorCode::InvalidArgument, "{error}");
    assert_eq!(error.detail_str("reason"), Some(reason), "{error}");
    assert_eq!(error.detail_str("field"), Some(field), "{error}");
}

/// Наименьшая ширина правой зоны рабочей области из отказа `resize_out_of_range`.
async fn right_zone_min(core: &CoreHandle, workbench: WorkbenchId) -> TestResult<u64> {
    let resize = Command::LayoutResize {
        workbench: Some(workbench),
        zone: Zone::Right,
        part_of: None,
        size: 10,
    };
    let error = core
        .request(resize)
        .await
        .err()
        .ok_or("правая зона шириной 10 точек принята")?;
    refused(&error, "resize_out_of_range", "size");
    let min = error
        .detail("min")
        .and_then(Value::as_u64)
        .ok_or("нет нижней границы")?;
    let max = error
        .detail("max")
        .and_then(Value::as_u64)
        .ok_or("нет верхней границы")?;
    assert!(min < max, "{min}..={max}");
    Ok(min)
}

#[test]
fn demo_declares_a_minimum_above_the_default() {
    let registry = plugins::registry().unwrap();
    let demo = registry.get(&PluginId::new(DEMO).unwrap()).unwrap();
    let min_size = demo.manifest().min_size;
    assert_eq!((min_size.width, min_size.height), (260.0, 160.0));
    assert!(min_size.width > MinSize::DEFAULT.width);
    assert!(min_size.height > MinSize::DEFAULT.height);
}

#[tokio::test]
async fn split_without_room_names_the_minimum_of_the_demo() -> TestResult<()> {
    let w = world().await?;
    let demo = open_demo(&w.core, w.first).await?;

    // Части правой зоны стоят одна над другой. Часть демо делится, пока её половина не ниже 160.
    let mut splits = 0;
    let error = loop {
        let split = Command::LayoutSplit {
            panel: demo,
            open: Some("terminal".into()),
            axis: None,
        };
        match w.core.request(split).await {
            Ok(_) => {
                splits += 1;
                assert!(splits < 8, "деление не отказало");
            }
            Err(error) => break error,
        }
    };
    assert!(splits >= 1, "первое деление правой зоны помещается");
    refused(&error, "split_no_room", "panel");

    let parts = error
        .detail("parts")
        .and_then(Value::as_array)
        .ok_or("нет частей в отказе")?;
    assert_eq!(parts.len(), 2);
    let (of_demo, of_new): (Vec<&Value>, Vec<&Value>) = parts
        .iter()
        .partition(|part| part["panel"] == json!(demo.to_string()));
    let [of_demo] = of_demo.as_slice() else {
        return Err(format!("часть демо в отказе одна: {parts:?}").into());
    };
    assert_eq!(of_demo["need"], json!({"width": 260.0, "height": 160.0}));
    let have = &of_demo["have"];
    assert!(
        have["height"].as_f64() < Some(160.0) && have["width"].as_f64() >= Some(260.0),
        "части демо не хватает высоты: {have}"
    );
    let [of_new] = of_new.as_slice() else {
        return Err(format!("новая часть в отказе одна: {parts:?}").into());
    };
    assert_eq!(of_new["need"], json!({"width": 200.0, "height": 120.0}));
    // Терминалу в той же половине места хватило бы: отказ дал минимум демо.
    assert!(
        have["height"].as_f64() >= Some(120.0),
        "половина части ниже и минимума по умолчанию: {have}"
    );
    Ok(())
}

#[tokio::test]
async fn resize_out_of_range_counts_the_minimum_of_the_demo() -> TestResult<()> {
    let w = world().await?;
    open_demo(&w.core, w.first).await?;
    let with_demo = right_zone_min(&w.core, w.first).await?;

    // Та же зона с панелью минимума по умолчанию — в соседней рабочей области.
    let open = Command::PanelOpen {
        plugin: "terminal".into(),
        workbench: Some(w.second),
        zone: Some(Zone::Right),
        args: None,
    };
    value(&w.core, open).await?;
    let with_terminal = right_zone_min(&w.core, w.second).await?;

    assert_eq!(
        with_demo - with_terminal,
        60,
        "нижняя граница зоны выше ровно на разницу минимумов 260 и 200: {with_demo} и {with_terminal}"
    );
    assert!(with_demo >= 262, "{with_demo}");
    Ok(())
}
