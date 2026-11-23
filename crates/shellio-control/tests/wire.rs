//! Эталоны JSON протокола v1 в `tests/wire/*.json` и отказы разбора кадров.
//!
//! Каждый эталон сравнивается со своим образцом в обе стороны: сериализация образца равна
//! эталону как значение JSON, разбор эталона даёт образец. Значения сравниваются, а не текст.
//! Эталон меняется только вместе с записью причины в плане: после публикации v1 формат
//! меняется только новыми вариантами.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use shellio_control::{
    Accepted, Activation, AnimationPath, AnimationRecord, Axis, AxisSource, ChromeView,
    ClassAliveView, ColorSpaceView, ComponentState, ComponentView, Contrast, ControlError,
    ControlErrorCode, GalleryCommand, GalleryFocus, GalleryStrip, GalleryTreeView, GalleryView,
    Hello, HelloReply, InspectView, InstanceKind, InstanceView, IntentKind, IntentRecord,
    KeyLatencyView, KeyPressView, LateCallbackView, Launch, LaunchReceipt, Lightness, ListRowView,
    MaterialView, MenuItemView, MenuView, Motion, OkValue, OwnerState, PROTOCOL,
    REQUEST_LINE_LIMIT, Rect, RefuseReason, RefusedArg, Rejected, Request, RequestBody, RequestId,
    Response, RowViewsView, RunLoopView, ScreenView, Size, StartupView, StatsView, StripButtonView,
    TabAxis, TabItemView, TabRegime, TabStatusView, TabStripView, TabZone, TextFieldView,
    TreeListView, TreeMutation, TreeRowView, UI_TIMEOUT, UiReply, UiRequest, VirtualListView,
    WindowMode, WindowView,
};
use shellio_core::test_support::SequentialIds;
use shellio_core::{
    AppearanceChoice, Change, Command, CommandValue, CoreError, Entity, Event, EventsSince, IdGen,
    Query, Reply, Revision, Route, Snapshot, State, Title, TreeScope, UiCommand, WindowState,
    Workspace, WorkspaceId,
};

fn wire_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/wire")
}

fn core_wire_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../shellio-core/tests/wire")
}

/// Образец эталона любого сериализуемого типа.
trait Sample {
    /// Сверить образец с текстом эталона. `Err` — описание расхождения.
    fn check(&self, text: &str) -> Result<(), String>;
    fn pretty(&self) -> String;
}

impl<T: Serialize + DeserializeOwned + PartialEq + Debug> Sample for T {
    fn check(&self, text: &str) -> Result<(), String> {
        let expected: Value =
            serde_json::from_str(text).map_err(|error| format!("эталон не JSON: {error}"))?;
        let actual = serde_json::to_value(self).map_err(|error| error.to_string())?;
        if expected != actual {
            return Err(format!(
                "сериализация расходится с эталоном, получено:\n{}",
                self.pretty()
            ));
        }
        let parsed: T = serde_json::from_str(text)
            .map_err(|error| format!("эталон не разбирается: {error}"))?;
        if parsed != *self {
            return Err(format!(
                "разбор эталона расходится с образцом:\n{parsed:#?}\n!=\n{self:#?}"
            ));
        }
        Ok(())
    }

    fn pretty(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|error| error.to_string())
    }
}

type Samples = Vec<(String, Box<dyn Sample>)>;

fn sample<T: Sample + 'static>(name: impl Into<String>, value: T) -> (String, Box<dyn Sample>) {
    (name.into(), Box::new(value))
}

fn check_goldens(samples: &Samples) -> Vec<String> {
    let mut failures = Vec::new();
    for (name, sample) in samples {
        let path = wire_dir().join(format!("{name}.json"));
        match fs::read_to_string(&path) {
            Ok(text) => {
                if let Err(reason) = sample.check(&text) {
                    failures.push(format!("{name}: {reason}"));
                }
            }
            Err(error) => failures.push(format!(
                "{name}: нет эталона {} ({error}), образец:\n{}",
                path.display(),
                sample.pretty()
            )),
        }
    }
    failures
}

fn assert_goldens(samples: &Samples) {
    let failures = check_goldens(samples);
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

fn title(name: &str) -> Result<Title, String> {
    Title::new(name).map_err(|error| error.to_string())
}

/// Первый идентификатор последовательного генератора: эталоны читаемы.
fn workspace_id() -> WorkspaceId {
    let mut generator = SequentialIds::new();
    let generator: &mut dyn IdGen = &mut generator;
    WorkspaceId::generate(generator)
}

/// Состояние снимков: один воркспейс, он же активный.
fn state() -> Result<Arc<State>, String> {
    let id = workspace_id();
    let mut state = State {
        window: WindowState {
            active_workspace: Some(id),
            route: Route::WorkspaceHome,
        },
        ..State::default()
    };
    state.workspaces.insert(
        id,
        Workspace {
            id,
            title: title("Работа")?,
            position: 0,
            last_workbench: None,
            deleted_at: None,
        },
    );
    assert_eq!(state.check_invariants(), Vec::new());
    Ok(Arc::new(state))
}

fn snapshot(revision: u64) -> Result<Snapshot, String> {
    Ok(Snapshot {
        revision: Revision(revision),
        state: state()?,
    })
}

fn request_golden(request: &Request) -> &'static str {
    match &request.body {
        RequestBody::Hello(_) => "request.hello",
        RequestBody::Command { .. } => "request.command",
        RequestBody::Query(_) => "request.query",
        RequestBody::EventsSince { .. } => "request.events_since",
        RequestBody::Subscribe => "request.subscribe",
        RequestBody::Launch(_) => "request.launch",
        RequestBody::Ui(UiRequest::Window) => "request.ui.window",
        RequestBody::Ui(UiRequest::Inspect) => "request.ui.inspect",
        RequestBody::Ui(UiRequest::Stats) => "request.ui.stats",
        RequestBody::Ui(UiRequest::Command(UiCommand::WindowClose)) => "request.ui.window_close",
        RequestBody::Ui(UiRequest::Command(UiCommand::AppQuit)) => "request.ui.app_quit",
        RequestBody::Ui(UiRequest::Command(UiCommand::AppearanceSet(_))) => {
            "request.ui.appearance_set"
        }
        RequestBody::Ui(UiRequest::Gallery(command)) => match command {
            GalleryCommand::Cell { .. } => "request.ui.gallery_cell",
            GalleryCommand::Motion(_) => "request.ui.gallery_motion",
            GalleryCommand::Focus(_) => "request.ui.gallery_focus",
            GalleryCommand::MenuOpen => "request.ui.gallery_menu_open",
            GalleryCommand::Rebuild => "request.ui.gallery_rebuild",
            GalleryCommand::WindowRecreate => "request.ui.gallery_window_recreate",
            GalleryCommand::Place { .. } => "request.ui.gallery_place",
            GalleryCommand::Reset => "request.ui.gallery_reset",
            GalleryCommand::TabsSelect { .. } => "request.ui.gallery_tabs_select",
            GalleryCommand::ListReplace { .. } => "request.ui.gallery_list_replace",
            GalleryCommand::ListScroll { .. } => "request.ui.gallery_list_scroll",
            GalleryCommand::ListTicks { .. } => "request.ui.gallery_list_ticks",
            GalleryCommand::TreeMutate { .. } => "request.ui.gallery_tree_mutate",
        },
    }
}

fn requests() -> Vec<Request> {
    let workspace = workspace_id();
    let request = |id, body| Request {
        id: RequestId(id),
        body,
    };
    vec![
        request(
            1,
            RequestBody::Hello(Hello {
                protocol: PROTOCOL,
                client: "shellioctl 0.1.0".into(),
            }),
        ),
        request(
            2,
            RequestBody::Command {
                command: Command::WorkspaceRename {
                    workspace,
                    title: "Дом".into(),
                },
                scope: TreeScope::Workspace(workspace),
            },
        ),
        request(
            3,
            RequestBody::Query(Query::Tree {
                scope: TreeScope::All,
            }),
        ),
        request(
            4,
            RequestBody::EventsSince {
                cursor: Revision(12),
            },
        ),
        request(5, RequestBody::Subscribe),
        request(
            6,
            RequestBody::Launch(Launch {
                args: vec!["/abs/path".into()],
                cwd: "/abs".into(),
            }),
        ),
        // P1.3: запросы `ui`.
        request(7, RequestBody::Ui(UiRequest::Window)),
        request(8, RequestBody::Ui(UiRequest::Inspect)),
        request(9, RequestBody::Ui(UiRequest::Stats)),
        request(
            10,
            RequestBody::Ui(UiRequest::Command(UiCommand::WindowClose)),
        ),
        request(11, RequestBody::Ui(UiRequest::Command(UiCommand::AppQuit))),
        request(
            12,
            RequestBody::Ui(UiRequest::Command(UiCommand::AppearanceSet(
                AppearanceChoice::Light,
            ))),
        ),
        // P1.3, задача 8: служебные команды галереи.
        request(
            13,
            RequestBody::Ui(UiRequest::Gallery(GalleryCommand::Cell {
                appearance: None,
                contrast: Some(Contrast::Increased),
            })),
        ),
        request(
            14,
            RequestBody::Ui(UiRequest::Gallery(GalleryCommand::Motion(Motion::Reduced))),
        ),
        request(
            15,
            RequestBody::Ui(UiRequest::Gallery(GalleryCommand::Focus(
                GalleryFocus::Field,
            ))),
        ),
        request(
            16,
            RequestBody::Ui(UiRequest::Gallery(GalleryCommand::MenuOpen)),
        ),
        request(
            17,
            RequestBody::Ui(UiRequest::Gallery(GalleryCommand::Rebuild)),
        ),
        request(
            18,
            RequestBody::Ui(UiRequest::Gallery(GalleryCommand::WindowRecreate)),
        ),
        request(
            19,
            RequestBody::Ui(UiRequest::Gallery(GalleryCommand::Place { screen: 1 })),
        ),
        request(
            20,
            RequestBody::Ui(UiRequest::Gallery(GalleryCommand::Reset)),
        ),
        // P1.3, задача 11: выбор вкладки полосы галереи.
        request(
            21,
            RequestBody::Ui(UiRequest::Gallery(GalleryCommand::TabsSelect {
                strip: GalleryStrip::Side,
                tab: 7,
            })),
        ),
        // P1.3, задача 12: замена и прокрутка списка галереи.
        request(
            22,
            RequestBody::Ui(UiRequest::Gallery(GalleryCommand::ListReplace { seed: 3 })),
        ),
        request(
            23,
            RequestBody::Ui(UiRequest::Gallery(GalleryCommand::ListScroll {
                row: 9_999,
            })),
        ),
        // P1.3, задача 13: изменение дерева галереи.
        request(
            24,
            RequestBody::Ui(UiRequest::Gallery(GalleryCommand::TreeMutate {
                op: TreeMutation::Insert,
            })),
        ),
        // P1.3, закрытие шага: такты `CADisplayLink` в серии задержки списка.
        request(
            25,
            RequestBody::Ui(UiRequest::Gallery(GalleryCommand::ListTicks {
                enabled: true,
            })),
        ),
    ]
}

fn request_samples() -> Samples {
    requests()
        .into_iter()
        .map(|request| sample(request_golden(&request), request))
        .collect()
}

fn response_golden(response: &Response) -> &'static str {
    match response {
        Response::Ok {
            value: OkValue::Hello(_),
            ..
        } => "response.ok.hello",
        Response::Ok {
            value: OkValue::Reply(_),
            ..
        } => "response.ok.reply",
        Response::Ok {
            value: OkValue::EventsSince(EventsSince::Events(_)),
            ..
        } => "response.ok.events_since.events",
        Response::Ok {
            value: OkValue::EventsSince(EventsSince::Reset(_)),
            ..
        } => "response.ok.events_since.reset",
        Response::Ok {
            value: OkValue::Launch(receipt),
            ..
        } if receipt
            .refused
            .iter()
            .any(|refused| refused.reason == RefuseReason::GalleryNotAtStart) =>
        {
            "response.ok.launch.gallery_not_at_start"
        }
        Response::Ok {
            value: OkValue::Launch(_),
            ..
        } => "response.ok.launch",
        Response::Ok {
            value: OkValue::Ui(reply),
            ..
        } => match reply {
            UiReply::Window(_) => "response.ok.ui.window",
            UiReply::Inspect(inspect) if inspect.gallery.is_some() => {
                "response.ok.ui.inspect.gallery"
            }
            UiReply::Inspect(inspect)
                if inspect.components.iter().any(|component| {
                    matches!(component.state, Some(ComponentState::TabStrip(_)))
                }) =>
            {
                "response.ok.ui.inspect.tab_strip"
            }
            UiReply::Inspect(inspect)
                if inspect.components.iter().any(|component| {
                    matches!(component.state, Some(ComponentState::VirtualList(_)))
                }) =>
            {
                "response.ok.ui.inspect.virtual_list"
            }
            UiReply::Inspect(inspect)
                if inspect.components.iter().any(|component| {
                    matches!(component.state, Some(ComponentState::TreeList(_)))
                }) =>
            {
                "response.ok.ui.inspect.tree_list"
            }
            UiReply::Inspect(inspect)
                if inspect.components.iter().any(|component| {
                    matches!(
                        component.state,
                        Some(ComponentState::TextField(_) | ComponentState::Menu(_))
                    )
                }) =>
            {
                "response.ok.ui.inspect.components"
            }
            UiReply::Inspect(_) => "response.ok.ui.inspect",
            UiReply::Stats(stats) if !stats.gallery_trees.is_empty() => {
                "response.ok.ui.stats.gallery"
            }
            UiReply::Stats(stats) if !stats.animations.is_empty() => {
                "response.ok.ui.stats.animations"
            }
            UiReply::Stats(stats) if stats.key_latency.is_some() => "response.ok.ui.stats.list",
            UiReply::Stats(_) => "response.ok.ui.stats",
            UiReply::Accepted(_) => "response.ok.ui.accepted",
        },
        Response::CoreError { .. } => "response.core_error",
        Response::Error { id: Some(_), .. } => "response.error",
        Response::Error { id: None, .. } => "response.error.unnumbered",
        Response::Snapshot { .. } => "response.snapshot",
        Response::Event { .. } => "response.event",
        Response::Reset { .. } => "response.reset",
    }
}

fn responses() -> Result<Vec<Response>, String> {
    let workspace = workspace_id();
    let ok = |id, value| Response::Ok {
        id: RequestId(id),
        value,
    };
    let refused = |arg: &str, reason| RefusedArg {
        arg: arg.into(),
        reason,
    };
    Ok(vec![
        ok(
            1,
            OkValue::Hello(HelloReply {
                protocol: PROTOCOL,
                server: "0.1.0".into(),
                pid: 123,
                owner: OwnerState::Ready,
            }),
        ),
        ok(
            2,
            OkValue::Reply(Reply {
                revision: Revision(13),
                value: CommandValue::None.into(),
            }),
        ),
        ok(
            4,
            OkValue::EventsSince(EventsSince::Events(vec![Event {
                revision: Revision(13),
                changes: vec![Change::WorkspaceRenamed {
                    workspace,
                    title: title("Дом")?,
                }],
            }])),
        ),
        ok(4, OkValue::EventsSince(EventsSince::Reset(snapshot(13)?))),
        ok(
            6,
            OkValue::Launch(LaunchReceipt {
                activation: Activation::Requested,
                refused: vec![
                    refused("/abs/path", RefuseReason::FolderOpenUnsupported),
                    refused("/abs/notes.txt", RefuseReason::NotADirectory),
                    refused("/abs/missing", RefuseReason::PathNotFound),
                    refused("--unknown", RefuseReason::UnknownArgument),
                ],
            }),
        ),
        // P1.3: `--gallery` второго запуска; эталон P1.2 `response.ok.launch` не меняется.
        ok(
            7,
            OkValue::Launch(LaunchReceipt {
                activation: Activation::Requested,
                refused: vec![refused("--gallery", RefuseReason::GalleryNotAtStart)],
            }),
        ),
        ok(7, OkValue::Ui(UiReply::Window(Box::new(window_view())))),
        ok(8, OkValue::Ui(UiReply::Inspect(Box::new(inspect_view())))),
        // P1.3, задача 7: состояния `TextField` и меню кнопки; эталон `response.ok.ui.inspect`
        // не меняется.
        ok(
            8,
            OkValue::Ui(UiReply::Inspect(Box::new(inspect_components_view()))),
        ),
        // P1.3, задача 8: состояние галереи; прежние эталоны `ui.inspect` без ключа `gallery`.
        ok(
            8,
            OkValue::Ui(UiReply::Inspect(Box::new(inspect_gallery_view()))),
        ),
        // P1.3, задача 11: состояние `TabStrip` и журнал анимаций; прежние эталоны не меняются.
        ok(
            8,
            OkValue::Ui(UiReply::Inspect(Box::new(inspect_tab_strip_view()))),
        ),
        ok(
            9,
            OkValue::Ui(UiReply::Stats(Box::new(StatsView {
                animations: vec![
                    AnimationRecord {
                        generation: 12,
                        component: "gallery.tabs.side".into(),
                        path: AnimationPath::Animated,
                        added: 1,
                    },
                    AnimationRecord {
                        generation: 13,
                        component: "gallery.tabs.side".into(),
                        path: AnimationPath::Instant,
                        added: 0,
                    },
                ],
                ..stats_view()
            }))),
        ),
        // P1.3, задача 12: состояние `VirtualList`, виды строк и ряд задержек клавиш; прежние
        // эталоны без ключа `key_latency`.
        ok(
            8,
            OkValue::Ui(UiReply::Inspect(Box::new(inspect_virtual_list_view()))),
        ),
        ok(
            9,
            OkValue::Ui(UiReply::Stats(Box::new(StatsView {
                row_views: vec![RowViewsView {
                    component: "gallery.list".into(),
                    created: 17,
                }],
                key_latency: Some(KeyLatencyView {
                    component: "gallery.list".into(),
                    presses: vec![
                        KeyPressView {
                            key: "down".into(),
                            event: 1520.25,
                            wake: Some(1520.256),
                            entry: 1520.257,
                            handled: 1520.2571,
                            applied: Some(1520.259),
                            target: "s0.r00001".into(),
                            generation: Some(41),
                            drawn: Some(1520.265),
                            drawn_generation: Some(41),
                        },
                        KeyPressView {
                            key: "up".into(),
                            event: 1521.5,
                            wake: None,
                            entry: 1521.507,
                            handled: 1521.5071,
                            applied: None,
                            target: "s0.r00000".into(),
                            generation: None,
                            drawn: None,
                            drawn_generation: None,
                        },
                    ],
                    ticks: vec![1520.25, 1520.258333, 1520.266666],
                }),
                ..stats_view()
            }))),
        ),
        // P1.3, задача 13: состояние `TreeList` с выбором и раскрытием у `NSOutlineView`.
        ok(
            8,
            OkValue::Ui(UiReply::Inspect(Box::new(inspect_tree_list_view()))),
        ),
        ok(9, OkValue::Ui(UiReply::Stats(Box::new(stats_view())))),
        // P1.3, задача 10: прежние деревья галереи с живыми по классам; эталон
        // `response.ok.ui.stats` не меняется.
        ok(
            9,
            OkValue::Ui(UiReply::Stats(Box::new(StatsView {
                gallery_trees: vec![
                    GalleryTreeView {
                        tree: 1,
                        views: 48,
                        alive: 0,
                        alive_classes: Vec::new(),
                    },
                    GalleryTreeView {
                        tree: 2,
                        views: 61,
                        alive: 2,
                        alive_classes: vec![
                            ClassAliveView {
                                class: "NSButton".into(),
                                alive: 1,
                            },
                            ClassAliveView {
                                class: "NSTextView".into(),
                                alive: 1,
                            },
                        ],
                    },
                ],
                ..stats_view()
            }))),
        ),
        ok(
            10,
            OkValue::Ui(UiReply::Accepted(Accepted {
                command: "window.close".into(),
                pid: 123,
                generation: 5,
            })),
        ),
        Response::CoreError {
            id: RequestId(2),
            error: CoreError::not_found(Entity::Workspace, workspace),
        },
        Response::Error {
            id: Some(RequestId(2)),
            error: ControlError::owner_leaving(),
        },
        Response::Error {
            id: None,
            error: ControlError::request_too_large(REQUEST_LINE_LIMIT),
        },
        Response::Snapshot {
            id: RequestId(5),
            snapshot: snapshot(13)?,
        },
        Response::Event {
            id: RequestId(5),
            event: Event {
                revision: Revision(14),
                changes: vec![Change::WindowSet {
                    active: workspace,
                    route: Route::WorkspaceHome,
                }],
            },
        },
        Response::Reset {
            id: RequestId(5),
            snapshot: snapshot(1300)?,
        },
    ])
}

fn rect(x: f64, y: f64, width: f64, height: f64) -> Rect {
    Rect {
        x,
        y,
        width,
        height,
    }
}

fn window_view() -> WindowView {
    WindowView {
        pid: 123,
        mode: WindowMode::Gallery,
        window_number: 4242,
        frame: rect(370.0, 240.0, 1180.0, 720.0),
        backing_scale_factor: 2.0,
        visible: true,
        key: true,
        color_space: ColorSpaceView {
            name: Some("sRGB IEC61966-2.1".into()),
            srgb: true,
        },
        title: "Работа".into(),
        appearance_choice: AppearanceChoice::Light,
        lightness: Axis {
            value: Lightness::Light,
            source: AxisSource::UiAppearance,
        },
        contrast: Axis {
            value: Contrast::Normal,
            source: AxisSource::Gallery,
        },
        motion: Axis {
            value: Motion::Normal,
            source: AxisSource::System,
        },
        screen: Some(0),
        maximum_frames_per_second: Some(120),
        screens: vec![
            ScreenView {
                index: 0,
                name: "Built-in Retina Display".into(),
                frame: rect(0.0, 0.0, 1512.0, 982.0),
                backing_scale_factor: 2.0,
                maximum_frames_per_second: 120,
            },
            ScreenView {
                index: 1,
                name: "DELL U2720Q".into(),
                frame: rect(1512.0, 0.0, 2560.0, 1440.0),
                backing_scale_factor: 1.0,
                maximum_frames_per_second: 60,
            },
        ],
        mirror_revision: Some(13),
        generation: 7,
    }
}

fn inspect_view() -> InspectView {
    InspectView {
        window_number: 4242,
        content: Size {
            width: 1180.0,
            height: 720.0,
        },
        generation: 7,
        components: vec![
            ComponentView {
                id: "chrome".into(),
                class: "ChromeBar".into(),
                object: 105_553_116_266_496,
                parent: None,
                frame: rect(0.0, 680.0, 1180.0, 40.0),
                visible: true,
                first_responder: false,
                enabled: None,
                role: Some("AXGroup".into()),
                label: None,
                help: None,
                selected: None,
                expanded: None,
                animation_keys: 0,
                state: Some(ComponentState::Chrome(ChromeView {
                    workspace: Some("Работа".into()),
                    workspace_text: "Работа".into(),
                    appearance_choice: AppearanceChoice::System,
                    workspace_menu: false,
                    zones: false,
                })),
            },
            ComponentView {
                id: "chrome.workspace".into(),
                class: "WorkspaceButton".into(),
                object: 105_553_116_267_008,
                parent: Some("chrome".into()),
                frame: rect(84.0, 689.0, 96.0, 22.0),
                visible: true,
                first_responder: false,
                enabled: Some(true),
                role: Some("AXButton".into()),
                label: Some("Воркспейс: Работа".into()),
                help: None,
                selected: None,
                expanded: None,
                animation_keys: 0,
                state: None,
            },
        ],
        materials: vec![MaterialView {
            class: "NSTitlebarBackgroundView".into(),
            visible: false,
            own: false,
        }],
        gallery: None,
    }
}

fn inspect_gallery_view() -> InspectView {
    let node = |id: &str, class: &str, object, parent: Option<&str>, frame| ComponentView {
        id: id.into(),
        class: class.into(),
        object,
        parent: parent.map(str::to_owned),
        frame,
        visible: true,
        first_responder: false,
        enabled: None,
        role: None,
        label: None,
        help: None,
        selected: None,
        expanded: None,
        animation_keys: 0,
        state: None,
    };
    InspectView {
        window_number: 4243,
        content: Size {
            width: 1180.0,
            height: 720.0,
        },
        generation: 12,
        components: vec![
            node(
                "gallery.marker",
                "GenerationMarker",
                105_553_116_269_056,
                None,
                rect(12.0, 644.0, 24.0, 24.0),
            ),
            node(
                "gallery.surface.focus",
                "Surface",
                105_553_116_269_568,
                None,
                rect(48.0, 12.0, 554.0, 656.0),
            ),
        ],
        materials: Vec::new(),
        gallery: Some(GalleryView {
            tree: 2,
            window: 1,
            appearance: None,
            contrast: Some(Contrast::Increased),
            motion: Some(Motion::Reduced),
            focus: GalleryFocus::Field,
            screen: None,
            intents: vec![
                IntentRecord {
                    generation: 10,
                    component: "gallery.field".into(),
                    intent: IntentKind::Changed,
                    value: Some("s".into()),
                },
                IntentRecord {
                    generation: 11,
                    component: "gallery.field".into(),
                    intent: IntentKind::Cancelled,
                    value: None,
                },
                IntentRecord {
                    generation: 12,
                    component: "gallery.menu".into(),
                    intent: IntentKind::Chosen,
                    value: Some("copy".into()),
                },
            ],
        }),
    }
}

fn inspect_tab_strip_view() -> InspectView {
    let tab = |id: &str, title: &str, status, icon: Option<&str>, close_visible, fully_visible| {
        TabItemView {
            id: id.into(),
            title: title.into(),
            kind: "terminal".into(),
            status,
            icon: icon.map(str::to_owned),
            close_visible,
            fully_visible,
        }
    };
    let button = |id: &str, label: &str, symbol: &str| StripButtonView {
        id: id.into(),
        label: label.into(),
        symbol: symbol.into(),
        hint: format!("{label}. Клавиша не назначена"),
        help: "Клавиша не назначена".into(),
    };
    InspectView {
        window_number: 4242,
        content: Size {
            width: 1180.0,
            height: 720.0,
        },
        generation: 12,
        components: vec![ComponentView {
            id: "gallery.tabs.side".into(),
            class: "TabStrip".into(),
            object: 105_553_116_268_800,
            parent: Some("gallery.surface.focus".into()),
            frame: rect(60.0, 530.0, 372.0, 34.0),
            visible: true,
            first_responder: true,
            enabled: None,
            role: Some("AXTabGroup".into()),
            label: Some("Вкладки боковой зоны".into()),
            help: None,
            selected: None,
            expanded: None,
            animation_keys: 1,
            state: Some(ComponentState::TabStrip(Box::new(TabStripView {
                label: "Вкладки боковой зоны".into(),
                zone: TabZone::Side,
                axis: TabAxis::Rows,
                navigator: false,
                zoomed: false,
                focused: true,
                keyboard_focus: true,
                regime: Some(TabRegime::Scroll),
                selected: Some("review".into()),
                payload: Some("1,24 ГБ · 3,5 % CPU".into()),
                payload_visible: false,
                offset: 312.5,
                max_offset: 312.5,
                tabs: vec![
                    tab("term", "zsh", TabStatusView::Running, None, false, false),
                    tab("review", "Ревью", TabStatusView::None, None, false, true),
                ],
                buttons: vec![
                    button("split", "Разделить: одна над другой", "rectangle.split.1x2"),
                    button(
                        "zoom",
                        "Развернуть панель",
                        "arrow.up.left.and.arrow.down.right",
                    ),
                    button("menu", "Действия панели", "ellipsis"),
                ],
            }))),
        }],
        materials: Vec::new(),
        gallery: None,
    }
}

fn inspect_virtual_list_view() -> InspectView {
    let row = |index: u64, id: &str, text: &str| ListRowView {
        index,
        id: id.into(),
        text: text.into(),
    };
    InspectView {
        window_number: 4242,
        content: Size {
            width: 1180.0,
            height: 720.0,
        },
        generation: 41,
        components: vec![
            ComponentView {
                id: "gallery.list".into(),
                class: "VirtualList".into(),
                object: 105_553_116_269_312,
                parent: Some("gallery.surface.plain".into()),
                frame: rect(626.0, 240.0, 260.0, 416.0),
                visible: true,
                first_responder: true,
                enabled: None,
                role: Some("AXTable".into()),
                label: Some("Список галереи".into()),
                help: None,
                selected: None,
                expanded: None,
                animation_keys: 0,
                state: Some(ComponentState::VirtualList(Box::new(VirtualListView {
                    label: "Список галереи".into(),
                    count: 10_000,
                    selected: Some("s0.r00001".into()),
                    offset: 26.0,
                    max_offset: 259_584.0,
                    keyboard_focus: true,
                    row_views_created: 17,
                    cell_views_created: 17,
                    visible: vec![
                        row(1, "s0.r00001", "Строка 00001 · 0"),
                        row(2, "s0.r00002", "Строка 00002 · 0"),
                    ],
                }))),
            },
            ComponentView {
                id: "gallery.list.row.s0.r00001".into(),
                class: "ListRow".into(),
                object: 105_553_116_269_824,
                parent: Some("gallery.list".into()),
                frame: rect(626.0, 630.0, 260.0, 26.0),
                visible: true,
                first_responder: false,
                enabled: None,
                role: Some("AXRow".into()),
                label: Some("Строка 00001 · 0".into()),
                help: None,
                selected: Some(true),
                expanded: None,
                animation_keys: 0,
                state: None,
            },
        ],
        materials: Vec::new(),
        gallery: None,
    }
}

fn inspect_tree_list_view() -> InspectView {
    let row = |index: u64, id: &str, text: &str, level: u64, expandable, expanded| TreeRowView {
        index,
        id: id.into(),
        text: text.into(),
        level,
        expandable,
        expanded,
    };
    InspectView {
        window_number: 4242,
        content: Size {
            width: 1180.0,
            height: 720.0,
        },
        generation: 57,
        components: vec![
            ComponentView {
                id: "gallery.tree".into(),
                class: "TreeList".into(),
                object: 105_553_116_270_336,
                parent: Some("gallery.surface.plain".into()),
                frame: rect(898.0, 240.0, 258.0, 416.0),
                visible: true,
                first_responder: false,
                enabled: None,
                role: Some("AXOutline".into()),
                label: Some("Дерево галереи".into()),
                help: None,
                selected: None,
                expanded: None,
                animation_keys: 0,
                state: Some(ComponentState::TreeList(Box::new(TreeListView {
                    label: "Дерево галереи".into(),
                    count: 119,
                    nodes: 10_000,
                    selected: Some("n000.0.0".into()),
                    expanded: vec!["n000".into(), "n000.0".into()],
                    offset: 0.0,
                    max_offset: 2_678.0,
                    keyboard_focus: false,
                    row_views_created: 17,
                    cell_views_created: 17,
                    visible: vec![
                        row(0, "n000", "Корень 000", 0, true, true),
                        row(1, "n000.0", "Ветка 000.0", 1, true, true),
                        row(2, "n000.0.0", "Лист 000.0.0", 2, false, false),
                    ],
                }))),
            },
            ComponentView {
                id: "gallery.tree.row.n000".into(),
                class: "TreeRow".into(),
                object: 105_553_116_270_848,
                parent: Some("gallery.tree".into()),
                frame: rect(898.0, 630.0, 258.0, 26.0),
                visible: true,
                first_responder: false,
                enabled: None,
                role: Some("AXRow".into()),
                label: Some("Корень 000".into()),
                help: None,
                selected: Some(false),
                expanded: Some(true),
                animation_keys: 0,
                state: None,
            },
        ],
        materials: Vec::new(),
        gallery: None,
    }
}

fn inspect_components_view() -> InspectView {
    let part =
        |id: &str, class: &str, object, frame, role: &str, label: &str, state| ComponentView {
            id: id.into(),
            class: class.into(),
            object,
            parent: Some("gallery".into()),
            frame,
            visible: true,
            first_responder: false,
            enabled: Some(true),
            role: Some(role.into()),
            label: Some(label.into()),
            help: None,
            selected: None,
            expanded: None,
            animation_keys: 0,
            state: Some(state),
        };
    let item = |id: &str, title: &str| MenuItemView {
        id: id.into(),
        separator: false,
        title: title.into(),
        key: None,
        enabled: true,
        checked: false,
        submenu: None,
    };
    let mut field = part(
        "gallery.field",
        "TextField",
        105_553_116_268_032,
        rect(24.0, 600.0, 320.0, 32.0),
        "AXTextField",
        "Поле галереи",
        ComponentState::TextField(TextFieldView {
            value: "shellio".into(),
            placeholder: "Поиск".into(),
            editable: true,
            editing: true,
        }),
    );
    field.first_responder = true;
    InspectView {
        window_number: 4242,
        content: Size {
            width: 1180.0,
            height: 720.0,
        },
        generation: 9,
        components: vec![
            field,
            part(
                "gallery.menu",
                "MenuButton",
                105_553_116_268_544,
                rect(352.0, 604.0, 28.0, 24.0),
                "AXButton",
                "Ещё",
                ComponentState::Menu(MenuView {
                    open: true,
                    highlighted: Some("gallery.menu.rename".into()),
                    items: vec![
                        MenuItemView {
                            enabled: false,
                            ..item("gallery.menu.payload", "3 ждут · 1 ошибка")
                        },
                        MenuItemView {
                            separator: true,
                            title: String::new(),
                            enabled: false,
                            ..item("gallery.menu.sep", "")
                        },
                        MenuItemView {
                            key: Some("cmd+r".into()),
                            ..item("gallery.menu.rename", "Переименовать")
                        },
                        MenuItemView {
                            submenu: Some(vec![MenuItemView {
                                checked: true,
                                ..item("gallery.menu.move.left", "Влево")
                            }]),
                            ..item("gallery.menu.move", "Перенести")
                        },
                    ],
                }),
            ),
        ],
        materials: Vec::new(),
        gallery: None,
    }
}

fn stats_view() -> StatsView {
    StatsView {
        pid: 123,
        generation: 7,
        instances: vec![InstanceView {
            kind: InstanceKind::Class,
            name: "ShellioChromeBar".into(),
            created: 1,
            released: 0,
            live: 1,
        }],
        late_callbacks: vec![LateCallbackView {
            site: "MainWindow.root".into(),
            count: 0,
        }],
        run_loop: Some(RunLoopView {
            iterations: 90,
            wakeups: 90,
            unpaired: 2,
            display_passes: 170,
            over_budget: 1,
            max_ms: 37.5,
            p95_ms: Some(1.25),
            window: 90,
            draws: 12,
        }),
        startup: Some(StartupView {
            first_draw_ms: 182.5,
            key_ms: 190.0,
            key_draw_ms: 190.0,
        }),
        animations: Vec::new(),
        row_views: Vec::new(),
        gallery_trees: Vec::new(),
        key_latency: None,
    }
}

fn response_samples() -> Result<Samples, String> {
    Ok(responses()?
        .into_iter()
        .map(|response| sample(response_golden(&response), response))
        .collect())
}

fn error_sample(code: ControlErrorCode) -> ControlError {
    match code {
        ControlErrorCode::HelloRequired => ControlError::hello_required(),
        ControlErrorCode::ProtocolMismatch => ControlError::protocol_mismatch(2),
        ControlErrorCode::MalformedRequest => {
            ControlError::malformed_request("лишнее поле `extra`")
        }
        ControlErrorCode::RequestTooLarge => ControlError::request_too_large(REQUEST_LINE_LIMIT),
        ControlErrorCode::ScopeRequired => ControlError::scope_required("command"),
        ControlErrorCode::ScopeUnsupported => ControlError::scope_unsupported("subscribe"),
        ControlErrorCode::OwnerLeaving => ControlError::owner_leaving(),
        ControlErrorCode::NotRunning => ControlError::not_running(),
        ControlErrorCode::OwnerStarting => ControlError::owner_starting(),
        ControlErrorCode::Unavailable => ControlError::unavailable("срок 5000 мс истёк"),
        ControlErrorCode::OutcomeUnknown => {
            ControlError::outcome_unknown("соединение оборвалось после отправки запроса")
        }
        ControlErrorCode::NoUi => ControlError::no_ui(),
        ControlErrorCode::UiTimeout => ControlError::ui_timeout(UI_TIMEOUT),
        ControlErrorCode::GalleryOnly => ControlError::gallery_only(),
    }
}

fn error_samples() -> Samples {
    ControlErrorCode::ALL
        .into_iter()
        .map(|code| {
            let error = error_sample(code);
            assert_eq!(error.code(), code);
            sample(format!("error.{code}"), error)
        })
        .collect()
}

fn all_samples() -> Result<Samples, String> {
    Ok([request_samples(), response_samples()?, error_samples()]
        .into_iter()
        .flatten()
        .collect())
}

#[test]
fn requests_match_goldens() {
    assert_goldens(&request_samples());
}

#[test]
fn responses_match_goldens() {
    assert_goldens(&response_samples().unwrap());
}

#[test]
fn errors_match_goldens() {
    assert_goldens(&error_samples());
}

#[test]
fn request_golden_names_follow_frame_keys() {
    for request in requests() {
        let name = request_golden(&request);
        let kind = format!("request.{}", request.body.kind());
        match &request.body {
            RequestBody::Ui(ui) => {
                let suffix = ui.name().replace('.', "_");
                let suffix = suffix.strip_prefix("ui_").unwrap_or(&suffix);
                assert_eq!(name, format!("{kind}.{suffix}"));
            }
            _ => assert_eq!(name, kind),
        }
    }
}

#[test]
fn request_goldens_pass_strict_parse() {
    for request in requests() {
        let name = request_golden(&request);
        let text = fs::read_to_string(wire_dir().join(format!("{name}.json"))).unwrap();
        assert_eq!(Request::parse(text.as_bytes()), Ok(request), "{name}");
    }
}

#[test]
fn every_golden_has_exactly_one_sample() {
    let mut names = BTreeMap::new();
    for (name, _) in all_samples().unwrap() {
        *names.entry(name).or_insert(0_u32) += 1;
    }
    let duplicates: Vec<_> = names.iter().filter(|(_, count)| **count > 1).collect();
    assert!(
        duplicates.is_empty(),
        "эталоны с несколькими образцами: {duplicates:?}"
    );

    let files: BTreeSet<String> = fs::read_dir(wire_dir())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    let expected: BTreeSet<String> = names.keys().map(|name| format!("{name}.json")).collect();
    let orphans: Vec<_> = files.difference(&expected).collect();
    assert!(orphans.is_empty(), "эталоны без образца: {orphans:?}");
    let missing: Vec<_> = expected.difference(&files).collect();
    assert!(missing.is_empty(), "образцы без эталона: {missing:?}");
}

/// Эталоны P1.1 идут внутри кадров без изменений: кадр с ними разбирается, даёт ожидаемый вид
/// и сериализуется обратно в тот же JSON.
#[test]
fn p1_1_messages_travel_inside_frames_unchanged() {
    let mut checked = 0;
    for entry in fs::read_dir(core_wire_dir()).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
        let message: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let prefix = name.split('.').next().unwrap_or_default();
        match prefix {
            "command" | "query" => {
                let frame = if prefix == "command" {
                    json!({"id": 9, "command": message, "scope": "all"})
                } else {
                    json!({"id": 9, "query": message})
                };
                let request = Request::parse(frame.to_string().as_bytes())
                    .unwrap_or_else(|rejected| panic!("{name}: {rejected:?}"));
                assert_eq!(request.body.kind(), prefix, "{name}");
                assert_eq!(serde_json::to_value(&request).unwrap(), frame, "{name}");
            }
            "reply" | "events_since" | "error" | "event" => {
                let key = match prefix {
                    "error" => "core_error",
                    "event" => "event",
                    _ => "ok",
                };
                let frame = json!({"id": 9, key: message});
                let response = Response::parse(frame.to_string().as_bytes())
                    .unwrap_or_else(|error| panic!("{name}: {error}"));
                let kind_matches = matches!(
                    (&response, prefix),
                    (
                        Response::Ok {
                            value: OkValue::Reply(_),
                            ..
                        },
                        "reply",
                    ) | (
                        Response::Ok {
                            value: OkValue::EventsSince(_),
                            ..
                        },
                        "events_since",
                    ) | (Response::CoreError { .. }, "error")
                        | (Response::Event { .. }, "event")
                );
                assert!(kind_matches, "{name}: разобран как {response:?}");
                assert_eq!(serde_json::to_value(&response).unwrap(), frame, "{name}");
            }
            // Изменения идут внутри событий.
            "change" => continue,
            other => panic!("{name}: эталон P1.1 вида «{other}» не отнесён к кадру"),
        }
        checked += 1;
    }
    assert!(checked > 20, "проверено {checked} эталонов P1.1");
}

fn rejected(line: &[u8]) -> Rejected {
    match Request::parse(line) {
        Ok(request) => panic!("{} разобран как {request:?}", String::from_utf8_lossy(line)),
        Err(rejected) => rejected,
    }
}

/// Строка отвергается кодом `code` с номером `id`, сообщение содержит `reason`.
fn assert_rejected(line: &str, code: ControlErrorCode, id: Option<u64>, reason: &str) {
    let rejected = rejected(line.as_bytes());
    assert_eq!(rejected.error.code(), code, "{line}: {:?}", rejected.error);
    assert_eq!(rejected.id, id.map(RequestId), "{line}");
    assert!(
        rejected.error.message().contains(reason),
        "{line}: «{}» не содержит «{reason}»",
        rejected.error.message()
    );
}

fn assert_malformed(line: &str, id: Option<u64>, reason: &str) {
    assert_rejected(line, ControlErrorCode::MalformedRequest, id, reason);
}

const HELLO: &str = r#"{"protocol":1,"client":"test"}"#;
const ID: &str = "00000000-0000-0000-0000-000000000001";

#[test]
fn extra_field_is_malformed_and_keeps_id() {
    assert_malformed(
        &format!(r#"{{"id":1,"hello":{HELLO},"extra":1}}"#),
        Some(1),
        "unknown field `extra`",
    );
    assert_malformed(
        r#"{"id":1,"hello":{"protocol":1,"client":"test","extra":1}}"#,
        Some(1),
        "unknown field `extra`",
    );
    assert_malformed(
        r#"{"id":2,"command":{"cmd":"workspace.create","title":"Работа","extra":1},"scope":"all"}"#,
        Some(2),
        "unknown field `extra`",
    );
    assert_malformed(
        r#"{"id":3,"query":{"query":"status","extra":1}}"#,
        Some(3),
        "unknown field `extra`",
    );
    assert_malformed(
        r#"{"id":4,"events_since":{"cursor":1,"extra":1},"scope":"all"}"#,
        Some(4),
        "unknown field `extra`",
    );
    assert_malformed(
        r#"{"id":5,"subscribe":{"extra":1},"scope":"all"}"#,
        Some(5),
        "unknown field `extra`",
    );
    assert_malformed(
        r#"{"id":6,"launch":{"args":[],"cwd":"/","extra":1}}"#,
        Some(6),
        "unknown field `extra`",
    );
}

#[test]
fn scope_on_hello_query_and_launch_is_malformed() {
    for scope in [r#""all""#, &format!(r#"{{"workspace":"{ID}"}}"#)] {
        assert_malformed(
            &format!(r#"{{"id":1,"hello":{HELLO},"scope":{scope}}}"#),
            Some(1),
            "поле scope у запроса hello не допускается",
        );
        assert_malformed(
            &format!(r#"{{"id":3,"query":{{"query":"status"}},"scope":{scope}}}"#),
            Some(3),
            "поле scope у запроса query не допускается",
        );
        assert_malformed(
            &format!(r#"{{"id":6,"launch":{{"args":[],"cwd":"/"}},"scope":{scope}}}"#),
            Some(6),
            "поле scope у запроса launch не допускается",
        );
        assert_malformed(
            &format!(r#"{{"id":7,"ui":{{"ui.window":{{}}}},"scope":{scope}}}"#),
            Some(7),
            "поле scope у запроса ui не допускается",
        );
    }
}

#[test]
fn null_in_place_of_body_or_scope_is_malformed() {
    // Внешне помеченный перечень области на `null` отвечает «expected value».
    assert_malformed(
        &format!(r#"{{"id":1,"hello":{HELLO},"scope":null}}"#),
        Some(1),
        "expected value",
    );
    assert_malformed(r#"{"id":3,"query":null}"#, Some(3), "null");
    assert_malformed(
        r#"{"id":5,"subscribe":null,"scope":"all"}"#,
        Some(5),
        "null",
    );
    assert_malformed(
        &format!(r#"{{"id":1,"hello":{HELLO},"query":null}}"#),
        Some(1),
        "null",
    );
}

#[test]
fn frame_needs_exactly_one_kind() {
    assert_malformed(r#"{"id":1}"#, Some(1), "нет вида запроса");
    assert_malformed(r#"{"id":1,"scope":"all"}"#, Some(1), "нет вида запроса");
    assert_malformed(
        &format!(r#"{{"id":1,"hello":{HELLO},"query":{{"query":"status"}}}}"#),
        Some(1),
        "2 вида запроса",
    );
    assert_malformed(
        r#"{"id":1,"events_since":{"cursor":1},"subscribe":{},"scope":"all"}"#,
        Some(1),
        "2 вида запроса",
    );
    assert_malformed(
        &format!(r#"{{"id":1,"hello":{HELLO},"ui":{{"ui.stats":{{}}}}}}"#),
        Some(1),
        "2 вида запроса",
    );
}

/// Тело `ui` — ровно один запрос с именем из ADR-0007 и строгими аргументами.
/// Полная матрица «запрос × вид ответа»: чтение принимает только свой вид, изменение — только
/// квитанцию.
#[test]
fn ui_replies_answer_only_their_request() {
    let replies = [
        ("window", UiReply::Window(Box::new(window_view()))),
        ("inspect", UiReply::Inspect(Box::new(inspect_view()))),
        ("stats", UiReply::Stats(Box::new(stats_view()))),
        (
            "accepted",
            UiReply::Accepted(Accepted {
                command: "window.close".into(),
                pid: 1,
                generation: 2,
            }),
        ),
    ];
    let requests = [
        (UiRequest::Window, "window"),
        (UiRequest::Inspect, "inspect"),
        (UiRequest::Stats, "stats"),
        (UiRequest::Command(UiCommand::WindowClose), "accepted"),
        (UiRequest::Command(UiCommand::AppQuit), "accepted"),
        (UiRequest::Gallery(GalleryCommand::Reset), "accepted"),
    ];
    for (request, answer) in requests {
        for (kind, reply) in &replies {
            assert_eq!(
                request.answered_by(reply),
                *kind == answer,
                "{} ← {kind}",
                request.name()
            );
        }
    }
}

#[test]
fn ui_body_is_strict() {
    for body in [
        r#"{}"#,
        r#"{"ui.window":{},"ui.stats":{}}"#,
        r#"{"ui.window":{"extra":1}}"#,
        r#"{"ui.window":null}"#,
        r#""ui.window""#,
        r#"{"gallery.reset":{"seed":1}}"#,
        r#"{"gallery.unknown":{}}"#,
        r#"{"ui.appearance.set":{}}"#,
        r#"{"ui.appearance.set":{"appearance":"sepia"}}"#,
        r#"{"gallery.tabs.select":{"strip":"top","tab":0}}"#,
        r#"{"gallery.tabs.select":{"strip":"side","tab":-1}}"#,
        r#"{"gallery.tabs.select":{"strip":"side"}}"#,
        r#"{"gallery.tabs.select":{"strip":"side","tab":0,"extra":1}}"#,
    ] {
        let line = format!(r#"{{"id":4,"ui":{body}}}"#);
        let rejected = rejected(line.as_bytes());
        assert_eq!(rejected.id, Some(RequestId(4)), "{body}");
        assert_eq!(
            rejected.error.code(),
            ControlErrorCode::MalformedRequest,
            "{body}"
        );
    }
}

#[test]
fn unknown_variant_is_malformed() {
    assert_malformed(
        r#"{"id":1,"shutdown":{}}"#,
        Some(1),
        "unknown field `shutdown`",
    );
    assert_malformed(
        &format!(
            r#"{{"id":2,"command":{{"cmd":"workspace.delete","workspace":"{ID}"}},"scope":"all"}}"#
        ),
        Some(2),
        "unknown variant `workspace.delete`",
    );
    assert_malformed(
        r#"{"id":3,"query":{"query":"tree.all"}}"#,
        Some(3),
        "unknown variant `tree.all`",
    );
    assert_malformed(
        r#"{"id":4,"events_since":{"cursor":1},"scope":"none"}"#,
        Some(4),
        "unknown variant `none`",
    );
}

#[test]
fn missing_or_non_numeric_id_is_malformed_without_id() {
    assert_malformed(
        &format!(r#"{{"hello":{HELLO}}}"#),
        None,
        "missing field `id`",
    );
    for id in [
        r#""1""#,
        "1.5",
        "-1",
        "null",
        "true",
        "18446744073709551616",
    ] {
        assert_malformed(&format!(r#"{{"id":{id},"hello":{HELLO}}}"#), None, "");
    }
    assert_malformed(
        &format!(r#"{{"id":"1","hello":{HELLO},"extra":1}}"#),
        None,
        "",
    );
}

#[test]
fn not_a_json_object_is_malformed_without_id() {
    for line in [&b""[..], b"hello", b"{\"id\":1", b"[]", b"1", b"\xff\xfe"] {
        let rejected = rejected(line);
        assert_eq!(rejected.error.code(), ControlErrorCode::MalformedRequest);
        assert_eq!(rejected.id, None, "{}", String::from_utf8_lossy(line));
    }
}

#[test]
fn largest_id_is_kept() {
    let request =
        Request::parse(format!(r#"{{"id":{},"hello":{HELLO}}}"#, u64::MAX).as_bytes()).unwrap();
    assert_eq!(request.id, RequestId(u64::MAX));
}

#[test]
fn missing_scope_is_scope_required() {
    assert_rejected(
        r#"{"id":2,"command":{"cmd":"workspace.create","title":"Работа"}}"#,
        ControlErrorCode::ScopeRequired,
        Some(2),
        "command",
    );
    assert_rejected(
        r#"{"id":4,"events_since":{"cursor":1}}"#,
        ControlErrorCode::ScopeRequired,
        Some(4),
        "events_since",
    );
    assert_rejected(
        r#"{"id":5,"subscribe":{}}"#,
        ControlErrorCode::ScopeRequired,
        Some(5),
        "subscribe",
    );
}

#[test]
fn workspace_scope_of_events_is_unsupported() {
    let scope = format!(r#"{{"workspace":"{ID}"}}"#);
    assert_rejected(
        &format!(r#"{{"id":4,"events_since":{{"cursor":1}},"scope":{scope}}}"#),
        ControlErrorCode::ScopeUnsupported,
        Some(4),
        "events_since",
    );
    assert_rejected(
        &format!(r#"{{"id":5,"subscribe":{{}},"scope":{scope}}}"#),
        ControlErrorCode::ScopeUnsupported,
        Some(5),
        "subscribe",
    );
}

#[test]
fn command_takes_either_scope() {
    for (scope, expected) in [
        (r#""all""#.to_owned(), TreeScope::All),
        (
            format!(r#"{{"workspace":"{ID}"}}"#),
            TreeScope::Workspace(workspace_id()),
        ),
    ] {
        let request = Request::parse(
            format!(r#"{{"id":2,"command":{{"cmd":"workspace.create","title":"Работа"}},"scope":{scope}}}"#)
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(
            request.body,
            RequestBody::Command {
                command: Command::WorkspaceCreate {
                    title: "Работа".into()
                },
                scope: expected,
            }
        );
    }
}

#[test]
fn rejection_becomes_error_frame() {
    let response = rejected(br#"{"id":7,"shutdown":{}}"#).into_response();
    let json = serde_json::to_value(&response).unwrap();
    assert_eq!(json["id"], json!(7));
    assert_eq!(json["error"]["code"], json!("malformed_request"));
    assert_eq!(json["error"]["retryable"], json!(false));

    let response = rejected(b"not json").into_response();
    assert_eq!(serde_json::to_value(&response).unwrap()["id"], Value::Null);
}

/// Кадр сервера отвергается, и причина — ожидаемая.
fn assert_response_rejected(line: &str, reason: &str) {
    match Response::parse(line.as_bytes()) {
        Ok(response) => panic!("{line} разобран как {response:?}"),
        Err(error) => assert!(
            error.to_string().contains(reason),
            "{line}: «{error}» не содержит «{reason}»"
        ),
    }
}

const HELLO_REPLY: &str = r#"{"protocol":1,"server":"0.1.0","pid":1,"owner":"ready"}"#;
const LEAVING: &str = r#"{"code":"owner_leaving","message":"","retryable":true,"details":{}}"#;

#[test]
fn response_frame_is_strict() {
    assert_response_rejected(
        &format!(r#"{{"id":1,"ok":{{"hello":{HELLO_REPLY}}},"extra":1}}"#),
        "unknown field `extra`",
    );
    assert_response_rejected(
        r#"{"ok":{"revision":1,"value":null}}"#,
        "missing field `id`",
    );
    assert_response_rejected(
        r#"{"id":null,"ok":{"revision":1,"value":null}}"#,
        "id равен null только у кадра error",
    );
    assert_response_rejected(r#"{"id":1}"#, "нет вида ответа");
    assert_response_rejected(
        &format!(r#"{{"id":1,"error":{LEAVING},"ok":{{"revision":1,"value":null}}}}"#),
        "2 вида ответа",
    );
    assert_response_rejected(r#"{"id":1,"ok":null}"#, "did not match any variant");
    assert_response_rejected(
        &format!(r#"{{"id":1,"error":{LEAVING},"ok":null}}"#),
        "did not match any variant",
    );
    assert_response_rejected(r#"{"id":"1","error":{LEAVING}}"#, "invalid type");
}

#[test]
fn ok_value_is_strict() {
    for ok in [
        r#"{"status":{}}"#.to_owned(),
        format!(r#"{{"hello":{HELLO_REPLY},"extra":1}}"#),
        r#"{"hello":{"protocol":1,"server":"0.1.0","pid":1,"owner":"ready","extra":1}}"#.to_owned(),
        r#"{"hello":{"protocol":1,"server":"0.1.0","pid":1,"owner":"starting"}}"#.to_owned(),
        r#"{"launch":{"activation":"requested","refused":[{"arg":"x","reason":"busy"}]}}"#
            .to_owned(),
        r#"{"launch":{"activation":"done","refused":[]}}"#.to_owned(),
        r#"{"launch":{"activation":"requested","refused":[],"extra":1}}"#.to_owned(),
        r#"{"revision":1,"value":null,"extra":1}"#.to_owned(),
        r#"{"events":[],"extra":1}"#.to_owned(),
        r#"{"ui":{"accepted":{"command":"app.quit","pid":1,"generation":0,"extra":1}}}"#.to_owned(),
        r#"{"ui":{"accepted":{"command":"app.quit","pid":1}}}"#.to_owned(),
        r#"{"ui":{"window":{}}}"#.to_owned(),
        r#"{"ui":{"unknown":{}}}"#.to_owned(),
    ] {
        assert_response_rejected(
            &format!(r#"{{"id":1,"ok":{ok}}}"#),
            "did not match any variant",
        );
    }
}

#[test]
fn error_frame_checks_retry_flag() {
    assert_response_rejected(
        r#"{"id":1,"error":{"code":"owner_leaving","message":"","retryable":false,"details":{}}}"#,
        "retryable",
    );
    assert_response_rejected(
        r#"{"id":1,"error":{"code":"gone","message":"","retryable":false,"details":{}}}"#,
        "unknown variant `gone`",
    );
    assert_response_rejected(
        r#"{"id":1,"error":{"code":"owner_leaving","message":"","retryable":true,"details":{},"extra":1}}"#,
        "unknown field `extra`",
    );
}

#[test]
fn launch_receipt_reasons_round_trip() {
    let receipt = LaunchReceipt {
        activation: Activation::NoUi,
        refused: RefuseReason::ALL
            .into_iter()
            .map(|reason| RefusedArg {
                arg: reason.as_str().into(),
                reason,
            })
            .collect(),
    };
    let response = Response::Ok {
        id: RequestId(6),
        value: OkValue::Launch(receipt),
    };
    let line = serde_json::to_vec(&response).unwrap();
    assert_eq!(Response::parse(&line).unwrap(), response);
}
