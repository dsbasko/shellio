//! Реестр поверхностей на главном потоке: `SurfaceId` и поколение, создание только при ненулевом
//! размере в пикселях, буферы C-строк до освобождения и единственная функция освобождения
//! [`release`].
//!
//! Жизненный цикл — модель [`Registry`] без libghostty, она же стоит под настоящим реестром:
//! ожидание размера → живая → закрывается (намерение принято) → освобождена на следующем проходе
//! главного потока. Сообщение для поколения, которого больше нет, отбрасывается.

use std::collections::BTreeMap;
use std::ffi::{CString, c_void};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use objc2::rc::Retained;
use shellio_sys_ghostty as sys;

use super::clipboard::{self, Requests, Token};
use super::runtime::{Event, Message, Shared, Target};
use crate::clock;
use crate::keys::KeyInput;
use crate::view::SurfaceView;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SurfaceId(pub u32);

/// Поверхность конкретного поколения: сообщения колбэков несут этот ключ.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SurfaceKey {
    pub id: SurfaceId,
    pub generation: u32,
}

impl fmt::Display for SurfaceKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "s{}g{}", self.id.0, self.generation)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    WaitingSize,
    Live,
    Closing,
    Freed,
}

impl Phase {
    pub fn name(self) -> &'static str {
        match self {
            Phase::WaitingSize => "waiting-size",
            Phase::Live => "live",
            Phase::Closing => "closing",
            Phase::Freed => "freed",
        }
    }
}

enum State<H> {
    WaitingSize,
    Live(H),
    Closing { handle: H, since_pass: u64 },
    Freed,
}

struct Slot<H> {
    generation: u32,
    state: State<H>,
}

/// Решение о создании поверхности по размеру вида.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Размер в пикселях нулевой: создание ждёт.
    Wait,
    Create(SurfaceKey),
    /// Поверхность уже есть или закрывается.
    Exists(Phase),
}

/// Куда идёт сообщение колбэка.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route<H> {
    Live(H),
    Closing(H),
    /// Поколения нет: сообщение отброшено.
    Stale,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Close {
    /// Первое намерение: освобождение на следующем проходе.
    Scheduled,
    /// Поверхность уже закрывается: второе намерение ничего не меняет.
    AlreadyClosing,
    Stale,
}

/// Модель жизненного цикла поверхностей. `H` — ручка поверхности.
pub struct Registry<H> {
    slots: Vec<Slot<H>>,
    created: usize,
    freed: usize,
    dropped: usize,
    close_intents: usize,
}

impl<H> Default for Registry<H> {
    fn default() -> Registry<H> {
        Registry {
            slots: Vec::new(),
            created: 0,
            freed: 0,
            dropped: 0,
            close_intents: 0,
        }
    }
}

impl<H: Copy> Registry<H> {
    pub fn add(&mut self) -> SurfaceId {
        let id = SurfaceId(self.slots.len() as u32);
        self.slots.push(Slot {
            generation: 0,
            state: State::WaitingSize,
        });
        id
    }

    pub fn phase(&self, id: SurfaceId) -> Option<Phase> {
        self.slots.get(id.0 as usize).map(|slot| match slot.state {
            State::WaitingSize => Phase::WaitingSize,
            State::Live(_) => Phase::Live,
            State::Closing { .. } => Phase::Closing,
            State::Freed => Phase::Freed,
        })
    }

    /// Создание разрешено только при ненулевом размере в пикселях и только из ожидания или после
    /// освобождения; новое поколение больше прежнего.
    pub fn decide(&self, id: SurfaceId, pixels: (u32, u32)) -> Result<Decision, String> {
        let slot = self.slot(id)?;
        match slot.state {
            State::WaitingSize | State::Freed if pixels.0 == 0 || pixels.1 == 0 => {
                Ok(Decision::Wait)
            }
            State::WaitingSize | State::Freed => Ok(Decision::Create(SurfaceKey {
                id,
                generation: slot.generation + 1,
            })),
            State::Live(_) => Ok(Decision::Exists(Phase::Live)),
            State::Closing { .. } => Ok(Decision::Exists(Phase::Closing)),
        }
    }

    pub fn created(&mut self, key: SurfaceKey, handle: H) -> Result<(), String> {
        let slot = self.slot_mut(key.id)?;
        if !matches!(slot.state, State::WaitingSize | State::Freed)
            || key.generation != slot.generation + 1
        {
            return Err(format!("поверхность {key} создана вне решения реестра"));
        }
        slot.generation = key.generation;
        slot.state = State::Live(handle);
        self.created += 1;
        Ok(())
    }

    /// Живая поверхность и её ключ.
    pub fn current(&self, id: SurfaceId) -> Option<(SurfaceKey, H)> {
        let slot = self.slots.get(id.0 as usize)?;
        match slot.state {
            State::Live(handle) => Some((
                SurfaceKey {
                    id,
                    generation: slot.generation,
                },
                handle,
            )),
            _ => None,
        }
    }

    /// Все живые поверхности по порядку слотов.
    pub fn live_handles(&self) -> Vec<(SurfaceKey, H)> {
        (0..self.slots.len())
            .filter_map(|index| self.current(SurfaceId(index as u32)))
            .collect()
    }

    pub fn route(&mut self, key: SurfaceKey) -> Route<H> {
        let route = match self.slots.get(key.id.0 as usize) {
            Some(slot) if slot.generation == key.generation => match slot.state {
                State::Live(handle) => Route::Live(handle),
                State::Closing { handle, .. } => Route::Closing(handle),
                State::WaitingSize | State::Freed => Route::Stale,
            },
            _ => Route::Stale,
        };
        if matches!(route, Route::Stale) {
            self.dropped += 1;
        }
        route
    }

    pub fn close_intent(&mut self, key: SurfaceKey, pass: u64) -> Close {
        self.close_intents += 1;
        match self.route(key) {
            Route::Live(handle) => {
                if let Some(slot) = self.slots.get_mut(key.id.0 as usize) {
                    slot.state = State::Closing {
                        handle,
                        since_pass: pass,
                    };
                }
                Close::Scheduled
            }
            Route::Closing(_) => Close::AlreadyClosing,
            Route::Stale => Close::Stale,
        }
    }

    /// Поверхности, намерение закрыть которые принято на прошлых проходах. Каждая ручка
    /// возвращается ровно один раз.
    pub fn take_due(&mut self, pass: u64) -> Vec<(SurfaceKey, H)> {
        self.take_where(|since_pass| since_pass < pass, false)
    }

    /// Все живые и закрывающиеся поверхности: завершение перед `ghostty_app_free`.
    pub fn take_all(&mut self) -> Vec<(SurfaceKey, H)> {
        self.take_where(|_| true, true)
    }

    pub fn created_count(&self) -> usize {
        self.created
    }

    pub fn freed_count(&self) -> usize {
        self.freed
    }

    pub fn dropped_count(&self) -> usize {
        self.dropped
    }

    pub fn close_intent_count(&self) -> usize {
        self.close_intents
    }

    fn take_where(&mut self, due: impl Fn(u64) -> bool, live_too: bool) -> Vec<(SurfaceKey, H)> {
        let mut taken = Vec::new();
        for (index, slot) in self.slots.iter_mut().enumerate() {
            let handle = match slot.state {
                State::Closing { handle, since_pass } if due(since_pass) => handle,
                State::Live(handle) if live_too => handle,
                _ => continue,
            };
            slot.state = State::Freed;
            taken.push((
                SurfaceKey {
                    id: SurfaceId(index as u32),
                    generation: slot.generation,
                },
                handle,
            ));
        }
        self.freed += taken.len();
        taken
    }

    fn slot(&self, id: SurfaceId) -> Result<&Slot<H>, String> {
        self.slots
            .get(id.0 as usize)
            .ok_or(format!("нет поверхности s{}", id.0))
    }

    fn slot_mut(&mut self, id: SurfaceId) -> Result<&mut Slot<H>, String> {
        self.slots
            .get_mut(id.0 as usize)
            .ok_or(format!("нет поверхности s{}", id.0))
    }
}

// ---- libghostty ----

/// Ручка живой поверхности. Её методы вызываются только пока поверхность жива: вид снимает ручку
/// в [`release`] до `ghostty_surface_free`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Handle(sys::ghostty_surface_t);

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

impl Handle {
    /// Адрес поверхности для сверки идентичности; не разыменовывается.
    pub fn addr(self) -> usize {
        self.0 as usize
    }

    /// # Safety
    /// Поверхность жива.
    pub unsafe fn set_size(self, width: u32, height: u32) {
        // SAFETY: по контракту поверхность жива.
        unsafe { sys::ghostty_surface_set_size(self.0, width, height) }
    }

    /// # Safety
    /// Поверхность жива.
    pub unsafe fn refresh(self) {
        // SAFETY: по контракту поверхность жива.
        unsafe { sys::ghostty_surface_refresh(self.0) }
    }

    /// # Safety
    /// Поверхность жива.
    pub unsafe fn set_content_scale(self, x: f64, y: f64) {
        // SAFETY: по контракту поверхность жива.
        unsafe { sys::ghostty_surface_set_content_scale(self.0, x, y) }
    }

    /// # Safety
    /// Поверхность жива.
    pub unsafe fn set_focus(self, focused: bool) {
        // SAFETY: по контракту поверхность жива.
        unsafe { sys::ghostty_surface_set_focus(self.0, focused) }
    }

    /// Условное состояние темы поверхности; ядро просит `reload_config` поверхности.
    ///
    /// # Safety
    /// Поверхность жива, вызов вне заимствования состояния runtime.
    pub unsafe fn set_color_scheme(self, scheme: sys::ghostty_color_scheme_e) {
        // SAFETY: по контракту поверхность жива.
        unsafe { sys::ghostty_surface_set_color_scheme(self.0, scheme) }
    }

    /// Мягкая перезагрузка: поверхность применяет к конфигурации своё условное состояние.
    ///
    /// # Safety
    /// Поверхность и конфигурация живы, вызов вне заимствования состояния runtime.
    pub unsafe fn update_config(self, config: sys::ghostty_config_t) {
        // SAFETY: по контракту поверхность и конфигурация живы.
        unsafe { sys::ghostty_surface_update_config(self.0, config) }
    }

    /// # Safety
    /// Поверхность жива.
    pub unsafe fn set_display_id(self, display: u32) {
        // SAFETY: по контракту поверхность жива.
        unsafe { sys::ghostty_surface_set_display_id(self.0, display) }
    }

    /// `tcgetpgrp` мастера PTY: группа переднего плана, а не PID ребёнка.
    ///
    /// # Safety
    /// Поверхность жива.
    pub unsafe fn foreground_pid(self) -> u64 {
        // SAFETY: по контракту поверхность жива.
        unsafe { sys::ghostty_surface_foreground_pid(self.0) }
    }

    /// # Safety
    /// Поверхность жива.
    pub unsafe fn tty_name(self) -> Option<String> {
        // SAFETY: по контракту поверхность жива; строку освобождает `take_string`.
        super::take_string(unsafe { sys::ghostty_surface_tty_name(self.0) })
    }

    /// # Safety
    /// Поверхность жива.
    pub unsafe fn size(self) -> GridSize {
        // SAFETY: по контракту поверхность жива.
        let size = unsafe { sys::ghostty_surface_size(self.0) };
        GridSize {
            columns: size.columns,
            rows: size.rows,
            width_px: size.width_px,
            height_px: size.height_px,
            cell_width_px: size.cell_width_px,
            cell_height_px: size.cell_height_px,
        }
    }

    /// Текст видимой области модели терминала.
    ///
    /// # Safety
    /// Поверхность жива.
    pub unsafe fn viewport_text(self) -> Option<String> {
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
        let mut text = empty_text();
        // SAFETY: по контракту поверхность жива; `text` живёт на время вызова.
        if !unsafe { sys::ghostty_surface_read_text(self.0, selection, &mut text) } {
            return None;
        }
        // SAFETY: успешное чтение заполнило `text` строкой библиотеки длины `text_len`.
        let copy = unsafe { super::copy_bytes(text.text, text.text_len) };
        // SAFETY: текст выделен этим чтением и освобождается один раз.
        unsafe { sys::ghostty_surface_free_text(self.0, &mut text) };
        copy
    }

    /// # Safety
    /// Поверхность жива, `token` принадлежит ей и ещё не завершён.
    pub unsafe fn deny_clipboard(self, token: Token) {
        // SAFETY: по контракту токен принадлежит этой живой поверхности.
        unsafe { sys::ghostty_surface_deny_clipboard_request(self.0, token as *mut c_void) }
    }

    /// Завершает чтение текстом доски; без текста — пустым ответом.
    ///
    /// # Safety
    /// Поверхность жива, `token` получен её колбэком чтения, вернувшим `STARTED`, и не завершён.
    pub unsafe fn complete_clipboard(self, token: Token, text: Option<&str>) {
        let data = text
            .map(|text| text.as_bytes().to_vec())
            .unwrap_or_default();
        let content = sys::ghostty_clipboard_content_s {
            mime: c"text/plain".as_ptr(),
            data: data.as_ptr().cast(),
            len: data.len(),
        };
        let available = [c"text/plain".as_ptr()];
        let complete = sys::ghostty_clipboard_complete_s {
            contents: if text.is_some() {
                &content
            } else {
                std::ptr::null()
            },
            contents_len: usize::from(text.is_some()),
            available: available.as_ptr(),
            available_len: usize::from(text.is_some()),
            confirmed: false,
            remember: false,
        };
        // SAFETY: по контракту токен принадлежит этой живой поверхности; данные живут на время
        // вызова, библиотека их не сохраняет.
        unsafe {
            sys::ghostty_surface_complete_clipboard_request(self.0, &complete, token as *mut c_void)
        }
    }

    /// Модификаторы перевода символов с учётом `macos-option-as-alt`.
    ///
    /// # Safety
    /// Поверхность жива.
    pub unsafe fn key_translation_mods(self, mods: u32) -> u32 {
        // SAFETY: по контракту поверхность жива.
        unsafe { sys::ghostty_surface_key_translation_mods(self.0, mods) }
    }

    /// # Safety
    /// Поверхность жива; вызов на главном потоке вне заимствований: библиотека синхронно зовёт
    /// колбэки действий и буфера обмена.
    pub unsafe fn key(self, input: &KeyInput) -> Result<bool, String> {
        // SAFETY: по контракту поверхность жива; текст структуры живёт на время вызова.
        input.with_raw(|raw| unsafe { sys::ghostty_surface_key(self.0, raw) })
    }

    /// Флаги привязки, если событие сейчас сработало бы как привязка.
    ///
    /// # Safety
    /// Поверхность жива.
    pub unsafe fn key_binding(self, input: &KeyInput) -> Result<Option<u32>, String> {
        let mut flags: sys::ghostty_binding_flags_e = 0;
        // SAFETY: по контракту поверхность жива; `flags` живёт на время вызова.
        let bound = input.with_raw(|raw| unsafe {
            sys::ghostty_surface_key_is_binding(self.0, raw, &mut flags)
        })?;
        Ok(bound.then_some(flags))
    }

    /// Preedit IME; пустой текст снимает его.
    ///
    /// # Safety
    /// Поверхность жива.
    pub unsafe fn preedit(self, text: &str) {
        // SAFETY: по контракту поверхность жива; байты живут на время вызова, длина явная.
        unsafe { sys::ghostty_surface_preedit(self.0, text.as_ptr().cast(), text.len()) }
    }

    /// Точка окна IME в точках вида от левого верхнего угла: x, y, ширина preedit, высота.
    ///
    /// # Safety
    /// Поверхность жива.
    pub unsafe fn ime_point(self) -> ImePoint {
        let (mut x, mut y, mut width, mut height) = (0.0, 0.0, 0.0, 0.0);
        // SAFETY: по контракту поверхность жива; указатели на локальные переменные.
        unsafe { sys::ghostty_surface_ime_point(self.0, &mut x, &mut y, &mut width, &mut height) };
        ImePoint {
            x,
            y,
            width,
            height,
        }
    }

    /// # Safety
    /// Поверхность жива; вызов на главном потоке.
    pub unsafe fn mouse_button(
        self,
        state: sys::ghostty_input_mouse_state_e,
        button: sys::ghostty_input_mouse_button_e,
        mods: u32,
    ) -> bool {
        // SAFETY: по контракту поверхность жива.
        unsafe { sys::ghostty_surface_mouse_button(self.0, state, button, mods) }
    }

    /// Положение мыши в точках вида от левого верхнего угла.
    ///
    /// # Safety
    /// Поверхность жива.
    pub unsafe fn mouse_pos(self, x: f64, y: f64, mods: u32) {
        // SAFETY: по контракту поверхность жива.
        unsafe { sys::ghostty_surface_mouse_pos(self.0, x, y, mods) }
    }

    /// # Safety
    /// Поверхность жива.
    pub unsafe fn mouse_scroll(self, x: f64, y: f64, mods: sys::ghostty_input_scroll_mods_t) {
        // SAFETY: по контракту поверхность жива.
        unsafe { sys::ghostty_surface_mouse_scroll(self.0, x, y, mods) }
    }

    /// # Safety
    /// Поверхность жива.
    pub unsafe fn mouse_pressure(self, stage: u32, pressure: f64) {
        // SAFETY: по контракту поверхность жива.
        unsafe { sys::ghostty_surface_mouse_pressure(self.0, stage, pressure) }
    }

    /// # Safety
    /// Поверхность жива.
    pub unsafe fn has_selection(self) -> bool {
        // SAFETY: по контракту поверхность жива.
        unsafe { sys::ghostty_surface_has_selection(self.0) }
    }

    /// Текст клетки видимой области и её левый верхний угол по базовой линии в точках вида:
    /// `tl_px_y` указывает на базовую линию строки, а не на верх клетки.
    ///
    /// # Safety
    /// Поверхность жива.
    pub unsafe fn cell(self, column: u32, row: u32) -> Option<Cell> {
        let point = sys::ghostty_point_s {
            tag: sys::ghostty_point_tag_e_GHOSTTY_POINT_VIEWPORT,
            coord: sys::ghostty_point_coord_e_GHOSTTY_POINT_COORD_EXACT,
            x: column,
            y: row,
        };
        let selection = sys::ghostty_selection_s {
            top_left: point,
            bottom_right: point,
            rectangle: false,
        };
        let mut text = empty_text();
        // SAFETY: по контракту поверхность жива; `text` живёт на время вызова.
        if !unsafe { sys::ghostty_surface_read_text(self.0, selection, &mut text) } {
            return None;
        }
        // SAFETY: успешное чтение заполнило `text` строкой библиотеки длины `text_len`.
        let copy = unsafe { super::copy_bytes(text.text, text.text_len) };
        let cell = Cell {
            text: copy.unwrap_or_default(),
            x: text.tl_px_x,
            baseline_y: text.tl_px_y,
        };
        // SAFETY: текст выделен этим чтением и освобождается один раз.
        unsafe { sys::ghostty_surface_free_text(self.0, &mut text) };
        Some(cell)
    }
}

fn empty_text() -> sys::ghostty_text_s {
    sys::ghostty_text_s {
        tl_px_x: 0.0,
        tl_px_y: 0.0,
        offset_start: 0,
        offset_len: 0,
        text: std::ptr::null(),
        text_len: 0,
    }
}

/// Точка IME по `ghostty_surface_ime_point`: ширина — preedit в клетках на ширину клетки.
#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize)]
pub struct ImePoint {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Клетка видимой области по `ghostty_surface_read_text`.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Cell {
    pub text: String,
    pub x: f64,
    pub baseline_y: f64,
}

/// Контекст `userdata` поверхности: живёт до возврата `ghostty_surface_free`.
pub struct SurfaceContext {
    pub key: SurfaceKey,
    pub shared: Arc<Shared>,
    /// Намерение закрыть принято: сообщения, кроме подтверждений буфера обмена, отбрасываются.
    pub retired: AtomicBool,
    /// Запросы буфера обмена, которые libghostty держит до ответа.
    pub clipboard: Arc<Requests>,
}

impl SurfaceContext {
    /// Сообщение колбэка поверхности. После снятия контекста отбрасывается всё, кроме запросов,
    /// которые обязаны получить ответ.
    pub fn push(&self, event: Event) {
        let must_answer = matches!(event, Event::ClipboardConfirm { .. });
        if self.retired.load(Ordering::Acquire) && !must_answer {
            self.shared.dropped_at_source();
            return;
        }
        self.shared.push(Message {
            target: Target::Surface(self.key),
            t_ns: clock::uptime_ns(),
            event,
        });
    }
}

/// Что запускает поверхность.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spec {
    /// Команда для shell libghostty (`exec -l` она добавляет сама).
    pub command: String,
    pub working_directory: String,
    pub env: Vec<(String, String)>,
    pub wait_after_command: bool,
}

/// Строки C параметров поверхности: живут до освобождения поверхности.
struct Buffers {
    command: CString,
    working_directory: CString,
    _env_strings: Vec<CString>,
    env_vars: Vec<sys::ghostty_env_var_s>,
}

impl Buffers {
    fn new(spec: &Spec) -> Result<Buffers, String> {
        let mut env_strings = Vec::new();
        let mut env_vars = Vec::new();
        for (name, value) in &spec.env {
            let name = super::c_string("имя переменной поверхности", name)?;
            let value = super::c_string("значение переменной поверхности", value)?;
            env_vars.push(sys::ghostty_env_var_s {
                key: name.as_ptr(),
                value: value.as_ptr(),
            });
            env_strings.push(name);
            env_strings.push(value);
        }
        Ok(Buffers {
            command: super::c_string("команда поверхности", &spec.command)?,
            working_directory: super::c_string("рабочий каталог", &spec.working_directory)?,
            _env_strings: env_strings,
            env_vars,
        })
    }
}

struct Entry {
    view: Retained<SurfaceView>,
    spec: Spec,
    context: Option<Box<SurfaceContext>>,
    buffers: Option<Buffers>,
}

/// Подготовленное создание: параметры указывают в `context` и `buffers`, которые лежат в куче и
/// при перемещении структуры не двигаются.
pub struct Prepared {
    pub key: SurfaceKey,
    pub config: sys::ghostty_surface_config_s,
    context: Box<SurfaceContext>,
    buffers: Buffers,
}

/// Поверхность, снятая с реестра для освобождения.
pub struct Released {
    pub key: SurfaceKey,
    handle: Handle,
    view: Retained<SurfaceView>,
    context: Option<Box<SurfaceContext>>,
    buffers: Option<Buffers>,
}

/// Итог освобождения.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct ReleaseRecord {
    pub key: String,
    pub free_ms: f64,
    /// Запросы буфера обмена, отклонённые последней проверкой перед `free`.
    pub clipboard_denied_before_free: usize,
    pub clipboard: Option<clipboard::Stats>,
}

/// Настоящий реестр: модель жизненного цикла и данные каждой поверхности.
#[derive(Default)]
pub struct Surfaces {
    pub registry: Registry<Handle>,
    entries: BTreeMap<SurfaceId, Entry>,
}

impl Surfaces {
    pub fn add(&mut self, view: Retained<SurfaceView>, spec: Spec) -> SurfaceId {
        let id = self.registry.add();
        self.entries.insert(
            id,
            Entry {
                view,
                spec,
                context: None,
                buffers: None,
            },
        );
        id
    }

    /// Параметры `ghostty_surface_new`, если реестр разрешает создание.
    pub fn prepare(
        &self,
        id: SurfaceId,
        shared: &Arc<Shared>,
    ) -> Result<Result<Prepared, Decision>, String> {
        let entry = self
            .entries
            .get(&id)
            .ok_or(format!("нет поверхности s{}", id.0))?;
        let key = match self.registry.decide(id, entry.view.pixel_size())? {
            Decision::Create(key) => key,
            other => return Ok(Err(other)),
        };
        let context = Box::new(SurfaceContext {
            key,
            shared: Arc::clone(shared),
            retired: AtomicBool::new(false),
            clipboard: Arc::new(Requests::default()),
        });
        let mut buffers = Buffers::new(&entry.spec)?;
        // SAFETY: функция без аргументов возвращает значения по умолчанию.
        let mut config = unsafe { sys::ghostty_surface_config_new() };
        config.platform_tag = sys::ghostty_platform_e_GHOSTTY_PLATFORM_MACOS as _;
        config.platform = sys::ghostty_platform_u {
            macos: sys::ghostty_platform_macos_s {
                nsview: Retained::as_ptr(&entry.view).cast_mut().cast(),
            },
        };
        config.userdata = std::ptr::from_ref::<SurfaceContext>(&context)
            .cast_mut()
            .cast();
        config.scale_factor = entry.view.backing_scale();
        config.font_size = 0.0;
        config.working_directory = buffers.working_directory.as_ptr();
        config.command = buffers.command.as_ptr();
        config.env_vars = buffers.env_vars.as_mut_ptr();
        config.env_var_count = buffers.env_vars.len();
        config.initial_input = std::ptr::null();
        config.wait_after_command = entry.spec.wait_after_command;
        config.context = sys::ghostty_surface_context_e_GHOSTTY_SURFACE_CONTEXT_WINDOW;
        Ok(Ok(Prepared {
            key,
            config,
            context,
            buffers,
        }))
    }

    /// Принимает результат `ghostty_surface_new`. NULL оставляет поверхность в ожидании.
    pub fn commit(
        &mut self,
        prepared: Prepared,
        raw: sys::ghostty_surface_t,
    ) -> Result<(Retained<SurfaceView>, Handle), String> {
        let key = prepared.key;
        if raw.is_null() {
            return Err(format!("ghostty_surface_new вернул NULL для {key}"));
        }
        let handle = Handle(raw);
        self.registry.created(key, handle)?;
        let entry = self
            .entries
            .get_mut(&key.id)
            .ok_or(format!("нет поверхности {key}"))?;
        entry.context = Some(prepared.context);
        entry.buffers = Some(prepared.buffers);
        Ok((entry.view.clone(), handle))
    }

    /// Снимает контекст: намерение закрыть принято. Возвращает реестр буфера обмена, ожидающие
    /// запросы которого отклоняются, пока поверхность жива.
    pub fn retire(&self, key: SurfaceKey) -> Option<Arc<Requests>> {
        let context = self.context(key)?;
        context.retired.store(true, Ordering::Release);
        Some(Arc::clone(&context.clipboard))
    }

    /// Реестр буфера обмена поверхности этого поколения.
    pub fn clipboard(&self, key: SurfaceKey) -> Option<Arc<Requests>> {
        self.context(key)
            .map(|context| Arc::clone(&context.clipboard))
    }

    fn context(&self, key: SurfaceKey) -> Option<&SurfaceContext> {
        self.entries
            .get(&key.id)
            .and_then(|entry| entry.context.as_deref())
            .filter(|context| context.key == key)
    }

    pub fn take_due(&mut self, pass: u64) -> Vec<Released> {
        let due = self.registry.take_due(pass);
        self.detach(due)
    }

    pub fn take_all(&mut self) -> Vec<Released> {
        let all = self.registry.take_all();
        self.detach(all)
    }

    fn detach(&mut self, taken: Vec<(SurfaceKey, Handle)>) -> Vec<Released> {
        taken
            .into_iter()
            .filter_map(|(key, handle)| {
                let entry = self.entries.get_mut(&key.id)?;
                Some(Released {
                    key,
                    handle,
                    view: entry.view.clone(),
                    context: entry.context.take(),
                    buffers: entry.buffers.take(),
                })
            })
            .collect()
    }
}

/// Единственное место освобождения поверхности. Вызывается на главном потоке вне стека `tick` и
/// `ghostty_surface_key`, без заимствования состояния runtime: `free` ждёт рабочие потоки и
/// синхронно зовёт колбэки.
pub fn release(released: Released) -> ReleaseRecord {
    released.view.detach();
    // Ожидающие запросы буфера обмена отклоняются на ещё живой поверхности ровно один раз.
    let handle = released.handle;
    let requests = released
        .context
        .as_ref()
        .map(|context| Arc::clone(&context.clipboard));
    let clipboard_denied_before_free = requests.as_ref().map_or(0, |requests| {
        // SAFETY: токены реестра принадлежат этой поверхности, она ещё не освобождена.
        requests.close(|token| unsafe { handle.deny_clipboard(token) })
    });
    let started = clock::uptime_ns();
    // SAFETY: ручка снята с реестра ровно один раз; вид её больше не держит.
    unsafe { sys::ghostty_surface_free(released.handle.0) };
    let finished = clock::uptime_ns();
    // Контекст и строки нужны библиотеке до возврата `free`.
    drop(released.context);
    drop(released.buffers);
    ReleaseRecord {
        key: released.key.to_string(),
        free_ms: clock::interval_ms(started, finished),
        clipboard_denied_before_free,
        clipboard: requests.map(|requests| requests.stats()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: (u32, u32) = (2360, 1440);

    fn live(registry: &mut Registry<u32>, handle: u32) -> SurfaceKey {
        let id = registry.add();
        let Ok(Decision::Create(key)) = registry.decide(id, SIZE) else {
            panic!("ожидалось создание");
        };
        registry.created(key, handle).unwrap();
        key
    }

    #[test]
    fn lifecycle_waits_for_size_then_lives_closes_and_frees_next_pass() {
        let mut registry = Registry::<u32>::default();
        let id = registry.add();
        assert_eq!(registry.phase(id), Some(Phase::WaitingSize));
        assert_eq!(registry.decide(id, (0, 0)), Ok(Decision::Wait));
        assert_eq!(registry.decide(id, (100, 0)), Ok(Decision::Wait));
        let Ok(Decision::Create(key)) = registry.decide(id, SIZE) else {
            panic!("ожидалось создание");
        };
        assert_eq!(key.generation, 1);
        registry.created(key, 7).unwrap();
        assert_eq!(registry.phase(id), Some(Phase::Live));
        assert_eq!(registry.current(id), Some((key, 7)));
        assert_eq!(registry.decide(id, SIZE), Ok(Decision::Exists(Phase::Live)));

        assert_eq!(registry.close_intent(key, 3), Close::Scheduled);
        assert_eq!(registry.phase(id), Some(Phase::Closing));
        assert_eq!(registry.current(id), None);
        assert!(registry.take_due(3).is_empty(), "не на том же проходе");
        assert_eq!(registry.take_due(4), [(key, 7)]);
        assert_eq!(registry.phase(id), Some(Phase::Freed));
        assert_eq!(registry.created_count(), 1);
        assert_eq!(registry.freed_count(), 1);

        let Ok(Decision::Create(again)) = registry.decide(id, SIZE) else {
            panic!("после освобождения создание снова разрешено");
        };
        assert_eq!(again.generation, 2);
    }

    #[test]
    fn creation_outside_a_decision_is_rejected() {
        let mut registry = Registry::<u32>::default();
        let id = registry.add();
        let stale = SurfaceKey { id, generation: 5 };
        assert!(registry.created(stale, 1).is_err());
        assert_eq!(registry.phase(id), Some(Phase::WaitingSize));
        let key = live(&mut registry, 2);
        assert!(
            registry.created(key, 3).is_err(),
            "живая не создаётся снова"
        );
        assert!(
            registry
                .decide(SurfaceId(9), SIZE)
                .is_err_and(|e| e.contains("s9"))
        );
    }

    #[test]
    fn messages_of_a_gone_generation_are_dropped() {
        let mut registry = Registry::<u32>::default();
        let first = live(&mut registry, 1);
        assert_eq!(registry.route(first), Route::Live(1));
        registry.close_intent(first, 0);
        assert_eq!(registry.route(first), Route::Closing(1));
        registry.take_due(1);
        assert_eq!(registry.route(first), Route::Stale);
        assert_eq!(registry.dropped_count(), 1);

        let Ok(Decision::Create(second)) = registry.decide(first.id, SIZE) else {
            panic!("ожидалось создание");
        };
        registry.created(second, 2).unwrap();
        assert_eq!(registry.route(first), Route::Stale, "старое поколение");
        assert_eq!(registry.route(second), Route::Live(2));
        assert_eq!(registry.close_intent(first, 5), Close::Stale);
        assert_eq!(registry.phase(second.id), Some(Phase::Live));
        assert_eq!(registry.dropped_count(), 3);
    }

    #[test]
    fn two_close_intents_free_once() {
        let mut registry = Registry::<u32>::default();
        let key = live(&mut registry, 4);
        assert_eq!(registry.close_intent(key, 1), Close::Scheduled);
        assert_eq!(registry.close_intent(key, 1), Close::AlreadyClosing);
        assert_eq!(registry.close_intent(key, 2), Close::AlreadyClosing);
        assert_eq!(registry.take_due(2), [(key, 4)]);
        assert!(registry.take_due(3).is_empty());
        assert!(registry.take_all().is_empty());
        assert_eq!(registry.close_intent(key, 3), Close::Stale);
        assert_eq!(registry.freed_count(), 1);
        assert_eq!(registry.close_intent_count(), 4);
    }

    #[test]
    fn live_handles_skip_waiting_and_closing() {
        let mut registry = Registry::<u32>::default();
        let a = live(&mut registry, 1);
        let b = live(&mut registry, 2);
        let waiting = registry.add();
        registry.close_intent(b, 9);
        assert_eq!(registry.live_handles(), [(a, 1)]);
        assert_eq!(registry.phase(waiting), Some(Phase::WaitingSize));
    }

    #[test]
    fn take_all_frees_live_and_closing_once() {
        let mut registry = Registry::<u32>::default();
        let a = live(&mut registry, 1);
        let b = live(&mut registry, 2);
        let waiting = registry.add();
        registry.close_intent(b, 9);
        let mut all = registry.take_all();
        all.sort_by_key(|(_, handle)| *handle);
        assert_eq!(all, [(a, 1), (b, 2)]);
        assert!(registry.take_all().is_empty());
        assert_eq!(registry.phase(waiting), Some(Phase::WaitingSize));
        assert_eq!(registry.freed_count(), 2);
    }
}
