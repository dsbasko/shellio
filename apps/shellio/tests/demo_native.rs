//! Запросы тела панели на ядре в памяти (ADR-0008, «Уточнено в P1.5 (планирование)»): путь «вид →
//! отправитель хоста → команда ядра → исход телу» с подставными плагинами и с «Демо: поле». Сторону
//! окна заменяет [`Port`]: те же `bodies::sink`, `command_of`, `refusal` и `settle_request`, что у
//! адаптера, без AppKit.

// allow-unwrap-in-tests не распространяется на вспомогательные функции интеграционного теста.
#![allow(clippy::unwrap_used)]

use std::cell::RefCell;
use std::error::Error;
use std::rc::Rc;
use std::sync::Arc;

use plugin_demo_native::form::Form;
use plugin_demo_native::model::State as FieldState;
use plugin_demo_native::view;
use shellio::bodies::{self, Failure, LATE_DONE, Requests, SharedRequests, Ticket};
use shellio::plugins;
use shellio_appkit::late_callbacks;
use shellio_core::test_support::{
    FakeRootResolver, ManualClock, MemoryPersist, SequentialIds, TestPlugin, registry_of,
};
use shellio_core::{
    Command, CommandValue, Core, CoreError, CoreHandle, ErrorCode, OpenOutcome, PanelData, PanelId,
    ProjectId, Reply, ReplyValue, Revision, State, TreeScope, UnixMillis, WorkbenchId, WorkspaceId,
};
use shellio_layout::Viewport;
use shellio_plugin_api::{Cardinality, Registry};
use shellio_plugin_view::{PanelRequest, PanelSink};
use tokio::runtime::Handle;

type TestResult<T> = Result<T, Box<dyn Error>>;

const NOW: UnixMillis = UnixMillis(1_795_000_000_000);
const FIELD: &str = "demo-native";

/// Ядро в памяти с окном 1180×720 и двумя рабочими областями одного проекта.
struct World {
    core: CoreHandle,
    workspace: WorkspaceId,
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

async fn world(registry: Arc<Registry>) -> TestResult<World> {
    let core = Core::start(
        &Handle::current(),
        Box::new(MemoryPersist::new()),
        Box::new(ManualClock::new(NOW)),
        Box::new(SequentialIds::new()),
        Arc::new(FakeRootResolver::new()),
        registry,
    )?;
    let workspace = state(&core)
        .await?
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
        workspace,
        first,
        second,
    })
}

async fn state(core: &CoreHandle) -> TestResult<State> {
    Ok(State::clone(&core.subscribe().await?.0.state))
}

async fn revision(core: &CoreHandle) -> TestResult<Revision> {
    Ok(core.subscribe().await?.0.revision)
}

fn open_command(plugin: &str, workbench: WorkbenchId) -> Command {
    Command::PanelOpen {
        plugin: plugin.into(),
        workbench: Some(workbench),
        zone: None,
        args: None,
    }
}

/// `panel.open` плагина в рабочей области: панель, где она стоит, и исход.
async fn open(
    core: &CoreHandle,
    plugin: &str,
    workbench: WorkbenchId,
) -> TestResult<(PanelId, WorkbenchId, OpenOutcome)> {
    match value(core, open_command(plugin, workbench)).await? {
        CommandValue::Opened {
            panel,
            workbench,
            outcome,
        } => Ok((panel, workbench, outcome)),
        other => Err(format!("ожидался исход открытия, получено {other:?}").into()),
    }
}

fn raw(state: &State, panel: PanelId) -> &str {
    match state.panel_data.get(&panel) {
        Some(PanelData::Text { raw, .. }) => raw,
        Some(PanelData::Unreadable {}) | None => "нет данных",
    }
}

fn placed(state: &State, workbench: WorkbenchId, panel: PanelId) -> bool {
    state.layouts[&workbench]
        .panels()
        .into_iter()
        .any(|known| known == panel)
}

/// Сторона окна для тела одной панели: отправитель, который получает тело, запросы в пути и их
/// исходы.
struct Port {
    panel: PanelId,
    requests: SharedRequests,
    outbox: Rc<RefCell<Vec<(Ticket, PanelRequest)>>>,
    sink: PanelSink,
}

impl Port {
    /// Тело панели `panel` поколения `generation` построено.
    fn open(panel: PanelId, generation: u64) -> Port {
        let requests: SharedRequests = Rc::new(RefCell::new(Requests::default()));
        requests.borrow_mut().open(panel, generation);
        let outbox = Rc::new(RefCell::new(Vec::new()));
        let sink = bodies::sink(panel, generation, Rc::clone(&requests), {
            let outbox = Rc::clone(&outbox);
            move |ticket, request| outbox.borrow_mut().push((ticket, request))
        });
        Port {
            panel,
            requests,
            outbox,
            sink,
        }
    }

    /// Старейший запрос тела, который ещё не ушёл ядру.
    fn take(&self) -> (Ticket, PanelRequest) {
        self.outbox.borrow_mut().remove(0)
    }

    /// Команда ядру на запрос тела: рабочая область — та, где панель стоит сейчас.
    async fn run(
        &self,
        core: &CoreHandle,
        request: PanelRequest,
    ) -> TestResult<Result<Reply, CoreError>> {
        let workbench = state(core)
            .await?
            .panels
            .get(&self.panel)
            .map(|panel| panel.workbench);
        let command = bodies::command_of(self.panel, workbench, request);
        Ok(core.request(command).await)
    }

    /// Исход запроса возвращается телу, если его ещё ждут. `true` — тело его получило.
    fn settle(&self, ticket: Ticket, outcome: &Result<Reply, CoreError>) -> bool {
        let for_body = match outcome {
            Ok(_) => Ok(()),
            Err(error) => Err(bodies::refusal(&Failure::Core(error.clone()))),
        };
        match bodies::settle_request(&self.requests, ticket) {
            Some(done) => {
                done(for_body);
                true
            }
            None => false,
        }
    }

    /// Старейший запрос тела проходит весь путь: ядро и исход телу.
    async fn deliver(&self, core: &CoreHandle) -> TestResult<Result<Reply, CoreError>> {
        let (ticket, request) = self.take();
        let outcome = self.run(core, request).await?;
        assert!(self.settle(ticket, &outcome), "исход ждали");
        Ok(outcome)
    }
}

#[tokio::test]
async fn view_of_one_plugin_opens_a_panel_of_another_through_the_sink() -> TestResult<()> {
    // Две независимые регистрации: «alpha» знает о «beta» только строку plugin id в запросе.
    let registry = registry_of(vec![
        TestPlugin::new("alpha"),
        TestPlugin::new("beta").cardinality(Cardinality::PerApp),
    ]);
    let w = world(registry).await?;
    let (alpha, _, created) = open(&w.core, "alpha", w.second).await?;
    assert_eq!(created, OpenOutcome::Created);

    let port = Port::open(alpha, 1);
    let outcomes = Rc::new(RefCell::new(Vec::new()));
    let record = || {
        let outcomes = Rc::clone(&outcomes);
        Box::new(move |outcome: Result<(), _>| outcomes.borrow_mut().push(outcome.is_ok()))
    };
    let request = PanelRequest::Open {
        plugin: "beta".into(),
        args: None,
    };
    port.sink.send(request.clone(), record());
    assert!(outcomes.borrow().is_empty(), "отправка не ждёт ядра");

    let reply = port.deliver(&w.core).await??;
    let ReplyValue::Command(CommandValue::Opened {
        panel: beta,
        workbench,
        outcome,
    }) = reply.value
    else {
        return Err(format!("ожидался исход открытия, получено {:?}", reply.value).into());
    };
    assert_eq!(outcome, OpenOutcome::Created);
    assert_eq!(workbench, w.second, "панель открыта в рабочей области вида");
    assert_eq!(*outcomes.borrow(), [true], "исход дошёл до тела");

    let after = state(&w.core).await?;
    let record_of_beta = after.panels.get(&beta).ok_or("панели beta нет")?;
    assert_eq!(record_of_beta.plugin.as_str(), "beta");
    assert_eq!(record_of_beta.workbench, w.second);
    assert!(placed(&after, w.second, beta));
    assert_eq!(
        after.opened_from.get(&beta),
        Some(&alpha),
        "вызывающая панель — та, чьё тело прислало запрос"
    );

    // Повтор из того же тела — тот же экземпляр без новой панели.
    let panels = after.panels.len();
    port.sink.send(request, record());
    let reply = port.deliver(&w.core).await??;
    assert_eq!(
        reply.value,
        ReplyValue::Command(CommandValue::Opened {
            panel: beta,
            workbench: w.second,
            outcome: OpenOutcome::Focused,
        })
    );
    assert_eq!(state(&w.core).await?.panels.len(), panels);
    assert_eq!(*outcomes.borrow(), [true, true]);
    Ok(())
}

/// Путь `Return` тела «Демо: поле»: форма отдаёт значение, запрос уходит отправителю хоста, исход
/// возвращается в форму.
fn submit(form: &Rc<RefCell<Form>>, sink: &PanelSink, text: &str) {
    let value = form.borrow_mut().submit(text);
    let form = Rc::clone(form);
    sink.send(
        view::request(&value),
        Box::new(move |outcome| form.borrow_mut().done(outcome)),
    );
}

/// Данные панели доходят до формы, как их отдаёт телу хост.
async fn refresh(core: &CoreHandle, panel: PanelId, form: &RefCell<Form>) -> TestResult<()> {
    let state = state(core).await?;
    let data: FieldState = serde_json::from_str(raw(&state, panel))?;
    form.borrow_mut().update(data);
    Ok(())
}

async fn field(w: &World) -> TestResult<(PanelId, Rc<RefCell<Form>>, Port)> {
    let (panel, _, outcome) = open(&w.core, FIELD, w.first).await?;
    assert_eq!(outcome, OpenOutcome::Created);
    let form = Rc::new(RefCell::new(Form::new(FieldState::default())));
    refresh(&w.core, panel, &form).await?;
    Ok((panel, form, Port::open(panel, 1)))
}

#[tokio::test]
async fn refused_set_reaches_the_form_and_keeps_the_draft() -> TestResult<()> {
    let w = world(plugins::shared_registry()?).await?;
    let (panel, form, port) = field(&w).await?;
    let start = revision(&w.core).await?;

    let long = "x".repeat(257);
    submit(&form, &port.sink, &long);
    let error = port
        .deliver(&w.core)
        .await?
        .err()
        .ok_or("257 символов приняты")?;
    assert_eq!(error.code(), ErrorCode::InvalidArgument, "{error}");
    assert_eq!(error.detail_str("reason"), Some("value_too_long"));
    assert_eq!(error.detail_str("field"), Some("value"));
    assert_eq!(
        form.borrow().caption(),
        "Не сохранено: значение длиннее 256 символов: 257"
    );
    assert_eq!(form.borrow().draft(), long, "черновик остался в поле");
    assert_eq!(revision(&w.core).await?, start, "отказ без ревизии");
    assert_eq!(
        raw(&state(&w.core).await?, panel),
        r#"{"value":"","edits":0}"#
    );

    // Следующее успешное сохранение убирает причину: одна ревизия.
    submit(&form, &port.sink, "shellio-check");
    let saved = port.deliver(&w.core).await??.revision;
    assert_ne!(saved, start);
    refresh(&w.core, panel, &form).await?;
    assert_eq!(
        form.borrow().caption(),
        "Сохранено: shellio-check · правок 1"
    );
    assert!(!form.borrow().dirty());

    // Тот же текст: команда принята, ревизия прежняя.
    submit(&form, &port.sink, "shellio-check");
    assert_eq!(port.deliver(&w.core).await??.revision, saved);
    assert_eq!(revision(&w.core).await?, saved);
    assert_eq!(
        raw(&state(&w.core).await?, panel),
        r#"{"value":"shellio-check","edits":1}"#
    );
    Ok(())
}

fn late_done() -> u64 {
    late_callbacks()
        .iter()
        .find(|late| late.site == LATE_DONE)
        .map_or(0, |late| late.count)
}

#[tokio::test]
async fn outcome_of_an_older_request_and_of_a_closed_body_is_dropped() -> TestResult<()> {
    let w = world(plugins::shared_registry()?).await?;
    let (panel, form, port) = field(&w).await?;

    // Два запроса подряд: отказ первого телу уже не нужен.
    submit(&form, &port.sink, &"x".repeat(257));
    submit(&form, &port.sink, "b");
    let (older, long) = port.take();
    let (last, short) = port.take();
    let refused = port.run(&w.core, long).await?;
    assert!(refused.is_err());
    assert!(
        !port.settle(older, &refused),
        "исход более старого запроса отброшен"
    );
    assert_eq!(form.borrow().caption(), "Сохранено:  · правок 0");

    let accepted = port.run(&w.core, short).await?;
    assert!(accepted.is_ok());
    assert!(port.settle(last, &accepted));
    refresh(&w.core, panel, &form).await?;
    assert_eq!(form.borrow().caption(), "Сохранено: b · правок 1");

    // Тело закрыто, пока запрос в пути: исход не доходит и считается поздним.
    submit(&form, &port.sink, &"x".repeat(257));
    let (pending, long) = port.take();
    drop(port.requests.borrow_mut().close(panel));
    let late = late_done();
    let refused = port.run(&w.core, long).await?;
    assert!(refused.is_err());
    assert!(!port.settle(pending, &refused));
    assert!(late_done() > late);
    assert_eq!(
        form.borrow().caption(),
        "Сохранено: b · правок 1",
        "закрытое тело исхода не получило"
    );

    // Отправитель закрытого тела запрос не шлёт.
    submit(&form, &port.sink, "c");
    assert!(port.outbox.borrow().is_empty());
    Ok(())
}

#[tokio::test]
async fn field_moves_to_the_calling_workbench_with_its_data() -> TestResult<()> {
    let w = world(plugins::shared_registry()?).await?;
    let (panel, form, port) = field(&w).await?;
    submit(&form, &port.sink, "переезд");
    port.deliver(&w.core).await??;
    let before = state(&w.core).await?;
    let data = raw(&before, panel).to_owned();
    assert_eq!(data, r#"{"value":"переезд","edits":1}"#);
    assert!(placed(&before, w.first, panel));

    // Внешний вызов с областью воркспейса для `PerApp` отказывает до поиска экземпляра.
    let scoped = w
        .core
        .request_in(
            TreeScope::Workspace(w.workspace),
            open_command(FIELD, w.second),
        )
        .await
        .err()
        .ok_or("область воркспейса принята")?;
    assert_eq!(scoped.detail_str("reason"), Some("scope_requires_all"));

    let (moved, at, outcome) = open(&w.core, FIELD, w.second).await?;
    assert_eq!((moved, at, outcome), (panel, w.second, OpenOutcome::Moved));
    let after = state(&w.core).await?;
    assert_eq!(after.panels[&panel].workbench, w.second);
    assert!(!placed(&after, w.first, panel));
    assert!(placed(&after, w.second, panel));
    assert_eq!(
        raw(&after, panel),
        data,
        "данные переехали вместе с панелью"
    );
    assert!(
        after.opened_from.contains_key(&panel),
        "вызывающая панель целевой рабочей области записана"
    );

    // Запрос тела после переезда идёт в новую рабочую область панели.
    submit(&form, &port.sink, "после");
    port.deliver(&w.core).await??;
    assert_eq!(
        raw(&state(&w.core).await?, panel),
        r#"{"value":"после","edits":2}"#
    );
    Ok(())
}
