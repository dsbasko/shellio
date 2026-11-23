//! Зеркало состояния против актора ядра: снимок и события дают состояние актора, разрыв и
//! отставание требуют снимка, проекции следуют событиям.
//!
//! Случайность — детерминированный генератор с фиксированными зёрнами, синхронизация — только
//! квитанции команд и каналы подписки.

use std::error::Error;
use std::sync::Arc;

use shellio_core::test_support::{
    FakeRootResolver, FaultyPersist, ManualClock, MemoryPersist, SequentialIds,
};
use shellio_core::{
    BOOTSTRAP_WORKSPACE, Command, CommandValue, Core, CoreHandle, Delivery, Event, Mirror,
    MirrorStep, ProjectId, ReplyValue, Revision, SUBSCRIBER_CAPACITY, Snapshot, State, Title,
    UnixMillis, WorkbenchId, WorkspaceId, window_title, workspace_name,
};
use tokio::runtime::Handle;
use tokio::sync::broadcast::Receiver;
use tokio::sync::broadcast::error::{RecvError, TryRecvError};

type TestResult<T> = Result<T, Box<dyn Error>>;

const NOW: UnixMillis = UnixMillis(1_795_000_000_000);

fn start() -> TestResult<CoreHandle> {
    let (persist, _) = FaultyPersist::new(MemoryPersist::new());
    Ok(Core::start(
        &Handle::current(),
        Box::new(persist),
        Box::new(ManualClock::new(NOW)),
        Box::new(SequentialIds::new()),
        Arc::new(FakeRootResolver::new()),
    )?)
}

async fn snapshot(core: &CoreHandle) -> TestResult<Snapshot> {
    Ok(core.subscribe().await?.0)
}

fn title(name: &str) -> TestResult<Title> {
    Ok(Title::new(name)?)
}

/// SplitMix64: воспроизводимая последовательность без внешних зависимостей.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }

    fn pick<T: Copy>(&mut self, items: &[T]) -> Option<T> {
        (!items.is_empty()).then(|| items[self.below(items.len())])
    }
}

/// Небольшой набор имён: совпадения дают отказы `name_taken`, которые события не порождают.
const NAMES: [&str; 6] = ["Работа", "Дом", "orbit", "feature-sso", "Ревью", "Сад"];

/// Известные идентификаторы для генерации команд.
#[derive(Default)]
struct Known {
    workspaces: Vec<WorkspaceId>,
    projects: Vec<ProjectId>,
    workbenches: Vec<WorkbenchId>,
}

impl Known {
    fn command(&self, rng: &mut Rng) -> Option<Command> {
        let name = NAMES[rng.below(NAMES.len())].to_owned();
        Some(match rng.below(7) {
            0 => Command::WorkspaceCreate { title: name },
            1 => Command::WorkspaceRename {
                workspace: rng.pick(&self.workspaces)?,
                title: name,
            },
            2 => Command::ProjectAdd {
                workspace: rng.pick(&self.workspaces)?,
                path: format!("/src/p{}", rng.below(12)).into(),
                title: Some(name),
            },
            3 => Command::WorkbenchCreate {
                project: rng.pick(&self.projects)?,
                title: name,
            },
            4 => Command::WorkbenchRename {
                workbench: rng.pick(&self.workbenches)?,
                title: name,
            },
            5 => Command::WorkbenchOpen {
                workbench: rng.pick(&self.workbenches)?,
            },
            _ => Command::WindowFocusWorkspace {
                workspace: rng.pick(&self.workspaces)?,
            },
        })
    }

    fn record(&mut self, value: CommandValue) {
        match value {
            CommandValue::Workspace { workspace } => self.workspaces.push(workspace),
            CommandValue::Project { project, .. } => self.projects.push(project),
            CommandValue::Workbench { workbench } => self.workbenches.push(workbench),
            CommandValue::Panel { .. } | CommandValue::None => {}
        }
    }
}

/// Подписка, взятая между командами.
struct Subscription {
    snapshot: Snapshot,
    events: Receiver<Event>,
}

/// Зеркало подписки: снимок плюс все доставленные события. Случайные повторы прошлых
/// событий пропускаются и состояние не трогают.
fn replay(subscription: Subscription, rng: &mut Rng) -> TestResult<Mirror> {
    let Subscription {
        snapshot,
        mut events,
    } = subscription;
    let mut mirror = Mirror::from_snapshot(snapshot);
    let mut delivered: Vec<Event> = Vec::new();
    loop {
        let event = match events.try_recv() {
            Ok(event) => event,
            Err(TryRecvError::Empty) => return Ok(mirror),
            Err(error) => return Err(format!("подписка: {error:?}").into()),
        };
        let expected = mirror.revision().next();
        assert_eq!(event.revision, expected, "события подписки идут подряд");
        assert_eq!(
            mirror.apply(Delivery::Event(event.clone())),
            MirrorStep::Applied
        );
        assert_eq!(mirror.revision(), expected);
        delivered.push(event);
        if rng.below(4) == 0 {
            let before = mirror.clone();
            let repeat = delivered[rng.below(delivered.len())].clone();
            assert_eq!(mirror.apply(Delivery::Event(repeat)), MirrorStep::Ignored);
            assert_eq!(mirror, before, "повтор изменил зеркало");
        }
    }
}

#[tokio::test]
async fn snapshot_plus_events_equals_actor_state_for_random_commands() {
    const COMMANDS: usize = 160;
    // Подписчики читают после всех команд: событий меньше ёмкости их канала.
    const _: () = assert!(COMMANDS < SUBSCRIBER_CAPACITY);

    for seed in [1_u64, 7, 42, 2026, 0xDEAD_BEEF] {
        let mut rng = Rng(seed);
        let core = start().unwrap();
        let first = snapshot(&core).await.unwrap();
        let mut known = Known {
            workspaces: first.state.workspaces.keys().copied().collect(),
            ..Known::default()
        };
        let mut subscriptions = Vec::new();
        let mut refused = 0;
        for _ in 0..COMMANDS {
            if rng.below(8) == 0 {
                let (snapshot, events) = core.subscribe().await.unwrap();
                subscriptions.push(Subscription { snapshot, events });
            }
            let Some(command) = known.command(&mut rng) else {
                continue;
            };
            match core.request(command).await {
                Ok(reply) => {
                    if let ReplyValue::Command(value) = reply.value {
                        known.record(value);
                    }
                }
                Err(_) => refused += 1,
            }
        }
        let last = snapshot(&core).await.unwrap();
        let written = last.revision.0 - first.revision.0;
        assert!(
            written > (COMMANDS / 3) as u64,
            "зерно {seed}: записано {written} команд"
        );
        assert!(refused > 0, "зерно {seed}: нет отказов");
        assert!(subscriptions.len() > 3, "зерно {seed}: мало подписок");

        for (index, subscription) in subscriptions.into_iter().enumerate() {
            let mirror = replay(subscription, &mut rng).unwrap();
            assert_eq!(
                mirror.revision(),
                last.revision,
                "зерно {seed}, подписка {index}"
            );
            assert_eq!(
                *mirror.state(),
                *last.state,
                "зерно {seed}, подписка {index}"
            );
            assert_eq!(mirror.window_title(), window_title(&last.state));
        }
        assert_eq!(last.state.check_invariants(), Vec::new());
    }
}

#[tokio::test]
async fn lost_event_needs_snapshot_and_fresh_snapshot_continues() {
    let core = start().unwrap();
    let first = snapshot(&core).await.unwrap();
    let workspace = first.state.window.active_workspace.unwrap();
    let (snapshot_before, mut events) = core.subscribe().await.unwrap();
    let mut mirror = Mirror::from_snapshot(snapshot_before);

    for name in ["Дом", "Сад", "Работа"] {
        core.request(Command::WorkspaceRename {
            workspace,
            title: name.into(),
        })
        .await
        .unwrap();
    }
    let lost = events.recv().await.unwrap();
    let after_gap = events.recv().await.unwrap();
    assert_eq!(after_gap.revision, lost.revision.next());
    let before = mirror.clone();
    assert_eq!(
        mirror.apply(Delivery::Event(after_gap.clone())),
        MirrorStep::NeedSnapshot
    );
    assert_eq!(mirror, before, "разрыв изменил зеркало");

    let fresh = snapshot(&core).await.unwrap();
    assert_eq!(mirror.apply(Delivery::Snapshot(fresh)), MirrorStep::Applied);
    assert_eq!(
        mirror.apply(Delivery::Event(after_gap)),
        MirrorStep::Ignored,
        "событие до снимка"
    );
    let tail = events.recv().await.unwrap();
    assert_eq!(
        mirror.apply(Delivery::Event(tail)),
        MirrorStep::Ignored,
        "последнее событие уже в снимке"
    );
    assert_eq!(mirror.workspace_name(), Some(&title("Работа").unwrap()));
    assert_eq!(*mirror.state(), *snapshot(&core).await.unwrap().state);
}

#[tokio::test]
async fn lagging_subscription_resets_through_new_snapshot() {
    let core = start().unwrap();
    let workspace = snapshot(&core)
        .await
        .unwrap()
        .state
        .window
        .active_workspace
        .unwrap();
    let (first, mut events) = core.subscribe().await.unwrap();
    let mut mirror = Mirror::from_snapshot(first);
    let rename = |index: usize| Command::WorkspaceRename {
        workspace,
        title: NAMES[index % 2].into(),
    };

    for index in 0..=SUBSCRIBER_CAPACITY {
        core.request(rename(index)).await.unwrap();
    }
    let lagged = events.recv().await;
    assert!(matches!(lagged, Err(RecvError::Lagged(1))), "{lagged:?}");
    let before = mirror.clone();
    assert_eq!(mirror.apply(Delivery::Reset), MirrorStep::NeedSnapshot);
    assert_eq!(mirror, before);

    let (fresh, mut events) = core.subscribe().await.unwrap();
    let fresh_revision = fresh.revision;
    assert_eq!(mirror.apply(Delivery::Snapshot(fresh)), MirrorStep::Applied);
    assert_eq!(mirror.revision(), fresh_revision);

    for index in 0..5 {
        core.request(rename(index + 1)).await.unwrap();
        let event = events.recv().await.unwrap();
        assert_eq!(mirror.apply(Delivery::Event(event)), MirrorStep::Applied);
    }
    let last = snapshot(&core).await.unwrap();
    assert_eq!(mirror.revision(), last.revision);
    assert_eq!(*mirror.state(), *last.state);
}

/// Подписанное зеркало, догоняющее ядро по событиям после каждой команды.
struct Follower {
    mirror: Mirror,
    events: Receiver<Event>,
}

impl Follower {
    async fn new(core: &CoreHandle) -> TestResult<Self> {
        let (snapshot, events) = core.subscribe().await?;
        Ok(Self {
            mirror: Mirror::from_snapshot(snapshot),
            events,
        })
    }

    /// Выполнить команду и применить её событие, если оно есть.
    async fn run(&mut self, core: &CoreHandle, command: Command) -> TestResult<CommandValue> {
        let reply = core.request(command).await?;
        while self.mirror.revision() < reply.revision {
            let event = self.events.recv().await?;
            assert_eq!(
                self.mirror.apply(Delivery::Event(event)),
                MirrorStep::Applied
            );
        }
        assert_eq!(self.mirror.revision(), reply.revision);
        match reply.value {
            ReplyValue::Command(value) => Ok(value),
            other => Err(format!("ожидался ответ команды, получено {other:?}").into()),
        }
    }

    fn title(&self) -> Option<&str> {
        self.mirror.workspace_name().map(Title::as_str)
    }
}

#[tokio::test]
async fn projections_follow_window_place() {
    let core = start().unwrap();
    let mut follower = Follower::new(&core).await.unwrap();
    assert_eq!(follower.title(), Some(BOOTSTRAP_WORKSPACE));
    assert_eq!(
        follower.mirror.window_title().as_deref(),
        Some(BOOTSTRAP_WORKSPACE),
        "главная воркспейса: без области"
    );

    let CommandValue::Workspace { workspace: work } = follower
        .run(
            &core,
            Command::WorkspaceCreate {
                title: "Работа".into(),
            },
        )
        .await
        .unwrap()
    else {
        panic!("ожидался воркспейс");
    };
    assert_eq!(
        follower.mirror.window_title().as_deref(),
        Some(BOOTSTRAP_WORKSPACE),
        "новый неактивный воркспейс не меняет заголовок"
    );

    let CommandValue::Project { project, .. } = follower
        .run(
            &core,
            Command::ProjectAdd {
                workspace: work,
                path: "/src/orbit".into(),
                title: Some("orbit".into()),
            },
        )
        .await
        .unwrap()
    else {
        panic!("ожидался проект");
    };
    let CommandValue::Workbench { workbench } = follower
        .run(
            &core,
            Command::WorkbenchCreate {
                project,
                title: "feature-sso".into(),
            },
        )
        .await
        .unwrap()
    else {
        panic!("ожидалась рабочая область");
    };
    follower
        .run(&core, Command::WorkbenchOpen { workbench })
        .await
        .unwrap();
    assert_eq!(follower.title(), Some("Работа"));
    assert_eq!(
        follower.mirror.window_title().as_deref(),
        Some("Работа — orbit / feature-sso"),
        "открытое место"
    );

    follower
        .run(
            &core,
            Command::WorkspaceRename {
                workspace: work,
                title: "Дом".into(),
            },
        )
        .await
        .unwrap();
    follower
        .run(
            &core,
            Command::WorkbenchRename {
                workbench,
                title: "Ревью".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(follower.title(), Some("Дом"));
    assert_eq!(
        follower.mirror.window_title().as_deref(),
        Some("Дом — orbit / Ревью"),
        "переименования без нового снимка"
    );

    let bootstrap = *follower
        .mirror
        .state()
        .workspaces
        .keys()
        .find(|id| **id != work)
        .unwrap();
    follower
        .run(
            &core,
            Command::WindowFocusWorkspace {
                workspace: bootstrap,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        follower.mirror.window_title().as_deref(),
        Some(BOOTSTRAP_WORKSPACE)
    );
    assert_eq!(
        *follower.mirror.state(),
        *snapshot(&core).await.unwrap().state
    );
}

/// Слот без зеркала: событие и `Reset` требуют снимка и слот не трогают, снимок заводит зеркало,
/// дальше доставка идёт в [`Mirror::apply`].
#[test]
fn empty_slot_waits_for_snapshot_then_applies() {
    let event = |revision: u64| {
        Delivery::Event(Event {
            revision: Revision(revision),
            changes: Vec::new(),
        })
    };
    let mut slot = None;
    assert_eq!(
        Mirror::deliver_to(&mut slot, event(3)),
        MirrorStep::NeedSnapshot
    );
    assert_eq!(
        Mirror::deliver_to(&mut slot, Delivery::Reset),
        MirrorStep::NeedSnapshot
    );
    assert!(slot.is_none());

    let snapshot = Snapshot {
        revision: Revision(2),
        state: Arc::new(State::default()),
    };
    assert_eq!(
        Mirror::deliver_to(&mut slot, Delivery::Snapshot(snapshot.clone())),
        MirrorStep::Applied
    );
    assert_eq!(slot.as_ref().map(Mirror::revision), Some(Revision(2)));
    assert_eq!(Mirror::deliver_to(&mut slot, event(2)), MirrorStep::Ignored);
    assert_eq!(
        Mirror::deliver_to(&mut slot, event(4)),
        MirrorStep::NeedSnapshot
    );
    assert_eq!(Mirror::deliver_to(&mut slot, event(3)), MirrorStep::Applied);
    assert_eq!(slot.as_ref().map(Mirror::revision), Some(Revision(3)));
    assert_eq!(
        Mirror::deliver_to(&mut slot, Delivery::Snapshot(snapshot)),
        MirrorStep::Ignored
    );
}

#[test]
fn projections_without_workspace_or_place() {
    let empty = State::default();
    assert_eq!(workspace_name(&empty), None);
    assert_eq!(window_title(&empty), None);
    let mirror = Mirror::from_snapshot(Snapshot {
        revision: Revision::ZERO,
        state: Arc::new(State::default()),
    });
    assert_eq!(mirror.workspace_name(), None);
    assert_eq!(mirror.window_title(), None);
}

#[tokio::test]
async fn projections_skip_missing_or_foreign_place() {
    let core = start().unwrap();
    let mut follower = Follower::new(&core).await.unwrap();
    let home = follower.mirror.state().window.active_workspace.unwrap();
    let CommandValue::Workspace { workspace: work } = follower
        .run(
            &core,
            Command::WorkspaceCreate {
                title: "Работа".into(),
            },
        )
        .await
        .unwrap()
    else {
        panic!("ожидался воркспейс");
    };
    let CommandValue::Project { project, .. } = follower
        .run(
            &core,
            Command::ProjectAdd {
                workspace: work,
                path: "/src/orbit".into(),
                title: Some("orbit".into()),
            },
        )
        .await
        .unwrap()
    else {
        panic!("ожидался проект");
    };
    let CommandValue::Workbench { workbench } = follower
        .run(
            &core,
            Command::WorkbenchCreate {
                project,
                title: "feature-sso".into(),
            },
        )
        .await
        .unwrap()
    else {
        panic!("ожидалась рабочая область");
    };
    follower
        .run(&core, Command::WorkbenchOpen { workbench })
        .await
        .unwrap();

    let mut foreign = follower.mirror.state().clone();
    foreign.window.active_workspace = Some(home);
    assert_eq!(
        window_title(&foreign).as_deref(),
        Some(BOOTSTRAP_WORKSPACE),
        "область чужого воркспейса"
    );

    let mut missing = follower.mirror.state().clone();
    missing.workbenches.clear();
    assert_eq!(
        window_title(&missing).as_deref(),
        Some("Работа"),
        "области нет"
    );

    let mut removed = follower.mirror.state().clone();
    removed.projects.get_mut(&project).unwrap().removed_at = Some(NOW);
    assert_eq!(
        window_title(&removed).as_deref(),
        Some("Работа — orbit / feature-sso"),
        "убранный проект открытой области остаётся в заголовке"
    );

    let mut deleted = follower.mirror.state().clone();
    deleted.workspaces.get_mut(&work).unwrap().deleted_at = Some(NOW);
    assert_eq!(workspace_name(&deleted), None);
    assert_eq!(window_title(&deleted), None);
    assert_eq!(
        workspace_name(follower.mirror.state()),
        Some(&title("Работа").unwrap())
    );
}
