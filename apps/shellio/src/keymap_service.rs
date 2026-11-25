//! Служба keymap владельца (ADR-0007 и ADR-0018, «Уточнено в P1.6 (планирование)»): единственный
//! владелец действующей таблицы клавиш.
//!
//! Наблюдение за каталогом конфигурации и `keymap.reload` идут одной очередью, поэтому два разбора
//! не пересекаются. Файл читается и разбирается вне главного потока, служба публикует целый
//! неизменяемый снимок через `watch`. Доменную ревизию таблица не трогает.

use std::fs;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::thread;
use std::time::Duration;

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde_json::Value;
use shellio_control::{KeymapDiagnostic, KeymapHandler, KeymapRow, KeymapSource, KeymapView};
use shellio_keymap::{
    ActionError, Context, Diagnostic, KeyDefault, Keymap, PanelKeys, Source as KeySource,
};
use shellio_plugin_api::Registry;
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::spawn_blocking;

/// Имя файла в каталоге конфигурации профиля.
pub const KEYMAP_FILE: &str = "keymap.toml";

/// Объединение событий наблюдения: правка редактора даёт несколько событий подряд.
pub const WATCH_DEBOUNCE: Duration = Duration::from_millis(100);

/// Снимок службы: скомпилированная таблица и её представление для управляющего API.
#[derive(Clone, Debug, PartialEq)]
pub struct KeymapSnapshot {
    pub keymap: Keymap,
    pub view: KeymapView,
}

impl KeymapSnapshot {
    fn of(keymap: Keymap) -> Self {
        let view = view_of(&keymap);
        Self { keymap, view }
    }
}

impl AsRef<KeymapView> for KeymapSnapshot {
    fn as_ref(&self) -> &KeymapView {
        &self.view
    }
}

fn diagnostic_view(diagnostic: &Diagnostic) -> KeymapDiagnostic {
    let kind = serde_json::to_value(diagnostic.kind)
        .ok()
        .and_then(|kind| kind.as_str().map(str::to_owned))
        .unwrap_or_default();
    KeymapDiagnostic {
        line: diagnostic.line,
        kind,
        reason: diagnostic.reason.clone(),
        message: diagnostic.message(),
    }
}

fn view_of(keymap: &Keymap) -> KeymapView {
    KeymapView {
        version: keymap.version(),
        leader: keymap.leader().to_string(),
        bindings: keymap
            .bindings()
            .iter()
            .map(|binding| KeymapRow {
                context: binding.context.to_string(),
                sequence: binding.sequence.to_string(),
                action: binding.action.clone(),
                args: binding.args.clone(),
                source: match binding.source {
                    KeySource::Default => KeymapSource::Default,
                    KeySource::File => KeymapSource::File,
                },
            })
            .collect(),
        diagnostics: keymap.diagnostics().iter().map(diagnostic_view).collect(),
    }
}

/// Какие `args` принимает действие хоста.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HostArgs {
    None,
    /// Необязательная ось деления: `{axis: "row" | "stack"}`.
    SplitAxis,
    /// Обязательный номер вкладки с единицы: `{index}`.
    TabIndex,
    /// Необязательный текст фильтра: `{text}`.
    FilterText,
    /// Обязательная цель: `{panel}`.
    PanelTarget,
}

/// Действия хоста с именем keymap (ADR-0007 и ADR-0010, «Уточнено в P1.6 (планирование)»). Имена
/// `panel.open.<plugin>` строятся по реестру.
const HOST_ACTIONS: [(&str, HostArgs); 55] = [
    ("app.quit", HostArgs::None),
    ("history.back", HostArgs::None),
    ("history.forward", HostArgs::None),
    ("keymap.reload", HostArgs::None),
    ("layout.equal", HostArgs::None),
    ("layout.grow", HostArgs::None),
    ("layout.narrower", HostArgs::None),
    ("layout.only", HostArgs::None),
    ("layout.shrink", HostArgs::None),
    ("layout.split", HostArgs::SplitAxis),
    ("layout.toggle.bottom", HostArgs::None),
    ("layout.toggle.navigator", HostArgs::None),
    ("layout.toggle.plate", HostArgs::None),
    ("layout.toggle.right", HostArgs::None),
    ("layout.toggle.side", HostArgs::None),
    ("layout.wider", HostArgs::None),
    ("layout.zoom", HostArgs::None),
    ("list.activate", HostArgs::None),
    ("list.bottom", HostArgs::None),
    ("list.collapse", HostArgs::None),
    ("list.down", HostArgs::None),
    ("list.expand", HostArgs::None),
    ("list.page_down", HostArgs::None),
    ("list.page_up", HostArgs::None),
    ("list.top", HostArgs::None),
    ("list.up", HostArgs::None),
    ("mode.input", HostArgs::None),
    ("mode.navigation", HostArgs::None),
    ("mode.toggle", HostArgs::None),
    ("panel.close", HostArgs::None),
    ("panel.filter", HostArgs::FilterText),
    ("panel.focus.down", HostArgs::None),
    ("panel.focus.left", HostArgs::None),
    ("panel.focus.right", HostArgs::None),
    ("panel.focus.up", HostArgs::None),
    ("panel.move.down", HostArgs::None),
    ("panel.move.left", HostArgs::None),
    ("panel.move.right", HostArgs::None),
    ("panel.move.up", HostArgs::None),
    ("panel.move_part.next", HostArgs::None),
    ("panel.move_part.prev", HostArgs::None),
    ("panel.select", HostArgs::PanelTarget),
    ("tab.next", HostArgs::None),
    ("tab.prev", HostArgs::None),
    ("tab.select", HostArgs::TabIndex),
    ("ui.appearance.cycle", HostArgs::None),
    ("ui.appearance.dark", HostArgs::None),
    ("ui.appearance.light", HostArgs::None),
    ("ui.appearance.system", HostArgs::None),
    ("window.close", HostArgs::None),
    // Имена ниже заняты хостом, но клавишей не вызываются: привязка к ним — неверная строка
    // с понятной причиной, а не «неизвестное действие».
    ("gallery.input.reset", HostArgs::None),
    ("keymap.subscribe", HostArgs::None),
    ("query.keymap", HostArgs::None),
    ("ui.history", HostArgs::None),
    ("ui.mode", HostArgs::None),
];

/// Имена [`HOST_ACTIONS`], которые клавишей не вызываются.
const NOT_BINDABLE: [&str; 5] = [
    "gallery.input.reset",
    "keymap.subscribe",
    "query.keymap",
    "ui.history",
    "ui.mode",
];

const PANEL_OPEN: &str = "panel.open.";

fn only_keys(args: &Value, allowed: &[&str]) -> Result<(), String> {
    let Some(object) = args.as_object() else {
        return Err("аргументы — таблица".to_owned());
    };
    match object.keys().find(|key| !allowed.contains(&key.as_str())) {
        Some(extra) => Err(format!("лишнее поле «{extra}»")),
        None => Ok(()),
    }
}

fn host_args(rule: HostArgs, args: Option<&Value>) -> Result<(), String> {
    match (rule, args) {
        (HostArgs::None | HostArgs::SplitAxis | HostArgs::FilterText, None) => Ok(()),
        (HostArgs::None, Some(_)) => Err("действие не принимает аргументы".to_owned()),
        (HostArgs::SplitAxis, Some(args)) => {
            only_keys(args, &["axis"])?;
            match args.get("axis").and_then(Value::as_str) {
                Some("row" | "stack") => Ok(()),
                _ => Err("axis — row или stack".to_owned()),
            }
        }
        (HostArgs::FilterText, Some(args)) => {
            only_keys(args, &["text"])?;
            match args.get("text") {
                None | Some(Value::String(_)) => Ok(()),
                Some(_) => Err("text — строка".to_owned()),
            }
        }
        (HostArgs::TabIndex, args) => {
            let args = args.ok_or_else(|| "нужен index".to_owned())?;
            only_keys(args, &["index"])?;
            match args.get("index").and_then(Value::as_u64) {
                Some(index) if index >= 1 => Ok(()),
                _ => Err("index — номер вкладки с единицы".to_owned()),
            }
        }
        (HostArgs::PanelTarget, args) => {
            let args = args.ok_or_else(|| "нужен panel".to_owned())?;
            only_keys(args, &["panel"])?;
            match args.get("panel") {
                Some(Value::String(_)) => Ok(()),
                _ => Err("panel — id панели".to_owned()),
            }
        }
    }
}

/// Что служба знает о действиях: плагины реестра с их действиями и клавишами по умолчанию.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Actions {
    panels: Vec<PanelKeys>,
    /// Плагин и полные имена его действий.
    commands: Vec<(String, Vec<String>)>,
}

impl Actions {
    pub fn of(registry: &Registry) -> Self {
        let mut panels = Vec::new();
        let mut commands = Vec::new();
        for plugin in registry.iter() {
            let manifest = plugin.manifest();
            let id = plugin.id().as_str().to_owned();
            panels.push(PanelKeys {
                plugin: id.clone(),
                // Признак манифеста `list_keys` вводит задача намерений панели: до неё движения
                // списка ни одному контексту не подключаются.
                list_keys: false,
                defaults: manifest
                    .keymap
                    .iter()
                    .map(|key| KeyDefault {
                        sequence: key.key.to_owned(),
                        action: key.action.to_owned(),
                    })
                    .collect(),
            });
            let names = manifest
                .commands
                .iter()
                .map(|spec| spec.name.to_owned())
                .collect();
            commands.push((id, names));
        }
        Self { panels, commands }
    }

    pub fn panels(&self) -> &[PanelKeys] {
        &self.panels
    }

    /// Проверка строки файла: имя действия и `args` по командам хоста и реестру. Действие плагина
    /// привязывается только в контексте его панели: клавиша обращается к виду панели с фокусом.
    /// `args` действия плагина проверяет его модель при исполнении, здесь — только имя.
    pub fn check(
        &self,
        context: &Context,
        action: &str,
        args: Option<&Value>,
    ) -> Result<(), ActionError> {
        if NOT_BINDABLE.contains(&action) {
            return Err(ActionError::Args(
                "действие клавишей не вызывается".to_owned(),
            ));
        }
        if let Some((_, rule)) = HOST_ACTIONS.iter().find(|(name, _)| *name == action) {
            return host_args(*rule, args).map_err(ActionError::Args);
        }
        if let Some(plugin) = action.strip_prefix(PANEL_OPEN) {
            if !self.commands.iter().any(|(id, _)| id == plugin) {
                return Err(ActionError::Unknown);
            }
            return host_args(HostArgs::None, args).map_err(ActionError::Args);
        }
        let owner = self
            .commands
            .iter()
            .find(|(_, names)| names.iter().any(|name| name == action));
        match (owner, context) {
            (Some((plugin, _)), Context::Panel(panel)) if plugin == panel => Ok(()),
            (Some((plugin, _)), _) => Err(ActionError::Args(format!(
                "действие плагина привязывается в контексте panel.{plugin}"
            ))),
            (None, _) => Err(ActionError::Unknown),
        }
    }
}

/// Содержимое `keymap.toml`: текст, отсутствие файла или причина, по которой он не прочитан.
fn read(path: &Path) -> Result<Option<String>, String> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        // Каталога конфигурации ещё нет: то же, что нет файла.
        Err(error) if error.kind() == io::ErrorKind::NotADirectory => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

/// Следующая таблица по файлу `path`: синхронное чтение и разбор, вызывается вне главного потока.
pub fn load(current: &Keymap, path: &Path, actions: &Actions) -> Result<Keymap, String> {
    let file = match read(path) {
        Ok(file) => file,
        Err(reason) => return Ok(current.rejected(reason)),
    };
    let check = |context: &Context, action: &str, args: Option<&Value>| {
        actions.check(context, action, args)
    };
    current
        .apply(file.as_deref(), actions.panels(), &check)
        .map_err(|error| error.to_string())
}

/// Задание очереди службы: перечитать файл. Ответ получает таблицу после применения.
struct Reload(Option<oneshot::Sender<Arc<KeymapSnapshot>>>);

/// Порт службы для сервера управления и главного потока.
#[derive(Clone, Debug)]
pub struct KeymapPort {
    jobs: mpsc::UnboundedSender<Reload>,
    snapshots: watch::Receiver<Arc<KeymapSnapshot>>,
}

impl KeymapPort {
    /// Действующий снимок.
    pub fn current(&self) -> Arc<KeymapSnapshot> {
        Arc::clone(&self.snapshots.borrow())
    }

    /// Получатель снимков: текущий лежит в нём сразу.
    pub fn subscribe(&self) -> watch::Receiver<Arc<KeymapSnapshot>> {
        self.snapshots.clone()
    }

    /// Применить `keymap.toml` и вернуть снимок после применения.
    pub async fn apply_file(&self) -> Arc<KeymapSnapshot> {
        let (reply, applied) = oneshot::channel();
        if self.jobs.send(Reload(Some(reply))).is_err() {
            return self.current();
        }
        match applied.await {
            Ok(snapshot) => snapshot,
            // Служба остановлена: действует последний снимок.
            Err(_) => self.current(),
        }
    }
}

impl KeymapHandler for KeymapPort {
    type Snapshot = KeymapSnapshot;

    fn watch(&self) -> watch::Receiver<Arc<KeymapSnapshot>> {
        self.subscribe()
    }

    fn reload(&self) -> impl Future<Output = Arc<KeymapSnapshot>> + Send {
        self.apply_file()
    }
}

/// Сигнал потоку наблюдения.
enum Signal {
    Changed,
    Stop,
}

/// Служба keymap. Брошенная служба останавливает наблюдение; очередь живёт, пока жив порт.
#[derive(Debug)]
pub struct KeymapService {
    port: KeymapPort,
    stop: Option<std_mpsc::Sender<Signal>>,
}

impl KeymapService {
    /// Запустить службу на `runtime`: первичный снимок по файлу каталога `config_dir` готов до
    /// возврата. Отказ — привязки по умолчанию не собираются: это ошибка манифеста плагина.
    pub async fn start(
        runtime: &Handle,
        config_dir: PathBuf,
        actions: Actions,
        observe: bool,
    ) -> Result<Self, String> {
        let path = config_dir.join(KEYMAP_FILE);
        let actions = Arc::new(actions);
        let (jobs, queue) = mpsc::unbounded_channel();
        // Наблюдение ставится раньше первого чтения: правка между ними не теряется, а попадает в
        // очередь и перечитывается первым заданием.
        let stop = if observe {
            let (stop, installed) = observe_dir(config_dir, jobs.downgrade());
            let _ = spawn_blocking(move || installed.recv()).await;
            Some(stop)
        } else {
            None
        };
        let first = {
            let (path, actions) = (path.clone(), Arc::clone(&actions));
            spawn_blocking(move || {
                let defaults = Keymap::defaults(actions.panels()).map_err(|e| e.to_string())?;
                load(&defaults, &path, &actions)
            })
            .await
            .map_err(|error| format!("первый разбор keymap не выполнен: {error}"))??
        };
        let (publish, snapshots) = watch::channel(Arc::new(KeymapSnapshot::of(first)));
        runtime.spawn(work(queue, publish, path, actions));
        Ok(Self {
            port: KeymapPort { jobs, snapshots },
            stop,
        })
    }

    pub fn port(&self) -> KeymapPort {
        self.port.clone()
    }
}

impl Drop for KeymapService {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(Signal::Stop);
        }
    }
}

/// Очередь службы: задания по одному, скопившиеся к началу разбора получают один общий разбор.
async fn work(
    mut queue: mpsc::UnboundedReceiver<Reload>,
    publish: watch::Sender<Arc<KeymapSnapshot>>,
    path: PathBuf,
    actions: Arc<Actions>,
) {
    while let Some(Reload(reply)) = queue.recv().await {
        let mut replies: Vec<_> = reply.into_iter().collect();
        while let Ok(Reload(reply)) = queue.try_recv() {
            replies.extend(reply);
        }
        let current = Arc::clone(&publish.borrow());
        let (file, rules) = (path.clone(), Arc::clone(&actions));
        let loaded = spawn_blocking(move || load(&current.keymap, &file, &rules)).await;
        // Привязки по умолчанию собраны при старте и от файла не зависят: отказ здесь невозможен,
        // а паника разбора оставляет прежний снимок.
        if let Ok(Ok(keymap)) = loaded {
            publish.send_if_modified(|snapshot| {
                if snapshot.keymap == keymap {
                    return false;
                }
                *snapshot = Arc::new(KeymapSnapshot::of(keymap));
                true
            });
        }
        let applied = Arc::clone(&publish.borrow());
        for reply in replies {
            let _ = reply.send(Arc::clone(&applied));
        }
    }
}

/// Ближайший существующий каталог от `dir` вверх: каталога конфигурации может ещё не быть.
fn existing(dir: &Path) -> Option<&Path> {
    dir.ancestors().find(|path| path.is_dir())
}

/// Наблюдение FSEvents за каталогом конфигурации в своём потоке. Пока каталога нет, наблюдается
/// ближайший существующий предок, и появление каталога переносит наблюдение на него. Второй канал
/// закрывается, когда первое наблюдение поставлено или не состоялось.
fn observe_dir(
    config_dir: PathBuf,
    jobs: mpsc::WeakUnboundedSender<Reload>,
) -> (std_mpsc::Sender<Signal>, std_mpsc::Receiver<()>) {
    let (signal, signals) = std_mpsc::channel();
    let (installed, wait_installed) = std_mpsc::channel::<()>();
    let from_watcher = signal.clone();
    let spawned = thread::Builder::new()
        .name("shellio-keymap-watch".to_owned())
        .spawn(move || {
            let made = notify::recommended_watcher(move |_event: notify::Result<notify::Event>| {
                let _ = from_watcher.send(Signal::Changed);
            });
            // Без наблюдения таблица применяется командой `keymap.reload`.
            let Ok(mut watcher) = made else { return };
            let mut watched = retarget(&mut watcher, None, &config_dir);
            drop(installed);
            while let Ok(Signal::Changed) = signals.recv() {
                loop {
                    match signals.recv_timeout(WATCH_DEBOUNCE) {
                        Ok(Signal::Changed) => {}
                        Ok(Signal::Stop) | Err(std_mpsc::RecvTimeoutError::Disconnected) => return,
                        Err(std_mpsc::RecvTimeoutError::Timeout) => break,
                    }
                }
                watched = retarget(&mut watcher, watched, &config_dir);
                let Some(jobs) = jobs.upgrade() else { return };
                if jobs.send(Reload(None)).is_err() {
                    return;
                }
            }
        });
    drop(spawned);
    (signal, wait_installed)
}

/// Поставить наблюдение на ближайший существующий каталог, если он сменился.
fn retarget(
    watcher: &mut RecommendedWatcher,
    watched: Option<PathBuf>,
    config_dir: &Path,
) -> Option<PathBuf> {
    let target = existing(config_dir).map(Path::to_owned);
    if target == watched {
        return watched;
    }
    if let Some(old) = &watched {
        let _ = watcher.unwatch(old);
    }
    let target = target?;
    watcher
        .watch(&target, RecursiveMode::NonRecursive)
        .ok()
        .map(|()| target)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use shellio_store::Profile;
    use tempfile::TempDir;

    use super::*;
    use crate::plugins;

    /// Крайний срок ожидания наблюдения: ограничивает зависание, а не синхронизирует.
    const WATCH_GUARD: Duration = Duration::from_secs(30);

    fn actions() -> Actions {
        Actions::of(&plugins::registry().unwrap())
    }

    struct Fixture {
        _dir: TempDir,
        profile: Profile,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let profile = Profile::isolated(dir.path().canonicalize().unwrap().join("state"));
            Self { _dir: dir, profile }
        }

        fn file(&self) -> PathBuf {
            self.profile.config_dir().join(KEYMAP_FILE)
        }

        fn write(&self, text: &str) {
            fs::create_dir_all(self.profile.config_dir()).unwrap();
            fs::write(self.file(), text).unwrap();
        }

        async fn service(&self, observe: bool) -> KeymapService {
            KeymapService::start(
                &Handle::current(),
                self.profile.config_dir().to_owned(),
                actions(),
                observe,
            )
            .await
            .unwrap()
        }
    }

    fn row<'a>(view: &'a KeymapView, context: &str, sequence: &str) -> Option<&'a KeymapRow> {
        view.bindings
            .iter()
            .find(|row| row.context == context && row.sequence == sequence)
    }

    #[test]
    fn host_action_table_is_sorted_and_checks_args() {
        let bindable = &HOST_ACTIONS[..HOST_ACTIONS.len() - NOT_BINDABLE.len()];
        assert!(bindable.is_sorted_by(|a, b| a.0 < b.0));
        let actions = actions();
        let nav = Context::Navigation;
        let check = |action: &str, args: Option<Value>| actions.check(&nav, action, args.as_ref());

        // Каждая привязка по умолчанию проходит ту же проверку, что строка файла.
        let defaults = Keymap::defaults(actions.panels()).unwrap();
        for binding in defaults.bindings() {
            assert_eq!(
                actions.check(&binding.context, &binding.action, binding.args.as_ref()),
                Ok(()),
                "{} {}",
                binding.context,
                binding.sequence
            );
        }

        assert_eq!(check("layout.zoom", None), Ok(()));
        assert_eq!(check("keymap.reload", None), Ok(()));
        assert_eq!(check("layout.split", None), Ok(()));
        assert_eq!(
            check("layout.split", Some(json!({"axis": "stack"}))),
            Ok(())
        );
        assert_eq!(check("tab.select", Some(json!({"index": 3}))), Ok(()));
        assert_eq!(check("panel.filter", Some(json!({"text": "a"}))), Ok(()));
        assert_eq!(check("panel.open.demo-list", None), Ok(()));
        for (action, args) in [
            ("layout.zoom", Some(json!({}))),
            ("layout.split", Some(json!({"axis": "column"}))),
            ("layout.split", Some(json!({"axis": "row", "x": 1}))),
            ("tab.select", None),
            ("tab.select", Some(json!({"index": 0}))),
            ("panel.filter", Some(json!({"text": 1}))),
            ("panel.select", None),
            ("panel.open.demo-list", Some(json!({}))),
            ("query.keymap", None),
            ("ui.mode", None),
        ] {
            assert!(
                matches!(check(action, args.clone()), Err(ActionError::Args(_))),
                "{action} {args:?}"
            );
        }
        for action in ["palette.open", "panel.open.none", "demo-list.none", "x"] {
            assert_eq!(check(action, None), Err(ActionError::Unknown), "{action}");
        }
    }

    #[test]
    fn plugin_action_binds_only_in_its_panel_context() {
        let actions = actions();
        let own = Context::Panel("demo-list".to_owned());
        let args = json!({"open": true});
        assert_eq!(actions.check(&own, "demo-list.back", None), Ok(()));
        assert_eq!(
            actions.check(&own, "demo-list.details", Some(&args)),
            Ok(())
        );
        for context in [
            Context::Navigation,
            Context::Global,
            Context::Panel("demo-native".to_owned()),
        ] {
            assert!(matches!(
                actions.check(&context, "demo-list.back", None),
                Err(ActionError::Args(_))
            ));
        }
    }

    #[tokio::test]
    async fn missing_file_gives_defaults_without_diagnostics() {
        let fixture = Fixture::new();
        let service = fixture.service(false).await;
        let snapshot = service.port().current();
        let defaults = Keymap::defaults(actions().panels()).unwrap();

        assert_eq!(snapshot.keymap, defaults);
        assert_eq!(snapshot.view.version, 1);
        assert_eq!(snapshot.view.leader, "Space");
        assert!(snapshot.view.diagnostics.is_empty());
        assert_eq!(snapshot.view.bindings.len(), defaults.bindings().len());
        assert!(
            snapshot
                .view
                .bindings
                .iter()
                .all(|row| row.source == KeymapSource::Default)
        );
        let split = row(&snapshot.view, "navigation", "Ctrl-w v").unwrap();
        assert_eq!(split.action, "layout.split");
        assert_eq!(split.args, Some(json!({"axis": "row"})));
        assert!(
            !fixture.profile.config_dir().exists(),
            "служба каталог не создаёт"
        );
    }

    #[tokio::test]
    async fn reload_applies_an_edit_and_keeps_the_version_of_the_same_file() {
        let fixture = Fixture::new();
        let service = fixture.service(false).await;
        let port = service.port();
        let mut seen = port.subscribe();

        fixture.write("[navigation]\n\"Ctrl-w m\" = \"layout.zoom\"\n\"Ctrl-w z\" = false\n");
        let applied = port.apply_file().await;
        assert_eq!(applied.view.version, 2);
        assert!(applied.view.diagnostics.is_empty());
        let moved = row(&applied.view, "navigation", "Ctrl-w m").unwrap();
        assert_eq!(
            (moved.action.as_str(), moved.source),
            ("layout.zoom", KeymapSource::File)
        );
        assert_eq!(row(&applied.view, "navigation", "Ctrl-w z"), None);
        assert!(seen.has_changed().unwrap());
        assert_eq!(*seen.borrow_and_update(), applied);
        assert_eq!(port.current(), applied);

        // Тот же файл: версия прежняя, новый снимок не публикуется.
        let again = port.apply_file().await;
        assert!(Arc::ptr_eq(&again, &applied));
        assert!(!seen.has_changed().unwrap());

        // Файл удалён: таблица по умолчанию, версия растёт.
        fs::remove_file(fixture.file()).unwrap();
        let reset = port.apply_file().await;
        assert_eq!(reset.view.version, 3);
        assert!(row(&reset.view, "navigation", "Ctrl-w z").is_some());
    }

    #[tokio::test]
    async fn bad_line_is_disabled_and_the_rest_applies() {
        let fixture = Fixture::new();
        fixture.write(
            "[navigation]\n\"Ctrl-w m\" = \"layout.zoom\"\n\"Ctrl-w y\" = \"layout.none\"\n\
             \"Ctrl-w t\" = { action = \"tab.select\", args = { index = 0 } }\n",
        );
        let service = fixture.service(false).await;
        let view = service.port().current().view.clone();

        assert_eq!(view.version, 2, "файл применён при старте");
        assert!(row(&view, "navigation", "Ctrl-w m").is_some());
        assert_eq!(row(&view, "navigation", "Ctrl-w y"), None);
        assert_eq!(row(&view, "navigation", "Ctrl-w t"), None);
        let found: Vec<_> = view
            .diagnostics
            .iter()
            .map(|d| (d.line, d.kind.as_str()))
            .collect();
        assert_eq!(found, [(Some(3), "unknown_action"), (Some(4), "bad_args")]);
        assert_eq!(
            view.diagnostics[0].message,
            "keymap.toml, строка 3: неизвестное действие «layout.none» — привязка отключена"
        );
        assert!(!view.diagnostics[0].message.contains('/'));
    }

    #[tokio::test]
    async fn unreadable_file_keeps_the_last_good_table_and_version() {
        let fixture = Fixture::new();
        fixture.write("[navigation]\n\"Ctrl-w m\" = \"layout.zoom\"\n");
        let service = fixture.service(false).await;
        let port = service.port();
        let good = port.current();
        assert_eq!(good.view.version, 2);

        fixture.write("[navigation\n\"Ctrl-w n\" = \"layout.zoom\"\n");
        let broken = port.apply_file().await;
        assert_eq!(broken.view.version, good.view.version);
        assert_eq!(broken.view.bindings, good.view.bindings);
        assert_eq!(broken.view.diagnostics.len(), 1);
        let diagnostic = &broken.view.diagnostics[0];
        assert_eq!(
            (diagnostic.line, diagnostic.kind.as_str()),
            (None, "unreadable")
        );
        assert!(
            diagnostic
                .message
                .starts_with("keymap.toml не прочитан: строка 1: ")
        );
        assert!(diagnostic.message.ends_with(" — действует прежняя таблица"));

        // Не текст: файл не читается, таблица прежняя.
        fs::write(fixture.file(), [0xff, 0xfe, 0x00]).unwrap();
        let binary = port.apply_file().await;
        assert_eq!(binary.view.version, good.view.version);
        assert_eq!(binary.view.bindings, good.view.bindings);
        assert_eq!(binary.view.diagnostics[0].kind, "unreadable");

        // Исправленный файл с прежними строками: диагностика пуста, версия прежняя.
        fixture.write("[navigation]\n\"Ctrl-w m\" = \"layout.zoom\"\n");
        let fixed = port.apply_file().await;
        assert_eq!(fixed.view, good.view);
    }

    #[tokio::test]
    async fn broken_file_at_start_leaves_defaults_with_a_diagnostic() {
        let fixture = Fixture::new();
        fixture.write("leader = \n");
        let service = fixture.service(false).await;
        let view = service.port().current().view.clone();
        assert_eq!(view.version, 1);
        assert_eq!(view.diagnostics.len(), 1);
        assert_eq!(view.diagnostics[0].kind, "unreadable");
    }

    #[tokio::test]
    async fn concurrent_reloads_share_the_serialized_queue() {
        let fixture = Fixture::new();
        let service = fixture.service(false).await;
        let port = service.port();
        fixture.write("[global]\n\"Cmd-Alt-b\" = \"layout.toggle.bottom\"\n");
        let (a, b, c) = tokio::join!(port.apply_file(), port.apply_file(), port.apply_file());
        for applied in [&a, &b, &c] {
            assert_eq!(applied.view.version, 2);
        }
        assert_eq!(port.current().view.version, 2);
    }

    /// Правка файла без `keymap.reload`: наблюдение применяет её, и каталог конфигурации, которого
    /// при старте не было, подхватывается тем же наблюдением.
    #[tokio::test(flavor = "multi_thread")]
    async fn watcher_applies_an_edit_in_a_directory_created_after_start() {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.profile.state_dir()).unwrap();
        let service = fixture.service(true).await;
        let mut seen = service.port().subscribe();
        assert_eq!(seen.borrow_and_update().view.version, 1);

        fixture.write("[navigation]\n\"Ctrl-w m\" = \"layout.zoom\"\n");
        let first = tokio::time::timeout(WATCH_GUARD, seen.wait_for(|s| s.view.version == 2))
            .await
            .expect("наблюдение не применило новый файл до крайнего срока")
            .unwrap()
            .clone();
        assert!(row(&first.view, "navigation", "Ctrl-w m").is_some());

        fixture.write("[navigation]\n\"Ctrl-w n\" = \"layout.zoom\"\n");
        let second = tokio::time::timeout(WATCH_GUARD, seen.wait_for(|s| s.view.version == 3))
            .await
            .expect("наблюдение не применило правку файла до крайнего срока")
            .unwrap()
            .clone();
        assert!(row(&second.view, "navigation", "Ctrl-w n").is_some());
        assert_eq!(row(&second.view, "navigation", "Ctrl-w m"), None);
    }
}
