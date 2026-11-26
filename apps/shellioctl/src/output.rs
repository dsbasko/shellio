//! Вывод `shellioctl`: кадры ответа как есть при `--json`, иначе текст для человека.
//!
//! При `--json` в stdout идут только кадры: ответ сервера как есть или кадр `error` без
//! номера с отказом клиента. Локальные сбои без кода протокола — профиль, негодный ответ,
//! запись вывода — идут текстом в stderr в обоих режимах.

use std::fmt::{self, Write as _};
use std::io::{self, Write};

use shellio_control::{
    Activation, AxisSource, ClientError, ComponentState, ComponentView, Contrast, ControlError,
    Exit, GalleryView, HelloReply, InspectView, IntentKind, KeymapView, LaunchReceipt, Lightness,
    Motion, OkValue, OwnerState, ReceiptsView, Rect, Response, StatsView, TabRegime, UiReply,
    WindowView, encode_line,
};
use shellio_control::{ModeReceiver, ModeView};
use shellio_core::{
    Change, CommandValue, Event, EventsSince, OpenOutcome, Reply, ReplyValue, RootKind, Route,
    StatusView, TreeView,
};

use shellio_layout::{SizeClass, Zone};

use crate::args::{USAGE, Usage};

fn zone_text(zone: Zone) -> &'static str {
    match zone {
        Zone::Left => "left",
        Zone::Right => "right",
        Zone::Center => "center",
        Zone::Bottom => "bottom",
    }
}

/// Сообщение в stderr. Если и stderr закрыт, сообщить уже некуда.
fn complain(message: impl fmt::Display) {
    let _ = writeln!(io::stderr().lock(), "shellioctl: {message}");
}

/// Ошибка использования: сообщение и подсказка в stderr.
pub fn usage_error(usage: &Usage) -> Exit {
    complain(usage);
    complain("справка: shellioctl --help");
    Exit::Usage
}

/// Локальный сбой без кода протокола.
pub fn failure(message: impl fmt::Display) -> Exit {
    complain(message);
    Exit::Failure
}

/// Справка в stdout.
pub fn help() -> Exit {
    match write_stdout(USAGE) {
        Ok(()) => Exit::Success,
        Err(error) => failure(format_args!("вывод не записан: {error}")),
    }
}

fn write_stdout(text: &str) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    stdout.write_all(text.as_bytes())?;
    stdout.flush()
}

/// Печать исходов вызова.
#[derive(Clone, Copy, Debug)]
pub struct Printer {
    json: bool,
}

impl Printer {
    pub const fn new(json: bool) -> Self {
        Self { json }
    }

    /// Напечатать кадр сервера и вернуть код выхода по нему.
    pub fn frame(&self, frame: &Response) -> Exit {
        let printed = if self.json {
            json_line(frame)
        } else {
            match text(frame) {
                Text::Out(text) => write_stdout(&text),
                Text::Err(text) => {
                    complain(text);
                    Ok(())
                }
            }
        };
        match printed {
            Ok(()) => Exit::of_response(frame),
            Err(error) => failure(format_args!("вывод не записан: {error}")),
        }
    }

    /// Напечатать отказ клиента и вернуть код выхода по нему.
    pub fn client_error(&self, error: &ClientError) -> Exit {
        match error {
            ClientError::Control(control) if self.json => {
                let frame = Response::Error {
                    id: None,
                    error: control.clone(),
                };
                match json_line(&frame) {
                    Ok(()) => Exit::of_error(error),
                    Err(error) => failure(format_args!("вывод не записан: {error}")),
                }
            }
            ClientError::Control(control) => {
                complain(control_text(control));
                Exit::of_error(error)
            }
            ClientError::Failed(reason) => failure(reason),
        }
    }
}

fn json_line(frame: &Response) -> io::Result<()> {
    let line = encode_line(frame).map_err(io::Error::other)?;
    let mut stdout = io::stdout().lock();
    stdout.write_all(&line)?;
    stdout.flush()
}

/// Текст кадра: успех — в stdout, отказ — в stderr.
enum Text {
    Out(String),
    Err(String),
}

fn control_text(error: &ControlError) -> String {
    format!("{}: {}", error.code(), error.message())
}

fn text(frame: &Response) -> Text {
    match frame {
        Response::Ok { value, .. } => Text::Out(match value {
            OkValue::Hello(hello) => hello_text(hello),
            OkValue::Reply(reply) => reply_text(reply),
            OkValue::EventsSince(since) => events_since_text(since),
            OkValue::Launch(receipt) => receipt_text(receipt),
            OkValue::Ui(reply) => ui_text(reply),
            OkValue::Keymap(view) => keymap_text(view),
        }),
        Response::Snapshot { snapshot, .. } => {
            Text::Out(format!("снимок: ревизия {}\n", snapshot.revision.0))
        }
        Response::Event { event, .. } => Text::Out(event_text(event)),
        Response::Keymap { keymap, .. } => Text::Out(keymap_text(keymap)),
        Response::Reset { snapshot, .. } => Text::Out(format!(
            "сброс: подписка отстала, снимок на ревизии {}\n",
            snapshot.revision.0
        )),
        Response::CoreError { error, .. } => {
            Text::Err(format!("{}: {}", error.code(), error.message()))
        }
        Response::Error { error, .. } => Text::Err(control_text(error)),
    }
}

/// Таблица клавиш: версия и лидер, строки по одной, затем диагностика последнего разбора.
fn keymap_text(view: &KeymapView) -> String {
    let mut lines = vec![format!(
        "keymap версия {}, лидер {}, строк {}, диагностика {}",
        view.version,
        view.leader,
        view.bindings.len(),
        view.diagnostics.len()
    )];
    for row in &view.bindings {
        let args = row
            .args
            .as_ref()
            .map(|args| format!(" {args}"))
            .unwrap_or_default();
        lines.push(format!(
            "  {}  {}  {}{args}  [{}]",
            row.context,
            row.sequence,
            row.action,
            row.source.as_str()
        ));
    }
    for diagnostic in &view.diagnostics {
        lines.push(format!("  ! {}", diagnostic.message));
    }
    lines.join("\n")
}

fn hello_text(hello: &HelloReply) -> String {
    let owner = match hello.owner {
        OwnerState::Ready => "готов",
        OwnerState::Leaving => "завершает работу",
    };
    format!(
        "shellio {}, протокол {}, PID {}, владелец {owner}\n",
        hello.server, hello.protocol, hello.pid
    )
}

fn reply_text(reply: &Reply) -> String {
    let revision = reply.revision.0;
    match &reply.value {
        ReplyValue::Tree(tree) => tree_text(revision, tree),
        ReplyValue::Status(status) => status_text(revision, status),
        ReplyValue::Layout(view) => match serde_json::to_string_pretty(view) {
            Ok(json) => format!("{json}\n"),
            Err(error) => format!("ревизия {revision}: раскладка не выводится: {error}\n"),
        },
        ReplyValue::Panel(view) => match serde_json::to_string_pretty(view) {
            Ok(json) => format!("{json}\n"),
            Err(error) => format!("ревизия {revision}: панель не выводится: {error}\n"),
        },
        ReplyValue::Plugins(view) => match serde_json::to_string_pretty(view) {
            Ok(json) => format!("{json}\n"),
            Err(error) => format!("ревизия {revision}: плагины не выводятся: {error}\n"),
        },
        ReplyValue::Command(value) => {
            let value = match value {
                CommandValue::Opened {
                    panel,
                    workbench,
                    outcome,
                } => {
                    let outcome = outcome_text(*outcome);
                    format!("панель {panel} в рабочей области {workbench}: {outcome}")
                }
                CommandValue::Workspace { workspace } => format!("воркспейс {workspace}"),
                CommandValue::Project { project, root } => {
                    format!("проект {project}, корень {root}")
                }
                CommandValue::Workbench { workbench } => format!("рабочая область {workbench}"),
                CommandValue::Panel { panel } => format!("панель {panel}"),
                CommandValue::None => "готово".to_owned(),
            };
            format!("ревизия {revision}: {value}\n")
        }
    }
}

const fn outcome_text(outcome: OpenOutcome) -> &'static str {
    match outcome {
        OpenOutcome::Created => "создана",
        OpenOutcome::Focused => "в фокусе",
        OpenOutcome::Moved => "переехала",
        OpenOutcome::Routed => "окно перешло к ней",
    }
}

fn route_text(route: Route) -> String {
    match route {
        Route::WorkspaceHome => "главная".to_owned(),
        Route::Workbench(workbench) => format!("рабочая область {workbench}"),
    }
}

fn tree_text(revision: u64, tree: &TreeView) -> String {
    let mut out = format!("ревизия {revision}\n");
    match (tree.active_workspace, tree.route) {
        (Some(active), Some(route)) => {
            let _ = writeln!(out, "окно: воркспейс {active}, {}", route_text(route));
        }
        // У области всех воркспейсов маршрут есть всегда, а активного воркспейса нет, пока
        // не создан ни один.
        (None, Some(_)) => out.push_str("окно: активного воркспейса нет\n"),
        (_, None) => out.push_str("окно: вне области\n"),
    }
    for workspace in &tree.workspaces {
        let _ = writeln!(out, "воркспейс {} «{}»", workspace.id, workspace.title);
        for project in &workspace.projects {
            let listed = match project.list_position {
                Some(_) => "",
                None => " (вне списка)",
            };
            let _ = writeln!(out, "  проект {} «{}»{listed}", project.id, project.title);
            for root in &project.roots {
                let kind = match root.kind {
                    RootKind::Git => "git",
                    RootKind::Folder => "папка",
                };
                let _ = writeln!(out, "    корень {} ({kind})", root.path.display());
            }
            for workbench in &project.workbenches {
                let _ = writeln!(
                    out,
                    "    рабочая область {} «{}»",
                    workbench.id, workbench.title
                );
            }
        }
    }
    out
}

fn status_text(revision: u64, status: &StatusView) -> String {
    let mut out = format!(
        "ревизия {revision}\nсхема {}\nбаза {}\n",
        status.schema_version,
        status.state_db.display()
    );
    if status.diagnostics.is_empty() {
        out.push_str("диагностика: нет\n");
    } else {
        let _ = writeln!(out, "диагностика: {}", status.diagnostics.len());
        for diagnostic in &status.diagnostics {
            let _ = writeln!(
                out,
                "  {}#{}: {}",
                diagnostic.table, diagnostic.rowid, diagnostic.reason
            );
        }
    }
    out
}

fn events_since_text(since: &EventsSince) -> String {
    match since {
        EventsSince::Events(events) if events.is_empty() => "событий нет\n".to_owned(),
        EventsSince::Events(events) => events.iter().map(event_text).collect(),
        EventsSince::Reset(snapshot) => format!(
            "сброс: курсор вне буфера, снимок на ревизии {}\n",
            snapshot.revision.0
        ),
    }
}

fn event_text(event: &Event) -> String {
    let changes: Vec<String> = event.changes.iter().map(change_text).collect();
    format!("ревизия {}: {}\n", event.revision.0, changes.join("; "))
}

fn change_text(change: &Change) -> String {
    match change {
        Change::WorkspaceInserted { workspace } => {
            format!("workspace_inserted {} «{}»", workspace.id, workspace.title)
        }
        Change::WorkspaceRenamed { workspace, title } => {
            format!("workspace_renamed {workspace} «{title}»")
        }
        Change::WorkspaceLastWorkbenchSet {
            workspace,
            workbench,
        } => match workbench {
            Some(workbench) => format!("workspace_last_workbench_set {workspace} {workbench}"),
            None => format!("workspace_last_workbench_set {workspace} нет"),
        },
        Change::ProjectInserted { project, .. } => {
            format!("project_inserted {} «{}»", project.id, project.title)
        }
        Change::ProjectOpened { project, .. } => format!("project_opened {project}"),
        Change::WorkbenchInserted { workbench } => {
            format!("workbench_inserted {} «{}»", workbench.id, workbench.title)
        }
        Change::WorkbenchRenamed { workbench, title } => {
            format!("workbench_renamed {workbench} «{title}»")
        }
        Change::WindowSet { active, route } => {
            format!("window_set {active}, {}", route_text(*route))
        }
        Change::PanelAdd { panel } => format!(
            "panel_add {} {} «{}» в {}",
            panel.id, panel.plugin, panel.title, panel.workbench
        ),
        Change::PanelRemove { panel } => format!("panel_remove {panel}"),
        Change::PanelRehome { panel, workbench } => format!("panel_rehome {panel} в {workbench}"),
        Change::PanelData { panel, version, .. } => {
            format!("panel_data {panel} версии {version}")
        }
        Change::PanelOpenedFrom { panel, opened_from } => match opened_from {
            Some(caller) => format!("panel_opened_from {panel} {caller}"),
            None => format!("panel_opened_from {panel} нет"),
        },
        Change::PanelStatus { panel, status } => {
            format!("panel_status {panel} {}", status.as_str())
        }
        Change::LayoutSet { workbench, .. } => format!("layout_set {workbench}"),
    }
}

fn receipt_text(receipt: &LaunchReceipt) -> String {
    let activation = match receipt.activation {
        Activation::Requested => "запрошена",
        Activation::NoUi => "без UI",
    };
    let mut out = format!("активация: {activation}\n");
    for refused in &receipt.refused {
        let _ = writeln!(out, "{}: {}", refused.arg, refused.reason.message());
    }
    out
}

fn ui_text(reply: &UiReply) -> String {
    match reply {
        UiReply::Window(window) => window_text(window),
        UiReply::Inspect(inspect) => inspect_text(inspect),
        UiReply::Stats(stats) => stats_text(stats),
        UiReply::Receipts(receipts) => receipts_text(receipts),
        UiReply::Cell(cell) => format!(
            "ячейка: светлота {} ({}), контраст {} ({}), движение {} ({}), PID {}, поколение {}\n",
            lightness_text(cell.lightness.value),
            source_text(cell.lightness.source),
            contrast_text(cell.contrast.value),
            source_text(cell.contrast.source),
            motion_text(cell.motion.value),
            source_text(cell.motion.source),
            cell.pid,
            cell.generation
        ),
        UiReply::Mode(mode) => mode_text(mode),
        UiReply::Accepted(accepted) => format!(
            "принято: {}, PID {}, поколение {}\n",
            accepted.command, accepted.pid, accepted.generation
        ),
    }
}

/// Режим панели с фокусом и автомат аккорда выхода.
fn mode_text(view: &ModeView) -> String {
    let panel = view
        .panel
        .map_or_else(|| "нет".to_owned(), |panel| panel.to_string());
    let receiver = match view.receiver {
        Some(ModeReceiver::Panel(_)) => "панель",
        None => "нет",
    };
    let exit = match view.exit_receiver {
        Some(ModeReceiver::Panel(panel)) => format!("{}, панель {panel}", view.exit.as_str()),
        None => view.exit.as_str().to_owned(),
    };
    format!(
        "режим: {}, панель {panel}, получатель ввода: {receiver}, аккорд выхода: {exit}, PID {}\n",
        view.mode.as_str(),
        view.pid
    )
}

/// Квитанции действий главного потока: итог и по строке на запись.
fn receipts_text(view: &ReceiptsView) -> String {
    let mut out = format!(
        "квитанций {}, последняя {}\n",
        view.receipts.len(),
        view.last
    );
    for receipt in &view.receipts {
        let revision = receipt
            .revision
            .map_or_else(String::new, |revision| format!(", ревизия {}", revision.0));
        let result = match (receipt.outcome, receipt.panel, &receipt.error) {
            (_, _, Some(error)) => format!("отказ {error}"),
            (Some(outcome), Some(panel), None) => {
                format!("панель {panel} {}{revision}", outcome_text(outcome))
            }
            _ => format!("выполнено{revision}"),
        };
        let source = receipt.source.as_str();
        let _ = match &receipt.plugin {
            Some(plugin) => {
                let workbench = receipt.workbench.map_or_else(
                    || "без рабочей области".to_owned(),
                    |workbench| format!("рабочая область {workbench}"),
                );
                writeln!(
                    out,
                    "  {}: {} ({source}) {plugin}, {workbench}: {result}",
                    receipt.seq, receipt.action
                )
            }
            None => writeln!(
                out,
                "  {}: {} ({source}): {result}",
                receipt.seq, receipt.action
            ),
        };
    }
    out
}

fn rect_text(rect: &Rect) -> String {
    format!("{}×{} в ({}, {})", rect.width, rect.height, rect.x, rect.y)
}

fn yes_no(fact: bool) -> &'static str {
    if fact { "да" } else { "нет" }
}

fn source_text(source: AxisSource) -> &'static str {
    match source {
        AxisSource::System => "система",
        AxisSource::UiAppearance => "ui.appearance",
        AxisSource::Gallery => "галерея",
        AxisSource::Override => "ui.cell",
    }
}

fn window_text(window: &WindowView) -> String {
    let mut out = format!(
        "окно {} владельца PID {}, режим {}\n",
        window.window_number,
        window.pid,
        match window.mode {
            shellio_control::WindowMode::Normal => "normal",
            shellio_control::WindowMode::Gallery => "gallery",
        }
    );
    let _ = writeln!(out, "заголовок «{}»", window.title);
    let _ = writeln!(
        out,
        "кадр {}, масштаб {}, видимо — {}, key — {}",
        rect_text(&window.frame),
        window.backing_scale_factor,
        yes_no(window.visible),
        yes_no(window.key)
    );
    let _ = writeln!(
        out,
        "пространство цвета {}, sRGB — {}",
        window.color_space.name.as_deref().unwrap_or("нет"),
        yes_no(window.color_space.srgb)
    );
    let _ = writeln!(
        out,
        "оформление {}: светлота {} ({}), контраст {} ({}), движение {} ({})",
        window.appearance_choice.as_str(),
        lightness_text(window.lightness.value),
        source_text(window.lightness.source),
        contrast_text(window.contrast.value),
        source_text(window.contrast.source),
        motion_text(window.motion.value),
        source_text(window.motion.source),
    );
    for screen in &window.screens {
        let current = if window.screen == Some(screen.index) {
            ", окно здесь"
        } else {
            ""
        };
        let _ = writeln!(
            out,
            "экран {} «{}»: {}, масштаб {}, до {} кадров/с{current}",
            screen.index,
            screen.name,
            rect_text(&screen.frame),
            screen.backing_scale_factor,
            screen.maximum_frames_per_second
        );
    }
    let revision = window
        .mirror_revision
        .map_or_else(|| "нет".to_owned(), |revision| revision.to_string());
    let _ = writeln!(
        out,
        "ревизия зеркала {revision}, поколение {}",
        window.generation
    );
    out
}

fn component_text(component: &ComponentView) -> String {
    let mut facts = vec![rect_text(&component.frame)];
    if let Some(role) = &component.role {
        facts.push(format!("роль {role}"));
    }
    if let Some(label) = &component.label {
        facts.push(format!("«{label}»"));
    }
    if let Some(ck) = &component.ck {
        facts.push(format!("ключ {ck}"));
    }
    if component.truncated == Some(true) {
        facts.push("текст обрезан".to_owned());
    }
    if let Some(font) = component
        .style
        .as_ref()
        .and_then(|style| style.font.as_ref())
    {
        facts.push(format!("шрифт {} {}", font.face, font.size));
    }
    if let Some(enabled) = component.enabled {
        facts.push(format!("доступен — {}", yes_no(enabled)));
    }
    facts.push(format!("видим — {}", yes_no(component.visible)));
    if component.first_responder {
        facts.push("first responder".to_owned());
    }
    if component.animation_keys > 0 {
        facts.push(format!("анимаций {}", component.animation_keys));
    }
    match &component.state {
        Some(ComponentState::Chrome(chrome)) => facts.push(format!(
            "воркспейс «{}», оформление {}",
            chrome.workspace_text,
            chrome.appearance_choice.as_str()
        )),
        Some(ComponentState::TextField(field)) => facts.push(format!(
            "значение «{}», ввод — {}",
            field.value,
            yes_no(field.editing)
        )),
        Some(ComponentState::Menu(menu)) => facts.push(format!(
            "меню открыто — {}, пунктов {}",
            yes_no(menu.open),
            menu.items.len()
        )),
        Some(ComponentState::TabStrip(strip)) => facts.push(format!(
            "вкладок {}, выбрана {}, режим {}, нагрузка в строке — {}, смещение {}",
            strip.tabs.len(),
            strip.selected.as_deref().unwrap_or("нет"),
            strip.regime.map_or("нет", |regime| match regime {
                TabRegime::Visible => "visible",
                TabRegime::InMenu => "in_menu",
                TabRegime::Scroll => "scroll",
            }),
            yes_no(strip.payload_visible),
            strip.offset
        )),
        Some(ComponentState::VirtualList(list)) => facts.push(format!(
            "строк {}, выбрана {}, видимы {}…{}, смещение {}, создано видов строк {}",
            list.count,
            list.selected.as_deref().unwrap_or("нет"),
            list.visible.first().map_or("нет", |row| row.id.as_str()),
            list.visible.last().map_or("нет", |row| row.id.as_str()),
            list.offset,
            list.row_views_created
        )),
        Some(ComponentState::TreeList(tree)) => facts.push(format!(
            "строк {}, узлов {}, выбран {}, раскрыты {}, смещение {}, создано видов строк {}",
            tree.count,
            tree.nodes,
            tree.selected.as_deref().unwrap_or("нет"),
            if tree.expanded.is_empty() {
                "нет".to_owned()
            } else {
                tree.expanded.join(" ")
            },
            tree.offset,
            tree.row_views_created
        )),
        Some(ComponentState::Part(part)) => facts.push(format!(
            "часть {} {}, вкладок {}, активна {}, показана — {}, фокус — {}, развёрнута — {}",
            zone_text(part.zone),
            part.index,
            part.tabs.len(),
            part.active,
            yes_no(part.shown),
            yes_no(part.focused),
            yes_no(part.zoomed)
        )),
        Some(ComponentState::Panel(panel)) => facts.push(format!(
            "панель {} ({}), вид {}, показана — {}, зона {}, класс {}, статус {}, активна — {}, \
             фокус — {}, вариант {}",
            panel.panel,
            panel.plugin,
            panel.component,
            yes_no(panel.shown),
            panel.zone.map_or("нет", zone_text),
            panel.size.map_or("нет", |size| match size {
                SizeClass::Narrow => "narrow",
                SizeClass::Regular => "regular",
                SizeClass::Wide => "wide",
                SizeClass::ShortBar => "short_bar",
            }),
            panel.status.as_str(),
            yes_no(panel.active),
            yes_no(panel.focused),
            panel.variant.as_deref().unwrap_or("нет")
        )),
        None => {}
    }
    format!(
        "{} ({}): {}\n",
        component.id,
        component.class,
        facts.join(", ")
    )
}

fn inspect_text(inspect: &InspectView) -> String {
    let mut out = format!(
        "окно {}, содержимое {}×{}, поколение {}\n",
        inspect.window_number, inspect.content.width, inspect.content.height, inspect.generation
    );
    for component in &inspect.components {
        out.push_str(&component_text(component));
    }
    if inspect.materials.is_empty() {
        out.push_str("материал: нет\n");
    }
    for material in &inspect.materials {
        let _ = writeln!(
            out,
            "материал {}: видим — {}, свой — {}",
            material.class,
            yes_no(material.visible),
            yes_no(material.own)
        );
    }
    if let Some(gallery) = &inspect.gallery {
        out.push_str(&gallery_text(gallery));
    }
    out
}

const fn lightness_text(value: Lightness) -> &'static str {
    match value {
        Lightness::Light => "светлая",
        Lightness::Dark => "тёмная",
    }
}

const fn contrast_text(value: Contrast) -> &'static str {
    match value {
        Contrast::Normal => "обычный",
        Contrast::Increased => "повышенный",
    }
}

const fn motion_text(value: Motion) -> &'static str {
    match value {
        Motion::Normal => "обычное",
        Motion::Reduced => "уменьшенное",
    }
}

const fn intent_text(value: IntentKind) -> &'static str {
    match value {
        IntentKind::Changed => "changed",
        IntentKind::Submitted => "submitted",
        IntentKind::Cancelled => "cancelled",
        IntentKind::Chosen => "chosen",
        IntentKind::Select => "select",
        IntentKind::Close => "close",
        IntentKind::Split => "split",
        IntentKind::Zoom => "zoom",
        IntentKind::Restore => "restore",
        IntentKind::Activate => "activate",
        IntentKind::Toggle => "toggle",
    }
}

fn gallery_text(gallery: &GalleryView) -> String {
    fn axis<T>(value: Option<T>, text: fn(T) -> &'static str) -> &'static str {
        value.map_or("системная", text)
    }
    let mut out = format!(
        "галерея: дерево {}, окно {}, светлота {}, контраст {}, движение {}, фокус {}, экран {}\n",
        gallery.tree,
        gallery.window,
        axis(gallery.appearance, lightness_text),
        axis(gallery.contrast, contrast_text),
        axis(gallery.motion, motion_text),
        gallery.focus.as_str(),
        gallery
            .screen
            .map_or_else(|| "не задан".to_owned(), |screen| screen.to_string()),
    );
    for record in &gallery.intents {
        let _ = writeln!(
            out,
            "намерение {} {}{}, поколение {}",
            record.component,
            intent_text(record.intent),
            record
                .value
                .as_ref()
                .map_or_else(String::new, |value| format!(" «{value}»")),
            record.generation
        );
    }
    out
}

fn stats_text(stats: &StatsView) -> String {
    let mut out = format!("PID {}, поколение {}\n", stats.pid, stats.generation);
    match &stats.run_loop {
        Some(run_loop) => {
            let p95 = run_loop
                .p95_ms
                .map_or_else(|| "нет".to_owned(), |p95| format!("{p95:.2} мс"));
            let _ = writeln!(
                out,
                "цикл: итераций {}, пробуждений {}, проходов отображения {}, отрисовок {}, \
                 сверх бюджета {}, максимум {:.2} мс, p95 {p95}",
                run_loop.iterations,
                run_loop.wakeups,
                run_loop.display_passes,
                run_loop.draws,
                run_loop.over_budget,
                run_loop.max_ms
            );
        }
        None => out.push_str("цикл: монитор не поставлен\n"),
    }
    match &stats.startup {
        Some(startup) => {
            let _ = writeln!(
                out,
                "старт: отрисовка {:.1} мс, key {:.1} мс, окно key с первым снимком {:.1} мс",
                startup.first_draw_ms, startup.key_ms, startup.key_draw_ms
            );
        }
        None => out.push_str("старт: не наблюдался\n"),
    }
    for instance in &stats.instances {
        let _ = writeln!(
            out,
            "{} {}: создано {}, освобождено {}, живо {}",
            match instance.kind {
                shellio_control::InstanceKind::Class => "класс",
                shellio_control::InstanceKind::Owner => "владелец",
            },
            instance.name,
            instance.created,
            instance.released,
            instance.live
        );
    }
    for body in &stats.bodies_built {
        let _ = writeln!(out, "панель {}: построено тел {}", body.panel, body.built);
    }
    let late: u64 = stats.late_callbacks.iter().map(|late| late.count).sum();
    let _ = writeln!(out, "поздних обратных вызовов: {late}");
    out
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::str::FromStr;
    use std::sync::Arc;

    use shellio_control::{RefuseReason, RefusedArg, RequestId};
    use shellio_core::{
        Diagnostic, Panel, PanelId, PanelStatus, PluginId, ProjectId, ProjectRootId, Revision,
        RootKey, Snapshot, State, Title, TreeProject, TreeRoot, TreeWorkbench, TreeWorkspace,
        UnixMillis, WorkbenchId, WorkbenchLayout, WorkspaceId,
    };

    use super::*;

    fn out(frame: &Response) -> String {
        match text(frame) {
            Text::Out(text) => text,
            Text::Err(text) => panic!("ожидался вывод, получена ошибка {text}"),
        }
    }

    fn sample_tree(workspace: WorkspaceId, workbench: WorkbenchId) -> TreeView {
        TreeView {
            active_workspace: Some(workspace),
            route: Some(Route::Workbench(workbench)),
            workspaces: vec![TreeWorkspace {
                id: workspace,
                title: Title::new("Дом").unwrap(),
                last_workbench: Some(workbench),
                projects: vec![TreeProject {
                    id: ProjectId::from_str("00000000-0000-0000-0000-000000000002").unwrap(),
                    title: Title::new("Приложение").unwrap(),
                    list_position: Some(0),
                    last_opened_at: None,
                    roots: vec![TreeRoot {
                        id: ProjectRootId::from_str("00000000-0000-0000-0000-000000000003")
                            .unwrap(),
                        path: PathBuf::from("/src/app"),
                        kind: RootKind::Folder,
                        key: RootKey::new("/src/app"),
                    }],
                    workbenches: vec![TreeWorkbench {
                        id: workbench,
                        title: Title::new("Основная").unwrap(),
                    }],
                }],
            }],
        }
    }

    #[test]
    fn keymap_text_lists_rows_and_diagnostics() {
        use shellio_control::{KeymapDiagnostic, KeymapRow, KeymapSource};

        let view = KeymapView {
            version: 2,
            leader: "Space".into(),
            bindings: vec![
                KeymapRow {
                    context: "navigation".into(),
                    sequence: "Ctrl-w v".into(),
                    action: "layout.split".into(),
                    args: Some(serde_json::json!({"axis": "row"})),
                    source: KeymapSource::Default,
                },
                KeymapRow {
                    context: "global".into(),
                    sequence: "Cmd-Alt-b".into(),
                    action: "layout.toggle.bottom".into(),
                    args: None,
                    source: KeymapSource::File,
                },
            ],
            diagnostics: vec![KeymapDiagnostic {
                line: Some(4),
                kind: "unknown_action".into(),
                reason: "неизвестное действие «x»".into(),
                message: "keymap.toml, строка 4: неизвестное действие «x» — привязка отключена"
                    .into(),
            }],
        };
        let expected = "keymap версия 2, лидер Space, строк 2, диагностика 1\n  \
                        navigation  Ctrl-w v  layout.split {\"axis\":\"row\"}  [default]\n  \
                        global  Cmd-Alt-b  layout.toggle.bottom  [file]\n  \
                        ! keymap.toml, строка 4: неизвестное действие «x» — привязка отключена";
        let reply = Response::Ok {
            id: RequestId(2),
            value: OkValue::Keymap(view.clone()),
        };
        assert_eq!(out(&reply), expected);
        let frame = Response::Keymap {
            id: RequestId(2),
            keymap: view,
        };
        assert_eq!(out(&frame), expected);
    }

    #[test]
    fn tree_text_hides_window_outside_scope() {
        let workspace = WorkspaceId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
        let workbench = WorkbenchId::from_str("00000000-0000-0000-0000-000000000004").unwrap();
        let reply = |tree: TreeView| Response::Ok {
            id: RequestId(2),
            value: OkValue::Reply(Reply {
                revision: Revision(5),
                value: ReplyValue::Tree(tree),
            }),
        };
        let mut tree = sample_tree(workspace, workbench);
        let shown = out(&reply(tree.clone()));
        assert!(
            shown.contains(&format!(
                "окно: воркспейс {workspace}, рабочая область {workbench}"
            )),
            "{shown}"
        );
        assert!(shown.contains("корень /src/app (папка)"), "{shown}");
        assert!(
            shown.contains(&format!("рабочая область {workbench} «Основная»")),
            "{shown}"
        );

        tree.active_workspace = None;
        let shown = out(&reply(tree.clone()));
        assert!(shown.contains("окно: активного воркспейса нет"), "{shown}");

        tree.route = None;
        let shown = out(&reply(tree));
        assert!(shown.contains("окно: вне области"), "{shown}");
    }

    #[test]
    fn stream_command_and_events_frames_have_text() {
        let id = RequestId(2);
        let workspace = WorkspaceId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
        let project = ProjectId::from_str("00000000-0000-0000-0000-000000000002").unwrap();
        let root = ProjectRootId::from_str("00000000-0000-0000-0000-000000000003").unwrap();
        let workbench = WorkbenchId::from_str("00000000-0000-0000-0000-000000000004").unwrap();
        let snapshot = Snapshot {
            revision: Revision(7),
            state: Arc::new(State::default()),
        };
        assert_eq!(
            out(&Response::Snapshot {
                id,
                snapshot: snapshot.clone()
            }),
            "снимок: ревизия 7\n"
        );
        assert_eq!(
            out(&Response::Reset {
                id,
                snapshot: snapshot.clone()
            }),
            "сброс: подписка отстала, снимок на ревизии 7\n"
        );

        let event = Event {
            revision: Revision(8),
            changes: vec![
                Change::WorkspaceRenamed {
                    workspace,
                    title: Title::new("Бета").unwrap(),
                },
                Change::WindowSet {
                    active: workspace,
                    route: Route::Workbench(workbench),
                },
            ],
        };
        let event_line = format!(
            "ревизия 8: workspace_renamed {workspace} «Бета»; window_set {workspace}, рабочая область {workbench}\n"
        );
        assert_eq!(
            out(&Response::Event {
                id,
                event: event.clone()
            }),
            event_line
        );

        let panel = PanelId::from_str("00000000-0000-0000-0000-000000000005").unwrap();
        let layout_event = Event {
            revision: Revision(9),
            changes: vec![
                Change::PanelAdd {
                    panel: Panel {
                        id: panel,
                        workbench,
                        plugin: PluginId::new("terminal").unwrap(),
                        title: Title::new("Терминал").unwrap(),
                        created_at: UnixMillis(1),
                    },
                },
                Change::LayoutSet {
                    workbench,
                    layout: WorkbenchLayout::default_for(panel, panel),
                },
                Change::PanelRemove { panel },
            ],
        };
        assert_eq!(
            out(&Response::Event {
                id,
                event: layout_event
            }),
            format!(
                "ревизия 9: panel_add {panel} terminal «Терминал» в {workbench}; \
                 layout_set {workbench}; panel_remove {panel}\n"
            )
        );

        // Данные панели в текст не идут: их читает `--json`.
        let panel_event = Event {
            revision: Revision(10),
            changes: vec![
                Change::PanelData {
                    panel,
                    version: 2,
                    data: r#"{"key":"c"}"#.to_owned(),
                },
                Change::PanelStatus {
                    panel,
                    status: PanelStatus::StateNewer,
                },
                Change::PanelRehome { panel, workbench },
                Change::PanelOpenedFrom {
                    panel,
                    opened_from: Some(panel),
                },
                Change::PanelOpenedFrom {
                    panel,
                    opened_from: None,
                },
            ],
        };
        assert_eq!(
            out(&Response::Event {
                id,
                event: panel_event
            }),
            format!(
                "ревизия 10: panel_data {panel} версии 2; panel_status {panel} state_newer; \
                 panel_rehome {panel} в {workbench}; panel_opened_from {panel} {panel}; \
                 panel_opened_from {panel} нет\n"
            )
        );

        let command = |value| Response::Ok {
            id,
            value: OkValue::Reply(Reply {
                revision: Revision(9),
                value: ReplyValue::Command(value),
            }),
        };
        assert_eq!(
            out(&command(CommandValue::Workspace { workspace })),
            format!("ревизия 9: воркспейс {workspace}\n")
        );
        assert_eq!(
            out(&command(CommandValue::Project { project, root })),
            format!("ревизия 9: проект {project}, корень {root}\n")
        );
        assert_eq!(
            out(&command(CommandValue::Workbench { workbench })),
            format!("ревизия 9: рабочая область {workbench}\n")
        );
        assert_eq!(out(&command(CommandValue::None)), "ревизия 9: готово\n");
        assert_eq!(
            out(&command(CommandValue::Panel { panel })),
            format!("ревизия 9: панель {panel}\n")
        );
        for (outcome, said) in [
            (OpenOutcome::Created, "создана"),
            (OpenOutcome::Focused, "в фокусе"),
            (OpenOutcome::Moved, "переехала"),
            (OpenOutcome::Routed, "окно перешло к ней"),
        ] {
            assert_eq!(
                out(&command(CommandValue::Opened {
                    panel,
                    workbench,
                    outcome
                })),
                format!("ревизия 9: панель {panel} в рабочей области {workbench}: {said}\n")
            );
        }

        let since = |since| Response::Ok {
            id,
            value: OkValue::EventsSince(since),
        };
        assert_eq!(
            out(&since(EventsSince::Events(Vec::new()))),
            "событий нет\n"
        );
        assert_eq!(out(&since(EventsSince::Events(vec![event]))), event_line);
        assert_eq!(
            out(&since(EventsSince::Reset(snapshot))),
            "сброс: курсор вне буфера, снимок на ревизии 7\n"
        );
    }

    #[test]
    fn status_text_lists_diagnostics() {
        let status = |diagnostics| Response::Ok {
            id: RequestId(2),
            value: OkValue::Reply(Reply {
                revision: Revision(3),
                value: ReplyValue::Status(StatusView {
                    schema_version: 1,
                    state_db: PathBuf::from("/state/state.db"),
                    diagnostics,
                }),
            }),
        };
        let shown = out(&status(Vec::new()));
        assert_eq!(
            shown,
            "ревизия 3\nсхема 1\nбаза /state/state.db\nдиагностика: нет\n"
        );
        let shown = out(&status(vec![Diagnostic {
            table: "projects".into(),
            rowid: 4,
            reason: "висячая ссылка".into(),
        }]));
        assert!(
            shown.contains("диагностика: 1\n  projects#4: висячая ссылка\n"),
            "{shown}"
        );
    }

    #[test]
    fn refusals_go_to_stderr() {
        let frame = Response::Error {
            id: Some(RequestId(2)),
            error: ControlError::owner_leaving(),
        };
        assert!(matches!(text(&frame), Text::Err(text) if text.starts_with("owner_leaving: ")));
    }

    #[test]
    fn ui_mode_has_text() {
        use shellio_control::{ExitChordState, PanelMode};

        let idle = ModeView {
            pid: 42,
            panel: None,
            mode: PanelMode::Navigation,
            receiver: None,
            exit: ExitChordState::Idle,
            exit_receiver: None,
        };
        assert_eq!(
            ui_text(&UiReply::Mode(idle.clone())),
            "режим: navigation, панель нет, получатель ввода: нет, аккорд выхода: idle, PID 42\n"
        );
        let id = "00000000-0000-0000-0000-000000000003";
        let panel = serde_json::from_value(serde_json::json!(id)).unwrap();
        let armed = ModeView {
            panel: Some(panel),
            mode: PanelMode::Input,
            receiver: Some(ModeReceiver::Panel(panel)),
            exit: ExitChordState::Armed,
            exit_receiver: Some(ModeReceiver::Panel(panel)),
            ..idle
        };
        assert_eq!(
            ui_text(&UiReply::Mode(armed)),
            format!(
                "режим: input, панель {id}, получатель ввода: панель, аккорд выхода: armed, \
                 панель {id}, PID 42\n"
            )
        );
    }

    #[test]
    fn ui_accepted_and_reads_have_text() {
        use shellio_control::{Accepted, InspectView, Size, StatsView};

        let accepted = UiReply::Accepted(Accepted {
            command: "app.quit".into(),
            pid: 42,
            generation: 3,
        });
        assert_eq!(
            ui_text(&accepted),
            "принято: app.quit, PID 42, поколение 3\n"
        );
        let empty = InspectView {
            window_number: 7,
            content: Size {
                width: 1180.0,
                height: 720.0,
            },
            generation: 3,
            components: Vec::new(),
            materials: Vec::new(),
            gallery: None,
        };
        let shown = ui_text(&UiReply::Inspect(Box::new(empty.clone())));
        assert!(shown.starts_with("окно 7, содержимое 1180×720"), "{shown}");
        assert!(shown.contains("материал: нет"), "{shown}");
        assert!(!shown.contains("галерея"), "{shown}");
        let gallery = InspectView {
            gallery: Some(GalleryView {
                tree: 2,
                window: 1,
                appearance: None,
                contrast: Some(shellio_control::Contrast::Increased),
                motion: None,
                focus: shellio_control::GalleryFocus::Field,
                screen: None,
                intents: vec![shellio_control::IntentRecord {
                    generation: 4,
                    component: "gallery.menu".into(),
                    intent: IntentKind::Chosen,
                    value: Some("copy".into()),
                }],
            }),
            ..empty
        };
        let shown = ui_text(&UiReply::Inspect(Box::new(gallery)));
        assert!(
            shown.contains("галерея: дерево 2, окно 1, светлота системная, контраст повышенный"),
            "{shown}"
        );
        assert!(
            shown.contains("намерение gallery.menu chosen «copy», поколение 4"),
            "{shown}"
        );
        let stats = StatsView {
            pid: 42,
            generation: 3,
            instances: Vec::new(),
            late_callbacks: Vec::new(),
            run_loop: None,
            startup: None,
            animations: Vec::new(),
            row_views: Vec::new(),
            gallery_trees: Vec::new(),
            bodies_built: Vec::new(),
            key_latency: None,
        };
        let shown = ui_text(&UiReply::Stats(Box::new(stats)));
        assert!(shown.contains("монитор не поставлен"), "{shown}");
        assert!(shown.contains("старт: не наблюдался"), "{shown}");
    }

    #[test]
    fn ui_receipts_text_tells_success_from_refusal() {
        use shellio_control::{ReceiptSource, ReceiptView};
        use shellio_core::Revision;

        let empty = ui_text(&UiReply::Receipts(ReceiptsView::default()));
        assert_eq!(empty, "квитанций 0, последняя 0\n");

        let panel: shellio_core::PanelId = "00000000-0000-0000-0000-000000000003".parse().unwrap();
        let workbench: shellio_core::WorkbenchId =
            "00000000-0000-0000-0000-000000000002".parse().unwrap();
        let record = |seq, plugin: &str| ReceiptView {
            seq,
            action: "panel.open".into(),
            source: ReceiptSource::Menu,
            workbench: Some(workbench),
            plugin: Some(plugin.into()),
            outcome: None,
            panel: None,
            revision: None,
            error: None,
        };
        let view = ReceiptsView {
            last: 19,
            receipts: vec![
                ReceiptView {
                    outcome: Some(OpenOutcome::Created),
                    panel: Some(panel),
                    revision: Some(Revision(41)),
                    ..record(17, "demo-list")
                },
                ReceiptView {
                    error: Some("busy".into()),
                    ..record(18, "terminal")
                },
                ReceiptView {
                    workbench: None,
                    error: Some("invalid_argument".into()),
                    ..record(19, "demo-native")
                },
                ReceiptView {
                    action: "layout.toggle.navigator".into(),
                    source: ReceiptSource::Key,
                    workbench: None,
                    plugin: None,
                    revision: Some(Revision(42)),
                    ..record(20, "")
                },
                ReceiptView {
                    action: "ui.appearance.cycle".into(),
                    source: ReceiptSource::Button,
                    workbench: None,
                    plugin: None,
                    ..record(21, "")
                },
            ],
        };
        assert_eq!(
            ui_text(&UiReply::Receipts(view)),
            format!(
                "квитанций 5, последняя 19\n\
                 \x20 17: panel.open (menu) demo-list, рабочая область {workbench}: панель {panel} создана, ревизия 41\n\
                 \x20 18: panel.open (menu) terminal, рабочая область {workbench}: отказ busy\n\
                 \x20 19: panel.open (menu) demo-native, без рабочей области: отказ invalid_argument\n\
                 \x20 20: layout.toggle.navigator (key): выполнено, ревизия 42\n\
                 \x20 21: ui.appearance.cycle (button): выполнено\n"
            )
        );
        for (outcome, said) in [
            (OpenOutcome::Focused, "в фокусе"),
            (OpenOutcome::Moved, "переехала"),
            (OpenOutcome::Routed, "окно перешло к ней"),
        ] {
            let shown = ui_text(&UiReply::Receipts(ReceiptsView {
                last: 1,
                receipts: vec![ReceiptView {
                    outcome: Some(outcome),
                    panel: Some(panel),
                    revision: Some(Revision(2)),
                    ..record(1, "demo-list")
                }],
            }));
            assert!(
                shown.ends_with(&format!("панель {panel} {said}, ревизия 2\n")),
                "{shown}"
            );
        }
    }

    #[test]
    fn receipt_text_names_refused_folder() {
        let receipt = LaunchReceipt {
            activation: Activation::NoUi,
            refused: vec![RefusedArg {
                arg: "/tmp".into(),
                reason: RefuseReason::FolderOpenUnsupported,
            }],
        };
        let shown = receipt_text(&receipt);
        assert!(shown.contains("/tmp: папка не открыта"), "{shown}");
    }
}
