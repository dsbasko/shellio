//! Окно владельца на главном потоке: окно с заголовком, главное меню, зеркало состояния, монитор
//! цикла, единый обработчик [`UiCommand`], галерея в режиме `--gallery` и ответы на запросы `ui`
//! управляющего API.
//!
//! Поколение отрисовки и всё, что живёт до выхода процесса (меню, монитор, выбор оформления),
//! лежит рядом с окном, а не в нём: `gallery.window.recreate` заменяет окно и дерево галереи, а
//! поколение продолжает расти.
//!
//! В обычном режиме содержимое окна — раскладка открытой рабочей области ([`LayoutUi`]): её
//! получают доставки зеркала и изменения размера окна. Размер содержимого уходит ядру внутренним
//! входом [`CoreHandle::set_viewport`]: первый — до публикации сокета, дальше — с объединением до
//! конца оборота цикла и без повторов.
//!
//! Состояние живёт только на главном потоке. Функции модуля вызываются из замыканий
//! [`MainThread::exec`]. Обратные вызовы AppKit (меню, кнопки заголовка, смена ячейки) не трогают
//! состояние синхронно: они ставят работу в очередь главного потока, поэтому повторного входа в
//! состояние изнутри вызова AppKit нет.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, VecDeque};
use std::mem::ManuallyDrop;
use std::sync::Arc;

use shellio::bodies::{self, Failure, Ticket};
use shellio::gallery::{self, Changed, Effect, Generation, Intent};
use shellio::hint_targets::{HintTarget, hint_targets, retarget};
use shellio::history::{History, Jump};
use shellio::input_sink::{self, InputSink, KeyKind, KeyRecord};
use shellio::key_actions::{self, KeyAction};
use shellio::key_router::{Action, Decision, Focus, KeyRouter, Modal, PanelFocus, Responder};
use shellio::keymap_service::{KeymapPort, KeymapSnapshot};
use shellio::launch::WindowMode;
use shellio::owner;
use shellio::palette_source::{self, Facts};
use shellio::plugins;
use shellio::receipts::{Ending, Ring, Sent};
use shellio::scene::{self, SplitId, ViewportGate};
use shellio::which_titles;
use shellio_appkit::{
    App, ChromeIntent, ChromeState, CommandPalette, CommandPaletteState, CountKind, FieldFocus,
    Frame, HintItem, HintOverlay, HintOverlayState, InspectNode, KeyMonitor, KeyResponder,
    KeySinkEvent, KeyStroke, LoopMonitor, MENU_ACTIONS, MainMenu, MainThread, MainWindow,
    MenuAction, MenuEntry, MenuKey, MenuKeys, MenuState, MonitorEvent, NodeState, PaletteGroupSpec,
    PaletteIntent, PaletteRowSpec, PanelRow, Regime, SplitAxis, SplitDrag, Startup, StatusHint,
    StatusLine, TabStatus, TabStripFacts, TextFieldIntent, TextFieldState, ThemeSelection,
    Transition, TransitionPath, TreeListFacts, Verdict, VirtualListFacts,
    WhichKey as ShellioWhichKey, WhichKeyItem, WhichKeyState, ZoneButton, ZoneSide, chip_badges,
    instance_counts, late_callbacks,
};
use shellio_control::{
    Accepted, AnimationPath, AnimationRecord, Axis, AxisSource, BodiesBuiltView, CellContrast,
    CellMotion, CellView, ChromeView, CmdlineOutcome, CmdlineRunView, CmdlineStateView,
    ColorSpaceView, ComponentState, ComponentView, ExitChordState, GalleryCommand, GalleryView,
    HintLabelView, HintsStateView, HistoryPoint, HistoryView, InputEventKind, InputEventView,
    InputJournalView, InspectView, InstanceKind, InstanceView, LateCallbackView, ListRowView,
    MaterialView, MenuItemView, MenuView, ModeReceiver, ModeView, OverlaySourceView, OverlayView,
    OverlayViewKind, OwnedWindowRole, OwnedWindowView, PaletteCategory, PaletteGroupView,
    PaletteRowView, PaletteStateView, PanelMode, ReceiptSource, Rect, RunLoopView, ScreenView,
    Size, StartupView, StatsView, StripButtonView, TabAxis, TabItemView, TabRegime, TabStatusView,
    TabStripView, TabZone, TextFieldView, TreeListView, TreeRowView, UiRefusal, UiReply, UiRequest,
    VirtualListView, WhichItemView, WhichKeyView, WindowView,
};
use shellio_core::{
    AppearanceChoice, Command, CoreError, CoreHandle, Delivery, HistoryStep, ListAction, Mirror,
    MirrorStep, ModeAction, OverlayKind, PanelId, Reply, ToggleTarget, UiCommand, open_workbench,
};
use shellio_keymap::{
    Cancel, Category, Chord, Context, Diagnostic, HintLabels, HintQuery, HintStep, KeyCode, Keymap,
    Mode, Mods, Palette, PaletteRow, StatusText, WhichKey, status_hints, which_key,
    which_key_delay,
};
use shellio_plugin_view::{Handled, PanelRequest, ViewIntent};
use shellio_theme::AppearanceChoice as ThemeChoice;
use shellio_theme::{Appearance, Contrast, KeyMode, Motion, Overrides, Source};

use crate::gallery_ui::{GalleryUi, InputHooks};
use crate::layout_ui::{LayoutUi, PanelAction, StripKeys};

thread_local! {
    /// Окно владельца; создаётся один раз после выбора роли и живёт до выхода процесса.
    /// `ManuallyDrop` не даёт слоту деструктора потока: `exit()` выполняет деструкторы главного
    /// потока, а выходы владельца деструкторов не выполняют (`owner.rs`), окно и монитор цикла
    /// при выходе не освобождаются.
    static WINDOW: RefCell<Option<ManuallyDrop<OwnerWindow>>> = const { RefCell::new(None) };
    /// Зеркало состояния ядра; ведёт его задача владельца доставками.
    static MIRROR: RefCell<Option<Mirror>> = const { RefCell::new(None) };
    /// Ручка ядра: размер окна и команды раскладки из интерфейса. Ставится до публикации сокета.
    static CORE: RefCell<Option<CoreLink>> = const { RefCell::new(None) };
    /// Маршрутизатор клавиш: режимы панелей, набранный префикс и автомат аккорда выхода. Ячейка
    /// своя, а не поле окна: решение о клавише принимается и тогда, когда окно уже занято.
    static ROUTER: RefCell<KeyRouter<ModeReceiver, Pressed>> = RefCell::new(KeyRouter::default());
    /// Действующая таблица keymap: целый снимок службы, заменяется одной доставкой. Ячейка своя по
    /// той же причине, что у маршрутизатора.
    static KEYMAP: RefCell<Option<Arc<KeymapSnapshot>>> = const { RefCell::new(None) };
    /// Сообщение ошибки keymap, показанное последним снимком: исправный снимок убирает его.
    static KEYMAP_NOTE: RefCell<Option<String>> = const { RefCell::new(None) };
    /// Порт службы keymap и runtime его задач: действие `keymap.reload` с клавиши.
    static KEYMAP_PORT: RefCell<Option<(KeymapPort, tokio::runtime::Handle)>> =
        const { RefCell::new(None) };
    /// Номер последнего показанного сообщения строки статуса. Ячейка своя: обработчик монитора
    /// читает его, не заимствуя окно.
    static STATUS_SERIAL: Cell<u64> = const { Cell::new(0) };
    /// Панель с фокусом видимого окна для решения о клавише. Пишет её отрисовка раскладки:
    /// обработчик монитора работает внутри AppKit и окно не заимствует.
    static KEY_PANEL: RefCell<Option<PanelFocus<ModeReceiver>>> = const { RefCell::new(None) };
    /// История переходов воркспейсов (ADR-0010): точки по доставкам зеркала, в базу не пишется.
    static HISTORY: RefCell<History> = RefCell::new(History::default());
    /// Приёмник ввода галереи: поколение и журнал принятых событий.
    static INPUT_SINK: RefCell<InputSink> = const { RefCell::new(InputSink::new()) };
    /// Монитор клавиш живёт до выхода процесса, как окно.
    static KEY_MONITOR: RefCell<Option<ManuallyDrop<KeyMonitor>>> = const { RefCell::new(None) };
    /// Состав открытого наложения; открыто ли оно, решает маршрутизатор.
    static OVERLAY: RefCell<OverlayState> = RefCell::new(OverlayState::default());
    /// Подсказка which-key набранного префикса. Ячейка своя: подсказка не модальна и в состав
    /// наложения маршрутизатора не входит.
    static WHICH: RefCell<WhichState> = RefCell::new(WhichState::default());
}

/// Подсказка which-key: префикс, по которому стоит задержка появления, и показанный состав.
#[derive(Debug, Default)]
struct WhichState {
    prefix: Vec<Chord>,
    /// Поколение префикса: отложенный показ сверяет его и уходит, если префикс сменился.
    generation: u64,
    shown: Option<WhichKey>,
}

/// Состав открытого наложения до его вида: запрос и категория палитры, набранная часть меток,
/// текст командной строки. Строки палитры и цели меток строят задачи их видов.
#[derive(Debug, Default)]
struct OverlayState {
    kind: Option<OverlayKind>,
    palette: Option<Palette>,
    /// Панель-источник палитры в момент открытия: строки её действий выполняются для неё, даже
    /// если маршрутизатор уже снял закрытую панель из источника.
    panel: Option<PanelId>,
    /// Метки и их цели, собранные при открытии: номер метки — номер цели.
    hints: Option<(HintLabels, Vec<HintTarget>)>,
    text: String,
}

impl OverlayState {
    fn fresh(kind: OverlayKind) -> Self {
        Self {
            kind: Some(kind),
            ..Self::default()
        }
    }
}

/// Исходное нажатие, которое автомат аккорда выхода хранит для повтора. `in_filter` — в момент
/// нажатия ввод держала строка фильтра панели получателя: второй аккорд вернёт ввод в неё.
struct Pressed {
    stroke: KeyStroke,
    in_filter: bool,
}

/// Ядро для главного потока: вход размера окна и команды без ожидания, исход — задачей runtime.
struct CoreLink {
    core: CoreHandle,
    runtime: tokio::runtime::Handle,
    /// Квитанции действий главного потока: пишутся при завершении действия или квитанции ядра,
    /// читаются `ui.receipts`.
    receipts: RefCell<Ring>,
}

/// Завершение действия главного потока для кольца квитанций; действие без записи (`None`) в него
/// не пишется. Вызывается из очереди главного потока, вне заимствований слота ядра на запись.
fn note_receipt(sent: Option<Sent>, ending: Ending<'_>) {
    if let Some(sent) = sent {
        record_receipt(sent, ending);
    }
}

fn record_receipt(sent: Sent, ending: Ending<'_>) {
    CORE.with(|slot| {
        if let Some(link) = slot.borrow().as_ref() {
            link.receipts.borrow_mut().record(sent, ending);
        }
    });
}

impl CoreLink {
    /// Подать команду без ожидания. Отказ разбирает [`on_refused`] на главном потоке. `tag` —
    /// запись кольца квитанций о действии, которое подало команду.
    fn send(&self, command: Command, tag: Option<Sent>) {
        let sent = command.clone();
        match self.core.try_request(command) {
            Ok(receipt) => {
                self.runtime.spawn(async move {
                    match receipt.await {
                        // Размер без изменений события не даёт: если зеркало уже на ревизии ответа,
                        // раскладка ставится заново, иначе её поставит доставка. Так вид, сдвинутый
                        // перетаскиванием разделителя, возвращается к модели.
                        Ok(Ok(reply)) if matches!(sent, Command::LayoutResize { .. }) => {
                            defer(move |app| {
                                let caught_up = MIRROR.with(|mirror| {
                                    mirror
                                        .borrow()
                                        .as_ref()
                                        .is_some_and(|mirror| mirror.revision() >= reply.revision)
                                });
                                if caught_up {
                                    refresh_layout(app);
                                }
                            });
                        }
                        Ok(Ok(reply)) if tag.is_some() => {
                            defer(move |_| note_receipt(tag, Ending::Reply(&reply)));
                        }
                        Ok(Ok(_)) => {}
                        Ok(Err(error)) => defer(move |app| {
                            if !on_refused(app, &sent, &error) {
                                note_receipt(tag, Ending::Refused(&error));
                            }
                        }),
                        Err(_) => defer(move |app| {
                            note_receipt(tag, Ending::NoReply);
                            owner::say_err(format_args!(
                                "shellio: {} отклонена: ядро не ответило",
                                sent.name()
                            ));
                            with_window(|owner| owner.show_status(Some("Ядро не ответило")));
                            refresh_layout(app);
                        }),
                    }
                });
            }
            // Ранний отказ (`busy`) — тоже завершение квитанции.
            Err(error) => defer(move |app| {
                if !on_refused(app, &sent, &error) {
                    note_receipt(tag, Ending::Refused(&error));
                }
            }),
        }
    }

    /// Подать команду тела панели без ожидания. `done` получает исход в задаче runtime, а ранний
    /// отказ — сразу: строка статуса его не показывает, причину показывает само тело.
    fn request(
        &self,
        command: Command,
        done: impl FnOnce(Result<Reply, Failure>) + Send + 'static,
    ) {
        match self.core.try_request(command) {
            Ok(receipt) => {
                self.runtime.spawn(async move {
                    done(match receipt.await {
                        Ok(Ok(reply)) => Ok(reply),
                        Ok(Err(error)) => Err(Failure::Core(error)),
                        Err(_) => Err(Failure::NoReply),
                    });
                });
            }
            Err(error) => done(Err(Failure::Core(error))),
        }
    }
}

/// Запрос тела панели: команда своей панели или открытие панели плагина в её рабочей области.
/// Вызывается изнутри AppKit, поэтому окно владельца не трогает: команда уходит ядру сразу, исход
/// возвращается в очередь главного потока по номеру запроса.
fn send_request(ticket: Ticket, request: PanelRequest) {
    let command = MIRROR.with(|mirror| {
        let mirror = mirror.try_borrow().ok();
        let state = mirror
            .as_ref()
            .and_then(|mirror| mirror.as_ref())
            .map(|mirror| mirror.state());
        bodies::request_command(state, ticket.panel, request)
    });
    let name = command.name();
    // Запрос вида плагина — нажатие в теле панели.
    let opened = Sent::opened(&command, ReceiptSource::Button);
    let finish = move |outcome: Result<Reply, Failure>| {
        defer(move |_| {
            if let Some(sent) = opened {
                match &outcome {
                    Ok(reply) => record_receipt(sent, Ending::Reply(reply)),
                    Err(Failure::Core(error)) => record_receipt(sent, Ending::Refused(error)),
                    Err(Failure::NoReply) => record_receipt(sent, Ending::NoReply),
                    // Команда ядру не подавалась: квитанции нет.
                    Err(Failure::NoCore) => {}
                }
            }
            finish_request(ticket, name, outcome.map(|_| ()));
        });
    };
    // Исход уходит ровно один раз: ядру вместе с командой или сразу, если ядра нет.
    let mut finish = Some(finish);
    CORE.with(|slot| {
        if let Ok(link) = slot.try_borrow()
            && let Some(link) = link.as_ref()
            && let Some(finish) = finish.take()
        {
            link.request(command, finish);
        }
    });
    if let Some(finish) = finish {
        finish(Err(Failure::NoCore));
    }
}

/// Исход запроса тела `ticket`: его получает тело, если оно живо и запрос у него последний.
fn finish_request(ticket: Ticket, name: &'static str, outcome: Result<(), Failure>) {
    let outcome = outcome.map_err(|failure| {
        let refusal = bodies::refusal(&failure);
        owner::say_err(format_args!(
            "shellio: {name} панели {} отклонена: {refusal}",
            ticket.panel
        ));
        refusal
    });
    let done = with_window(|owner| {
        owner
            .layout
            .as_ref()
            .and_then(|layout| layout.take_done(ticket))
    })
    .flatten();
    // Вне окна владельца: тело вправе ответить на исход новым запросом.
    if let Some(done) = done {
        done(outcome);
    }
}

/// Отказ ядра на команду интерфейса. `split_needs_panel` деления без `open` открывает выбор
/// «Открыть в новой части» у кнопки (ADR-0009); остальные отказы пишут причину в строку статуса и
/// показывают раскладку модели заново: событий после отказа не будет, а вид мог остаться там, куда
/// его увёл пользователь. Ответ `true` — отказ открыл выбор: действие ещё не исполнено, его
/// исполнит пункт выбора, поэтому записи в кольце квитанций у такого нажатия нет.
fn on_refused(app: &App, sent: &Command, error: &CoreError) -> bool {
    owner::say_err(format_args!("shellio: {} отклонена: {error}", sent.name()));
    if let Some(panel) = scene::picker_target(sent, error)
        && with_window(|owner| owner.open_picker(panel)) == Some(true)
    {
        return true;
    }
    let text = wrong_axis(sent, error).unwrap_or_else(|| {
        MIRROR.with(|mirror| {
            let mirror = mirror.borrow();
            let state = mirror.as_ref().map(Mirror::state);
            scene::refusal_text(sent, error, |panel| {
                state
                    .and_then(|state| state.panels.get(&panel))
                    .map(|panel| panel.title.to_string())
            })
        })
    });
    with_window(|owner| owner.show_status(Some(&text)));
    refresh_layout(app);
    false
}

/// Сообщение отказа `split_wrong_axis`: зона панели по раскладке окна и действующая клавиша
/// деления её оси из таблицы keymap. `None` — отказ другой или зона не найдена.
fn wrong_axis(sent: &Command, error: &CoreError) -> Option<String> {
    if error.detail_str("reason") != Some("split_wrong_axis") {
        return None;
    }
    let Command::LayoutSplit { panel, .. } = sent else {
        return None;
    };
    let zone = with_window(|owner| {
        let bench = owner.layout.as_ref()?.scene()?.bench.as_ref()?;
        match bench.layout.locate(*panel)? {
            shellio_layout::Location::Part { zone, .. } => Some(zone),
            shellio_layout::Location::Navigator => None,
        }
    })
    .flatten()?;
    let axis = error.detail_str("axis")?;
    let key = KEYMAP.with(|slot| {
        let snapshot = slot.borrow().clone()?;
        snapshot
            .keymap
            .key_for(
                &[Context::Navigation, Context::Global],
                "layout.split",
                Some(&serde_json::json!({ "axis": axis })),
            )
            .map(ToString::to_string)
    });
    Some(scene::wrong_axis_text(zone, key.as_deref()))
}

/// Сколько последних записей держит журнал анимаций `ui.stats`.
const ANIMATION_LOG: usize = 256;

struct OwnerWindow {
    mode: WindowMode,
    menu: MainMenu,
    monitor: Option<LoopMonitor>,
    choice: AppearanceChoice,
    /// Контраст и движение `ui.cell`; `None` — ось без переопределения.
    cell_contrast: Option<Contrast>,
    cell_motion: Option<Motion>,
    /// Поколение отрисовки: растёт с каждой видимой сменой состояния окна.
    generation: Generation,
    /// Поколение первой проекции снимка зеркала: старт процесса считается до отрисовки с ним.
    first_snapshot: Option<u64>,
    /// Старт процесса, однажды наблюдённый.
    startup: Option<Startup>,
    /// Последние проекции зеркала: новое окно получает их сразу.
    projection: Option<Projection>,
    /// Выбор ячейки, который уже получили компоненты галереи.
    applied: ThemeSelection,
    /// Пересоздаваемая часть: окно и дерево галереи.
    window: MainWindow,
    gallery: Option<GalleryUi>,
    /// Раскладка обычного режима; в галерее её нет.
    layout: Option<LayoutUi>,
    /// Строка статуса обычного режима.
    status: Option<StatusLine>,
    /// Окно палитры обычного режима: один вид на всё время окна, показ по состоянию наложения.
    palette: Option<CommandPalette>,
    /// Подсказка which-key обычного режима: один вид, показ по ячейке `WHICH`.
    which: Option<ShellioWhichKey>,
    /// Слой меток `f` обычного режима: один вид, показ по составу открытого наложения.
    hint_layer: Option<HintOverlay>,
    /// Плагины пунктов меню «Открыть панель» по порядку пунктов.
    open_rows: Vec<&'static str>,
    /// Размер окна для ядра.
    viewport: ViewportGate,
    /// Смены компонентов, ещё не записанные в журнал: запись получает поколение публикации.
    pending: Vec<(&'static str, Transition)>,
    /// Журнал анимаций `ui.stats`.
    animations: VecDeque<AnimationRecord>,
}

impl OwnerWindow {
    fn bump(&mut self) {
        self.sync_which_view();
        self.sync_hint_view();
        self.generation.bump();
        self.publish();
    }

    /// Вид which-key следует за ячейкой `WHICH`: состав, показ и место над строкой статуса. Под
    /// модальным наложением подсказки нет, как в чтении `ui.overlay`.
    fn sync_which_view(&self) {
        let Some(view) = &self.which else {
            return;
        };
        let modal = with_router(|router| router.modal().is_some()).unwrap_or(false);
        let shown = WHICH
            .with(|which| which.borrow().shown.clone())
            .filter(|_| !modal);
        match shown {
            Some(shown) => {
                view.set_state(WhichKeyState {
                    prefix: shown.prefix,
                    title: shown.title.unwrap_or_default(),
                    items: shown
                        .items
                        .into_iter()
                        .map(|item| WhichKeyItem {
                            key: item.key,
                            label: item.label,
                            group: item.group,
                        })
                        .collect(),
                });
                view.show(&self.window);
            }
            None if view.is_shown() => view.hide(),
            None => {}
        }
    }

    /// Слой меток следует за составом открытого наложения: метки, совпавшие с набранной частью,
    /// у кадров своих целей. Без открытых меток слой скрыт.
    fn sync_hint_view(&self) {
        let Some(view) = &self.hint_layer else {
            return;
        };
        let modal = with_router(|router| router.modal().map(|modal| modal.kind)).flatten();
        let state = OVERLAY.with(|state| {
            let state = state.borrow();
            if modal != Some(OverlayKind::Hints) || state.kind != Some(OverlayKind::Hints) {
                return None;
            }
            let (labels, targets) = state.hints.as_ref()?;
            Some(HintOverlayState {
                typed: labels.typed().to_owned(),
                items: labels
                    .visible()
                    .filter_map(|(index, text)| {
                        let target = targets.get(index)?;
                        Some(HintItem {
                            number: index + 1,
                            label: text.to_owned(),
                            target: Frame::new(
                                target.frame.x,
                                target.frame.y,
                                target.frame.width,
                                target.frame.height,
                            ),
                        })
                    })
                    .collect(),
            })
        });
        match state {
            Some(state) => {
                view.set_state(state);
                view.show(&self.window);
            }
            None if view.is_shown() => view.hide(),
            None => {}
        }
    }

    /// Текущее поколение получают монитор цикла и маркер галереи, смены компонентов уходят в
    /// журнал анимаций с этим поколением.
    fn publish(&mut self) {
        let generation = self.generation.get();
        for (component, transition) in self.pending.drain(..) {
            if self.animations.len() == ANIMATION_LOG {
                self.animations.pop_front();
            }
            self.animations.push_back(AnimationRecord {
                generation,
                component: component.to_owned(),
                path: match transition.path {
                    TransitionPath::Animated => AnimationPath::Animated,
                    TransitionPath::Instant => AnimationPath::Instant,
                },
                added: transition.added,
            });
        }
        if let Some(monitor) = &self.monitor {
            monitor.set_generation(generation);
        }
        if let Some(gallery) = &self.gallery {
            gallery.set_generation(generation);
        }
    }

    /// Компоненты галереи получают действующую ячейку. `false` — она уже применена: отложенный
    /// отклик окна на смену, которую команда применила сама, поколения не прибавляет.
    fn sync_theme(&mut self) -> bool {
        let selection = self.window.selection();
        if selection == self.applied {
            return false;
        }
        self.applied = selection;
        if let Some(gallery) = &self.gallery {
            let transitions = gallery.set_theme(&selection.theme(), selection.motion);
            self.pending.extend(transitions);
        }
        if let Some(layout) = self.layout.as_mut() {
            layout.set_theme(&selection.theme(), selection.motion);
        }
        if let Some(status) = &self.status {
            status.set_theme(&selection.theme());
        }
        if let Some(palette) = &self.palette {
            palette.set_theme(&selection.theme());
        }
        if let Some(which) = &self.which {
            which.set_theme(&selection.theme());
        }
        if let Some(hints) = &self.hint_layer {
            hints.set_theme(&selection.theme());
        }
        true
    }

    /// Сообщение строки статуса; `None` убирает его.
    fn show_status(&self, message: Option<&str>) {
        if let Some(status) = &self.status {
            if message.is_some() {
                mark_status();
            }
            status.set_message(message);
        }
    }

    /// Сообщение ошибки `keymap.toml` по диагностике действующего снимка: первая запись замещает
    /// подсказки, исправный снимок убирает только своё прежнее сообщение. `fresh` — пришёл новый
    /// снимок: его сообщение показывается заново; иначе — только то, что ещё не показано (снимок
    /// пришёл раньше строки статуса). `true` — строка изменилась.
    fn show_keymap_note(&self, fresh: bool) -> bool {
        let Some(status) = &self.status else {
            return false;
        };
        let note = KEYMAP.with(|slot| {
            let snapshot = slot.borrow().clone()?;
            snapshot
                .keymap
                .diagnostics()
                .first()
                .map(Diagnostic::message)
        });
        let shown = status.message();
        let previous = KEYMAP_NOTE.with(|slot| slot.replace(note.clone()));
        match note {
            Some(note) if note != shown && (fresh || previous.as_ref() != Some(&note)) => {
                mark_status();
                status.set_message(Some(&note));
                true
            }
            None if previous.is_some_and(|previous| previous == shown) => {
                status.set_message(None);
                true
            }
            Some(_) | None => false,
        }
    }

    /// Плашка режима, набранный префикс и подсказки строки статуса по маршрутизатору и действующей
    /// таблице (правило «Строка статуса»). `true` — строка изменилась; занятый маршрутизатор её
    /// не трогает.
    fn show_hints(&self) -> bool {
        let Some(status) = &self.status else {
            return false;
        };
        let Some(snapshot) = KEYMAP.with(|slot| slot.borrow().clone()) else {
            return false;
        };
        let panel = KEY_PANEL.with(|slot| slot.borrow().clone());
        let field = self.layout.as_ref().and_then(LayoutUi::typing).is_some();
        let Some((mode, pending)) = with_router(|router| {
            let mode = panel
                .as_ref()
                .map_or(Mode::Navigation, |panel| router.mode(&panel.receiver));
            (mode, router.pending().to_vec())
        }) else {
            return false;
        };
        // which-key есть у окна с раскладкой: галерея её не показывает.
        let which = self.layout.is_some() && sync_which_key(&snapshot.keymap, &pending);
        let text = status_hints(
            &snapshot.keymap,
            &HintQuery {
                mode,
                field,
                pending: &pending,
                message: None,
                plugin: panel.as_ref().map(|panel| panel.plugin.as_str()),
                panel_hints: &[],
                input_receiver: panel.as_ref().is_some_and(|panel| panel.input),
                // Палитра есть у обычного окна: галерея её не открывает.
                palette: self.palette.is_some(),
            },
        );
        let (prefix, hints) = match text {
            StatusText::Prefix(prefix) => (Some(prefix), Vec::new()),
            StatusText::Hints(hints) => (None, hints),
            StatusText::Message(_) => (None, Vec::new()),
        };
        let hints: Vec<StatusHint> = hints
            .into_iter()
            .map(|hint| StatusHint {
                keys: hint.keys,
                label: hint.label,
            })
            .collect();
        let mode = match mode {
            Mode::Navigation => KeyMode::Navigation,
            Mode::Input => KeyMode::Input,
        };
        status.set_left(mode, prefix.as_deref(), &hints) || which
    }

    /// Выбор «Открыть в новой части» для панели `panel` по состоянию зеркала; `false` — выбора
    /// нет.
    fn open_picker(&self, panel: shellio_core::PanelId) -> bool {
        let Some(layout) = self.layout.as_ref() else {
            return false;
        };
        MIRROR.with(|mirror| {
            mirror
                .borrow()
                .as_ref()
                .is_some_and(|mirror| layout.open_picker(mirror.state(), panel))
        })
    }

    /// Размер содержимого окна в целых точках.
    fn content_viewport(&self) -> shellio_layout::Viewport {
        let (width, height) = self.window.content_size();
        scene::viewport_of(width, height)
    }

    /// Раскладка по зеркалу и текущему размеру окна, строка статуса под ней, кнопки зон заголовка
    /// и пункты меню «Вид» по сцене.
    fn render_layout(&mut self, app: &App) {
        let viewport = self.content_viewport();
        // Снимок keymap мог прийти раньше окна: клавиши кнопок и сообщение его диагностики
        // берутся из него при первой раскладке.
        self.show_keymap_note(false);
        let Some(layout) = self.layout.as_mut() else {
            return;
        };
        layout.set_button_keys(strip_keys());
        let visible = self.window.is_visible();
        let open = MIRROR.with(|mirror| {
            let mirror = mirror.borrow();
            let state = mirror.as_ref().map(Mirror::state);
            layout.render(app, state, viewport, visible);
            let focus = visible.then(|| layout.key_focus()).flatten();
            let typing = layout.typing().map(ModeReceiver::Panel);
            sync_keys(focus, typing, |panel| {
                state.is_none_or(|state| state.panels.contains_key(&panel))
            });
            if let Some(panels) = input_panels() {
                layout.set_input_panels(panels);
            }
            state.is_some_and(|state| open_workbench(state).is_some())
        });
        self.menu.set_open_enabled(open);
        self.show_hints();
        let Some(layout) = self.layout.as_ref() else {
            return;
        };
        let Some(scene) = layout.scene() else {
            return;
        };
        if let Some(status) = &self.status {
            let top = scene.area.y + scene.area.height;
            let height = f64::from(viewport.height) - top;
            self.window.place(
                status,
                Frame::new(0.0, top, f64::from(viewport.width), height.max(0.0)),
            );
        }
        let zones = scene.zone_buttons().map(|buttons| {
            (
                buttons.side == shellio_layout::Hideable::Plate,
                [buttons.navigator, buttons.bottom, buttons.side_shown],
            )
        });
        self.window.chrome().set_zones(zones);
        self.menu.set_zones(zones);
    }

    /// Переопределения окна: выбор оформления и оси галереи.
    fn overrides(&self) -> Overrides {
        let mut overrides = Overrides {
            choice: theme_choice(self.choice),
            cell_contrast: self.cell_contrast,
            cell_motion: self.cell_motion,
            ..Overrides::default()
        };
        if let Some(gallery) = &self.gallery {
            let state = gallery.model().state();
            overrides.gallery_appearance = state.appearance;
            overrides.gallery_contrast = state.contrast;
            overrides.gallery_motion = state.motion;
        }
        overrides
    }

    fn set_choice(&mut self, choice: AppearanceChoice) {
        self.choice = choice;
        let theme = theme_choice(choice);
        self.window.set_overrides(self.overrides());
        self.window.chrome().set_choice(theme);
        self.menu.set_appearance(theme);
        self.sync_theme();
        self.bump();
    }

    /// `ui.cell`: названная ось получает переопределение или теряет его, пропущенная не меняется.
    /// Поколение растёт на каждом запросе: сценарий ждёт его по `ui.window`.
    fn set_cell(&mut self, contrast: Option<CellContrast>, motion: Option<CellMotion>) -> UiReply {
        if let Some(contrast) = contrast {
            self.cell_contrast = contrast.value().map(|value| match value {
                shellio_control::Contrast::Normal => Contrast::Normal,
                shellio_control::Contrast::Increased => Contrast::Increased,
            });
        }
        if let Some(motion) = motion {
            self.cell_motion = motion.value().map(|value| match value {
                shellio_control::Motion::Normal => Motion::Normal,
                shellio_control::Motion::Reduced => Motion::Reduced,
            });
        }
        self.window.set_overrides(self.overrides());
        self.sync_theme();
        self.bump();
        let (lightness, contrast, motion) = self.axes();
        UiReply::Cell(CellView {
            pid: std::process::id(),
            lightness,
            contrast,
            motion,
            generation: self.generation.get(),
        })
    }

    /// `ui.window.size`: окно получает размер в точках, раскладка встаёт по нему до ответа.
    /// Поколение растёт на каждом запросе: сценарий ждёт его по `ui.window`.
    fn set_window_size(
        &mut self,
        app: &App,
        command: &'static str,
        width: u32,
        height: u32,
    ) -> UiReply {
        self.window.set_size(f64::from(width), f64::from(height));
        self.render_layout(app);
        self.bump();
        UiReply::Accepted(Accepted {
            command: command.to_owned(),
            pid: std::process::id(),
            generation: self.generation.get(),
        })
    }

    /// Оси окна с источниками.
    fn axes(
        &self,
    ) -> (
        Axis<shellio_control::Lightness>,
        Axis<shellio_control::Contrast>,
        Axis<shellio_control::Motion>,
    ) {
        let selection = self.window.selection();
        let cell = selection.cell;
        (
            Axis {
                value: match cell.cell.appearance {
                    Appearance::Light => shellio_control::Lightness::Light,
                    Appearance::Dark => shellio_control::Lightness::Dark,
                },
                source: source(cell.appearance_source),
            },
            Axis {
                value: match cell.cell.contrast {
                    Contrast::Normal => shellio_control::Contrast::Normal,
                    Contrast::Increased => shellio_control::Contrast::Increased,
                },
                source: source(cell.contrast_source),
            },
            Axis {
                value: match selection.motion {
                    Motion::Normal => shellio_control::Motion::Normal,
                    Motion::Reduced => shellio_control::Motion::Reduced,
                },
                source: source(selection.motion_source),
            },
        )
    }

    /// Проекции зеркала: имя воркспейса в заголовке и системный заголовок окна.
    fn project(&mut self, projection: Projection) {
        self.show_projection(&projection);
        self.projection = Some(projection);
        self.bump();
        self.first_snapshot.get_or_insert(self.generation.get());
    }

    fn show_projection(&self, projection: &Projection) {
        self.window.set_title(projection.title.as_deref());
        self.window
            .chrome()
            .set_workspace(projection.workspace.as_deref());
    }

    /// Служебная команда галереи. Эффект исполняется в этом же обороте главного потока, поэтому
    /// маркер поколения из ответа появляется в том же проходе отображения, что и состояние.
    fn gallery_command(
        &mut self,
        app: &App,
        command: GalleryCommand,
    ) -> Result<UiReply, UiRefusal> {
        gallery::admit(self.mode)?;
        let Some(gallery) = self.gallery.as_mut() else {
            return Err(UiRefusal::NoUi);
        };
        let effect = gallery.model_mut().apply(command, &mut self.generation);
        let generation = self.generation.get();
        match effect {
            Effect::Theme => self.window.set_overrides(self.overrides()),
            Effect::Focus(focus) => gallery.focus(&self.window, focus),
            Effect::OpenMenu => gallery.open_menu(),
            Effect::Rebuild => gallery.rebuild(app, &self.window, generation),
            Effect::RecreateWindow => self.recreate_window(app),
            Effect::Place(screen) => {
                let moved = usize::try_from(screen)
                    .is_ok_and(|screen| self.window.move_to_screen(app, screen));
                if !moved {
                    owner::say_err(format_args!(
                        "shellio: экрана {screen} нет, окно галереи осталось на месте"
                    ));
                }
            }
            Effect::Reset => {
                // Сброс со страницы «Калибровка» возвращает сцену компонентов новым деревом.
                if gallery.built_page() != Some(gallery.model().state().page) {
                    gallery.rebuild(app, &self.window, generation);
                }
                self.window.set_overrides(self.overrides());
                let motion = self.window.selection().motion;
                if let Some(gallery) = &self.gallery {
                    let transitions = gallery.reset_view(&self.window, motion);
                    self.pending.extend(transitions);
                }
            }
            Effect::Tabs(strip) => {
                let transition = gallery.show_tabs(strip);
                self.pending.extend(transition);
            }
            Effect::List => gallery.show_list(),
            Effect::ListScroll(row) => gallery.scroll_list(row),
            Effect::ListTicks => gallery.apply_list_ticks(),
            Effect::Tree => gallery.show_tree(),
            Effect::Page => {}
            Effect::InputReset => INPUT_SINK.with(|sink| sink.borrow_mut().reset()),
        }
        self.sync_theme();
        self.publish();
        Ok(UiReply::Accepted(Accepted {
            command: command.name().to_owned(),
            pid: std::process::id(),
            generation: self.generation.get(),
        }))
    }

    /// Закрывает и отпускает окно вместе с деревом галереи и строит новые. Видимость, ячейка,
    /// движение и проекции зеркала переходят в новое окно.
    fn recreate_window(&mut self, app: &App) {
        let visible = self.window.is_visible();
        if let Some(gallery) = self.gallery.as_mut() {
            gallery.retire_with_window(&self.window);
        }
        let old = std::mem::replace(&mut self.window, new_window(app));
        drop(old);
        self.window.set_overrides(self.overrides());
        self.window.chrome().set_choice(theme_choice(self.choice));
        if let Some(projection) = &self.projection {
            self.show_projection(projection);
        }
        let generation = self.generation.get();
        if let Some(gallery) = self.gallery.as_mut() {
            gallery.build(app, &self.window, generation);
        }
        // Новое дерево строится уже в ячейке нового окна.
        self.applied = self.window.selection();
        if visible {
            self.window.show();
        }
    }

    /// Чтение `gallery.input.journal`: журнал нынешнего приёмника и его режим по маршрутизатору.
    fn input_journal(&self) -> Result<UiReply, UiRefusal> {
        gallery::admit(self.mode)?;
        let (receiver, events) = INPUT_SINK.with(|sink| {
            let sink = sink.borrow();
            let events = sink.journal().map(input_event).collect();
            (ModeReceiver::GalleryInput(sink.generation()), events)
        });
        let mode = with_router(|router| router.mode(&receiver)).unwrap_or(Mode::Navigation);
        Ok(UiReply::InputJournal(InputJournalView {
            pid: std::process::id(),
            receiver,
            mode: wire_panel_mode(mode),
            events,
        }))
    }

    fn gallery_intent(&mut self, intent: &Intent) {
        if let Some(gallery) = self.gallery.as_mut() {
            match gallery.model_mut().intent(intent, &mut self.generation) {
                Some(Changed::Tabs(strip)) => {
                    let transition = gallery.show_tabs(strip);
                    self.pending.extend(transition);
                }
                Some(Changed::List) => gallery.show_list(),
                Some(Changed::Tree) => gallery.show_tree(),
                None => {}
            }
            self.publish();
        }
    }

    fn gallery_view(&self) -> Option<GalleryView> {
        let gallery = self.gallery.as_ref()?;
        let model = gallery.model();
        let state = model.state();
        Some(GalleryView {
            tree: gallery.tree_number().unwrap_or(model.tree()),
            window: model.window(),
            appearance: state.appearance.map(|value| match value {
                Appearance::Light => shellio_control::Lightness::Light,
                Appearance::Dark => shellio_control::Lightness::Dark,
            }),
            contrast: state.contrast.map(|value| match value {
                Contrast::Normal => shellio_control::Contrast::Normal,
                Contrast::Increased => shellio_control::Contrast::Increased,
            }),
            motion: state.motion.map(|value| match value {
                Motion::Normal => shellio_control::Motion::Normal,
                Motion::Reduced => shellio_control::Motion::Reduced,
            }),
            focus: state.focus,
            screen: state.screen,
            intents: state.intents.iter().cloned().collect(),
        })
    }

    fn window_view(&self, app: &App, mirror_revision: Option<u64>) -> WindowView {
        let facts = self.window.facts(app);
        let (lightness, contrast, motion) = self.axes();
        let screen = facts.screen;
        let main = OwnedWindowView {
            role: OwnedWindowRole::Main,
            window_number: facts.number,
            frame: rect(facts.frame),
            backing_scale_factor: facts.backing_scale_factor,
            visible: facts.visible,
            key: facts.key,
            screen: screen.and_then(|index| u32::try_from(index).ok()),
        };
        let palette = self.palette.as_ref().map(|palette| {
            let palette = palette.window_facts();
            OwnedWindowView {
                role: OwnedWindowRole::Palette,
                window_number: palette.number,
                frame: rect(palette.frame),
                backing_scale_factor: palette.backing_scale_factor,
                visible: palette.visible,
                key: palette.key,
                screen: palette.screen.and_then(|index| u32::try_from(index).ok()),
            }
        });
        WindowView {
            pid: std::process::id(),
            mode: wire_mode(self.mode),
            window_number: facts.number,
            frame: rect(facts.frame),
            backing_scale_factor: facts.backing_scale_factor,
            visible: facts.visible,
            key: facts.key,
            color_space: ColorSpaceView {
                name: facts.color_space,
                srgb: facts.color_space_srgb,
            },
            title: facts.title,
            appearance_choice: self.choice,
            lightness,
            contrast,
            motion,
            screen: screen.and_then(|index| u32::try_from(index).ok()),
            maximum_frames_per_second: screen
                .and_then(|index| facts.screens.get(index))
                .map(|screen| screen.maximum_frames_per_second),
            screens: facts
                .screens
                .into_iter()
                .enumerate()
                .map(|(index, screen)| ScreenView {
                    index: u32::try_from(index).unwrap_or(u32::MAX),
                    name: screen.name,
                    frame: rect(screen.frame),
                    backing_scale_factor: screen.backing_scale_factor,
                    maximum_frames_per_second: screen.maximum_frames_per_second,
                })
                .collect(),
            mirror_revision,
            generation: self.generation.get(),
            windows: std::iter::once(main).chain(palette).collect(),
        }
    }

    fn inspect_view(&self) -> InspectView {
        let inspection = self.window.inspect();
        let (width, height) = inspection.content;
        let gallery_nodes = self.gallery.as_ref().map(GalleryUi::inspect);
        let layout_nodes = self.layout.as_ref().map(LayoutUi::inspect);
        let status_nodes = self
            .status
            .as_ref()
            .map(|status| status.inspect(STATUS_ID, None))
            .map(|nodes| nodes.into_iter().map(status_keyed).collect::<Vec<_>>());
        let palette_nodes = self
            .palette
            .as_ref()
            .filter(|palette| palette.is_open())
            .map(|palette| palette.inspect(&self.window));
        let which_nodes = self.which.as_ref().map(ShellioWhichKey::inspect);
        let hint_nodes = self.hint_layer.as_ref().map(HintOverlay::inspect);
        InspectView {
            window_number: self.window.number(),
            content: Size { width, height },
            generation: self.generation.get(),
            components: inspection
                .nodes
                .into_iter()
                .chain(gallery_nodes.into_iter().flatten())
                .chain(status_nodes.into_iter().flatten())
                .chain(palette_nodes.into_iter().flatten())
                .chain(which_nodes.into_iter().flatten())
                .chain(hint_nodes.into_iter().flatten())
                .map(component)
                .chain(layout_nodes.into_iter().flatten())
                .collect(),
            materials: inspection
                .materials
                .into_iter()
                .map(|material| MaterialView {
                    class: material.class,
                    visible: material.visible,
                    own: material.own,
                })
                .collect(),
            gallery: self.gallery_view(),
        }
    }

    fn stats_view(&mut self) -> StatsView {
        if self.startup.is_none()
            && let (Some(monitor), Some(generation)) = (&self.monitor, self.first_snapshot)
        {
            self.startup = self.window.startup(monitor, generation);
        }
        StatsView {
            pid: std::process::id(),
            generation: self.generation.get(),
            instances: instance_counts()
                .into_iter()
                .map(|count| InstanceView {
                    kind: match count.kind {
                        CountKind::Class => InstanceKind::Class,
                        CountKind::Owner => InstanceKind::Owner,
                    },
                    name: count.name.to_owned(),
                    created: count.created,
                    released: count.released,
                    live: count.live(),
                })
                .collect(),
            late_callbacks: late_callbacks()
                .into_iter()
                .map(|late| LateCallbackView {
                    site: late.site.to_owned(),
                    count: late.count,
                })
                .collect(),
            run_loop: self.monitor.as_ref().map(|monitor| {
                let stats = monitor.stats();
                let loops = stats.iterations;
                RunLoopView {
                    iterations: loops.iterations,
                    wakeups: loops.wakeups,
                    unpaired: loops.unpaired,
                    display_passes: loops.display_passes,
                    over_budget: loops.over_budget,
                    max_ms: loops.max_ms,
                    p95_ms: loops.p95_ms,
                    window: u64::try_from(loops.window).unwrap_or(u64::MAX),
                    draws: stats.draws,
                }
            }),
            startup: self.startup.map(|startup| StartupView {
                first_draw_ms: startup.first_draw_ms,
                key_ms: startup.key_ms,
                key_draw_ms: startup.key_draw_ms,
            }),
            animations: self.animations.iter().cloned().collect(),
            row_views: self
                .gallery
                .as_ref()
                .map(GalleryUi::row_views)
                .unwrap_or_default(),
            gallery_trees: self
                .gallery
                .as_ref()
                .map(GalleryUi::retired_trees)
                .unwrap_or_default(),
            bodies_built: self
                .layout
                .as_ref()
                .map(LayoutUi::bodies_built)
                .unwrap_or_default()
                .into_iter()
                .map(|(panel, built)| BodiesBuiltView { panel, built })
                .collect(),
            key_latency: self
                .gallery
                .as_ref()
                .and_then(|gallery| gallery.key_latency(self.monitor.as_ref())),
        }
    }

    fn accepted(&self, command: UiCommand) -> UiReply {
        self.accepted_named(command.name())
    }

    fn accepted_named(&self, command: &str) -> UiReply {
        UiReply::Accepted(Accepted {
            command: command.to_owned(),
            pid: std::process::id(),
            generation: self.generation.get(),
        })
    }
}

fn rect(frame: Frame) -> Rect {
    Rect {
        x: frame.x,
        y: frame.y,
        width: frame.width,
        height: frame.height,
    }
}

fn wire_mode(mode: WindowMode) -> shellio_control::WindowMode {
    match mode {
        WindowMode::Normal => shellio_control::WindowMode::Normal,
        WindowMode::Gallery => shellio_control::WindowMode::Gallery,
    }
}

fn source(source: Source) -> AxisSource {
    match source {
        Source::System => AxisSource::System,
        Source::Choice => AxisSource::UiAppearance,
        Source::Gallery => AxisSource::Gallery,
        Source::Override => AxisSource::Override,
    }
}

fn chrome_view(state: ChromeState) -> ChromeView {
    ChromeView {
        workspace_text: state.workspace_text().to_owned(),
        workspace: state.workspace,
        appearance_choice: core_choice(state.choice),
        workspace_menu: state.workspace_menu,
        zones: state.zones,
    }
}

fn text_field_view(state: TextFieldState, editing: bool) -> TextFieldView {
    TextFieldView {
        value: state.value,
        placeholder: state.placeholder,
        editable: state.editable,
        editing,
    }
}

fn menu_items(entries: Vec<MenuEntry>) -> Vec<MenuItemView> {
    entries
        .into_iter()
        .map(|entry| match entry {
            MenuEntry::Item(item) => MenuItemView {
                id: item.id,
                separator: false,
                title: item.title,
                key: item.key.as_ref().map(shellio_appkit::MenuKey::chord),
                enabled: item.enabled,
                checked: item.checked,
                submenu: item.submenu.map(menu_items),
            },
            MenuEntry::Separator { id } => MenuItemView {
                id,
                separator: true,
                title: String::new(),
                key: None,
                enabled: false,
                checked: false,
                submenu: None,
            },
        })
        .collect()
}

fn menu_view(state: MenuState) -> MenuView {
    MenuView {
        open: state.open,
        highlighted: state.highlighted,
        items: menu_items(state.entries),
    }
}

fn component_state(state: NodeState) -> ComponentState {
    match state {
        NodeState::Chrome(state) => ComponentState::Chrome(chrome_view(state)),
        NodeState::TextField { state, editing } => {
            ComponentState::TextField(text_field_view(state, editing))
        }
        NodeState::Menu(state) => ComponentState::Menu(menu_view(state)),
        NodeState::TabStrip(facts) => ComponentState::TabStrip(Box::new(tab_strip_view(*facts))),
        NodeState::VirtualList(facts) => {
            ComponentState::VirtualList(Box::new(virtual_list_view(*facts)))
        }
        NodeState::TreeList(facts) => ComponentState::TreeList(Box::new(tree_list_view(*facts))),
    }
}

fn tree_list_view(facts: TreeListFacts) -> TreeListView {
    let wide = |value: usize| u64::try_from(value).unwrap_or(u64::MAX);
    TreeListView {
        label: facts.label,
        count: wide(facts.count),
        nodes: wide(facts.nodes),
        selected: facts.selected,
        expanded: facts.expanded,
        offset: facts.offset,
        max_offset: facts.max_offset,
        keyboard_focus: facts.keyboard_focus,
        row_views_created: facts.row_views_created,
        cell_views_created: facts.cell_views_created,
        visible: facts
            .visible
            .into_iter()
            .map(|row| TreeRowView {
                index: wide(row.index),
                id: row.id,
                text: row.text,
                level: wide(row.level),
                expandable: row.expandable,
                expanded: row.expanded,
            })
            .collect(),
    }
}

fn virtual_list_view(facts: VirtualListFacts) -> VirtualListView {
    let wide = |value: usize| u64::try_from(value).unwrap_or(u64::MAX);
    VirtualListView {
        label: facts.label,
        count: wide(facts.count),
        selected: facts.selected,
        offset: facts.offset,
        max_offset: facts.max_offset,
        keyboard_focus: facts.keyboard_focus,
        row_views_created: facts.row_views_created,
        cell_views_created: facts.cell_views_created,
        visible: facts
            .visible
            .into_iter()
            .map(|row| ListRowView {
                index: wide(row.index),
                id: row.id,
                text: row.text,
            })
            .collect(),
    }
}

fn tab_strip_view(facts: TabStripFacts) -> TabStripView {
    let state = facts.state;
    let presentation = state.presentation;
    let tabs = state
        .tabs
        .iter()
        .map(|tab| {
            let seen = facts.tabs.iter().find(|seen| seen.id == tab.id);
            TabItemView {
                id: tab.id.clone(),
                title: tab.title.clone(),
                kind: tab.kind.clone(),
                status: match tab.status {
                    TabStatus::None => TabStatusView::None,
                    TabStatus::Running => TabStatusView::Running,
                    TabStatus::Waiting => TabStatusView::Waiting,
                    TabStatus::Error => TabStatusView::Error,
                    TabStatus::Done => TabStatusView::Done,
                    TabStatus::Idle => TabStatusView::Idle,
                    TabStatus::Exited => TabStatusView::Exited,
                },
                icon: seen
                    .is_some_and(|seen| seen.icon_shown)
                    .then(|| tab.icon.clone()),
                close_visible: seen.is_some_and(|seen| seen.close_visible),
                fully_visible: seen.is_some_and(|seen| seen.fully_visible),
            }
        })
        .collect();
    TabStripView {
        label: state.label.clone(),
        zone: match presentation.zone {
            ZoneSide::Side => TabZone::Side,
            ZoneSide::Center => TabZone::Center,
            ZoneSide::Bottom => TabZone::Bottom,
        },
        axis: match presentation.axis() {
            SplitAxis::Columns => TabAxis::Columns,
            SplitAxis::Rows => TabAxis::Rows,
        },
        navigator: presentation.navigator,
        zoomed: presentation.zoomed,
        focused: state.focused,
        keyboard_focus: facts.keyboard_focus,
        regime: facts.regime.map(|regime| match regime {
            Regime::Visible => TabRegime::Visible,
            Regime::InMenu => TabRegime::InMenu,
            Regime::Scroll => TabRegime::Scroll,
        }),
        selected: state.selected.clone(),
        payload: state.payload.clone(),
        payload_visible: facts.payload_visible,
        offset: facts.offset,
        max_offset: facts.max_offset,
        tabs,
        buttons: facts
            .buttons
            .into_iter()
            .map(|button| StripButtonView {
                id: button.button.id().to_owned(),
                label: button.button.label().to_owned(),
                symbol: button.symbol.to_owned(),
                hint: button.hint,
                help: button.help,
            })
            .collect(),
    }
}

/// Имя узла строки статуса окна в `ui.inspect`.
const STATUS_ID: &str = "status";
/// Имя узла слота сообщения строки статуса.
const STATUS_MESSAGE_ID: &str = "status.message";

/// Ключ сверки узла строки статуса окна: строка — корень поверхности, слот сообщения входит в
/// сверку только с показанным текстом.
fn status_keyed(node: InspectNode) -> InspectNode {
    if node.id == STATUS_ID {
        return node.with_root_ck(shellio_appkit::ck::STATUS);
    }
    // Плашка режима и подсказки приходят с ключами; набранный префикс ключа не получает.
    if node.ck.is_some() || node.id != STATUS_MESSAGE_ID {
        return node;
    }
    let shown = node.facts.visible
        && node
            .facts
            .label
            .as_deref()
            .is_some_and(|text| !text.is_empty());
    if shown {
        node.with_ck(shellio_appkit::ck::STATUS_MESSAGE)
    } else {
        node
    }
}

fn wire_rgba(color: shellio_appkit::Rgba) -> shellio_control::Rgba {
    shellio_control::Rgba {
        r: color.r,
        g: color.g,
        b: color.b,
        a: color.a,
    }
}

/// Свойства вида в кадре `ui.inspect`: поля названы как у элемента паспорта.
fn style_view(style: shellio_appkit::Style) -> shellio_control::StyleView {
    shellio_control::StyleView {
        font: style.font.map(|font| shellio_control::FontView {
            family: font.family,
            face: font.face,
            weight: font.weight,
            size: font.size,
            file: font.file,
        }),
        line_height: style.line_height,
        letter_spacing: style.letter_spacing,
        color: style.color.map(wire_rgba),
        fill: style.fill.map(wire_rgba),
        line_color: style.line_color.map(wire_rgba),
        line_width: style.line_width,
        line_sides: style
            .line_sides
            .into_iter()
            .map(|side| match side {
                shellio_appkit::Side::Top => shellio_control::LineSide::Top,
                shellio_appkit::Side::Right => shellio_control::LineSide::Right,
                shellio_appkit::Side::Bottom => shellio_control::LineSide::Bottom,
                shellio_appkit::Side::Left => shellio_control::LineSide::Left,
            })
            .collect(),
        radius: style.radius,
        padding: style.padding.map(|padding| shellio_control::InsetsView {
            top: padding.top,
            right: padding.right,
            bottom: padding.bottom,
            left: padding.left,
        }),
        shadow: style
            .shadow
            .into_iter()
            .map(|shadow| shellio_control::ShadowView {
                color: wire_rgba(shadow.color),
                x: shadow.x,
                y: shadow.y,
                blur: shadow.blur,
                spread: shadow.spread,
                inset: shadow.inset,
            })
            .collect(),
        icon: style.icon.map(|icon| shellio_control::IconView {
            name: icon.name,
            size: icon.size,
        }),
    }
}

pub(crate) fn component(node: InspectNode) -> ComponentView {
    let facts = node.facts;
    ComponentView {
        id: node.id,
        class: node.class.to_owned(),
        object: node.object,
        parent: node.parent,
        ck: node.ck,
        frame: rect(facts.frame),
        visible: facts.visible,
        first_responder: facts.first_responder,
        enabled: facts.enabled,
        role: facts.role,
        label: facts.label,
        help: facts.help,
        selected: facts.selected,
        expanded: facts.expanded,
        animation_keys: facts.animation_keys,
        style: node.style.map(style_view),
        truncated: node.truncated,
        state: node.state.map(component_state),
    }
}

/// Проекции зеркала для окна.
#[derive(Clone)]
struct Projection {
    workspace: Option<String>,
    title: Option<String>,
}

impl Projection {
    fn of(mirror: &Mirror) -> Projection {
        Projection {
            workspace: mirror.workspace_name().map(|name| name.as_str().to_owned()),
            title: mirror.window_title(),
        }
    }
}

/// Работа с окном владельца, если оно есть. Повторный вход не паникует: работа пропускается со
/// строкой в stderr.
fn with_window<R>(work: impl FnOnce(&mut OwnerWindow) -> R) -> Option<R> {
    WINDOW.with(|slot| match slot.try_borrow_mut() {
        Ok(mut window) => window.as_deref_mut().map(work),
        Err(_) => {
            owner::say_err("shellio: окно владельца занято, повторный вход пропущен");
            None
        }
    })
}

/// Отложить работу на следующий оборот очереди главного потока.
fn defer(work: impl FnOnce(&App) + Send + 'static) {
    MainThread::new().exec(work);
}

/// Создаёт окно владельца в режиме `mode`, не показывая его, ставит главное меню и монитор
/// цикла. Повторный вызов ничего не создаёт. Возвращает `windowNumber`.
pub fn create(app: &App, mode: WindowMode) -> i64 {
    if let Some(number) = with_window(|owner| owner.window.number()) {
        return number;
    }
    let window = new_window(app);
    // Без верного списка плагинов владелец не запустится и скажет причину сам: меню до его отказа
    // хватает пустого списка.
    let rows = plugins::shared_registry()
        .map(|registry| scene::open_rows(&registry))
        .unwrap_or_default();
    let menu = MainMenu::install(
        app,
        rows.iter()
            .map(|row| PanelRow {
                id: row.plugin.to_owned(),
                title: row.title.to_owned(),
            })
            .collect(),
        |action| {
            defer(move |app| match command_of(action) {
                Some(command) => handle(app, command, Some(ReceiptSource::Menu)),
                None => match action {
                    MenuAction::OpenPanel(row) => on_open_panel(app, row),
                    MenuAction::Quit
                    | MenuAction::CloseWindow
                    | MenuAction::Appearance(_)
                    | MenuAction::Zone(_) => {}
                },
            });
        },
    );
    // Снимок keymap мог прийти раньше окна: эквиваленты пунктов берутся из него сразу.
    let open_rows: Vec<&'static str> = rows.iter().map(|row| row.plugin).collect();
    menu.set_keys(menu_keys(&open_rows));
    let monitor = match LoopMonitor::install(app) {
        Ok(monitor) => Some(monitor),
        Err(error) => {
            owner::say_err(format_args!("shellio: монитор цикла не поставлен: {error}"));
            None
        }
    };
    let number = window.number();
    let gallery = match mode {
        WindowMode::Gallery => Some(GalleryUi::new(
            app,
            &window,
            0,
            |intent| {
                defer(move |_| {
                    with_window(|owner| owner.gallery_intent(&intent));
                });
            },
            InputHooks {
                generation: Box::new(sink_generation),
                event: std::rc::Rc::new(on_sink_event),
                retired: Box::new(on_sink_retired),
            },
        )),
        WindowMode::Normal => None,
    };
    let layout = match mode {
        WindowMode::Normal => Some(LayoutUi::new(
            app,
            &window,
            |split, drag| defer(move |app| on_drag(app, split, drag)),
            |action| defer(move |app| on_panel(app, action)),
            send_request,
            |panel, intent| defer(move |_| on_filter_row(panel, intent)),
        )),
        WindowMode::Gallery => None,
    };
    let status = match mode {
        WindowMode::Normal => {
            let status = StatusLine::with_mode(app, &window.selection().theme());
            // Плашка режима — кнопка `mode.toggle`: тот же обработчик, что у клавиши и CLI.
            status.on_mode(|| {
                defer(|app| {
                    handle(
                        app,
                        UiCommand::Mode(ModeAction::Toggle),
                        Some(ReceiptSource::Button),
                    );
                });
            });
            Some(status)
        }
        WindowMode::Gallery => None,
    };
    let palette = match mode {
        WindowMode::Normal => Some(CommandPalette::new(
            app,
            &window.selection().theme(),
            |intent| defer(move |app| on_palette_intent(app, intent)),
        )),
        WindowMode::Gallery => None,
    };
    let which = match mode {
        WindowMode::Normal => Some(ShellioWhichKey::new(app, &window.selection().theme())),
        WindowMode::Gallery => None,
    };
    let hint_layer = match mode {
        WindowMode::Normal => Some(HintOverlay::new(app, &window.selection().theme())),
        WindowMode::Gallery => None,
    };
    let projection = MIRROR.with(|mirror| mirror.borrow().as_ref().map(Projection::of));
    let applied = window.selection();
    WINDOW.with(|slot| {
        *slot.borrow_mut() = Some(ManuallyDrop::new(OwnerWindow {
            mode,
            menu,
            monitor,
            choice: AppearanceChoice::System,
            cell_contrast: None,
            cell_motion: None,
            generation: Generation::default(),
            first_snapshot: None,
            startup: None,
            projection: None,
            applied,
            window,
            gallery,
            layout,
            status,
            palette,
            which,
            hint_layer,
            open_rows,
            viewport: ViewportGate::default(),
            pending: Vec::new(),
            animations: VecDeque::new(),
        }));
    });
    if let Some(projection) = projection {
        with_window(|owner| owner.project(projection));
    }
    with_window(|owner| owner.render_layout(app));
    install_keys(app);
    number
}

/// Ставит монитор клавиш и наблюдателя ввода в полях. Оба вызываются изнутри AppKit: наблюдатель
/// только ставит согласование фокуса и режимов в очередь главного потока.
fn install_keys(app: &App) {
    app.on_field_focus(|focus| {
        defer(move |app| {
            if focus == FieldFocus::Began {
                focus_typing(app);
            }
            follow_field();
        });
    });
    match KeyMonitor::install(app, on_key_event) {
        Some(monitor) => {
            KEY_MONITOR.with(|slot| *slot.borrow_mut() = Some(ManuallyDrop::new(monitor)));
        }
        None => owner::say_err("shellio: монитор клавиш не поставлен"),
    }
}

/// Маршрутизатор без повторного входа: занятый пропускает работу, событие идёт AppKit.
fn with_router<T>(work: impl FnOnce(&mut KeyRouter<ModeReceiver, Pressed>) -> T) -> Option<T> {
    ROUTER.with(|router| {
        router
            .try_borrow_mut()
            .ok()
            .map(|mut router| work(&mut router))
    })
}

/// Маршрутизатор после отрисовки раскладки: панель с фокусом для решения о клавише, закрытые
/// панели, смена панели с фокусом и режим вслед за полем.
fn sync_keys(
    focus: Option<PanelFocus<ModeReceiver>>,
    typing: Option<ModeReceiver>,
    alive: impl Fn(PanelId) -> bool,
) {
    let moved = KEY_PANEL.with(|slot| {
        let mut slot = slot.borrow_mut();
        let moved =
            slot.as_ref().map(|focus| focus.receiver) != focus.as_ref().map(|focus| focus.receiver);
        *slot = focus;
        moved
    });
    with_router(|router| {
        router.retain(|receiver| match receiver {
            ModeReceiver::Panel(panel) => alive(*panel),
            ModeReceiver::GalleryInput(generation) => *generation == sink_generation(),
        });
        if moved {
            router.cancel(Cancel::Focus);
        }
        router.follow_field(typing.as_ref());
    });
}

/// Режим вслед за полем (ADR-0010): панель, чьё поле держит ввод, в `Input`, остальные в
/// `Navigation`. Занятое окно и галерея режимов не трогают.
fn follow_field() {
    let typing = with_window(|owner| owner.layout.as_ref().map(LayoutUi::typing));
    if let Some(Some(typing)) = typing {
        let receiver = typing.map(ModeReceiver::Panel);
        with_router(|router| router.follow_field(receiver.as_ref()));
    }
    show_modes();
}

/// Ввод встал в поле панели без фокуса — обходом `Tab`, доступностью или щелчком в строку фильтра:
/// фокус раскладки переходит к этой панели той же командой, что щелчок в её тело.
fn focus_typing(app: &App) {
    let command = with_window(|owner| {
        let layout = owner.layout.as_ref()?;
        scene::focus_command(layout.scene()?, layout.typing()?)
    })
    .flatten();
    if let Some(command) = command {
        send_ui(app, command, None);
    }
}

/// Поле с вводом обычного окна для решения о клавише: панель-владелец поля и признак строки
/// фильтра. Владелец берётся из окна, а не из записи отрисовки: фокус раскладки следует за полем
/// командой ядра и может ещё не прийти.
fn field_owner() -> Option<(PanelFocus<ModeReceiver>, bool)> {
    with_window(|owner| {
        if owner.gallery.is_some() {
            return None;
        }
        let layout = owner.layout.as_ref()?;
        let focus = layout.key_focus()?;
        let in_filter = match focus.receiver {
            ModeReceiver::Panel(panel) => layout.filter_typing() == Some(panel),
            ModeReceiver::GalleryInput(_) => false,
        };
        Some((focus, in_filter))
    })
    .flatten()
}

/// Панели в режиме `Input` по маршрутизатору. `None` — маршрутизатор занят.
fn input_panels() -> Option<BTreeSet<PanelId>> {
    with_router(|router| {
        router
            .inputs()
            .filter_map(|receiver| match receiver {
                ModeReceiver::Panel(panel) => Some(*panel),
                ModeReceiver::GalleryInput(_) => None,
            })
            .collect()
    })
}

/// Тон плитки с фокусом и линия под активной вкладкой по режиму панели (ADR-0024). Вызывается
/// после каждой смены режима: клавишей режим меняет само решение маршрутизатора, и согласование
/// с полем после него расхождения уже не видит.
fn show_modes() {
    let Some(panels) = input_panels() else {
        return;
    };
    with_window(|owner| {
        let changed = owner
            .layout
            .as_mut()
            .is_some_and(|layout| layout.set_input_panels(panels));
        let hints = owner.show_hints();
        if changed || hints {
            owner.bump();
        }
    });
}

/// Приложение перестало быть активным: ожидание второго аккорда выхода и набранный префикс сняты.
pub fn resigned() {
    with_router(|router| router.cancel(Cancel::Resigned));
    // Префикс снят: строка статуса и which-key следуют за ним.
    defer(|_| show_modes());
}

/// Событие монитора клавиш. Вызывается изнутри AppKit до доставки события: решение принимает
/// маршрутизатор по своей ячейке, действие исполняется следующим оборотом очереди главного потока.
fn on_key_event(app: &App, event: MonitorEvent) -> Verdict {
    let decision = match event {
        MonitorEvent::KeyDown(stroke) => return on_key_down(app, stroke),
        MonitorEvent::KeyUp { key_code } => with_router(|router| router.key_up(KeyCode(key_code))),
        MonitorEvent::FlagsChanged => with_router(|router| router.flags_changed()),
        MonitorEvent::MouseDown { window } => {
            // Щелчок вне окна открытой палитры закрывает её и до слоя под ней не доходит.
            if palette_outside(window) {
                defer(|_| close_overlay(OverlayKind::Palette));
                return Verdict::Swallow;
            }
            // Щелчок при открытых метках закрывает их (правило «Отмена») и до слоя под ними не
            // доходит: метки — не поверхность, которую можно щёлкнуть.
            if hints_open() {
                defer(|_| close_overlay(OverlayKind::Hints));
                return Verdict::Swallow;
            }
            with_router(|router| router.cancel(Cancel::MouseDown));
            let seen = status_serial();
            defer(move |_| {
                forget_status(seen);
                show_modes();
            });
            None
        }
        MonitorEvent::ScrollWheel => {
            let had_prefix = with_router(|router| {
                let had = !router.pending().is_empty();
                router.cancel(Cancel::ScrollWheel);
                had
            });
            // Прокрутка идёт десятками событий в секунду: строку статуса и which-key трогает только
            // снятый ею префикс.
            if had_prefix == Some(true) {
                defer(|_| show_modes());
            }
            None
        }
    };
    match decision {
        Some(Decision::Swallow | Decision::Overlay { .. } | Decision::Close(_)) => Verdict::Swallow,
        Some(Decision::Pass | Decision::Run(_) | Decision::Replay { .. }) | None => Verdict::Pass,
    }
}

/// Метки открыты.
fn hints_open() -> bool {
    with_router(|router| router.modal().map(|modal| modal.kind))
        .flatten()
        .is_some_and(|kind| kind == OverlayKind::Hints)
}

/// Палитра открыта, а окно `window` — не её окно.
fn palette_outside(window: i64) -> bool {
    let open = OVERLAY.with(|state| state.borrow().kind == Some(OverlayKind::Palette));
    open && with_window(|owner| {
        owner
            .palette
            .as_ref()
            .is_some_and(|palette| palette.is_open() && palette.number() != window)
    }) == Some(true)
}

/// Строка статуса показала сообщение.
fn mark_status() {
    STATUS_SERIAL.with(|serial| serial.set(serial.get() + 1));
}

fn status_serial() -> u64 {
    STATUS_SERIAL.with(Cell::get)
}

/// Событие пользователя снимает только то сообщение, которое было на экране в его момент:
/// сообщение, показанное самим событием до отложенного снятия, остаётся.
fn outlived(seen: u64, now: u64) -> bool {
    seen == now
}

/// Следующее действие пользователя в окне — любая клавиша или нажатие указателя — убирает
/// сообщение строки статуса, которое было показано до него (`seen` — номер сообщения в момент
/// события). Команды CLI его не трогают.
fn forget_status(seen: u64) {
    if !outlived(seen, status_serial()) {
        return;
    }
    with_window(|owner| {
        if owner
            .status
            .as_ref()
            .is_some_and(|status| !status.message().is_empty())
        {
            owner.show_status(None);
            owner.bump();
        }
    });
}

/// Кто держит ввод сейчас для решения маршрутизатора и признак строки фильтра.
fn current_focus(app: &App) -> (Focus<ModeReceiver>, bool) {
    let facts = app.key_focus();
    // Поле ключевого окна палитры — поле наложения, а не поле панели.
    let overlay_field = facts.responder == KeyResponder::Field
        && with_window(|owner| owner.palette.as_ref().is_some_and(CommandPalette::is_key))
            == Some(true);
    let typing = match facts.responder {
        KeyResponder::Field if overlay_field => None,
        KeyResponder::Field => field_owner(),
        KeyResponder::None
        | KeyResponder::Other
        | KeyResponder::TabStrip
        | KeyResponder::Button => None,
    };
    let (panel, in_filter) = match typing {
        Some((panel, in_filter)) => (Some(panel), in_filter),
        None => (KEY_PANEL.with(|slot| slot.borrow().clone()), false),
    };
    let focus = Focus {
        responder: match facts.responder {
            KeyResponder::None => Responder::None,
            KeyResponder::Other => Responder::Body,
            KeyResponder::Field if overlay_field => Responder::OverlayField,
            KeyResponder::Field => Responder::Field,
            KeyResponder::TabStrip => Responder::TabStrip,
            KeyResponder::Button => Responder::Button,
        },
        composing: facts.composing,
        panel,
    };
    (focus, in_filter)
}

fn on_key_down(app: &App, stroke: KeyStroke) -> Verdict {
    let seen = status_serial();
    let Some(snapshot) = KEYMAP.with(|slot| slot.borrow().clone()) else {
        defer(move |_| forget_status(seen));
        return Verdict::Pass;
    };
    let (focus, in_filter) = current_focus(app);
    let mods = stroke.mods();
    let chord = Chord::new(
        KeyCode(stroke.key_code()),
        Mods {
            cmd: mods.cmd,
            ctrl: mods.ctrl,
            alt: mods.alt,
            shift: mods.shift,
        },
    );
    let repeat = stroke.repeat();
    let pressed = Pressed { stroke, in_filter };
    let decision =
        with_router(|router| router.key_down(&snapshot.keymap, &focus, chord, repeat, pressed));
    match decision {
        // Пропущенная клавиша идёт получателю штатно, сообщение она снимает, как любая другая.
        Some(Decision::Pass) | None => {
            defer(move |_| forget_status(seen));
            Verdict::Pass
        }
        // Поглощённая клавиша меняет набранный префикс: строка статуса показывает его.
        Some(Decision::Swallow) => {
            defer(move |_| {
                forget_status(seen);
                show_modes();
            });
            Verdict::Swallow
        }
        Some(Decision::Run(action)) => {
            defer(move |app| {
                forget_status(seen);
                run_key(app, action);
                show_modes();
            });
            Verdict::Swallow
        }
        Some(Decision::Overlay { kind, chord }) => {
            defer(move |app| on_overlay_key(app, kind, chord));
            Verdict::Swallow
        }
        Some(Decision::Close(modal)) => {
            defer(move |_| {
                forget_status(seen);
                restore_source(modal);
            });
            Verdict::Swallow
        }
        // Получатель снова держит ввод там, где держал его до первого аккорда, пара идёт ему
        // напрямую, мимо очереди событий.
        Some(Decision::Replay { receiver, event }) => {
            match receiver {
                ModeReceiver::Panel(panel) => {
                    with_window(|owner| {
                        if let Some(layout) = owner.layout.as_ref() {
                            layout.resume_input(panel, event.in_filter);
                        }
                    });
                    if app.key_focus().responder == KeyResponder::Field {
                        event.stroke.replay(app);
                    }
                }
                // Пара идёт виду приёмника, пока он первый ответчик: запись панели с фокусом
                // держит получателя только на это время.
                ModeReceiver::GalleryInput(generation) => {
                    let focused = KEY_PANEL.with(|slot| {
                        slot.borrow()
                            .as_ref()
                            .is_some_and(|focus| focus.receiver == receiver)
                    });
                    if focused && generation == sink_generation() {
                        event.stroke.replay(app);
                    }
                }
            }
            defer(move |_| {
                forget_status(seen);
                follow_field();
            });
            Verdict::Swallow
        }
    }
}

/// Действие привязки, исполненное клавишей: тот же обработчик, что у меню, кнопки и CLI, и ровно
/// одна запись кольца квитанций с источником `key`.
fn run_key(app: &App, action: Action) {
    run_action(app, action, Some(ReceiptSource::Key));
}

/// Действие keymap по имени: клавиша, палитра и строка `:`. `source` — источник записи кольца
/// квитанций; без него, как у CLI, записи нет.
fn run_action(app: &App, action: Action, source: Option<ReceiptSource>) {
    run_action_with(
        app,
        action,
        source,
        source == Some(ReceiptSource::Key),
        None,
    );
}

/// [`run_action`] с явным признаком решения маршрутизатора: `routed` — режим `mode.*` уже сменило
/// решение о клавише. Строка палитры выполняется клавишей `Enter`, но режим меняет обработчик.
/// `panel` — панель-источник наложения: действие плагина идёт ей, а не панели с фокусом, и её
/// исчезновение — сообщение в строке статуса без выполнения.
fn run_action_with(
    app: &App,
    action: Action,
    source: Option<ReceiptSource>,
    routed: bool,
    panel: Option<PanelId>,
) {
    let name = action.name.as_str();
    let not_applicable = || {
        note_receipt(
            source.map(|source| Sent::action(name, source)),
            Ending::NotApplicable,
        )
    };
    let refused = |text: &str| {
        with_window(|owner| owner.show_status(Some(text)));
        not_applicable();
    };
    let Ok(registry) = plugins::shared_registry() else {
        return not_applicable();
    };
    let path = match key_actions::recognize(&registry, name, action.args.as_ref()) {
        Some(Ok(path)) => path,
        Some(Err(reason)) => return refused(&format!("«{name}» не выполнено: {reason}")),
        None => return refused(&format!("«{name}» не выполнено: такого действия нет")),
    };
    match path {
        KeyAction::Ui(UiCommand::Mode(mode)) if routed => {
            on_mode_key(mode);
        }
        KeyAction::Ui(command) => handle(app, command, source),
        KeyAction::Filter(text) => on_filter(app, Some(&text), source),
        KeyAction::Layout(layout) => {
            let command = with_window(|owner| {
                if !owner.window.is_visible() {
                    return None;
                }
                let scene = owner.layout.as_ref().and_then(LayoutUi::scene)?;
                key_actions::command(scene, layout)
            })
            .flatten();
            match command {
                Some(command) => {
                    let sent = source.map(|source| Sent::command(name, &command, source));
                    send_ui(app, command, sent);
                }
                None => not_applicable(),
            }
        }
        KeyAction::Open(plugin) => {
            let visible = with_window(|owner| owner.window.is_visible()) == Some(true);
            let command = MIRROR.with(|mirror| {
                let mirror = mirror.borrow();
                scene::open_panel_command(mirror.as_ref()?.state(), &plugin)
            });
            match command.filter(|_| visible) {
                Some(command) => {
                    let sent = source.map(|source| Sent::command(name, &command, source));
                    send_ui(app, command, sent);
                }
                None => not_applicable(),
            }
        }
        KeyAction::Plugin {
            name: plugin_action,
            args,
        } => {
            let request = with_window(|owner| {
                if !owner.window.is_visible() {
                    return None;
                }
                let layout = owner.layout.as_ref()?;
                Some(match panel {
                    Some(panel) => layout
                        .key_request_for(panel, &plugin_action, args)
                        .map(|request| (panel, request)),
                    None => layout.key_request(&plugin_action, args),
                })
            })
            .flatten();
            match request {
                Some(Ok((panel, request))) => {
                    let command = MIRROR.with(|mirror| {
                        let mirror = mirror.borrow();
                        let state = mirror.as_ref().map(Mirror::state);
                        bodies::request_command(state, panel, request)
                    });
                    send_ui(
                        app,
                        command,
                        source.map(|source| Sent::action(name, source)),
                    );
                }
                Some(Err(refusal)) => refused(&refusal.text(name)),
                None => not_applicable(),
            }
        }
        KeyAction::Reload => {
            let tag = source.map(|source| Sent::action(RELOAD, source));
            if !reload_keymap(move || note_receipt(tag, Ending::Done)) {
                not_applicable();
            }
        }
    }
}

/// Имя действия перечитывания `keymap.toml` в keymap и в кольце квитанций.
const RELOAD: &str = "keymap.reload";

/// Перечитать `keymap.toml` через службу, не дожидаясь её на главном потоке. `done` вызывается в
/// очереди главного потока после того, как действует таблица не старше результата чтения: её
/// главный поток берёт из порта сам, поэтому доставка службы, пришедшая раньше, не откатывается.
/// `false` — порта службы нет, файл не перечитывался.
fn reload_keymap(done: impl FnOnce() + Send + 'static) -> bool {
    let Some((port, runtime)) = KEYMAP_PORT.with(|slot| slot.borrow().clone()) else {
        return false;
    };
    let table = port.clone();
    port.apply_then(&runtime, move |_| {
        defer(move |_| {
            set_keymap(table.current());
            done();
        });
    });
    true
}

/// Новое окно владельца: смена ячейки и намерения заголовка уходят в очередь главного потока.
fn new_window(app: &App) -> MainWindow {
    MainWindow::new(
        app,
        |_: ThemeSelection| defer(|_| on_theme()),
        |intent| defer(move |app| on_intent(app, intent)),
        || defer(on_resize),
        || defer(refresh_layout),
    )
}

/// Показывает окно владельца (`makeKeyAndOrderFront`). `false` — окна нет: процесс не владелец.
/// Тела панелей показанного окна снова активны.
pub fn show(app: &App) -> bool {
    with_window(|owner| {
        owner.window.show();
        owner.render_layout(app);
    })
    .is_some()
}

/// Приложение открыли снова: окно показывается, владелец просит передний план. Без окна ничего не
/// делается.
pub fn reopen(app: &App) {
    if show(app) {
        app.activate();
    }
}

/// Применить доставку к зеркалу. Применённая доставка обновляет проекции окна и раскладку.
pub fn deliver(app: &App, delivery: Delivery) -> MirrorStep {
    let (step, projection) = MIRROR.with(|slot| {
        let mut mirror = slot.borrow_mut();
        let step = Mirror::deliver_to(&mut mirror, delivery);
        let projection = match step {
            MirrorStep::Applied => mirror.as_ref().map(Projection::of),
            MirrorStep::Ignored | MirrorStep::NeedSnapshot => None,
        };
        (step, projection)
    });
    if let Some(projection) = projection {
        // До отрисовки: она согласует режимы с полем и сбросила бы режим оставленной панели.
        observe_history();
        with_window(|owner| {
            owner.project(projection);
            owner.render_layout(app);
        });
    }
    step
}

/// Ядро загружено, сокет ещё не опубликован: главный поток получает ручку ядра, обычное окно
/// передаёт первый размер содержимого. Галерея раскладку не показывает и размера не передаёт:
/// деление и размер отвечают `no_ui`.
pub fn attach_core(_app: &App, core: CoreHandle, runtime: tokio::runtime::Handle) {
    let first = with_window(|owner| {
        let current = owner.content_viewport();
        owner
            .layout
            .is_some()
            .then(|| owner.viewport.flush(current))
            .flatten()
    })
    .flatten();
    if let Some(viewport) = first {
        core.set_viewport(Some(viewport));
    }
    CORE.with(|slot| {
        *slot.borrow_mut() = Some(CoreLink {
            core,
            runtime,
            receipts: RefCell::new(Ring::default()),
        });
    });
}

/// Порт службы keymap: ставится до первого снимка таблицы.
pub fn attach_keymap(port: KeymapPort, runtime: tokio::runtime::Handle) {
    KEYMAP_PORT.with(|slot| *slot.borrow_mut() = Some((port, runtime)));
}

/// Новая таблица keymap от службы: первый снимок приходит до публикации сокета.
pub fn set_keymap(snapshot: Arc<KeymapSnapshot>) {
    KEYMAP.with(|slot| *slot.borrow_mut() = Some(snapshot));
    show_modes();
    with_window(|owner| {
        // Меню окну не принадлежит: его клавиши поколения окна не меняют.
        owner.menu.set_keys(menu_keys(&owner.open_rows));
        let keys = strip_keys();
        let buttons = owner
            .layout
            .as_mut()
            .is_some_and(|layout| layout.set_button_keys(keys));
        if owner.show_keymap_note(true) || buttons {
            owner.bump();
        }
    });
}

/// Эквиваленты пунктов главного меню из действующей таблицы: постоянные пункты и пункты «Открыть
/// панель» плагинов `open_rows`. Без снимка клавиш у пунктов нет.
fn menu_keys(open_rows: &[&str]) -> MenuKeys {
    KEYMAP.with(|slot| {
        let Some(snapshot) = slot.borrow().clone() else {
            return MenuKeys::new();
        };
        let actions = key_actions::menu_actions(&MENU_ACTIONS, open_rows);
        let names: Vec<&str> = actions.iter().map(String::as_str).collect();
        key_actions::menu_chords(&snapshot.keymap, &names)
            .into_iter()
            .map(|(action, chord)| {
                let key = MenuKey {
                    key: chord.key,
                    command: chord.mods.cmd,
                    shift: chord.mods.shift,
                    option: chord.mods.alt,
                    control: chord.mods.ctrl,
                };
                (action, key)
            })
            .collect()
    })
}

/// Клавиши кнопок строки вкладок из действующей таблицы: деление по обеим осям и разворот.
fn strip_keys() -> StripKeys {
    KEYMAP.with(|slot| {
        let Some(snapshot) = slot.borrow().clone() else {
            return StripKeys::default();
        };
        let keys = key_actions::button_keys(&snapshot.keymap);
        StripKeys {
            row: keys.row,
            stack: keys.stack,
            zoom: keys.zoom,
        }
    })
}

/// Размер содержимого окна изменился: раскладка встаёт по нему сразу, ядро получает его в конце
/// оборота цикла.
fn on_resize(app: &App) {
    let schedule = with_window(|owner| {
        owner.render_layout(app);
        if let Some(palette) = owner.palette.as_ref() {
            palette.follow(&owner.window);
        }
        owner.layout.is_some() && owner.viewport.request()
    });
    if schedule == Some(true) {
        defer(|_| flush_viewport());
    }
}

fn flush_viewport() {
    // Без ядра размер не отмечается отправленным: отправка остаётся запланированной, текущий размер
    // передаст `attach_core`.
    if CORE.with(|slot| slot.borrow().is_none()) {
        return;
    }
    let next = with_window(|owner| {
        let current = owner.content_viewport();
        owner.viewport.flush(current)
    })
    .flatten();
    if let Some(viewport) = next {
        CORE.with(|slot| {
            if let Some(link) = slot.borrow().as_ref() {
                link.core.set_viewport(Some(viewport));
            }
        });
    }
}

/// Раскладка модели заново: после отказа команды из интерфейса и после того, как окно спрятала
/// его кнопка закрытия.
fn refresh_layout(app: &App) {
    with_window(|owner| owner.render_layout(app));
}

/// Команда раскладки из интерфейса: сообщение строки статуса убирается, отказ ядра показывается в
/// ней. Без ядра раскладка возвращается к модели. `tag` — запись кольца квитанций о действии.
fn send_ui(app: &App, command: Command, tag: Option<Sent>) {
    with_window(|owner| owner.show_status(None));
    let sent = CORE.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|link| link.send(command, tag))
            .is_some()
    });
    if !sent {
        refresh_layout(app);
    }
}

/// Имя записи пункта меню панели до его команды ядра.
const MENU_ITEM: &str = "";

/// Имя и источник записи кольца квитанций о действии в раскладке. `None` — действие указателя,
/// а не кнопки или пункта меню: щелчок в тело панели и перетаскивание.
fn panel_action_tag(action: &PanelAction) -> Option<(&'static str, ReceiptSource)> {
    match action {
        PanelAction::Select(_) => Some(("panel.select", ReceiptSource::Button)),
        PanelAction::Close(_) => Some(("panel.close", ReceiptSource::Button)),
        PanelAction::Split(_) => Some(("layout.split", ReceiptSource::Button)),
        PanelAction::Zoom(_) | PanelAction::Expand(_) => {
            Some(("layout.zoom", ReceiptSource::Button))
        }
        // Пункт меню «…» и выбора «Открыть в новой части» называется именем своей команды ядра.
        PanelAction::Menu(..) | PanelAction::Open { .. } => Some((MENU_ITEM, ReceiptSource::Menu)),
        PanelAction::Focus(_)
        | PanelAction::StubPress(_)
        | PanelAction::Drag { .. }
        | PanelAction::Drop { .. } => None,
    }
}

/// Действие в раскладке: щелчок по вкладке или в тело панели (`panel.select`, `panel.focus`, если
/// он что-то меняет), кнопка закрытия вкладки, кнопки «Разделить» и «Развернуть панель» /
/// «Вернуть раскладку», пункты меню «…», пункт выбора «Открыть в новой части», кнопка и тело
/// заглушки «Развернуть», отпускание перетаскиваемой вкладки. Каждое — одна команда ядра; движение
/// перетаскивания только подсвечивает цель.
fn on_panel(app: &App, action: PanelAction) {
    let tag = panel_action_tag(&action);
    let command = with_window(|owner| {
        owner.show_status(None);
        let layout = owner.layout.as_ref()?;
        let scene = layout.scene()?;
        match action {
            PanelAction::Select(panel) => scene::select_command(scene, panel),
            PanelAction::Close(panel) => scene::close_command(scene, panel),
            PanelAction::Focus(panel) => scene::focus_command(scene, panel),
            PanelAction::Split(plate) => scene::split_command(scene, plate),
            PanelAction::Zoom(plate) => scene::zoom_command(scene, plate),
            PanelAction::Menu(plate, item) => scene::menu_command(scene, plate, item),
            PanelAction::Open { panel, plugin } => Some(scene::open_command(panel, plugin)),
            PanelAction::Expand(host) => scene::expand_command(scene, host),
            PanelAction::StubPress(host) => scene::stub_focus_command(scene, host),
            PanelAction::Drag { panel, point } => {
                layout.drag_over(panel, point);
                None
            }
            PanelAction::Drop { panel, point } => layout.drop_at(panel, point),
        }
    })
    .flatten();
    match (command, tag) {
        (Some(command), Some((action, source))) => {
            let action = if action == MENU_ITEM {
                command.name()
            } else {
                action
            };
            let sent = Sent::command(action, &command, source);
            send_ui(app, command, Some(sent));
        }
        (Some(command), None) => send_ui(app, command, None),
        // Пункт меню без команды — разделитель или недоступный пункт: действия не было.
        (None, Some((action, source))) if action != MENU_ITEM => {
            record_receipt(Sent::action(action, source), Ending::NotApplicable);
        }
        (None, _) => {}
    }
}

/// Цель действия `layout.toggle.*` кнопки зон заголовка и пункта меню «Вид».
const fn zone_target(button: ZoneButton) -> ToggleTarget {
    match button {
        ZoneButton::Navigator => ToggleTarget::Navigator,
        ZoneButton::Bottom => ToggleTarget::Bottom,
        ZoneButton::Side => ToggleTarget::Side,
    }
}

/// Действие `layout.toggle.*`: одна команда `layout.visibility`.
fn on_toggle(app: &App, target: ToggleTarget, source: Option<ReceiptSource>) {
    let tag =
        source.map(|source| Sent::action(UiCommand::LayoutToggle(target).keymap_name(), source));
    // Клавиша пункта меню «Вид» срабатывает и без окна: скрытое окно её не получает.
    let command = with_window(|owner| {
        if !owner.window.is_visible() {
            return None;
        }
        let scene = owner.layout.as_ref().and_then(LayoutUi::scene)?;
        scene::toggle_target_command(scene, target)
    })
    .flatten();
    match command {
        Some(command) => send_ui(app, command, tag),
        None => note_receipt(tag, Ending::NotApplicable),
    }
}

/// Действие `list.*`: намерение виду панели с фокусом. Скрытое окно его не получает; панель без
/// признака `list_keys` и вид, который намерение не принял, — действовать не на что.
fn on_list(action: ListAction, source: Option<ReceiptSource>) {
    let tag = source.map(|source| Sent::action(action.keymap_name(), source));
    let handled = with_window(|owner| {
        if !owner.window.is_visible() {
            return None;
        }
        let intent = ViewIntent::List(bodies::list_move(action));
        owner.layout.as_mut()?.intent(&intent)
    })
    .flatten();
    let ending = match handled {
        Some(Handled::Yes) => Ending::Done,
        Some(Handled::No) | None => Ending::NotApplicable,
    };
    note_receipt(tag, ending);
}

/// Действие `panel.filter` для панели с фокусом: открывает строку фильтра хоста над её телом и
/// ставит в неё ввод. С текстом — ещё и намерение её виду: текст фильтра, пустой снимает фильтр.
/// Скрытое окно, панель без признака `filter` и вид, который намерение не принял, — действовать
/// не на что.
fn on_filter(app: &App, text: Option<&str>, source: Option<ReceiptSource>) {
    let tag = source.map(|source| Sent::action(UiCommand::PanelFilter.keymap_name(), source));
    let handled = with_window(|owner| {
        if !owner.window.is_visible() {
            return None;
        }
        owner.layout.as_mut()?.open_filter(app, text)
    })
    .flatten();
    let ending = match handled {
        Some(Handled::Yes) => Ending::Done,
        Some(Handled::No) | None => Ending::NotApplicable,
    };
    note_receipt(tag, ending);
}

/// Получатель с фокусом видимого окна для действий `mode.*` и признак того, что он принимает
/// ввод: панель с фокусом и её плагин, в галерее — приёмник ввода, пока он первый ответчик.
/// Внешнее `None` — окна нет.
fn mode_target() -> Option<Option<(ModeReceiver, bool)>> {
    with_window(|owner| {
        if !owner.window.is_visible() {
            return None;
        }
        if owner.gallery.is_some() {
            return KEY_PANEL.with(|slot| {
                slot.borrow()
                    .as_ref()
                    .map(|focus| (focus.receiver, focus.input))
            });
        }
        let (panel, input) = owner.layout.as_ref()?.mode_target()?;
        Some((ModeReceiver::Panel(panel), input))
    })
}

/// Действие `mode.*` меню, кнопки и CLI для панели с фокусом: целевой режим считается здесь, а
/// ставит его [`apply_mode`]. Скрытое окно, отсутствие панели и `mode.input` или `mode.toggle` у
/// панели без получателя ввода — действовать не на что.
fn on_mode(action: ModeAction, source: Option<ReceiptSource>) {
    let tag = source.map(|source| Sent::action(action.keymap_name(), source));
    let applied = mode_target().flatten().and_then(|(receiver, input)| {
        let mode =
            with_router(|router| router.target_mode(&receiver, input, action.keymap_name()))??;
        set_receiver_mode(receiver, mode);
        Some(())
    });
    let ending = match applied {
        Some(()) => Ending::Done,
        None => Ending::NotApplicable,
    };
    note_receipt(tag, ending);
}

/// Действие `mode.*` с клавиши. Режим уже сменило решение маршрутизатора: здесь он только
/// применяется к первому ответчику, а запись кольца пишется с источником `key`.
fn on_mode_key(action: ModeAction) {
    let applied = mode_target().flatten().and_then(|(receiver, input)| {
        let mode = match action {
            ModeAction::Navigation => Mode::Navigation,
            ModeAction::Input => input.then_some(Mode::Input)?,
            ModeAction::Toggle => {
                input.then_some(())?;
                with_router(|router| router.mode(&receiver))?
            }
        };
        set_receiver_mode(receiver, mode);
        Some(())
    });
    let ending = match applied {
        Some(()) => Ending::Done,
        None => Ending::NotApplicable,
    };
    record_receipt(
        Sent::action(action.keymap_name(), ReceiptSource::Key),
        ending,
    );
}

/// Поставить режим получателя: запись маршрутизатора и первый ответчик. У панели `Input` ставит
/// ввод в её поле, `Navigation` отдаёт ввод окну; режим, за которым поле не последовало,
/// возвращается к факту окна. Приёмник галереи держит ввод сам: первый ответчик не меняется.
fn set_receiver_mode(receiver: ModeReceiver, mode: Mode) {
    with_router(|router| router.set_mode(receiver, mode));
    match receiver {
        ModeReceiver::Panel(panel) => {
            with_window(|owner| {
                if let Some(layout) = owner.layout.as_ref() {
                    layout.set_input(panel, mode == Mode::Input);
                }
            });
            follow_field();
        }
        ModeReceiver::GalleryInput(_) => show_modes(),
    }
}

/// Чтение `ui.mode`. `None` — окна нет.
fn mode_view() -> Option<ModeView> {
    let target = mode_target()?;
    with_router(|router| {
        let receiver = target.map(|(receiver, _)| receiver);
        let mode = receiver.map_or(Mode::Navigation, |receiver| router.mode(&receiver));
        let exit_receiver = router.exit_armed().copied();
        ModeView {
            pid: std::process::id(),
            panel: receiver.and_then(|receiver| match receiver {
                ModeReceiver::Panel(panel) => Some(panel),
                ModeReceiver::GalleryInput(_) => None,
            }),
            mode: match mode {
                Mode::Navigation => PanelMode::Navigation,
                Mode::Input => PanelMode::Input,
            },
            receiver: receiver.filter(|_| target.is_some_and(|(_, input)| input)),
            exit: if exit_receiver.is_some() {
                ExitChordState::Armed
            } else {
                ExitChordState::Idle
            },
            exit_receiver,
        }
    })
}

/// Поколение нынешнего приёмника ввода галереи.
fn sink_generation() -> u64 {
    INPUT_SINK.with(|sink| sink.borrow().generation())
}

/// Событие вида приёмника поколения `generation`. Вызывается изнутри AppKit, в том числе при
/// занятом окне: трогает только ячейки маршрутизатора, панели с фокусом и журнала, строка статуса
/// обновляется из очереди главного потока.
fn on_sink_event(generation: u64, event: KeySinkEvent) {
    let receiver = ModeReceiver::GalleryInput(generation);
    match event {
        // Приёмник с фокусом — получатель в `Input`; без фокуса получателя в окне галереи нет.
        KeySinkEvent::Focus(focused) => {
            if generation != sink_generation() {
                return;
            }
            KEY_PANEL.with(|slot| {
                *slot.borrow_mut() = focused.then(|| PanelFocus {
                    receiver,
                    plugin: input_sink::PLUGIN.to_owned(),
                    input: true,
                });
            });
            let mode = if focused {
                Mode::Input
            } else {
                Mode::Navigation
            };
            with_router(|router| {
                router.cancel(Cancel::Focus);
                router.set_mode(receiver, mode);
            });
            defer(|_| show_modes());
        }
        // Режим берётся у маршрутизатора в момент события: клавиша, дошедшая в `Navigation`,
        // принятой не считается.
        KeySinkEvent::Key {
            down,
            key_code,
            mods,
            repeat,
        } => {
            let Some(mode) = with_router(|router| router.mode(&receiver)) else {
                return;
            };
            let record = KeyRecord {
                kind: if down { KeyKind::Down } else { KeyKind::Up },
                key_code,
                mods: Mods {
                    cmd: mods.cmd,
                    ctrl: mods.ctrl,
                    alt: mods.alt,
                    shift: mods.shift,
                },
                repeat,
            };
            INPUT_SINK.with(|sink| sink.borrow_mut().accept(generation, mode, record));
        }
    }
}

/// Дерево галереи с приёмником снято: получатель закрыт вместе с ожиданием второго аккорда,
/// следующий вид получает новое поколение с пустым журналом.
fn on_sink_retired() {
    let closed = ModeReceiver::GalleryInput(sink_generation());
    KEY_PANEL.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.as_ref().is_some_and(|focus| focus.receiver == closed) {
            *slot = None;
        }
    });
    with_router(|router| router.closed(&closed));
    INPUT_SINK.with(|sink| sink.borrow_mut().rebuilt());
}

fn input_event(record: &KeyRecord) -> InputEventView {
    InputEventView {
        kind: match record.kind {
            KeyKind::Down => InputEventKind::Down,
            KeyKind::Up => InputEventKind::Up,
        },
        key_code: record.key_code,
        cmd: record.mods.cmd,
        ctrl: record.mods.ctrl,
        alt: record.mods.alt,
        shift: record.mods.shift,
        repeat: record.repeat,
    }
}

fn wire_panel_mode(mode: Mode) -> PanelMode {
    match mode {
        Mode::Navigation => PanelMode::Navigation,
        Mode::Input => PanelMode::Input,
    }
}

/// Режим панели по маршрутизатору; занятый маршрутизатор — `Navigation`.
fn panel_mode(panel: PanelId) -> Mode {
    with_router(|router| router.mode(&ModeReceiver::Panel(panel))).unwrap_or(Mode::Navigation)
}

/// Точки истории по состоянию зеркала после применённой доставки.
fn observe_history() {
    MIRROR.with(|mirror| {
        if let Some(mirror) = mirror.borrow().as_ref() {
            HISTORY.with(|history| history.borrow_mut().observe(mirror.state(), panel_mode));
        }
    });
}

/// Действие `history.*`: шаг по истории активного воркспейса штатными командами ядра. Цель в
/// открытой рабочей области — одна `panel.focus`, в другой — `panel.focus`, затем `workbench.open`:
/// фокус открытой области между ними не меняется. Скрытое окно, галерея, отсутствие ядра и
/// история без цели — действовать не на что. Курсор встаёт на цель до ответа ядра, отказ его
/// возвращает.
fn on_history(step: HistoryStep, source: Option<ReceiptSource>) {
    let tag = source.map(|source| Sent::action(step.keymap_name(), source));
    let shown =
        with_window(|owner| owner.window.is_visible() && owner.layout.is_some()) == Some(true);
    let linked = CORE.with(|slot| slot.borrow().is_some());
    let jump = (shown && linked)
        .then(|| {
            MIRROR.with(|mirror| {
                let mirror = mirror.borrow();
                let state = mirror.as_ref()?.state();
                let jump =
                    HISTORY.with(|history| history.borrow_mut().jump(state, step, panel_mode))?;
                Some((jump, open_workbench(state)))
            })
        })
        .flatten();
    let Some((jump, open)) = jump else {
        return note_receipt(tag, Ending::NotApplicable);
    };
    with_window(|owner| owner.show_status(None));
    let focus = Command::PanelFocus {
        panel: Some(jump.point.panel),
        workbench: None,
        direction: None,
    };
    let next = (open != Some(jump.point.workbench)).then_some(Command::WorkbenchOpen {
        workbench: jump.point.workbench,
    });
    let sent = focus.clone();
    request_jump(focus, move |app, outcome| match (outcome, next) {
        (Ok(_), Some(next)) => {
            let sent = next.clone();
            request_jump(next, move |app, outcome| {
                finish_jump(app, &jump, tag, &sent, outcome);
            });
        }
        (outcome, _) => finish_jump(app, &jump, tag, &sent, outcome),
    });
}

/// Команда шага истории: исход приходит в очередь главного потока.
fn request_jump(
    command: Command,
    done: impl FnOnce(&App, Result<Reply, Failure>) + Send + 'static,
) {
    CORE.with(|slot| match slot.borrow().as_ref() {
        Some(link) => link.request(command, move |outcome| {
            defer(move |app| done(app, outcome));
        }),
        None => defer(move |app| done(app, Err(Failure::NoCore))),
    });
}

/// Конец шага истории: одна запись кольца квитанций. Отказ возвращает курсор и показывает причину
/// в строке статуса.
fn finish_jump(
    app: &App,
    jump: &Jump,
    tag: Option<Sent>,
    sent: &Command,
    outcome: Result<Reply, Failure>,
) {
    let failure = match outcome {
        Ok(reply) => return note_receipt(tag, Ending::Reply(&reply)),
        Err(failure) => failure,
    };
    HISTORY.with(|history| history.borrow_mut().undo(jump));
    match failure {
        Failure::Core(error) => {
            on_refused(app, sent, &error);
            note_receipt(tag, Ending::Refused(&error));
        }
        Failure::NoReply | Failure::NoCore => {
            note_receipt(tag, Ending::NoReply);
            owner::say_err(format_args!(
                "shellio: {} отклонена: ядро не ответило",
                sent.name()
            ));
            with_window(|owner| owner.show_status(Some("Ядро не ответило")));
            refresh_layout(app);
        }
    }
}

/// Чтение `ui.history`: точки активного воркспейса и курсор. `None` — окна нет.
fn history_view() -> Option<HistoryView> {
    with_window(|_| ())?;
    Some(MIRROR.with(|mirror| {
        let mirror = mirror.borrow();
        let state = mirror.as_ref().map(Mirror::state);
        HISTORY.with(|history| {
            let history = history.borrow();
            let list = state
                .and_then(|state| history.of_active(state))
                .map(|(_, list)| list);
            HistoryView {
                pid: std::process::id(),
                workspace: state.and_then(|state| state.window.active_workspace),
                points: list
                    .map(|list| {
                        list.points()
                            .iter()
                            .map(|point| HistoryPoint {
                                workbench: point.workbench,
                                panel: point.panel,
                                mode: wire_panel_mode(point.mode),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                cursor: list.and_then(|list| list.cursor()),
            }
        })
    }))
}

/// Намерение строки фильтра панели `panel`: ввод, `Enter` или `Esc`. Закрытая строка прерывает
/// ввод без события поля (`abortEditing` конец ввода не объявляет), поэтому режим панели
/// согласуется здесь: без поля с вводом она возвращается в `Navigation`.
fn on_filter_row(panel: PanelId, intent: TextFieldIntent) {
    let closed = with_window(|owner| {
        owner
            .layout
            .as_mut()
            .is_some_and(|layout| layout.filter_intent(panel, intent))
    });
    if closed == Some(true) {
        follow_field();
    }
}

/// Пункт `row` меню «Вид → Открыть панель»: одна команда `panel.open` в рабочую область, открытую
/// в момент нажатия. Экземпляр ищет актор, завершение команды пишется в кольцо квитанций. Без
/// открытой рабочей области пункт недоступен, и нажатие ничего не делает. Скрытое окно пункт не
/// получает, как пункты зон: панель не открывается там, где её не видно.
fn on_open_panel(app: &App, row: usize) {
    let plugin = with_window(|owner| {
        if !owner.window.is_visible() {
            return None;
        }
        owner.open_rows.get(row).copied()
    })
    .flatten();
    let Some(plugin) = plugin else {
        return;
    };
    let command = MIRROR.with(|mirror| {
        let mirror = mirror.borrow();
        scene::open_panel_command(mirror.as_ref()?.state(), plugin)
    });
    if let Some(command) = command {
        let sent = Sent::opened(&command, ReceiptSource::Menu);
        send_ui(app, command, sent);
    }
}

/// Конец перетаскивания разделителя: одна команда `layout.resize`. Разделитель, который размер не
/// меняет, возвращается на место модели.
fn on_drag(app: &App, split: SplitId, drag: SplitDrag) {
    let command = with_window(|owner| {
        owner.show_status(None);
        let command = owner
            .layout
            .as_ref()
            .and_then(LayoutUi::scene)
            .and_then(|scene| scene::resize_command(scene, split, drag.divider, drag.position));
        if command.is_none() {
            owner.render_layout(app);
        }
        command
    })
    .flatten();
    if let Some(command) = command {
        send_ui(app, command, None);
    }
}

/// Запрос `ui` управляющего API. Без окна — `no_ui`. Действие выполняет [`handle`]; `app.quit`
/// отвечает квитанцией до `terminate:`, который [`App::terminate`] ставит таймером цикла.
pub fn request(app: &App, request: UiRequest) -> Result<UiReply, UiRefusal> {
    let name = request.name();
    if let UiRequest::Gallery(command) = request {
        return with_window(|owner| owner.gallery_command(app, command))
            .unwrap_or(Err(UiRefusal::NoUi));
    }
    let reply = match request {
        UiRequest::Window => {
            let revision = MIRROR.with(|mirror| mirror.borrow().as_ref().map(|m| m.revision().0));
            with_window(|owner| UiReply::Window(Box::new(owner.window_view(app, revision))))
        }
        UiRequest::Inspect => with_window(|owner| UiReply::Inspect(Box::new(owner.inspect_view()))),
        UiRequest::Stats => {
            let reply = with_window(|owner| UiReply::Stats(Box::new(owner.stats_view())));
            // Отложенные освобождения AppKit ждут в пуле `run` очередного события: следующее чтение
            // считает живые объекты уже после слива, и утечку не путает с ожиданием ввода.
            app.drain_event_pool();
            reply
        }
        UiRequest::Receipts { after } => with_window(|_| ()).map(|()| {
            UiReply::Receipts(CORE.with(|slot| {
                slot.borrow()
                    .as_ref()
                    .map(|link| link.receipts.borrow().view(after))
                    .unwrap_or_default()
            }))
        }),
        UiRequest::Mode => mode_view().map(UiReply::Mode),
        UiRequest::History => history_view().map(UiReply::History),
        UiRequest::InputJournal => {
            return with_window(|owner| owner.input_journal()).unwrap_or(Err(UiRefusal::NoUi));
        }
        UiRequest::Command(command) => {
            let owner = with_window(|_| ());
            owner.and_then(|()| {
                handle(app, command, None);
                with_window(|owner| owner.accepted(command))
            })
        }
        UiRequest::Filter { text } => {
            let owner = with_window(|_| ());
            owner.and_then(|()| {
                on_filter(app, Some(&text), None);
                with_window(|owner| owner.accepted(UiCommand::PanelFilter))
            })
        }
        UiRequest::Cell { contrast, motion } => {
            with_window(|owner| owner.set_cell(contrast, motion))
        }
        UiRequest::WindowSize { width, height } => {
            with_window(|owner| owner.set_window_size(app, name, width, height))
        }
        UiRequest::Overlay => overlay_view().map(|view| UiReply::Overlay(Box::new(view))),
        UiRequest::PaletteOpen { query, category } => with_window(|_| ()).and_then(|()| {
            let command = UiCommand::OverlayOpen(OverlayKind::Palette);
            handle(app, command, None);
            OVERLAY.with(|state| {
                let mut state = state.borrow_mut();
                if state.kind == Some(OverlayKind::Palette)
                    && let Some(palette) = state.palette.as_mut()
                {
                    if let Some(query) = query {
                        palette.set_query(&query);
                    }
                    if let Some(category) = category {
                        palette.set_category(model_category(category));
                    }
                    let typed = !palette.query().is_empty();
                    with_router(|router| router.set_typed(typed));
                }
            });
            show_palette();
            with_window(|owner| {
                owner.bump();
                owner.accepted(command)
            })
        }),
        UiRequest::OverlayClose(kind) => with_window(|_| ()).and_then(|()| {
            close_overlay(kind);
            with_window(|owner| owner.accepted_named(name))
        }),
        UiRequest::CmdlineRun { text } => with_window(|_| ())
            .and_then(|()| cmdline_run(app, text))
            .map(|run| UiReply::Cmdline(Box::new(run))),
        UiRequest::Gallery(_) => None,
    };
    reply.ok_or(UiRefusal::NoUi)
}

/// Единый обработчик действий интерфейса: меню, кнопка заголовка и CLI. `source` — источник для
/// кольца квитанций: клавиша, меню или кнопка; вызов CLI записи не оставляет.
pub fn handle(app: &App, command: UiCommand, source: Option<ReceiptSource>) {
    let done = || {
        note_receipt(
            source.map(|source| Sent::action(command.keymap_name(), source)),
            Ending::Done,
        )
    };
    match command {
        // Тела панелей скрытого окна неактивны.
        UiCommand::WindowClose => {
            // Скрытое окно наложения не держит: модальный слой закрывается с возвратом источника.
            let open = with_router(|router| router.modal().map(|modal| modal.kind)).flatten();
            if let Some(kind) = open {
                close_overlay(kind);
            }
            with_window(|owner| {
                owner.window.hide();
                owner.render_layout(app);
            });
            done();
        }
        // Вне состояния окна: `terminate:` спрашивает делегата приложения на следующем обороте цикла.
        UiCommand::AppQuit => {
            done();
            app.terminate();
        }
        UiCommand::AppearanceSet(choice) => {
            with_window(|owner| owner.set_choice(choice));
            done();
        }
        UiCommand::AppearanceCycle => {
            with_window(|owner| owner.set_choice(owner.choice.next()));
            done();
        }
        UiCommand::LayoutToggle(target) => on_toggle(app, target, source),
        UiCommand::List(action) => on_list(action, source),
        UiCommand::PanelFilter => on_filter(app, None, source),
        UiCommand::Mode(action) => on_mode(action, source),
        UiCommand::History(step) => on_history(step, source),
        UiCommand::OverlayOpen(kind) => on_overlay_open(app, kind, source),
    }
}

/// Действие `*.open`: модальный слой маршрутизатора получает наложение с источником, составление
/// IME снимается без коммита. С клавиши слой уже открыт самим решением. Скрытое окно и галерея
/// наложений не получают.
fn on_overlay_open(app: &App, kind: OverlayKind, source: Option<ReceiptSource>) {
    let tag = source.map(|source| Sent::action(kind.keymap_name(), source));
    let shown = with_window(|owner| owner.window.is_visible() && owner.gallery.is_none());
    if shown != Some(true) {
        // Слой, открытый решением клавиши, без вида не остаётся: иначе он забирал бы клавиши.
        close_overlay(kind);
        return note_receipt(tag, Ending::NotApplicable);
    }
    app.discard_composition();
    let (focus, _) = current_focus(app);
    let open = with_router(|router| {
        router.open(kind, &focus);
        router.modal().map(|modal| modal.kind)
    })
    .flatten();
    if open != Some(kind) {
        return note_receipt(tag, Ending::NotApplicable);
    }
    let fresh = OVERLAY.with(|state| state.borrow().kind != Some(kind));
    if fresh {
        let (palette, panel) = match kind {
            OverlayKind::Palette => modal_source()
                .map(|(panel, mode)| (new_palette(panel, mode), panel))
                .unwrap_or_default(),
            OverlayKind::Hints | OverlayKind::Cmdline => (None, None),
        };
        OVERLAY.with(|state| {
            let mut state = state.borrow_mut();
            *state = OverlayState::fresh(kind);
            state.palette = palette;
            state.panel = panel;
        });
    }
    show_palette();
    with_window(|owner| {
        owner.show_hints();
        owner.bump();
    });
    // Цели собираются после снятия подсказки which-key: показанная подсказка не перекрывает их.
    if fresh && kind == OverlayKind::Hints {
        let targets =
            with_window(|owner| hint_targets(&owner.inspect_view().components)).unwrap_or_default();
        OVERLAY.with(|state| {
            state.borrow_mut().hints = Some((HintLabels::new(targets.len()), targets));
        });
        // Метки появляются на уже собранных целях: слой не попадает в их отбор.
        with_window(|owner| owner.bump());
    }
    note_receipt(tag, Ending::Done);
}

/// Служебное закрытие наложения вида `kind`: как `Esc`, источник восстанавливается. Другое
/// открытое наложение не трогается.
fn close_overlay(kind: OverlayKind) {
    let modal = with_router(|router| {
        if router.modal().is_some_and(|modal| modal.kind == kind) {
            router.close()
        } else {
            None
        }
    })
    .flatten();
    if let Some(modal) = modal {
        restore_source(modal);
    }
}

/// Наложение закрыто без действия: состав забыт, источнику возвращаются фокус и режим. Закрытая
/// с тех пор панель не трогается: маршрутизатор уже снял её из источника.
fn restore_source(modal: Modal<ModeReceiver>) {
    OVERLAY.with(|state| *state.borrow_mut() = OverlayState::default());
    show_palette();
    if let Some(ModeReceiver::Panel(panel)) = modal.source.receiver {
        with_window(|owner| {
            if let Some(layout) = owner.layout.as_ref() {
                layout.set_input(panel, modal.source.mode == Mode::Input);
            }
        });
    }
    follow_field();
    with_window(|owner| owner.bump());
}

/// Наложение закрыто перед своим действием: состав забыт, источник не восстанавливается.
fn dismiss_overlay() {
    if with_router(|router| router.dismiss()).flatten().is_some() {
        OVERLAY.with(|state| *state.borrow_mut() = OverlayState::default());
        show_palette();
        follow_field();
        with_window(|owner| owner.bump());
    }
}

/// Клавиша управления открытым наложением. `Tab` и `Shift-Tab` перебирают категории палитры.
/// Однозначная метка нажимает свою цель, набор без совпадений закрывает метки без действия.
fn on_overlay_key(app: &App, kind: OverlayKind, chord: Chord) {
    let open = with_router(|router| router.modal().map(|modal| modal.kind)).flatten();
    if open != Some(kind) {
        return;
    }
    match kind {
        OverlayKind::Palette => {
            let chosen = OVERLAY.with(|state| {
                let mut state = state.borrow_mut();
                let palette = state.palette.as_mut()?;
                palette_key(palette, chord)
            });
            if let Some(row) = chosen {
                return run_palette_row(app, row, ReceiptSource::Key);
            }
            show_palette();
        }
        OverlayKind::Hints => {
            let Some(sign) = chord.sign() else {
                return;
            };
            let step = OVERLAY.with(|state| {
                let mut state = state.borrow_mut();
                let (labels, targets) = state.hints.as_mut()?;
                Some(match labels.press(sign) {
                    HintStep::Narrowed => None,
                    HintStep::Activate(index) => Some(targets.get(index).cloned()),
                    HintStep::Cancel => Some(None),
                })
            });
            match step.flatten() {
                None => {}
                Some(Some(target)) => return press_hint(&target.id),
                // Набор без совпадений закрывает метки без действия.
                Some(None) => return close_overlay(OverlayKind::Hints),
            }
        }
        OverlayKind::Cmdline => {}
    }
    with_window(|owner| owner.bump());
}

/// Метка набрана целиком. Как строка палитры (правило «Выполнение»): метки закрываются, источник
/// восстанавливается, затем цель, найденная заново по имени узла, нажимается тем же обработчиком,
/// что щелчок, — квитанцию пишет этот обработчик. Цель, которой больше нет, не нажимается. Нажатие
/// идёт вне заимствования окна: обработчик щелчка может обратиться к окну сам.
fn press_hint(id: &str) {
    if let Some(modal) = with_router(KeyRouter::close).flatten() {
        restore_source(modal);
    }
    let Some(found) = with_window(|owner| {
        let target = retarget(&owner.inspect_view().components, id)?;
        Some((target.object, owner.window.pressable(target.object)))
    })
    .flatten() else {
        // Цель исчезла, скрылась, ушла за край, перекрыта или отключена: не нажимается.
        return;
    };
    match found {
        (_, Some(pressable)) => {
            if !pressable.press() {
                owner::say_err(format_args!(
                    "shellio: цель метки {id} не нажалась: у вида нет обработчика нажатия"
                ));
            }
        }
        (object, None) => owner::say_err(format_args!(
            "shellio: цель метки {id} не найдена среди видов окна (объект {object:#x})"
        )),
    }
}

/// Намерение вида палитры. Щелчок по строке выполняет её, как в прототипе (`overlays.js:247`).
fn on_palette_intent(app: &App, intent: PaletteIntent) {
    let open = with_router(|router| router.modal().map(|modal| modal.kind)).flatten();
    if open != Some(OverlayKind::Palette) {
        return;
    }
    match intent {
        PaletteIntent::Edited(query) => {
            let typed = OVERLAY.with(|state| {
                let mut state = state.borrow_mut();
                let palette = state.palette.as_mut()?;
                palette.set_query(&query);
                Some(!palette.query().is_empty())
            });
            if let Some(typed) = typed {
                with_router(|router| router.set_typed(typed));
            }
            show_palette();
            with_window(|owner| owner.bump());
        }
        PaletteIntent::Select(id) | PaletteIntent::Activate(id) => {
            let row = OVERLAY.with(|state| {
                let state = state.borrow();
                let palette = state.palette.as_ref()?;
                palette.rows().iter().find(|row| row.id == id).cloned()
            });
            if let Some(row) = row {
                run_palette_row(app, row, ReceiptSource::Button);
            }
        }
        PaletteIntent::Dismiss => close_overlay(OverlayKind::Palette),
    }
}

/// Вид палитры по состоянию наложения: открытая палитра показывает свой состав, иначе окно
/// палитры снято и key вернулся окну владельца.
fn show_palette() {
    let state = OVERLAY.with(|state| {
        let state = state.borrow();
        // Палитра без состава (реестр или keymap недоступны) — пустая выдача, а не невидимое
        // модальное наложение.
        (state.kind == Some(OverlayKind::Palette)).then(|| {
            state
                .palette
                .as_ref()
                .map_or_else(CommandPaletteState::default, palette_spec)
        })
    });
    with_window(|owner| {
        let Some(palette) = owner.palette.as_ref() else {
            return;
        };
        match state {
            Some(state) => {
                palette.set_state(state);
                if !palette.is_open() {
                    palette.open(&owner.window);
                }
            }
            None if palette.is_open() => palette.close(&owner.window),
            None => {}
        }
    });
}

/// Состояние вида палитры из модели: подписи плашек клавиш — по правилу `K.keyChips` прототипа.
fn palette_spec(palette: &Palette) -> CommandPaletteState {
    let view = palette.view();
    CommandPaletteState {
        query: palette.query().to_owned(),
        category: (palette.category() != Category::All)
            .then(|| palette.category().title().to_owned()),
        groups: view
            .groups
            .iter()
            .map(|group| PaletteGroupSpec {
                title: group.group.title().to_owned(),
                rows: group
                    .rows
                    .iter()
                    .filter_map(|&index| palette.rows().get(index))
                    .map(palette_row_spec)
                    .collect(),
            })
            .collect(),
        selected: palette.selected_row().map(|row| row.id.clone()),
    }
}

fn palette_row_spec(row: &PaletteRow) -> PaletteRowSpec {
    let chords: Vec<String> = row
        .keys
        .as_deref()
        .map(|keys| keys.split(' ').map(str::to_owned).collect())
        .unwrap_or_default();
    PaletteRowSpec {
        id: row.id.clone(),
        icon: row.icon.clone(),
        title: row.title.clone(),
        detail: row.detail.clone(),
        keys: chip_badges(&chords),
    }
}

/// Чтение `ui.overlay`. `None` — окна нет.
fn overlay_view() -> Option<OverlayView> {
    with_window(|_| ())?;
    let modal = with_router(|router| router.modal().cloned()).flatten();
    Some(OVERLAY.with(|state| {
        let state = state.borrow();
        let kind = modal.as_ref().map(|modal| modal.kind);
        OverlayView {
            pid: std::process::id(),
            kind: kind.map(|kind| match kind {
                OverlayKind::Palette => OverlayViewKind::Palette,
                OverlayKind::Hints => OverlayViewKind::Hints,
                OverlayKind::Cmdline => OverlayViewKind::Cmdline,
            }),
            source: modal.as_ref().map(|modal| OverlaySourceView {
                panel: match modal.source.receiver {
                    Some(ModeReceiver::Panel(panel)) => Some(panel),
                    Some(ModeReceiver::GalleryInput(_)) | None => None,
                },
                receiver: modal.source.receiver,
                mode: wire_panel_mode(modal.source.mode),
                field: modal.source.field,
            }),
            palette: (kind == Some(OverlayKind::Palette))
                .then(|| palette_view(state.palette.as_ref())),
            hints: (kind == Some(OverlayKind::Hints)).then(|| hints_view(state.hints.as_ref())),
            cmdline: (kind == Some(OverlayKind::Cmdline)).then(|| CmdlineStateView {
                text: state.text.clone(),
            }),
            which_key: kind.is_none().then(which_view).flatten(),
        }
    }))
}

/// Метки, совпавшие с набранной частью, для `ui.overlay`.
fn hints_view(hints: Option<&(HintLabels, Vec<HintTarget>)>) -> HintsStateView {
    let Some((labels, targets)) = hints else {
        return HintsStateView {
            typed: String::new(),
            labels: Vec::new(),
        };
    };
    HintsStateView {
        typed: labels.typed().to_owned(),
        labels: labels
            .visible()
            .filter_map(|(index, text)| {
                let target = targets.get(index)?;
                Some(HintLabelView {
                    text: text.to_owned(),
                    target: target.key.clone(),
                    frame: target.frame,
                })
            })
            .collect(),
    }
}

/// Состав показанной подсказки which-key для `ui.overlay`.
fn which_view() -> Option<WhichKeyView> {
    WHICH.with(|which| {
        which.borrow().shown.as_ref().map(|shown| WhichKeyView {
            prefix: shown.prefix.clone(),
            title: shown.title.clone(),
            items: shown
                .items
                .iter()
                .map(|item| WhichItemView {
                    key: item.key.clone(),
                    label: item.label.clone(),
                    group: item.group,
                })
                .collect(),
        })
    })
}

/// Сверяет подсказку which-key с набранным префиксом `pending` (правило «which-key не
/// модальный»): пустой префикс её снимает, показанная подсказка сразу следует за новым префиксом,
/// иначе появление ждёт задержки `which_key_delay`. `true` — показанный состав изменился.
fn sync_which_key(keymap: &Keymap, pending: &[Chord]) -> bool {
    WHICH.with(|slot| {
        let mut which = slot.borrow_mut();
        if which.prefix == pending {
            return false;
        }
        which.prefix = pending.to_vec();
        which.generation += 1;
        if pending.is_empty() {
            return which.shown.take().is_some();
        }
        if which.shown.is_some() {
            let next = which_now(keymap, pending);
            let changed = which.shown != next;
            which.shown = next;
            return changed;
        }
        if let Some(delay) = which_key_delay(pending, keymap.leader()) {
            let generation = which.generation;
            MainThread::new().exec_after(delay, move |_| show_which_key(generation));
        }
        false
    })
}

/// Отложенный показ: префикс поколения `generation` всё ещё набран.
fn show_which_key(generation: u64) {
    let Some(snapshot) = KEYMAP.with(|slot| slot.borrow().clone()) else {
        return;
    };
    let Some(pending) = with_router(|router| router.pending().to_vec()) else {
        return;
    };
    let changed = WHICH.with(|slot| {
        let mut which = slot.borrow_mut();
        if which.generation != generation || which.prefix != pending || pending.is_empty() {
            return false;
        }
        which.shown = which_now(&snapshot.keymap, &pending);
        which.shown.is_some()
    });
    if changed {
        with_window(OwnerWindow::bump);
    }
}

/// Продолжения префикса в контекстах панели с фокусом, как их решит маршрутизатор.
fn which_now(keymap: &Keymap, pending: &[Chord]) -> Option<WhichKey> {
    let registry = plugins::shared_registry().ok()?;
    let panel = KEY_PANEL.with(|slot| slot.borrow().clone());
    let mode = panel.as_ref().map_or(Some(Mode::Navigation), |panel| {
        with_router(|router| router.mode(&panel.receiver))
    })?;
    let contexts = Keymap::contexts(mode, panel.as_ref().map(|panel| panel.plugin.as_str()));
    which_key(keymap, &contexts, pending, &|binding| {
        which_titles::title(binding, &registry)
    })
}

/// Панель и режим источника открытого наложения.
fn modal_source() -> Option<(Option<PanelId>, Mode)> {
    with_router(|router| {
        router.modal().map(|modal| {
            let panel = match modal.source.receiver {
                Some(ModeReceiver::Panel(panel)) => Some(panel),
                Some(ModeReceiver::GalleryInput(_)) | None => None,
            };
            (panel, modal.source.mode)
        })
    })
    .flatten()
}

/// Палитра из сцены, реестра и действующей таблицы для панели-источника `panel` в режиме `mode`.
fn new_palette(panel: Option<PanelId>, mode: Mode) -> Option<Palette> {
    let registry = plugins::shared_registry().ok()?;
    let source = with_window(|owner| {
        let dark = matches!(
            owner.window.selection().cell.cell.appearance,
            Appearance::Dark
        );
        let layout = owner.layout.as_ref();
        let scene = layout.and_then(LayoutUi::scene);
        let resolve = |name: &str| layout?.resolve_args(panel?, name);
        palette_source::source(&Facts {
            scene,
            registry: &registry,
            panel,
            mode,
            dark,
            resolve: &resolve,
        })
    })?;
    KEYMAP.with(|slot| {
        let slot = slot.borrow();
        let keymap = &slot.as_ref()?.keymap;
        Some(Palette::new(&source, keymap, "", Category::All))
    })
}

fn model_category(category: PaletteCategory) -> Category {
    match category {
        PaletteCategory::All => Category::All,
        PaletteCategory::Commands => Category::Commands,
        PaletteCategory::Panels => Category::Panels,
    }
}

fn wire_category(category: Category) -> PaletteCategory {
    match category {
        Category::All => PaletteCategory::All,
        Category::Commands => PaletteCategory::Commands,
        Category::Panels => PaletteCategory::Panels,
    }
}

/// Клавиша управления палитрой: категории, выбор и выполнение. Возвращает строку для выполнения;
/// `Enter` без строк ничего не выполняет, палитра остаётся открытой.
fn palette_key(palette: &mut Palette, chord: Chord) -> Option<PaletteRow> {
    let is = |names: &[&str]| {
        names
            .iter()
            .any(|name| Chord::parse(name).ok() == Some(chord))
    };
    if is(&["Tab"]) {
        palette.next_category();
    } else if is(&["Shift-Tab"]) {
        palette.prev_category();
    } else if is(&["Down", "Ctrl-n", "Ctrl-j"]) {
        palette.select_next();
    } else if is(&["Up", "Ctrl-p", "Ctrl-k"]) {
        palette.select_prev();
    } else if is(&["Enter"]) {
        return palette.selected_row().cloned();
    }
    None
}

/// Выполнение строки палитры по правилу «Выполнение»: палитра закрывается, источнику
/// возвращаются фокус и режим, затем действие идёт тем же обработчиком, что клавиша, с одной
/// записью кольца квитанций: `source` — клавиша `Enter` или щелчок по строке.
fn run_palette_row(app: &App, row: PaletteRow, source: ReceiptSource) {
    let panel = OVERLAY.with(|state| state.borrow().panel);
    if let Some(modal) = with_router(KeyRouter::close).flatten() {
        restore_source(modal);
    }
    let action = Action {
        name: row.action,
        args: row.args,
    };
    run_action_with(app, action, Some(source), false, panel);
}

/// Состав палитры для `ui.overlay`.
fn palette_view(palette: Option<&Palette>) -> PaletteStateView {
    let Some(palette) = palette else {
        return PaletteStateView {
            query: String::new(),
            category: PaletteCategory::All,
            groups: Vec::new(),
            selected: None,
            matched: 0,
        };
    };
    let view = palette.view();
    PaletteStateView {
        query: palette.query().to_owned(),
        category: wire_category(palette.category()),
        groups: view
            .groups
            .iter()
            .map(|group| PaletteGroupView {
                title: group.group.title().to_owned(),
                rows: group
                    .rows
                    .iter()
                    .filter_map(|&index| palette.rows().get(index))
                    .map(|row| PaletteRowView {
                        id: row.id.clone(),
                        title: row.title.clone(),
                        detail: row.detail.clone(),
                        key: row.keys.clone(),
                    })
                    .collect(),
            })
            .collect(),
        selected: palette.selected_row().map(|row| row.id.clone()),
        matched: view.matched,
    }
}

/// `cmdline.run`: текст строки `:` по грамматике. Открытая строка закрывается до действия,
/// источник не восстанавливается; действие идёт обработчиком клавиши без записи кольца.
/// Неизвестная команда называется в строке статуса.
fn cmdline_run(app: &App, text: String) -> Option<CmdlineRunView> {
    let source = modal_source().unwrap_or_else(|| {
        let panel = KEY_PANEL.with(|slot| match slot.borrow().as_ref()?.receiver {
            ModeReceiver::Panel(panel) => Some(panel),
            ModeReceiver::GalleryInput(_) => None,
        });
        (panel, Mode::Navigation)
    });
    let palette = new_palette(source.0, source.1);
    let rows = palette.as_ref().map_or(&[][..], Palette::rows);
    let resolved = shellio_keymap::resolve_cmdline(&text, rows);
    let mut view = CmdlineRunView {
        pid: std::process::id(),
        text: text.clone(),
        outcome: CmdlineOutcome::Empty,
        action: None,
        args: None,
        message: None,
    };
    let open = with_router(|router| router.modal().map(|modal| modal.kind)).flatten();
    if open == Some(OverlayKind::Cmdline) {
        dismiss_overlay();
    }
    match resolved {
        shellio_keymap::Cmdline::Empty => {}
        shellio_keymap::Cmdline::Command(command) => {
            let action = cmdline_action(command);
            view.outcome = CmdlineOutcome::Run;
            view.action = Some(action.name.clone());
            view.args.clone_from(&action.args);
            run_action(app, action, None);
        }
        shellio_keymap::Cmdline::Fallback(row) => {
            let action = Action {
                name: row.action.clone(),
                args: row.args.clone(),
            };
            view.outcome = CmdlineOutcome::Run;
            view.action = Some(action.name.clone());
            view.args.clone_from(&action.args);
            run_action_with(app, action, None, false, source.0);
        }
        shellio_keymap::Cmdline::Unknown(unknown) => {
            let key = KEYMAP.with(|slot| {
                let slot = slot.borrow();
                let keymap = &slot.as_ref()?.keymap;
                keymap
                    .bindings()
                    .iter()
                    .find(|binding| binding.action == OverlayKind::Palette.keymap_name())
                    .map(|binding| binding.sequence.to_string())
            });
            let message = shellio_keymap::unknown_message(&unknown, key.as_deref());
            with_window(|owner| {
                owner.show_status(Some(&message));
                owner.bump();
            });
            mark_status();
            view.outcome = CmdlineOutcome::Unknown;
            view.message = Some(message);
        }
    }
    Some(view)
}

/// Действие keymap команды грамматики `:`.
fn cmdline_action(command: shellio_keymap::CmdlineCommand) -> Action {
    use shellio_keymap::{Axis, CmdlineCommand, Theme};
    let plain = |name: &str| Action {
        name: name.to_owned(),
        args: None,
    };
    match command {
        CmdlineCommand::Close => plain("panel.close"),
        CmdlineCommand::Zoom => plain("layout.zoom"),
        CmdlineCommand::Split(axis) => Action {
            name: "layout.split".to_owned(),
            args: Some(serde_json::json!({ "axis": match axis {
                Axis::Row => "row",
                Axis::Stack => "stack",
            } })),
        },
        CmdlineCommand::Theme(Theme::Light) => plain("ui.appearance.light"),
        CmdlineCommand::Theme(Theme::Dark) => plain("ui.appearance.dark"),
        CmdlineCommand::Theme(Theme::System) => plain("ui.appearance.system"),
    }
}

fn on_theme() {
    with_window(|owner| {
        if owner.sync_theme() {
            owner.bump();
        }
    });
}

fn on_intent(app: &App, intent: ChromeIntent) {
    match intent {
        ChromeIntent::AppearanceCycle => {
            handle(app, UiCommand::AppearanceCycle, Some(ReceiptSource::Button));
        }
        // Меню воркспейса появится вместе с источником в P1.7.
        ChromeIntent::WorkspaceMenu => {}
        ChromeIntent::Search => handle(
            app,
            UiCommand::OverlayOpen(OverlayKind::Palette),
            Some(ReceiptSource::Button),
        ),
        ChromeIntent::Zone(button) => handle(
            app,
            UiCommand::LayoutToggle(zone_target(button)),
            Some(ReceiptSource::Button),
        ),
    }
}

/// Действие меню в команде `ui`; `None` — пункт «Открыть панель»: он идёт командой ядра.
fn command_of(action: MenuAction) -> Option<UiCommand> {
    match action {
        MenuAction::Quit => Some(UiCommand::AppQuit),
        MenuAction::CloseWindow => Some(UiCommand::WindowClose),
        MenuAction::Appearance(choice) => Some(UiCommand::AppearanceSet(core_choice(choice))),
        MenuAction::Zone(button) => Some(UiCommand::LayoutToggle(zone_target(button))),
        MenuAction::OpenPanel(_) => None,
    }
}

fn theme_choice(choice: AppearanceChoice) -> ThemeChoice {
    match choice {
        AppearanceChoice::System => ThemeChoice::System,
        AppearanceChoice::Light => ThemeChoice::Light,
        AppearanceChoice::Dark => ThemeChoice::Dark,
    }
}

fn core_choice(choice: ThemeChoice) -> AppearanceChoice {
    match choice {
        ThemeChoice::System => AppearanceChoice::System,
        ThemeChoice::Light => AppearanceChoice::Light,
        ThemeChoice::Dark => AppearanceChoice::Dark,
    }
}

#[cfg(test)]
mod tests {
    use shellio_appkit::MenuItem;

    use super::*;

    #[test]
    fn palette_spec_follows_the_model() {
        use shellio_keymap::{Args, Candidate, Keymap, PaletteSource};
        let candidate = |id: &str, icon: &str, title: &str, action: &str| Candidate {
            id: id.to_owned(),
            icon: icon.to_owned(),
            title: title.to_owned(),
            detail: String::new(),
            action: action.to_owned(),
            args: Args::None,
            key_action: None,
            usual: None,
        };
        let source = PaletteSource {
            pinned: vec![candidate(
                "mode.toggle",
                "cmd",
                "Режим «Ввод»",
                "mode.toggle",
            )],
            plugins: vec![candidate(
                "panel.open.terminal",
                "term",
                "Открыть: Терминал",
                "panel.open.terminal",
            )],
            contexts: Keymap::contexts(Mode::Navigation, None),
            ..PaletteSource::default()
        };
        let keymap = Keymap::defaults(&[]).unwrap();
        let mut palette = Palette::new(&source, &keymap, "", Category::All);
        let spec = palette_spec(&palette);
        assert_eq!(spec.category, None);
        let titles: Vec<_> = spec
            .groups
            .iter()
            .map(|group| group.title.as_str())
            .collect();
        assert_eq!(titles, ["Команды", "Панели"]);
        let mode = &spec.groups[0].rows[0];
        assert_eq!(spec.selected.as_deref(), Some("mode.toggle"));
        assert_eq!(mode.icon, "cmd");
        // Плашки клавиш — по правилу строки статуса: `Ctrl` отдельной плашкой.
        assert_eq!(mode.keys, ["Ctrl", "\\"]);
        assert_eq!(spec.groups[1].rows[0].icon, "term");

        palette.next_category();
        let spec = palette_spec(&palette);
        assert_eq!(spec.category.as_deref(), Some("Команды"));
        assert_eq!(spec.groups.len(), 1);
        palette.set_query("нет такой строки");
        let spec = palette_spec(&palette);
        assert!(spec.is_empty());
        assert_eq!(spec.selected, None);
        assert_eq!(spec.query, "нет такой строки");
    }

    #[test]
    fn component_states_reach_the_wire() {
        let field = component_state(NodeState::TextField {
            state: TextFieldState {
                value: "shellio".into(),
                placeholder: "Поиск".into(),
                editable: true,
            },
            editing: true,
        });
        assert_eq!(
            field,
            ComponentState::TextField(TextFieldView {
                value: "shellio".into(),
                placeholder: "Поиск".into(),
                editable: true,
                editing: true,
            })
        );
        let menu = component_state(NodeState::Menu(MenuState {
            open: true,
            highlighted: Some("copy".into()),
            entries: vec![
                MenuEntry::Item(MenuItem {
                    key: Some(MenuKey::command("c")),
                    checked: true,
                    submenu: Some(vec![MenuEntry::Item(MenuItem::new("inner", "Внутри"))]),
                    ..MenuItem::new("copy", "Копировать")
                }),
                MenuEntry::Separator { id: "sep".into() },
            ],
        }));
        let ComponentState::Menu(menu) = menu else {
            panic!("не меню: {menu:?}");
        };
        assert!(menu.open);
        assert_eq!(menu.highlighted.as_deref(), Some("copy"));
        assert_eq!(menu.items.len(), 2);
        let copy = &menu.items[0];
        assert_eq!(
            (
                copy.id.as_str(),
                copy.key.as_deref(),
                copy.checked,
                copy.separator
            ),
            ("copy", Some("cmd+c"), true, false)
        );
        assert_eq!(copy.submenu.as_ref().map(Vec::len), Some(1));
        assert!(menu.items[1].separator);
        assert_eq!(menu.items[1].id, "sep");
    }

    /// Строка статуса окна — корень поверхности, пустой или скрытый слот сообщения ключа не несёт.
    #[test]
    fn status_message_is_keyed_only_with_shown_text() {
        let node = |id: &str, visible: bool, label: Option<&str>| InspectNode {
            id: id.to_owned(),
            class: "StatusMessage",
            object: 1,
            parent: None,
            ck: None,
            style: None,
            truncated: None,
            under: Some(shellio_appkit::Rgba {
                r: 30,
                g: 29,
                b: 27,
                a: 1.0,
            }),
            behind: None,
            facts: shellio_appkit::ViewFacts {
                frame: Frame::new(0.0, 0.0, 1.0, 1.0),
                visible,
                first_responder: false,
                enabled: None,
                role: None,
                label: label.map(str::to_owned),
                help: None,
                selected: None,
                expanded: None,
                animation_keys: 0,
            },
            state: None,
        };
        let line = status_keyed(node("status", true, None));
        assert_eq!(line.ck.as_deref(), Some("status"));
        assert!(line.style.is_some_and(|style| style.fill.is_some()));
        let shown = status_keyed(node("status.message", true, Some("Не хватает места")));
        assert_eq!(shown.ck.as_deref(), Some("status.message"));
        assert_eq!(shown.style, None, "слот — не корень поверхности");
        for (visible, label) in [(true, None), (true, Some("")), (false, Some("текст"))] {
            assert_eq!(
                status_keyed(node("status.message", visible, label)).ck,
                None
            );
        }
        // Плашка режима и подсказки приходят со своими ключами, набранный префикс — без ключа.
        for (id, ck) in [
            ("status.mode", "status.mode"),
            ("status.hint.1", "status.hint.1"),
            ("status.hint.1.key.1", "status.hint.1.key"),
            ("status.hint.2.label", "status.hint.2.label"),
        ] {
            let keyed = status_keyed(node(id, true, Some("текст")).with_ck(ck));
            assert_eq!(keyed.ck.as_deref(), Some(ck), "{id}");
            assert_eq!(keyed.style, None, "{id} — не корень поверхности");
        }
        assert_eq!(
            status_keyed(node("status.prefix", true, Some("Ctrl-w"))).ck,
            None
        );
        // Подпись в теле панели — тот же компонент под другим именем: ключей окна она не получает.
        for id in ["caption", "caption.message"] {
            assert_eq!(status_keyed(node(id, true, Some("текст"))).ck, None, "{id}");
        }
    }

    #[test]
    fn user_event_clears_only_the_message_shown_before_it() {
        let seen = status_serial();
        assert!(outlived(seen, status_serial()));
        // Событие само показало сообщение до отложенного снятия: оно остаётся.
        mark_status();
        assert!(!outlived(seen, status_serial()));
        assert!(outlived(status_serial(), status_serial()));
    }

    #[test]
    fn menu_actions_map_to_ui_commands() {
        assert_eq!(command_of(MenuAction::Quit), Some(UiCommand::AppQuit));
        assert_eq!(
            command_of(MenuAction::CloseWindow),
            Some(UiCommand::WindowClose)
        );
        for button in ZoneButton::ALL {
            assert_eq!(
                command_of(MenuAction::Zone(button)),
                Some(UiCommand::LayoutToggle(zone_target(button)))
            );
        }
        assert_eq!(
            command_of(MenuAction::OpenPanel(0)),
            None,
            "пункт «Открыть панель» идёт командой ядра"
        );
        for choice in AppearanceChoice::ALL {
            assert_eq!(core_choice(theme_choice(choice)), choice);
            assert_eq!(
                command_of(MenuAction::Appearance(theme_choice(choice))),
                Some(UiCommand::AppearanceSet(choice))
            );
        }
    }
}
