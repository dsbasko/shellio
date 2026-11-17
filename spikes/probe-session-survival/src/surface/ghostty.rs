//! Весь `unsafe` вокруг libghostty в пробе P0.4 (ADR-0034): упрощённая копия запуска из P0.3.
//!
//! Правила владения:
//! - `ghostty_init` вызывается один раз до `NSApplication` и рабочих потоков пробы;
//! - колбэки C только ставят флаг прохода и будят главный цикл, главное состояние они не трогают;
//! - поверхность освобождается в одном месте, [`Surface::drop`], до приложения: стенд держит
//!   поверхности в области видимости, которая заканчивается раньше [`App`].
//!
//! Действия runtime проба не выполняет (`action_cb` отвечает `false`), буфер обмена недоступен:
//! сценарию нужны только байты, которые поверхность пишет в PTY.

use std::ffi::{CStr, CString, c_char, c_void};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use objc2_app_kit::NSView;
use objc2_core_foundation::CFRunLoop;
use shellio_sys_ghostty as sys;

static INIT_CALLED: AtomicBool = AtomicBool::new(false);

/// Код клавиши macOS «стрелка вверх» (`kVK_UpArrow`).
pub const KEY_UP: u32 = 0x7e;

/// Вызывает `ghostty_init` один раз за процесс. `argv` — собственный массив из имени программы,
/// чтобы флаги пробы не стали настройками ghostty; строки живут до конца процесса.
pub fn init_once(program: &str) -> Result<(), String> {
    if INIT_CALLED.swap(true, Ordering::AcqRel) {
        return Err("ghostty_init уже вызван в этом процессе".to_owned());
    }
    let name = c_string("имя программы", program)?;
    let name: &'static CStr = Box::leak(name.into_boxed_c_str());
    let argv: &'static mut [*mut c_char; 2] =
        Box::leak(Box::new([name.as_ptr().cast_mut(), std::ptr::null_mut()]));
    // SAFETY: один вызов за процесс до потоков пробы; `argv` из одной строки с нулевым
    // завершением живёт до конца процесса.
    let code = unsafe { sys::ghostty_init(1, argv.as_mut_ptr()) };
    if code != 0 {
        return Err(format!("ghostty_init вернул {code}"));
    }
    Ok(())
}

fn c_string(what: &str, text: &str) -> Result<CString, String> {
    CString::new(text).map_err(|_| format!("{what}: внутренний нулевой байт"))
}

/// Конфигурация из файла `run.sh` в `--out` сценария; пользовательский `ghostty.conf` не читается.
pub struct Config {
    raw: sys::ghostty_config_t,
}

impl Config {
    pub fn load(path: &Path) -> Result<Config, String> {
        let text = path.to_str().ok_or("путь конфигурации не в UTF-8")?;
        let path = c_string("путь конфигурации", text)?;
        // SAFETY: функция без аргументов, NULL проверяется.
        let raw = unsafe { sys::ghostty_config_new() };
        if raw.is_null() {
            return Err("ghostty_config_new вернул NULL".to_owned());
        }
        let config = Config { raw };
        // SAFETY: конфигурация жива, путь — строка с нулём на время вызова.
        unsafe {
            sys::ghostty_config_load_file(config.raw, path.as_ptr());
            sys::ghostty_config_finalize(config.raw);
        }
        Ok(config)
    }

    /// Сообщения `ghostty_config_get_diagnostic`.
    pub fn diagnostics(&self) -> Vec<String> {
        // SAFETY: конфигурация жива.
        let count = unsafe { sys::ghostty_config_diagnostics_count(self.raw) };
        (0..count)
            .map(|index| {
                // SAFETY: индекс меньше числа диагностик, сообщение живёт вместе с конфигурацией.
                let diagnostic = unsafe { sys::ghostty_config_get_diagnostic(self.raw, index) };
                if diagnostic.message.is_null() {
                    return "<пустое сообщение>".to_owned();
                }
                // SAFETY: сообщение — строка с нулём, живая вместе с конфигурацией.
                unsafe { CStr::from_ptr(diagnostic.message) }
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    }
}

impl Drop for Config {
    fn drop(&mut self) {
        // SAFETY: конфигурация принадлежит этой структуре и освобождается один раз.
        unsafe { sys::ghostty_config_free(self.raw) };
    }
}

/// Общая часть приложения: её указатель — `userdata` приложения, живой до `ghostty_app_free`.
#[derive(Debug, Default)]
struct Shared {
    wake: AtomicBool,
    wakeups: AtomicU64,
    actions: AtomicU64,
}

/// Приложение libghostty. Освобождается после всех поверхностей.
pub struct App {
    raw: sys::ghostty_app_t,
    shared: Box<Shared>,
    _config: Config,
}

impl App {
    pub fn new(config: Config) -> Result<App, String> {
        let shared = Box::new(Shared::default());
        let runtime = sys::ghostty_runtime_config_s {
            userdata: std::ptr::from_ref::<Shared>(&shared).cast_mut().cast(),
            supports_selection_clipboard: false,
            wakeup_cb: Some(wakeup_cb),
            action_cb: Some(action_cb),
            read_clipboard_cb: Some(read_clipboard_cb),
            confirm_read_clipboard_cb: Some(confirm_read_clipboard_cb),
            write_clipboard_cb: Some(write_clipboard_cb),
            close_surface_cb: Some(close_surface_cb),
        };
        // SAFETY: конфигурация жива; `userdata` указывает на `Shared` в куче, который живёт до
        // `ghostty_app_free` в `drop`.
        let raw = unsafe { sys::ghostty_app_new(&runtime, config.raw) };
        if raw.is_null() {
            return Err("ghostty_app_new вернул NULL".to_owned());
        }
        Ok(App {
            raw,
            shared,
            _config: config,
        })
    }

    /// Проход приложения: почта поверхностей к приложению. Снимает флаг до `tick`, поэтому
    /// `wakeup` во время прохода назначает следующий.
    pub fn tick(&self) {
        self.shared.wake.store(false, Ordering::Release);
        // SAFETY: приложение живо, вызов на главном потоке.
        unsafe { sys::ghostty_app_tick(self.raw) };
    }

    /// Библиотека просила проход после последнего `tick`.
    pub fn wants_tick(&self) -> bool {
        self.shared.wake.load(Ordering::Acquire)
    }

    pub fn counts(&self) -> (u64, u64) {
        (
            self.shared.wakeups.load(Ordering::Relaxed),
            self.shared.actions.load(Ordering::Relaxed),
        )
    }
}

impl Drop for App {
    fn drop(&mut self) {
        // SAFETY: приложение принадлежит этой структуре, поверхности освобождены раньше.
        unsafe { sys::ghostty_app_free(self.raw) };
    }
}

unsafe extern "C" fn wakeup_cb(userdata: *mut c_void) {
    // SAFETY: `userdata` приложения — `Shared`, живой до возврата `ghostty_app_free`.
    if let Some(shared) = unsafe { userdata.cast::<Shared>().cast_const().as_ref() } {
        shared.wakeups.fetch_add(1, Ordering::Relaxed);
        shared.wake.store(true, Ordering::Release);
    }
    if let Some(main) = CFRunLoop::main() {
        main.wake_up();
    }
}

unsafe extern "C" fn action_cb(
    app: sys::ghostty_app_t,
    _target: sys::ghostty_target_s,
    _action: sys::ghostty_action_s,
) -> bool {
    // SAFETY: приложение живо, пока библиотека зовёт его колбэк; `userdata` — `Shared`.
    let shared = unsafe {
        sys::ghostty_app_userdata(app)
            .cast::<Shared>()
            .cast_const()
            .as_ref()
    };
    if let Some(shared) = shared {
        shared.actions.fetch_add(1, Ordering::Relaxed);
    }
    false
}

unsafe extern "C" fn read_clipboard_cb(
    _userdata: *mut c_void,
    _location: sys::ghostty_clipboard_e,
    _state: *mut c_void,
    _mimes: *const *const c_char,
    _mimes_len: usize,
    _list: bool,
) -> sys::ghostty_clipboard_read_result_e {
    sys::ghostty_clipboard_read_result_e_GHOSTTY_CLIPBOARD_READ_UNAVAILABLE
}

unsafe extern "C" fn confirm_read_clipboard_cb(
    _userdata: *mut c_void,
    _confirm: *const sys::ghostty_clipboard_confirm_s,
    _state: *mut c_void,
    _request: sys::ghostty_clipboard_request_e,
) {
}

unsafe extern "C" fn write_clipboard_cb(
    _userdata: *mut c_void,
    _location: sys::ghostty_clipboard_e,
    _contents: *const sys::ghostty_clipboard_content_s,
    _contents_len: usize,
    _confirm: bool,
) {
}

unsafe extern "C" fn close_surface_cb(_userdata: *mut c_void, _process_alive: bool) {}

/// Размер поверхности по `ghostty_surface_size`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct GridSize {
    pub columns: u16,
    pub rows: u16,
    pub width_px: u32,
    pub height_px: u32,
    pub cell_width_px: u32,
    pub cell_height_px: u32,
}

/// Что запускает поверхность.
#[derive(Clone, Debug, PartialEq)]
pub struct Spec {
    /// Команда для shell libghostty (`exec -l` она добавляет сама).
    pub command: String,
    pub working_directory: String,
    pub scale: f64,
}

/// Строки C параметров поверхности: живут до освобождения поверхности.
struct Buffers {
    command: CString,
    working_directory: CString,
}

/// Живая поверхность. Освобождается только в `drop`, до приложения, из которого создана.
pub struct Surface {
    raw: sys::ghostty_surface_t,
    _buffers: Buffers,
}

impl Surface {
    /// `view` должен жить дольше поверхности: libghostty ставит ему свой слой.
    pub fn new(app: &App, view: &NSView, spec: &Spec) -> Result<Surface, String> {
        let buffers = Buffers {
            command: c_string("команда поверхности", &spec.command)?,
            working_directory: c_string("рабочий каталог", &spec.working_directory)?,
        };
        // SAFETY: функция без аргументов возвращает значения по умолчанию.
        let mut config = unsafe { sys::ghostty_surface_config_new() };
        config.platform_tag = sys::ghostty_platform_e_GHOSTTY_PLATFORM_MACOS as _;
        config.platform = sys::ghostty_platform_u {
            macos: sys::ghostty_platform_macos_s {
                nsview: std::ptr::from_ref::<NSView>(view).cast_mut().cast(),
            },
        };
        config.userdata = std::ptr::null_mut();
        config.scale_factor = spec.scale;
        config.font_size = 0.0;
        config.working_directory = buffers.working_directory.as_ptr();
        config.command = buffers.command.as_ptr();
        config.env_vars = std::ptr::null_mut();
        config.env_var_count = 0;
        config.initial_input = std::ptr::null();
        config.wait_after_command = false;
        config.context = sys::ghostty_surface_context_e_GHOSTTY_SURFACE_CONTEXT_WINDOW;
        // SAFETY: приложение живо, строки параметров живут в `buffers` до освобождения
        // поверхности, вид жив дольше поверхности по контракту функции.
        let raw = unsafe { sys::ghostty_surface_new(app.raw, &config) };
        if raw.is_null() {
            return Err("ghostty_surface_new вернул NULL".to_owned());
        }
        Ok(Surface {
            raw,
            _buffers: buffers,
        })
    }

    pub fn set_size(&self, width: u32, height: u32) {
        // SAFETY: поверхность жива, пока жива структура.
        unsafe { sys::ghostty_surface_set_size(self.raw, width, height) }
    }

    pub fn set_content_scale(&self, scale: f64) {
        // SAFETY: поверхность жива.
        unsafe { sys::ghostty_surface_set_content_scale(self.raw, scale, scale) }
    }

    pub fn set_focus(&self, focused: bool) {
        // SAFETY: поверхность жива.
        unsafe { sys::ghostty_surface_set_focus(self.raw, focused) }
    }

    /// Параметр `ghostty_surface_set_occlusion` означает `visible`.
    pub fn set_visible(&self, visible: bool) {
        // SAFETY: поверхность жива.
        unsafe { sys::ghostty_surface_set_occlusion(self.raw, visible) }
    }

    pub fn set_dark(&self, dark: bool) {
        let scheme = if dark {
            sys::ghostty_color_scheme_e_GHOSTTY_COLOR_SCHEME_DARK
        } else {
            sys::ghostty_color_scheme_e_GHOSTTY_COLOR_SCHEME_LIGHT
        };
        // SAFETY: поверхность жива.
        unsafe { sys::ghostty_surface_set_color_scheme(self.raw, scheme) }
    }

    /// Текст как вставка: `ghostty_surface_text` идёт в `completeClipboardPaste`, поэтому при
    /// включённом 2004 поверхность оборачивает его в `CSI 200 ~` и `CSI 201 ~`.
    pub fn paste(&self, text: &str) {
        // SAFETY: поверхность жива, байты живы на время вызова.
        unsafe { sys::ghostty_surface_text(self.raw, text.as_ptr().cast(), text.len()) }
    }

    /// Нажатие и отпускание клавиши без модификаторов и текста.
    pub fn tap(&self, keycode: u32) -> (bool, bool) {
        let key = |action| sys::ghostty_input_key_s {
            action,
            mods: sys::ghostty_input_mods_e_GHOSTTY_MODS_NONE,
            consumed_mods: sys::ghostty_input_mods_e_GHOSTTY_MODS_NONE,
            keycode,
            text: std::ptr::null(),
            unshifted_codepoint: 0,
            composing: false,
        };
        // SAFETY: поверхность жива, структура клавиши без указателей на данные.
        let press = unsafe {
            sys::ghostty_surface_key(
                self.raw,
                key(sys::ghostty_input_action_e_GHOSTTY_ACTION_PRESS),
            )
        };
        // SAFETY: см. выше.
        let release = unsafe {
            sys::ghostty_surface_key(
                self.raw,
                key(sys::ghostty_input_action_e_GHOSTTY_ACTION_RELEASE),
            )
        };
        (press, release)
    }

    pub fn size(&self) -> GridSize {
        // SAFETY: поверхность жива.
        let size = unsafe { sys::ghostty_surface_size(self.raw) };
        GridSize {
            columns: size.columns,
            rows: size.rows,
            width_px: size.width_px,
            height_px: size.height_px,
            cell_width_px: size.cell_width_px,
            cell_height_px: size.cell_height_px,
        }
    }

    /// Текст видимой области модели терминала по `ghostty_surface_read_text`.
    pub fn viewport_text(&self) -> Option<String> {
        let point = |coord| sys::ghostty_point_s {
            tag: sys::ghostty_point_tag_e_GHOSTTY_POINT_VIEWPORT,
            coord,
            x: 0,
            y: 0,
        };
        let selection = sys::ghostty_selection_s {
            top_left: point(sys::ghostty_point_coord_e_GHOSTTY_POINT_COORD_TOP_LEFT),
            bottom_right: point(sys::ghostty_point_coord_e_GHOSTTY_POINT_COORD_BOTTOM_RIGHT),
            rectangle: false,
        };
        let mut text = sys::ghostty_text_s {
            tl_px_x: 0.0,
            tl_px_y: 0.0,
            offset_start: 0,
            offset_len: 0,
            text: std::ptr::null(),
            text_len: 0,
        };
        // SAFETY: поверхность жива; `text` живёт на время вызова.
        if !unsafe { sys::ghostty_surface_read_text(self.raw, selection, &mut text) } {
            return None;
        }
        let copy = if text.text.is_null() {
            String::new()
        } else {
            // SAFETY: успешное чтение заполнило `text` строкой библиотеки длины `text_len`.
            let bytes =
                unsafe { std::slice::from_raw_parts(text.text.cast::<u8>(), text.text_len) };
            String::from_utf8_lossy(bytes).into_owned()
        };
        // SAFETY: текст выделен этим чтением и освобождается один раз.
        unsafe { sys::ghostty_surface_free_text(self.raw, &mut text) };
        Some(copy)
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        // SAFETY: единственное место освобождения; поверхность принадлежит структуре, приложение
        // живо (стенд освобождает поверхности раньше него), вызов на главном потоке.
        unsafe { sys::ghostty_surface_free(self.raw) };
    }
}
