//! Runtime приложения libghostty: колбэки C, слитый `wakeup` в главную очередь GCD и состояние
//! на главном потоке.
//!
//! `wakeup_cb` с любого потока ставит атомарный флаг и, если флаг был снят, отправляет одну
//! задачу в главную очередь. Задача снимает флаг до `ghostty_app_tick`, поэтому `wakeup` во время
//! `tick` назначает следующий проход. Периодического таймера `tick` нет.
//!
//! Колбэки копируют полезную нагрузку в [`Message`] с ключом поверхности и ничего не делают с
//! главным состоянием. Проход главного потока: освобождение поверхностей, намерение закрыть
//! которые принято раньше, затем `tick`, затем разбор сообщений и работа буфера обмена.
//!
//! Буфер обмена (`clipboard`): поддержан только текст в обычном буфере. Чтение заносится в реестр
//! поверхности до возврата `STARTED` и завершается на следующем проходе текстом доски; каждое
//! подтверждение — OSC 52, kitty, небезопасная вставка — отклоняется на следующем проходе; запись
//! без подтверждения кладёт текст на доску, запись с подтверждением отклоняется (не выполняется).
//! Намерение закрыть отклоняет ожидающие запросы, пока поверхность жива.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::{c_char, c_void};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplicationDidBecomeActiveNotification, NSApplicationDidResignActiveNotification,
    NSEventTrackingRunLoopMode,
};
use objc2_foundation::{NSNotification, NSNotificationCenter, NSObject, NSRunLoop};
use serde::Serialize;
use shellio_sys_ghostty as sys;

use super::clipboard::{self, ReadDecision, RequestId, Requests, Served};
use super::config::{ColorScheme, Config};
use super::surface::{
    self, Close, Decision, GridSize, Handle, Phase, ReleaseRecord, Route, Spec, SurfaceContext,
    SurfaceId, SurfaceKey, Surfaces,
};
use crate::clock;
use crate::scenario::counters::{Live, counter};
use crate::view::SurfaceView;

counter!(APP_ACTIVITY, "ProbeAppActivity");

/// Сколько строк журнала сообщений хранится для итога.
const LOG_LIMIT: usize = 400;

/// Слитый запрос прохода главного потока.
#[derive(Debug, Default)]
pub struct WakeFlag(AtomicBool);

impl WakeFlag {
    pub const fn new() -> WakeFlag {
        WakeFlag(AtomicBool::new(false))
    }

    /// `true` — флаг был снят, и нужно отправить один проход в главную очередь.
    pub fn request(&self) -> bool {
        !self.0.swap(true, Ordering::AcqRel)
    }

    /// Снимает флаг до `tick`: запрос во время прохода назначает следующий.
    pub fn begin_pass(&self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Цель сообщения.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    App,
    Surface(SurfaceKey),
    /// Поверхность без контекста `userdata`.
    UnknownSurface,
}

impl Target {
    fn describe(self) -> String {
        match self {
            Target::App => "app".to_owned(),
            Target::Surface(key) => key.to_string(),
            Target::UnknownSurface => "surface?".to_owned(),
        }
    }
}

/// Полезная нагрузка колбэка, скопированная в данные Rust.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Title(Option<String>),
    Pwd(Option<String>),
    /// `process_alive` есть только у `close_surface_cb`.
    CloseIntent {
        process_alive: Option<bool>,
        source: &'static str,
    },
    Bell,
    OpenUrl(Option<String>),
    ColorChange {
        kind: i32,
        rgb: [u8; 3],
    },
    ChildExited {
        exit_code: u32,
        runtime_ms: u64,
    },
    ReloadConfig {
        soft: bool,
    },
    ConfigChange,
    /// Чтение; `id` есть только у запроса, на который колбэк ответил `STARTED`.
    ClipboardRead {
        location: u32,
        mimes: usize,
        list: bool,
        id: Option<RequestId>,
        decision: &'static str,
    },
    /// Подтверждение, которое библиотека держит до `deny`; токен лежит в реестре поверхности.
    ClipboardConfirm {
        id: RequestId,
        request: u32,
    },
    /// Запись: представления скопированы в колбэке по длине.
    ClipboardWrite {
        location: u32,
        mimes: Vec<String>,
        text: Option<String>,
        confirm: bool,
    },
    Unsupported {
        tag: u32,
    },
}

impl Event {
    pub fn kind(&self) -> &'static str {
        match self {
            Event::Title(_) => "title",
            Event::Pwd(_) => "pwd",
            Event::CloseIntent { .. } => "close-intent",
            Event::Bell => "bell",
            Event::OpenUrl(_) => "open-url",
            Event::ColorChange { .. } => "color-change",
            Event::ChildExited { .. } => "child-exited",
            Event::ReloadConfig { .. } => "reload-config",
            Event::ConfigChange => "config-change",
            Event::ClipboardRead { .. } => "clipboard-read",
            Event::ClipboardConfirm { .. } => "clipboard-confirm",
            Event::ClipboardWrite { .. } => "clipboard-write",
            Event::Unsupported { .. } => "unsupported",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub target: Target,
    pub t_ns: u64,
    pub event: Event,
}

/// Общая часть приложения: её указатель — `userdata` приложения, живой до `ghostty_app_free`.
pub struct Shared {
    wake: WakeFlag,
    inbox: Mutex<Vec<Message>>,
    post: fn(),
    dropped_at_source: AtomicU64,
    wakeups: AtomicU64,
}

impl Shared {
    fn new(post: fn()) -> Shared {
        Shared {
            wake: WakeFlag::new(),
            inbox: Mutex::new(Vec::new()),
            post,
            dropped_at_source: AtomicU64::new(0),
            wakeups: AtomicU64::new(0),
        }
    }

    /// Запрос прохода главного потока с любого потока; повторные запросы сливаются.
    pub fn request_pass(&self) {
        self.wakeups.fetch_add(1, Ordering::Relaxed);
        if self.wake.request() {
            (self.post)();
        }
    }

    pub fn push(&self, message: Message) {
        self.inbox
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(message);
        self.request_pass();
    }

    pub fn dropped_at_source(&self) {
        self.dropped_at_source.fetch_add(1, Ordering::Relaxed);
    }

    fn take(&self) -> Vec<Message> {
        std::mem::take(
            &mut *self
                .inbox
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }

    fn count(&self, kind: &str) -> usize {
        self.inbox
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .filter(|message| message.event.kind() == kind)
            .count()
    }
}

fn post_main_pass() {
    DispatchQueue::main().exec_async(main_pass);
}

// ---- колбэки C ----

/// # Safety
/// `userdata` — NULL или контекст поверхности, живой на время вызова.
unsafe fn context<'a>(userdata: *mut c_void) -> Option<&'a SurfaceContext> {
    // SAFETY: контекст живёт до возврата `ghostty_surface_free`, колбэки приходят раньше.
    unsafe { userdata.cast::<SurfaceContext>().cast_const().as_ref() }
}

unsafe extern "C" fn wakeup_cb(userdata: *mut c_void) {
    // SAFETY: `userdata` приложения — `Shared`, живой до возврата `ghostty_app_free`.
    if let Some(shared) = unsafe { userdata.cast::<Shared>().cast_const().as_ref() } {
        shared.request_pass();
    }
}

unsafe extern "C" fn action_cb(
    app: sys::ghostty_app_t,
    target: sys::ghostty_target_s,
    action: sys::ghostty_action_s,
) -> bool {
    // SAFETY: приложение живо, пока библиотека зовёт его колбэк; `userdata` — `Shared`.
    let shared = unsafe {
        sys::ghostty_app_userdata(app)
            .cast::<Shared>()
            .cast_const()
            .as_ref()
    };
    let Some(shared) = shared else {
        return false;
    };
    let (target, context) = if target.tag == sys::ghostty_target_tag_e_GHOSTTY_TARGET_SURFACE {
        // SAFETY: у цели-поверхности активно поле `surface`; поверхность жива во время колбэка,
        // и внутри `ghostty_surface_new` её `userdata` уже выставлен.
        let context = unsafe { context(sys::ghostty_surface_userdata(target.target.surface)) };
        match context {
            Some(context) => (Target::Surface(context.key), Some(context)),
            None => (Target::UnknownSurface, None),
        }
    } else {
        (Target::App, None)
    };
    // SAFETY: поле объединения выбирается по тегу действия.
    let (event, handled) = unsafe { decode_action(target, &action) };
    match context {
        Some(context) => context.push(event),
        None => shared.push(Message {
            target,
            t_ns: clock::uptime_ns(),
            event,
        }),
    }
    handled
}

/// Событие и ответ `action_cb`: `true` только для поддержанных действий.
///
/// # Safety
/// Поля объединения читаются по тегу; строки действия живы на время колбэка.
unsafe fn decode_action(target: Target, action: &sys::ghostty_action_s) -> (Event, bool) {
    let surface = matches!(target, Target::Surface(_));
    let payload = &action.action;
    // SAFETY: каждое поле читается только при своём теге.
    unsafe {
        match action.tag {
            sys::ghostty_action_tag_e_GHOSTTY_ACTION_SET_TITLE => (
                Event::Title(super::copy_c_str(payload.set_title.title)),
                true,
            ),
            sys::ghostty_action_tag_e_GHOSTTY_ACTION_PWD => {
                (Event::Pwd(super::copy_c_str(payload.pwd.pwd)), true)
            }
            sys::ghostty_action_tag_e_GHOSTTY_ACTION_CLOSE_WINDOW if surface => (
                Event::CloseIntent {
                    process_alive: None,
                    source: "action-close-window",
                },
                true,
            ),
            sys::ghostty_action_tag_e_GHOSTTY_ACTION_RING_BELL => (Event::Bell, true),
            sys::ghostty_action_tag_e_GHOSTTY_ACTION_OPEN_URL => (
                Event::OpenUrl(super::copy_bytes(
                    payload.open_url.url,
                    payload.open_url.len,
                )),
                true,
            ),
            sys::ghostty_action_tag_e_GHOSTTY_ACTION_COLOR_CHANGE => {
                let change = payload.color_change;
                (
                    Event::ColorChange {
                        kind: change.kind,
                        rgb: [change.r, change.g, change.b],
                    },
                    true,
                )
            }
            // Ответ `false`: libghostty сама пишет сообщение о выходе в терминал.
            sys::ghostty_action_tag_e_GHOSTTY_ACTION_SHOW_CHILD_EXITED => (
                Event::ChildExited {
                    exit_code: payload.child_exited.exit_code,
                    runtime_ms: payload.child_exited.timetime_ms,
                },
                false,
            ),
            // Перезагрузку приложения выполняет вызывающий смену схемы приложения сразу после
            // неё; мягкую перезагрузку поверхности — следующий проход (`update_config`).
            sys::ghostty_action_tag_e_GHOSTTY_ACTION_RELOAD_CONFIG => (
                Event::ReloadConfig {
                    soft: payload.reload_config.soft,
                },
                !surface || payload.reload_config.soft,
            ),
            sys::ghostty_action_tag_e_GHOSTTY_ACTION_CONFIG_CHANGE => (Event::ConfigChange, true),
            tag => (Event::Unsupported { tag }, false),
        }
    }
}

unsafe extern "C" fn read_clipboard_cb(
    userdata: *mut c_void,
    location: sys::ghostty_clipboard_e,
    state: *mut c_void,
    _mimes: *const *const c_char,
    mimes_len: usize,
    list: bool,
) -> sys::ghostty_clipboard_read_result_e {
    // SAFETY: `userdata` поверхности — её контекст.
    let Some(context) = (unsafe { context(userdata) }) else {
        // Без контекста ответить некому: состояние освобождает сама библиотека.
        return ReadDecision::Unavailable.raw();
    };
    // Запрос заносится в реестр до возврата `STARTED`; снятый контекст отвечает «недоступно».
    let decision = if context.retired.load(Ordering::Acquire) {
        ReadDecision::Unavailable
    } else {
        context.clipboard.begin_read(state as usize, location)
    };
    let (id, name) = match decision {
        ReadDecision::Started(id) => (Some(id), "started"),
        ReadDecision::Unavailable => (None, "unavailable"),
        ReadDecision::Unsupported => (None, "unsupported"),
    };
    context.push(Event::ClipboardRead {
        location,
        mimes: mimes_len,
        list,
        id,
        decision: name,
    });
    decision.raw()
}

unsafe extern "C" fn confirm_read_clipboard_cb(
    userdata: *mut c_void,
    _confirm: *const sys::ghostty_clipboard_confirm_s,
    state: *mut c_void,
    request: sys::ghostty_clipboard_request_e,
) {
    // SAFETY: `userdata` поверхности — её контекст.
    if let Some(context) = unsafe { context(userdata) } {
        let id = context.clipboard.confirm(state as usize);
        context.push(Event::ClipboardConfirm { id, request });
    }
}

unsafe extern "C" fn write_clipboard_cb(
    userdata: *mut c_void,
    location: sys::ghostty_clipboard_e,
    contents: *const sys::ghostty_clipboard_content_s,
    contents_len: usize,
    confirm: bool,
) {
    // SAFETY: `userdata` поверхности — её контекст.
    let Some(context) = (unsafe { context(userdata) }) else {
        return;
    };
    // SAFETY: библиотека передаёт `contents_len` представлений, живых на время колбэка.
    let copied = unsafe { copy_contents(contents, contents_len) };
    context.push(Event::ClipboardWrite {
        location,
        mimes: copied.iter().map(|(mime, _)| mime.clone()).collect(),
        text: clipboard::plain_text(&copied),
        confirm,
    });
}

/// Копия представлений записи: тип и байты по явной длине.
///
/// # Safety
/// `contents` — NULL или `len` структур, живых на время вызова, с данными указанной длины.
unsafe fn copy_contents(
    contents: *const sys::ghostty_clipboard_content_s,
    len: usize,
) -> Vec<(String, Vec<u8>)> {
    if contents.is_null() {
        return Vec::new();
    }
    // SAFETY: по контракту функции массив жив и длины `len`.
    let items = unsafe { std::slice::from_raw_parts(contents, len) };
    items
        .iter()
        .map(|item| {
            // SAFETY: тип — строка с нулём, данные — `item.len` байт; живы на время колбэка.
            let mime = unsafe { super::copy_c_str(item.mime) }.unwrap_or_default();
            let data = if item.data.is_null() {
                Vec::new()
            } else {
                // SAFETY: см. выше.
                unsafe { std::slice::from_raw_parts(item.data.cast::<u8>(), item.len) }.to_vec()
            };
            (mime, data)
        })
        .collect()
}

unsafe extern "C" fn close_surface_cb(userdata: *mut c_void, process_alive: bool) {
    // SAFETY: `userdata` поверхности — её контекст.
    if let Some(context) = unsafe { context(userdata) } {
        context.push(Event::CloseIntent {
            process_alive: Some(process_alive),
            source: "close-surface-cb",
        });
    }
}

// ---- главный поток ----

/// Счётчики runtime для итога сценария.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Stats {
    pub passes: u64,
    /// Проходы, выполненные в режиме цикла `NSEventTrackingRunLoopMode`.
    pub passes_tracking: u64,
    pub wakeups: u64,
    pub messages: u64,
    /// Принятые сообщения по виду.
    pub delivered: BTreeMap<String, u64>,
    pub unsupported_actions: BTreeMap<u32, u64>,
    pub dropped_stale: usize,
    pub dropped_at_source: u64,
    pub close_intents: usize,
    pub surfaces_created: usize,
    pub surfaces_freed: usize,
    /// Подтверждения, отклонённые на проходе.
    pub clipboard_denied: u64,
    /// Ожидающие запросы, отклонённые при намерении закрыть.
    pub clipboard_denied_on_retire: u64,
    pub clipboard_served: u64,
    pub clipboard_late_skipped: u64,
    pub clipboard_written: u64,
    /// Записи с подтверждением: не выполняются.
    pub clipboard_write_confirm_denied: u64,
    /// Подачи `ghostty_app_set_focus` после `start`: активность приложения.
    pub app_focus: Vec<bool>,
    /// `ghostty_surface_update_config` в ответ на мягкий `reload_config` поверхности.
    pub surface_config_updates: u64,
    /// Живые смены схемы: схема и число поверхностей, получивших её.
    pub scheme_changes: Vec<(&'static str, usize)>,
    pub releases: Vec<ReleaseRecord>,
    pub log: Vec<String>,
}

struct Runtime {
    app: sys::ghostty_app_t,
    shared: Arc<Shared>,
    /// Указатель, отданный libghostty как `userdata` приложения: `Arc::into_raw`.
    app_userdata: *const Shared,
    _config: Config,
    surfaces: Surfaces,
    pass: u64,
    stats: Stats,
    /// Наблюдатель активности приложения; снимается в `shutdown`.
    activity: Option<Retained<AppActivity>>,
}

thread_local! {
    static RUNTIME: RefCell<Option<Runtime>> = const { RefCell::new(None) };
}

/// Короткое заимствование состояния. Вызовы в libghostty внутри `f` запрещены.
fn with<T>(f: impl FnOnce(&mut Runtime) -> T) -> Option<T> {
    RUNTIME.with(|cell| cell.borrow_mut().as_mut().map(f))
}

/// Что сделал `start`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct StartRecord {
    pub color_scheme: &'static str,
    pub app_focused: bool,
    /// `reload_config` приложения, пришедшие из `ghostty_app_set_color_scheme`.
    pub reload_config_from_scheme: usize,
    /// `config_change`, пришедшие из `ghostty_app_update_config`.
    pub config_change_from_update: usize,
    /// Отметки `CLOCK_UPTIME_RAW` вокруг `ghostty_app_new`.
    pub app_new_started_ns: u64,
    pub app_new_finished_ns: u64,
}

/// Создаёт приложение и выставляет схему цвета до первой поверхности. `set_color_scheme` меняет
/// только условное состояние ядра и просит перезагрузку: без `update_config` поверхность при
/// создании переиграет конфигурацию и потеряет `command` и окружение.
pub fn start(config: Config, scheme: ColorScheme, focused: bool) -> Result<StartRecord, String> {
    if RUNTIME.with(|cell| cell.borrow().is_some()) {
        return Err("runtime libghostty уже запущен".to_owned());
    }
    let shared = Arc::new(Shared::new(post_main_pass));
    let app_userdata = Arc::into_raw(Arc::clone(&shared));
    let runtime_config = sys::ghostty_runtime_config_s {
        userdata: app_userdata.cast_mut().cast(),
        supports_selection_clipboard: false,
        wakeup_cb: Some(wakeup_cb),
        action_cb: Some(action_cb),
        read_clipboard_cb: Some(read_clipboard_cb),
        confirm_read_clipboard_cb: Some(confirm_read_clipboard_cb),
        write_clipboard_cb: Some(write_clipboard_cb),
        close_surface_cb: Some(close_surface_cb),
    };
    let app_new_started_ns = clock::uptime_ns();
    // SAFETY: параметры живут на время вызова, конфигурация финализирована; `userdata` живёт до
    // `ghostty_app_free`.
    let app = unsafe { sys::ghostty_app_new(&runtime_config, config.raw()) };
    let app_new_finished_ns = clock::uptime_ns();
    if app.is_null() {
        // SAFETY: указатель из `Arc::into_raw` выше, библиотека его не сохранила.
        drop(unsafe { Arc::from_raw(app_userdata) });
        return Err("ghostty_app_new вернул NULL".to_owned());
    }
    // SAFETY: приложение живо; колбэки этих вызовов только кладут сообщения.
    let (reloads, changes) = unsafe {
        sys::ghostty_app_set_focus(app, focused);
        let before = shared.count("reload-config");
        sys::ghostty_app_set_color_scheme(app, scheme.raw());
        let reloads = shared.count("reload-config") - before;
        let before = shared.count("config-change");
        sys::ghostty_app_update_config(app, config.raw());
        (reloads, shared.count("config-change") - before)
    };
    let activity = MainThreadMarker::new().map(AppActivity::observe);
    RUNTIME.with(|cell| {
        *cell.borrow_mut() = Some(Runtime {
            app,
            shared: Arc::clone(&shared),
            app_userdata,
            _config: config,
            surfaces: Surfaces::default(),
            pass: 0,
            stats: Stats::default(),
            activity,
        })
    });
    shared.request_pass();
    Ok(StartRecord {
        color_scheme: scheme.name(),
        app_focused: focused,
        reload_config_from_scheme: reloads,
        config_change_from_update: changes,
        app_new_started_ns,
        app_new_finished_ns,
    })
}

/// Итог живой смены схемы.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SchemeRecord {
    pub scheme: &'static str,
    pub reload_config_from_app: usize,
    pub config_change_from_update: usize,
    /// Живые поверхности, которым подана схема.
    pub surfaces: usize,
    pub started_ns: u64,
    pub finished_ns: u64,
}

/// Живая смена схемы: приложению, как в `start`, затем каждой живой поверхности. Поверхность
/// хранит своё условное состояние и просит мягкий `reload_config`; его выполняет следующий проход
/// через `ghostty_surface_update_config`, иначе тема поверхности не сменится.
pub fn set_color_scheme(scheme: ColorScheme) -> Result<SchemeRecord, String> {
    let (app, shared, config, handles) = with(|rt| {
        let handles: Vec<Handle> = rt
            .surfaces
            .registry
            .live_handles()
            .into_iter()
            .map(|(_, handle)| handle)
            .collect();
        (rt.app, Arc::clone(&rt.shared), rt._config.raw(), handles)
    })
    .ok_or_else(not_started)?;
    let started_ns = clock::uptime_ns();
    // SAFETY: приложение, конфигурация и поверхности живы; вызовы вне заимствования, колбэки
    // только кладут сообщения.
    let (reloads, changes) = unsafe {
        let before = shared.count("reload-config");
        sys::ghostty_app_set_color_scheme(app, scheme.raw());
        let reloads = shared.count("reload-config") - before;
        let before = shared.count("config-change");
        sys::ghostty_app_update_config(app, config);
        let changes = shared.count("config-change") - before;
        for handle in &handles {
            handle.set_color_scheme(scheme.raw());
        }
        (reloads, changes)
    };
    let finished_ns = clock::uptime_ns();
    with(|rt| {
        rt.stats.scheme_changes.push((scheme.name(), handles.len()));
    });
    shared.request_pass();
    Ok(SchemeRecord {
        scheme: scheme.name(),
        reload_config_from_app: reloads,
        config_change_from_update: changes,
        surfaces: handles.len(),
        started_ns,
        finished_ns,
    })
}

pub fn add_surface(view: Retained<SurfaceView>, spec: Spec) -> Result<SurfaceId, String> {
    with(|rt| rt.surfaces.add(view, spec)).ok_or_else(not_started)
}

/// Итог попытки создания.
#[derive(Clone, Debug, PartialEq)]
pub enum Created {
    Created { key: SurfaceKey, new_ms: f64 },
    Waiting(Decision),
}

/// Создаёт поверхность, если реестр разрешает. `ghostty_surface_new` вызывается без
/// заимствования состояния: внутри неё библиотека зовёт `action_cb`.
pub fn create(id: SurfaceId) -> Result<Created, String> {
    let (prepared, app) = with(|rt| {
        rt.surfaces
            .prepare(id, &rt.shared)
            .map(|prepared| (prepared, rt.app))
    })
    .ok_or_else(not_started)??;
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(decision) => return Ok(Created::Waiting(decision)),
    };
    let key = prepared.key;
    let started = clock::uptime_ns();
    // SAFETY: приложение живо; параметры указывают в контекст и строки, которые реестр держит
    // до освобождения поверхности.
    let raw = unsafe { sys::ghostty_surface_new(app, &prepared.config) };
    let new_ms = clock::interval_ms(started, clock::uptime_ns());
    let (view, handle) = with(|rt| rt.surfaces.commit(prepared, raw)).ok_or_else(not_started)??;
    view.attach(key, handle);
    request_pass();
    Ok(Created::Created { key, new_ms })
}

/// Намерение закрыть от самой пробы: идёт тем же путём, что и колбэки.
pub fn request_close(id: SurfaceId) -> Result<SurfaceKey, String> {
    let (shared, key) = with(|rt| {
        let key = rt.surfaces.registry.current(id).map(|(key, _)| key);
        (Arc::clone(&rt.shared), key)
    })
    .ok_or_else(not_started)?;
    let key = key.ok_or(format!("поверхность s{} не живая", id.0))?;
    shared.push(Message {
        target: Target::Surface(key),
        t_ns: clock::uptime_ns(),
        event: Event::CloseIntent {
            process_alive: None,
            source: "probe",
        },
    });
    Ok(key)
}

pub fn phase(id: SurfaceId) -> Option<Phase> {
    with(|rt| rt.surfaces.registry.phase(id)).flatten()
}

/// Ключ и ручка живой поверхности.
pub fn current(id: SurfaceId) -> Option<(SurfaceKey, Handle)> {
    with(|rt| rt.surfaces.registry.current(id)).flatten()
}

/// Счётчики runtime посреди сценария.
pub fn stats() -> Option<Stats> {
    with(|rt| rt.snapshot())
}

fn live_handle(id: SurfaceId) -> Option<Handle> {
    with(|rt| rt.surfaces.registry.current(id).map(|(_, handle)| handle)).flatten()
}

/// Ожидающие запросы буфера обмена живой поверхности и счётчики её реестра.
pub fn clipboard(id: SurfaceId) -> Option<(usize, clipboard::Stats)> {
    let requests = with(|rt| {
        let (key, _) = rt.surfaces.registry.current(id)?;
        rt.surfaces.clipboard(key)
    })
    .flatten()?;
    Some((requests.pending(), requests.stats()))
}

/// Подача активности приложения в `ghostty_app_set_focus` вне заимствования состояния.
fn app_focus_changed(focused: bool) {
    let Some(app) = with(|rt| {
        rt.stats.app_focus.push(focused);
        rt.app
    }) else {
        return;
    };
    // SAFETY: приложение живо, пока runtime стоит в потоке; вызов на главном потоке.
    unsafe { sys::ghostty_app_set_focus(app, focused) };
}

/// Наблюдатель `NSApplicationDidBecomeActive` и `DidResignActive` для `ghostty_app_set_focus`.
pub struct ActivityIvars {
    _live: Live,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeAppActivity"]
    #[ivars = ActivityIvars]
    pub struct AppActivity;

    impl AppActivity {
        #[unsafe(method(appDidBecomeActive:))]
        fn app_did_become_active(&self, _notification: &NSNotification) {
            app_focus_changed(true);
        }

        #[unsafe(method(appDidResignActive:))]
        fn app_did_resign_active(&self, _notification: &NSNotification) {
            app_focus_changed(false);
        }
    }
);

impl AppActivity {
    fn observe(mtm: MainThreadMarker) -> Retained<AppActivity> {
        let this = AppActivity::alloc(mtm).set_ivars(ActivityIvars {
            _live: APP_ACTIVITY.live(),
        });
        // SAFETY: `init` у NSObject без аргументов.
        let this: Retained<AppActivity> = unsafe { msg_send![super(this), init] };
        let center = NSNotificationCenter::defaultCenter();
        // SAFETY: имена уведомлений — неизменяемые константы AppKit; селекторы реализованы
        // классом; подписка снимается в `forget` до освобождения наблюдателя.
        unsafe {
            center.addObserver_selector_name_object(
                &this,
                sel!(appDidBecomeActive:),
                Some(NSApplicationDidBecomeActiveNotification),
                None,
            );
            center.addObserver_selector_name_object(
                &this,
                sel!(appDidResignActive:),
                Some(NSApplicationDidResignActiveNotification),
                None,
            );
        }
        this
    }

    fn forget(&self) {
        // SAFETY: снятие всех подписок этого наблюдателя.
        unsafe { NSNotificationCenter::defaultCenter().removeObserver(self) };
    }
}

/// Чтения живой поверхности, которые проверяют сценарии.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SurfaceReading {
    pub foreground_pid: u64,
    pub tty_name: Option<String>,
    pub size: GridSize,
}

pub fn read(id: SurfaceId) -> Option<SurfaceReading> {
    let handle = live_handle(id)?;
    // SAFETY: ручка живой поверхности, освобождение идёт только на главном потоке, а этот вызов
    // тоже на нём.
    unsafe {
        Some(SurfaceReading {
            foreground_pid: handle.foreground_pid(),
            tty_name: handle.tty_name(),
            size: handle.size(),
        })
    }
}

pub fn viewport_text(id: SurfaceId) -> Option<String> {
    let handle = live_handle(id)?;
    // SAFETY: см. `read`.
    unsafe { handle.viewport_text() }
}

fn request_pass() {
    if let Some(shared) = with(|rt| Arc::clone(&rt.shared)) {
        shared.request_pass();
    }
}

fn not_started() -> String {
    "runtime libghostty не запущен".to_owned()
}

impl Runtime {
    fn snapshot(&self) -> Stats {
        let mut stats = self.stats.clone();
        let registry = &self.surfaces.registry;
        stats.wakeups = self.shared.wakeups.load(Ordering::Relaxed);
        stats.dropped_at_source = self.shared.dropped_at_source.load(Ordering::Relaxed);
        stats.dropped_stale = registry.dropped_count();
        stats.close_intents = registry.close_intent_count();
        stats.surfaces_created = registry.created_count();
        stats.surfaces_freed = registry.freed_count();
        stats
    }

    fn log(&mut self, line: String) {
        if self.stats.log.len() < LOG_LIMIT {
            self.stats.log.push(line);
        }
    }

    /// Разбирает сообщения и возвращает работу буфера обмена, которую нужно выполнить вне
    /// заимствования: вызовы в libghostty синхронно зовут колбэки.
    fn accept(&mut self, messages: Vec<Message>) -> Work {
        let mut work = Work::default();
        for message in messages {
            self.stats.messages += 1;
            let kind = message.event.kind();
            let outcome = match (message.target, &message.event) {
                (Target::Surface(key), Event::CloseIntent { .. }) => {
                    let live = self
                        .surfaces
                        .registry
                        .current(key.id)
                        .filter(|(current, _)| *current == key)
                        .map(|(_, handle)| handle);
                    match self.surfaces.registry.close_intent(key, self.pass) {
                        Close::Scheduled => {
                            if let (Some(requests), Some(handle)) =
                                (self.surfaces.retire(key), live)
                            {
                                work.closes.push((handle, requests));
                            }
                            work.again = true;
                            "scheduled"
                        }
                        Close::AlreadyClosing => "already-closing",
                        Close::Stale => "stale",
                    }
                }
                (Target::Surface(key), Event::ClipboardConfirm { id, .. }) => {
                    match self.surfaces.registry.route(key) {
                        Route::Live(handle) | Route::Closing(handle) => {
                            match self.surfaces.clipboard(key) {
                                Some(requests) => {
                                    work.denies.push((handle, requests, *id));
                                    "deny"
                                }
                                None => "no-context",
                            }
                        }
                        Route::Stale => "stale",
                    }
                }
                (Target::Surface(key), Event::ClipboardRead { id: Some(id), .. }) => {
                    match self.surfaces.registry.route(key) {
                        Route::Live(handle) | Route::Closing(handle) => {
                            match self.surfaces.clipboard(key) {
                                Some(requests) => {
                                    work.serves.push((handle, requests, *id));
                                    "serve"
                                }
                                None => "no-context",
                            }
                        }
                        Route::Stale => "stale",
                    }
                }
                (
                    Target::Surface(key),
                    Event::ClipboardWrite {
                        location,
                        text,
                        confirm,
                        ..
                    },
                ) => match self.surfaces.registry.route(key) {
                    Route::Live(_) if *confirm => {
                        self.stats.clipboard_write_confirm_denied += 1;
                        "write-confirm-denied"
                    }
                    Route::Live(_)
                        if *location == sys::ghostty_clipboard_e_GHOSTTY_CLIPBOARD_STANDARD =>
                    {
                        match text {
                            Some(text) => {
                                work.writes.push(text.clone());
                                "write"
                            }
                            None => "write-no-text",
                        }
                    }
                    Route::Live(_) => "write-unsupported",
                    Route::Closing(_) => "closing",
                    Route::Stale => "stale",
                },
                (Target::Surface(key), Event::ReloadConfig { soft: true }) => {
                    match self.surfaces.registry.route(key) {
                        Route::Live(handle) => {
                            work.reloads.push(handle);
                            "update-config"
                        }
                        Route::Closing(_) => "closing",
                        Route::Stale => "stale",
                    }
                }
                (Target::Surface(key), _) => match self.surfaces.registry.route(key) {
                    Route::Live(_) => "delivered",
                    Route::Closing(_) => "closing",
                    Route::Stale => "stale",
                },
                (Target::App | Target::UnknownSurface, _) => "delivered",
            };
            if outcome != "stale" {
                *self.stats.delivered.entry(kind.to_owned()).or_default() += 1;
            }
            if let Event::Unsupported { tag } = message.event {
                *self.stats.unsupported_actions.entry(tag).or_default() += 1;
            } else {
                let line = format!(
                    "pass={} t={} {} {:?} → {outcome}",
                    self.pass,
                    message.t_ns,
                    message.target.describe(),
                    message.event
                );
                self.log(line);
            }
        }
        work
    }
}

type Pending = (Handle, Arc<Requests>, RequestId);

/// Работа буфера обмена после разбора сообщений.
#[derive(Default)]
struct Work {
    serves: Vec<Pending>,
    denies: Vec<Pending>,
    /// Намерение закрыть: ожидающие запросы отклоняются, пока поверхность жива.
    closes: Vec<(Handle, Arc<Requests>)>,
    writes: Vec<String>,
    /// Мягкий `reload_config` живой поверхности.
    reloads: Vec<Handle>,
    again: bool,
}

/// Разбор входящих сообщений и работа буфера обмена на живых поверхностях. Освобождение идёт
/// только на главном потоке в начале прохода, поэтому ручки здесь живы.
fn drain(shared: &Shared) {
    let messages = shared.take();
    if messages.is_empty() {
        return;
    }
    let Some(work) = with(|rt| rt.accept(messages)) else {
        return;
    };
    if !work.reloads.is_empty()
        && let Some(config) = with(|rt| rt._config.raw())
    {
        for handle in &work.reloads {
            // SAFETY: поверхность жива (освобождение идёт только в начале прохода), конфигурация
            // принадлежит runtime; вызов вне заимствования, колбэки только кладут сообщения.
            unsafe { handle.update_config(config) };
        }
        let count = work.reloads.len() as u64;
        with(|rt| rt.stats.surface_config_updates += count);
    }
    let mut written = 0;
    for text in &work.writes {
        if clipboard::write_text(text) {
            written += 1;
        }
    }
    let mut on_retire = 0;
    for (handle, requests) in &work.closes {
        // SAFETY: токены реестра принадлежат этой живой поверхности.
        on_retire += requests.close(|token| unsafe { handle.deny_clipboard(token) });
    }
    let text = (!work.serves.is_empty())
        .then(clipboard::read_text)
        .flatten();
    let (mut served, mut skipped) = (0, 0);
    for (handle, requests, id) in &work.serves {
        // SAFETY: токен получен колбэком чтения этой поверхности со `STARTED`; реестр отдаёт его
        // один раз и без блокировки на время вызова: подтверждение может прийти изнутри.
        let outcome = requests.serve(*id, |token| unsafe {
            handle.complete_clipboard(token, text.as_deref())
        });
        match outcome {
            Served::Skipped => skipped += 1,
            Served::Completed | Served::Confirming(_) => served += 1,
        }
    }
    let mut denied = 0;
    for (handle, requests, id) in &work.denies {
        // SAFETY: токен подтверждения этой живой поверхности; реестр отдаёт его один раз.
        if requests.deny(*id, |token| unsafe { handle.deny_clipboard(token) }) {
            denied += 1;
        } else {
            skipped += 1;
        }
    }
    with(|rt| {
        rt.stats.clipboard_written += written;
        rt.stats.clipboard_denied_on_retire += on_retire as u64;
        rt.stats.clipboard_served += served;
        rt.stats.clipboard_denied += denied;
        rt.stats.clipboard_late_skipped += skipped;
    });
    if work.again {
        shared.request_pass();
    }
}

/// Проход главного потока из главной очереди.
fn main_pass() {
    let Some((app, shared)) = with(|rt| (rt.app, Arc::clone(&rt.shared))) else {
        return;
    };
    shared.wake.begin_pass();
    let tracking = in_tracking_mode();
    let due = with(|rt| {
        rt.pass += 1;
        rt.stats.passes += 1;
        if tracking {
            rt.stats.passes_tracking += 1;
        }
        rt.surfaces.take_due(rt.pass)
    })
    .unwrap_or_default();
    for released in due {
        let record = surface::release(released);
        with(|rt| rt.stats.releases.push(record));
    }
    // SAFETY: приложение живо, пока runtime стоит в потоке; вызов идёт вне заимствования.
    unsafe { sys::ghostty_app_tick(app) };
    drain(&shared);
}

/// Главный цикл крутится в режиме отслеживания: ресайз окна, меню, перетаскивание.
pub fn in_tracking_mode() -> bool {
    let mode = NSRunLoop::currentRunLoop().currentMode();
    // SAFETY: `NSEventTrackingRunLoopMode` — неизменяемая константа AppKit.
    let tracking = unsafe { NSEventTrackingRunLoopMode };
    mode.is_some_and(|mode| mode.isEqualToString(tracking))
}

/// Итог завершения runtime.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ShutdownRecord {
    pub stats: Stats,
    pub app_free_ms: f64,
}

/// Завершение: сообщения разобраны и подтверждения отклонены, затем все поверхности, затем
/// приложение, затем его контекст и конфигурация.
pub fn shutdown() -> Result<ShutdownRecord, String> {
    let shared = with(|rt| Arc::clone(&rt.shared)).ok_or_else(not_started)?;
    drain(&shared);
    let remaining = with(|rt| rt.surfaces.take_all()).unwrap_or_default();
    for released in remaining {
        let record = surface::release(released);
        with(|rt| rt.stats.releases.push(record));
    }
    let mut runtime = RUNTIME
        .with(|cell| cell.borrow_mut().take())
        .ok_or_else(not_started)?;
    if let Some(activity) = runtime.activity.take() {
        activity.forget();
    }
    let stats = runtime.snapshot();
    let started = clock::uptime_ns();
    // SAFETY: все поверхности освобождены; приложение освобождается один раз.
    unsafe { sys::ghostty_app_free(runtime.app) };
    let app_free_ms = clock::interval_ms(started, clock::uptime_ns());
    runtime.app = std::ptr::null_mut();
    // SAFETY: указатель из `Arc::into_raw` в `start`; библиотека больше его не зовёт.
    drop(unsafe { Arc::from_raw(runtime.app_userdata) });
    drop(runtime);
    Ok(ShutdownRecord { stats, app_free_ms })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    #[test]
    fn wake_flag_coalesces_until_the_pass_begins() {
        let flag = WakeFlag::new();
        assert!(flag.request(), "первый запрос отправляет проход");
        assert!(!flag.request(), "повторный сливается");
        flag.begin_pass();
        assert!(
            flag.request(),
            "флаг снят до tick: wakeup во время tick назначает проход"
        );
        assert!(!flag.request());
    }

    static POSTED: AtomicUsize = AtomicUsize::new(0);

    fn count_post() {
        POSTED.fetch_add(1, Ordering::SeqCst);
    }

    #[test]
    fn shared_posts_one_pass_per_raised_flag() {
        let shared = Shared::new(count_post);
        let message = |event| Message {
            target: Target::App,
            t_ns: 1,
            event,
        };
        shared.request_pass();
        shared.push(message(Event::Bell));
        shared.push(message(Event::ReloadConfig { soft: true }));
        assert_eq!(POSTED.load(Ordering::SeqCst), 1);
        assert_eq!(shared.count("reload-config"), 1);
        shared.wake.begin_pass();
        let taken = shared.take();
        assert_eq!(taken.len(), 2);
        shared.push(message(Event::ConfigChange));
        assert_eq!(
            POSTED.load(Ordering::SeqCst),
            2,
            "запрос после начала прохода"
        );
        assert_eq!(shared.wakeups.load(Ordering::Relaxed), 4);
        assert!(shared.take().len() == 1 && shared.take().is_empty());
    }

    #[test]
    fn event_kinds_are_distinct() {
        let events = [
            Event::Title(None),
            Event::Pwd(None),
            Event::CloseIntent {
                process_alive: None,
                source: "probe",
            },
            Event::Bell,
            Event::OpenUrl(None),
            Event::ColorChange {
                kind: 0,
                rgb: [0; 3],
            },
            Event::ChildExited {
                exit_code: 0,
                runtime_ms: 0,
            },
            Event::ReloadConfig { soft: true },
            Event::ConfigChange,
            Event::ClipboardRead {
                location: 0,
                mimes: 1,
                list: false,
                id: None,
                decision: "unavailable",
            },
            Event::ClipboardConfirm {
                id: RequestId(1),
                request: 0,
            },
            Event::ClipboardWrite {
                location: 0,
                mimes: Vec::new(),
                text: None,
                confirm: false,
            },
            Event::Unsupported { tag: 1 },
        ];
        let mut kinds: Vec<&str> = events.iter().map(Event::kind).collect();
        kinds.sort_unstable();
        kinds.dedup();
        assert_eq!(kinds.len(), events.len());
    }
}
