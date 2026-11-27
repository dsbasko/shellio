//! Чистая часть маршрутизатора клавиш (ADR-0010, «Уточнено в P1.6 (планирование)»): решение о
//! нажатии по действующей таблице, режиму панели с фокусом и тому, кто держит ввод в окне.
//!
//! Монитор событий отвечает AppKit сразу, а действие исполняется позже, поэтому всё, от чего
//! зависит следующее решение, меняется внутри самого решения: набранный префикс, режим панели и
//! автомат аккорда выхода. На одно событие приходится не больше одного действия.
//!
//! Модальный слой наложений (палитра, метки, командная строка) стоит перед таблицей: пока
//! наложение открыто, клавиши получает оно, под него не проходит ничего, кроме `Cmd-q` и `Cmd-w`.
//! Открытие и закрытие клавишей меняют слой в самом решении.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use shellio_core::OverlayKind;
use shellio_keymap::{Cancel, Chord, Context, ExitChord, KeyCode, KeyDown, Keymap, Lookup, Mode};

/// Перевести панель с фокусом в `Navigation`.
pub const MODE_NAVIGATION: &str = "mode.navigation";
/// Перевести панель с фокусом в `Input`.
pub const MODE_INPUT: &str = "mode.input";
/// Сменить режим панели с фокусом.
pub const MODE_TOGGLE: &str = "mode.toggle";

const SPACE: KeyCode = KeyCode(0x31);
const ENTER: KeyCode = KeyCode(0x24);
const ESCAPE: KeyCode = KeyCode(0x35);

/// Действия, которые действуют и при открытом наложении.
const THROUGH_OVERLAY: [&str; 2] = ["app.quit", "window.close"];

/// Клавиши управления наложением, пока ввод держит его поле: выбор, выполнение и категории.
/// Остальное поле редактирует само.
const OVERLAY_KEYS: [&str; 9] = [
    "Up",
    "Down",
    "Enter",
    "Tab",
    "Shift-Tab",
    "Ctrl-n",
    "Ctrl-p",
    "Ctrl-j",
    "Ctrl-k",
];

/// Кто держит ввод в ключевом окне в момент события.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Responder {
    /// Ключевого окна нет: действуют только привязки контекста `global`.
    None,
    /// Тело панели или само окно.
    Body,
    /// Поле с редактором: печатаемые клавиши без `Cmd` и `Ctrl` принадлежат ему.
    Field,
    /// Полоса вкладок с клавиатурным фокусом: `Space` и `Enter` подтверждают вкладку.
    TabStrip,
    /// Кнопка с кольцом фокуса: `Space` и `Enter` нажимают её.
    Button,
    /// Поле открытого наложения: запрос палитры или строка `:`.
    OverlayField,
}

/// Панель с фокусом видимой части открытой рабочей области.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanelFocus<R> {
    /// Получатель ввода: ему возвращается пара аккорда выхода, по нему хранится режим.
    pub receiver: R,
    pub plugin: String,
    /// У панели есть получатель `Input`: без него `mode.input` и `mode.toggle` не действуют.
    pub input: bool,
}

/// Что маршрутизатор знает об окне в момент события.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Focus<R> {
    pub responder: Responder,
    /// Первый ответчик составляет текст в IME: клавиша принадлежит composition.
    pub composing: bool,
    pub panel: Option<PanelFocus<R>>,
}

/// Действие привязки: имя keymap и её `args`.
#[derive(Clone, Debug, PartialEq)]
pub struct Action {
    pub name: String,
    pub args: Option<Value>,
}

/// Решение о событии.
#[derive(Debug, PartialEq)]
pub enum Decision<R, E> {
    /// Событие идёт AppKit без изменений.
    Pass,
    /// Событие поглощено, действия нет.
    Swallow,
    /// Событие поглощено, исполняется одно действие.
    Run(Action),
    /// Второй аккорд выхода: получатель получает сохранённую пару, его режим уже `Input`.
    Replay { receiver: R, event: E },
    /// Клавиша открытого наложения: её обрабатывает само наложение.
    Overlay { kind: OverlayKind, chord: Chord },
    /// Клавиша закрыла наложение: источнику возвращаются фокус и режим.
    Close(Modal<R>),
}

/// Кому возвращаются фокус и режим, когда наложение закрыто без действия.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverlaySource<R> {
    /// Получатель панели с фокусом; `None` — панели не было или она закрыта.
    pub receiver: Option<R>,
    /// Режим панели в момент открытия.
    pub mode: Mode,
    /// Ввод держало поле панели.
    pub field: bool,
}

/// Открытое модальное наложение.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Modal<R> {
    pub kind: OverlayKind,
    pub source: OverlaySource<R>,
    /// В поле наложения есть текст: повторный аккорд открытия его не закрывает.
    pub typed: bool,
}

/// `R` — получатель ввода панели, `E` — исходное событие нажатия для повтора.
#[derive(Debug)]
pub struct KeyRouter<R, E> {
    pending: Vec<Chord>,
    exit: ExitChord<R, E>,
    /// Панели в `Input`; остальные в `Navigation`.
    input: BTreeMap<R, ()>,
    /// Клавиши, чьё нажатие получателю не ушло: их отпускание он тоже не получает.
    held: BTreeSet<KeyCode>,
    modal: Option<Modal<R>>,
}

impl<R, E> Default for KeyRouter<R, E> {
    fn default() -> Self {
        Self {
            pending: Vec::new(),
            exit: ExitChord::default(),
            input: BTreeMap::new(),
            held: BTreeSet::new(),
            modal: None,
        }
    }
}

/// Клавиша, которую получатель обрабатывает сам, пока префикс не набран.
fn native(responder: Responder, chord: Chord) -> bool {
    let bare = chord.mods == shellio_keymap::Mods::NONE;
    match responder {
        Responder::Field => chord.types_text(),
        Responder::TabStrip | Responder::Button => {
            bare && (chord.key == SPACE || chord.key == ENTER)
        }
        Responder::None | Responder::Body | Responder::OverlayField => false,
    }
}

/// Наложение, которое открывает действие.
pub fn overlay_of(action: &str) -> Option<OverlayKind> {
    OverlayKind::ALL
        .into_iter()
        .find(|kind| kind.keymap_name() == action)
}

/// Клавиша управления наложением, а не правка его поля.
fn overlay_key(chord: Chord) -> bool {
    OVERLAY_KEYS
        .iter()
        .filter_map(|name| Chord::parse(name).ok())
        .any(|key| key == chord)
}

/// Действует ли привязка у этой панели: режим `Input` есть только у панели с получателем ввода.
fn applies<R>(action: &str, focus: &Focus<R>) -> bool {
    match action {
        MODE_INPUT | MODE_TOGGLE => focus.panel.as_ref().is_some_and(|panel| panel.input),
        _ => true,
    }
}

impl<R: Ord + Clone, E> KeyRouter<R, E> {
    /// Набранное начало последовательности: его показывает строка статуса.
    pub fn pending(&self) -> &[Chord] {
        &self.pending
    }

    /// Режим панели. Панель, о которой маршрутизатор не знает, в `Navigation`.
    pub fn mode(&self, receiver: &R) -> Mode {
        if self.input.contains_key(receiver) {
            Mode::Input
        } else {
            Mode::Navigation
        }
    }

    /// Получатели в режиме `Input`: по ним окно красит плитку с фокусом и линию под вкладкой.
    pub fn inputs(&self) -> impl Iterator<Item = &R> {
        self.input.keys()
    }

    /// Режим панели задаёт окно: первым ответчиком стало поле или перестало им быть.
    pub fn set_mode(&mut self, receiver: R, mode: Mode) {
        match mode {
            Mode::Input => {
                self.input.insert(receiver, ());
            }
            Mode::Navigation => {
                self.input.remove(&receiver);
            }
        }
    }

    /// Режим вслед за полем: `typing` — получатель, чьё поле держит ввод, `None` — ввода в полях
    /// нет. Записанные режимы приводятся к этому факту. Совпадение ничего не меняет, поэтому
    /// смена первого ответчика, которую вызвало само действие `mode.*` или повтор пары, ожидание
    /// второго аккорда не снимает; расхождение — чужая смена ответчика, она ожидание снимает.
    /// `true` — режимы изменились.
    pub fn follow_field(&mut self, typing: Option<&R>) -> bool {
        // Поле наложения держит ввод вместо поля источника: режим источника ждёт закрытия.
        if self.modal.is_some() {
            return false;
        }
        let agrees = match typing {
            Some(receiver) => self.input.len() == 1 && self.input.contains_key(receiver),
            None => self.input.is_empty(),
        };
        if agrees {
            return false;
        }
        self.input.clear();
        if let Some(receiver) = typing {
            self.input.insert(receiver.clone(), ());
        }
        self.cancel(Cancel::Responder);
        true
    }

    /// Режим панели после действия `mode.*`, которое пришло не клавишей: меню, кнопка или CLI.
    /// Клавиша режим меняет в самом решении, и её действие целевой режим не считает заново.
    /// `None` — действие у панели не действует: `input` — есть ли у неё получатель ввода.
    pub fn target_mode(&self, receiver: &R, input: bool, action: &str) -> Option<Mode> {
        match action {
            MODE_NAVIGATION => Some(Mode::Navigation),
            MODE_INPUT => input.then_some(Mode::Input),
            MODE_TOGGLE => input.then(|| match self.mode(receiver) {
                Mode::Navigation => Mode::Input,
                Mode::Input => Mode::Navigation,
            }),
            _ => None,
        }
    }

    /// Получатель, которому второй аккорд выхода вернёт пару.
    pub fn exit_armed(&self) -> Option<&R> {
        self.exit.receiver()
    }

    /// Открытое модальное наложение.
    pub fn modal(&self) -> Option<&Modal<R>> {
        self.modal.as_ref()
    }

    /// Источник наложения по тому, кто держит ввод в момент открытия.
    pub fn source_of(&self, focus: &Focus<R>) -> OverlaySource<R> {
        let receiver = focus.panel.as_ref().map(|panel| panel.receiver.clone());
        OverlaySource {
            mode: receiver
                .as_ref()
                .map_or(Mode::Navigation, |receiver| self.mode(receiver)),
            field: focus.responder == Responder::Field && receiver.is_some(),
            receiver,
        }
    }

    /// Открыть наложение. Открытие снимает ожидание аккорда выхода и префикс. Открытое того же
    /// вида не меняется, другое вид заменяет: его источник остаётся источником нового.
    /// `true` — наложение открыто этим вызовом.
    pub fn open(&mut self, kind: OverlayKind, focus: &Focus<R>) -> bool {
        if self.modal.as_ref().is_some_and(|modal| modal.kind == kind) {
            return false;
        }
        let source = match self.modal.take() {
            Some(previous) => previous.source,
            None => self.source_of(focus),
        };
        self.cancel(Cancel::Overlay);
        self.modal = Some(Modal {
            kind,
            source,
            typed: false,
        });
        true
    }

    /// Закрыть открытое наложение: источнику возвращается режим. Первого ответчика возвращает
    /// окно по [`OverlaySource`]. `None` — наложения не было.
    pub fn close(&mut self) -> Option<Modal<R>> {
        let modal = self.modal.take()?;
        if let Some(receiver) = &modal.source.receiver {
            self.set_mode(receiver.clone(), modal.source.mode);
        }
        Some(modal)
    }

    /// В поле наложения появился текст или поле опустело.
    pub fn set_typed(&mut self, typed: bool) {
        if let Some(modal) = self.modal.as_mut() {
            modal.typed = typed;
        }
    }

    /// Клавиша при открытом наложении: `Cmd-q` и `Cmd-w` действуют, `Esc` и повторный аккорд
    /// открытия при пустом поле закрывают, клавиши управления получает наложение, текст — его
    /// поле. Под наложение не проходит ничего.
    fn decide_modal(&mut self, keymap: &Keymap, focus: &Focus<R>, chord: Chord) -> Decision<R, E> {
        let Some(modal) = self.modal.as_ref() else {
            return Decision::Pass;
        };
        let kind = modal.kind;
        if let Lookup::Exact(binding) = keymap.lookup(&[Context::Global], &[chord]) {
            if THROUGH_OVERLAY.contains(&binding.action.as_str()) {
                return Decision::Run(Action {
                    name: binding.action.clone(),
                    args: binding.args.clone(),
                });
            }
            // Повторный аккорд открытия закрывает пустое наложение; при тексте его получает
            // наложение, а не поле.
            if overlay_of(&binding.action) == Some(kind) {
                if modal.typed {
                    return Decision::Overlay { kind, chord };
                }
                return self.close().map_or(Decision::Swallow, Decision::Close);
            }
            // Глобальный аккорд под наложение не проходит: из поля наложения его поймал бы
            // эквивалент пункта главного меню.
            return Decision::Swallow;
        }
        if chord.key == ESCAPE && chord.mods == shellio_keymap::Mods::NONE {
            return self.close().map_or(Decision::Swallow, Decision::Close);
        }
        if focus.responder == Responder::OverlayField && !overlay_key(chord) {
            return Decision::Pass;
        }
        Decision::Overlay { kind, chord }
    }

    /// Режим, по которому разбирается событие: поле с вводом — всегда `Input`.
    fn effective(&self, focus: &Focus<R>) -> Mode {
        if focus.responder == Responder::Field {
            return Mode::Input;
        }
        focus
            .panel
            .as_ref()
            .map_or(Mode::Navigation, |panel| self.mode(&panel.receiver))
    }

    /// Нажатие клавиши, не модификатора. `event` — исходное событие: его хранит автомат аккорда
    /// выхода до второго аккорда.
    pub fn key_down(
        &mut self,
        keymap: &Keymap,
        focus: &Focus<R>,
        chord: Chord,
        repeat: bool,
        event: E,
    ) -> Decision<R, E> {
        let decision = self.decide(keymap, focus, chord, repeat, event);
        match decision {
            // Новое нажатие ушло получателю: его отпускание принадлежит ему же. Автоповтор
            // клавиши, чьё первое нажатие поглощено, отпускание получателю не возвращает.
            Decision::Pass => {
                if !repeat {
                    self.held.remove(&chord.key);
                }
            }
            Decision::Swallow
            | Decision::Run(_)
            | Decision::Replay { .. }
            | Decision::Overlay { .. }
            | Decision::Close(_) => {
                self.held.insert(chord.key);
            }
        }
        decision
    }

    fn decide(
        &mut self,
        keymap: &Keymap,
        focus: &Focus<R>,
        chord: Chord,
        repeat: bool,
        event: E,
    ) -> Decision<R, E> {
        if focus.composing {
            return Decision::Pass;
        }
        if self.modal.is_some() {
            return self.decide_modal(keymap, focus, chord);
        }
        match self.exit.key_down(chord, repeat) {
            KeyDown::Swallow => return Decision::Swallow,
            KeyDown::Replay { receiver, event } => {
                self.pending.clear();
                self.set_mode(receiver.clone(), Mode::Input);
                return Decision::Replay { receiver, event };
            }
            KeyDown::Pass => {}
        }
        if self.pending.is_empty() && native(focus.responder, chord) {
            return Decision::Pass;
        }

        let mode = self.effective(focus);
        let contexts = match focus.responder {
            Responder::None => vec![Context::Global],
            Responder::Body
            | Responder::Field
            | Responder::TabStrip
            | Responder::Button
            | Responder::OverlayField => {
                let plugin = focus.panel.as_ref().map(|panel| panel.plugin.as_str());
                Keymap::contexts(mode, plugin)
            }
        };
        let mut typed = self.pending.clone();
        typed.push(chord);

        let mut found = None;
        let mut prefix = false;
        for context in &contexts {
            match keymap.lookup(std::slice::from_ref(context), &typed) {
                Lookup::Exact(binding) if applies(&binding.action, focus) => {
                    found = Some(binding);
                    break;
                }
                // Привязка, которая у этой панели не действует, клавишу не занимает.
                Lookup::Exact(_) | Lookup::None => {}
                Lookup::Prefix => {
                    prefix = true;
                    break;
                }
            }
        }

        if let Some(binding) = found {
            self.pending.clear();
            let single = binding.sequence.chords().len() == 1;
            let exit = binding.context == Context::Input && single;
            return self.run(
                &binding.action,
                binding.args.clone(),
                focus,
                exit,
                chord,
                repeat,
                event,
            );
        }
        if prefix {
            self.pending = typed;
            return Decision::Swallow;
        }
        if !self.pending.is_empty() {
            self.pending.clear();
            return Decision::Swallow;
        }
        // Буква без привязки в `Navigation` никому не печатается: тело панели её не получает.
        let stray = mode == Mode::Navigation
            && focus.responder == Responder::Body
            && focus.panel.is_some()
            && chord.types_text();
        if stray {
            Decision::Swallow
        } else {
            Decision::Pass
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn run(
        &mut self,
        action: &str,
        args: Option<Value>,
        focus: &Focus<R>,
        exit: bool,
        chord: Chord,
        repeat: bool,
        event: E,
    ) -> Decision<R, E> {
        if let Some(panel) = &focus.panel {
            match action {
                MODE_NAVIGATION => {
                    // Аккорд выхода: автоповтор автомат не взводит и режим не меняет.
                    if exit && !self.exit.arm(panel.receiver.clone(), event, chord, repeat) {
                        return Decision::Swallow;
                    }
                    self.set_mode(panel.receiver.clone(), Mode::Navigation);
                }
                MODE_INPUT => self.set_mode(panel.receiver.clone(), Mode::Input),
                MODE_TOGGLE => {
                    let next = match self.effective(focus) {
                        Mode::Navigation => Mode::Input,
                        Mode::Input => Mode::Navigation,
                    };
                    self.set_mode(panel.receiver.clone(), next);
                }
                _ => {}
            }
        }
        if let Some(kind) = overlay_of(action) {
            self.open(kind, focus);
        }
        Decision::Run(Action {
            name: action.to_owned(),
            args,
        })
    }

    /// Отпускание клавиши действий не исполняет. `Swallow` — отпускание клавиши, чьё нажатие
    /// получателю не ушло: он не получает отпускание без нажатия. Пару аккорда выхода получателю
    /// доставляет повтор.
    pub fn key_up(&mut self, key: KeyCode) -> Decision<R, E> {
        let held = self.held.remove(&key);
        if self.exit.key_up(key) || held {
            Decision::Swallow
        } else {
            Decision::Pass
        }
    }

    /// Смена модификаторов: не клавиша, ожидание и префикс остаются.
    pub fn flags_changed(&self) -> Decision<R, E> {
        self.exit.flags_changed();
        Decision::Pass
    }

    /// Событие окна, которое снимает ожидание второго аккорда. Прокрутка набранный префикс не
    /// трогает, остальное сбрасывает и его: контексты под ним могли смениться. `true` — ожидание
    /// было и снято.
    pub fn cancel(&mut self, cause: Cancel) -> bool {
        match cause {
            Cancel::ScrollWheel => {}
            Cancel::Responder | Cancel::Focus | Cancel::MouseDown | Cancel::Overlay => {
                self.pending.clear()
            }
            // Отпускание клавиши, удержанной при уходе приложения, может не прийти.
            Cancel::Resigned => {
                self.pending.clear();
                self.held.clear();
            }
        }
        self.exit.cancel(cause)
    }

    /// Остаются только получатели, для которых `alive` истинно: остальные закрыты.
    pub fn retain(&mut self, alive: impl Fn(&R) -> bool) {
        self.input.retain(|receiver, ()| alive(receiver));
        self.forget_source(&alive);
        if let Some(receiver) = self.exit.receiver().cloned()
            && !alive(&receiver)
        {
            self.exit.receiver_closed(&receiver);
        }
    }

    /// Панель закрыта: её режим и её ожидание второго аккорда забыты, источником наложения она
    /// больше не считается.
    pub fn closed(&mut self, receiver: &R) {
        self.input.remove(receiver);
        self.exit.receiver_closed(receiver);
        self.forget_source(|source| source != receiver);
    }

    fn forget_source(&mut self, alive: impl Fn(&R) -> bool) {
        if let Some(modal) = self.modal.as_mut()
            && modal
                .source
                .receiver
                .as_ref()
                .is_some_and(|source| !alive(source))
        {
            modal.source.receiver = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use shellio_keymap::{KeyDefault, PanelKeys};

    use super::*;

    type Router = KeyRouter<u8, &'static str>;

    const LIST: u8 = 1;
    const FIELD: u8 = 2;
    const TERMINAL: u8 = 3;

    fn keymap() -> Keymap {
        Keymap::defaults(&[
            PanelKeys {
                plugin: "demo-list".to_owned(),
                list_keys: true,
                defaults: vec![KeyDefault {
                    sequence: "u".to_owned(),
                    action: "demo-list.back".to_owned(),
                }],
            },
            PanelKeys {
                plugin: "demo-native".to_owned(),
                list_keys: false,
                defaults: vec![KeyDefault {
                    sequence: "s".to_owned(),
                    action: "demo-native.set".to_owned(),
                }],
            },
        ])
        .unwrap()
    }

    fn chord(name: &str) -> Chord {
        Chord::parse(name).unwrap()
    }

    fn focus(responder: Responder, receiver: u8) -> Focus<u8> {
        let (plugin, input) = match receiver {
            LIST => ("demo-list", false),
            FIELD => ("demo-native", true),
            _ => ("terminal", false),
        };
        Focus {
            responder,
            composing: false,
            panel: Some(PanelFocus {
                receiver,
                plugin: plugin.to_owned(),
                input,
            }),
        }
    }

    fn body(receiver: u8) -> Focus<u8> {
        focus(Responder::Body, receiver)
    }

    fn down(
        router: &mut Router,
        focus: &Focus<u8>,
        key: &'static str,
    ) -> Decision<u8, &'static str> {
        router.key_down(&keymap(), focus, chord(key), false, key)
    }

    fn held(
        router: &mut Router,
        focus: &Focus<u8>,
        key: &'static str,
    ) -> Decision<u8, &'static str> {
        router.key_down(&keymap(), focus, chord(key), true, key)
    }

    fn ran(name: &str) -> Decision<u8, &'static str> {
        Decision::Run(Action {
            name: name.to_owned(),
            args: None,
        })
    }

    #[test]
    fn panel_context_goes_first_then_mode_then_global() {
        let mut router = Router::default();
        // Панель со списком держит `j` у себя, панель без списка отдаёт её фокусу.
        assert_eq!(down(&mut router, &body(LIST), "j"), ran("list.down"));
        assert_eq!(down(&mut router, &body(LIST), "u"), ran("demo-list.back"));
        assert_eq!(
            down(&mut router, &body(LIST), "Enter"),
            ran("list.activate")
        );
        assert_eq!(
            down(&mut router, &body(TERMINAL), "j"),
            ran("panel.focus.down")
        );
        // Привязки `navigation` и `global` видны из-под контекста панели.
        assert_eq!(down(&mut router, &body(LIST), "L"), ran("tab.next"));
        assert_eq!(
            down(&mut router, &body(LIST), "Cmd-0"),
            ran("layout.toggle.navigator")
        );
        // В `Input` действуют только `input` и `global`, остальное уходит получателю.
        router.set_mode(FIELD, Mode::Input);
        assert_eq!(down(&mut router, &body(FIELD), "j"), Decision::Pass);
        assert_eq!(down(&mut router, &body(FIELD), "s"), Decision::Pass);
        assert_eq!(
            down(&mut router, &body(FIELD), "Cmd-j"),
            ran("layout.toggle.bottom")
        );
    }

    #[test]
    fn prefix_waits_and_a_stray_key_drops_it_without_an_action() {
        let mut router = Router::default();
        assert_eq!(
            down(&mut router, &body(TERMINAL), "Ctrl-w"),
            Decision::Swallow
        );
        assert_eq!(router.pending(), [chord("Ctrl-w")]);
        assert_eq!(
            down(&mut router, &body(TERMINAL), "l"),
            ran("panel.focus.right")
        );
        assert!(router.pending().is_empty());

        assert_eq!(
            down(&mut router, &body(TERMINAL), "Ctrl-w"),
            Decision::Swallow
        );
        assert_eq!(down(&mut router, &body(TERMINAL), "x"), Decision::Swallow);
        assert!(router.pending().is_empty());

        assert_eq!(
            down(&mut router, &body(TERMINAL), "Ctrl-w"),
            Decision::Swallow
        );
        assert_eq!(down(&mut router, &body(TERMINAL), "Esc"), Decision::Swallow);
        assert!(router.pending().is_empty());
        // Без префикса `Esc` не привязан и идёт AppKit.
        assert_eq!(down(&mut router, &body(TERMINAL), "Esc"), Decision::Pass);

        // Более длинная последовательность панели скрывает короткую ниже.
        assert_eq!(down(&mut router, &body(LIST), "g"), Decision::Swallow);
        assert_eq!(down(&mut router, &body(LIST), "g"), ran("list.top"));
    }

    #[test]
    fn a_sequence_runs_exactly_one_action_with_its_args() {
        let mut router = Router::default();
        let decisions = [
            down(&mut router, &body(TERMINAL), "Ctrl-w"),
            down(&mut router, &body(TERMINAL), "v"),
        ];
        assert_eq!(decisions[0], Decision::Swallow);
        assert_eq!(
            decisions[1],
            Decision::Run(Action {
                name: "layout.split".to_owned(),
                args: Some(json!({ "axis": "row" })),
            })
        );
        assert_eq!(
            down(&mut router, &body(TERMINAL), "2"),
            Decision::Run(Action {
                name: "tab.select".to_owned(),
                args: Some(json!({ "index": 2 })),
            })
        );
    }

    #[test]
    fn stray_letter_in_navigation_reaches_nobody() {
        let mut router = Router::default();
        assert_eq!(down(&mut router, &body(TERMINAL), "x"), Decision::Swallow);
        assert_eq!(down(&mut router, &body(TERMINAL), "Up"), Decision::Pass);
        assert_eq!(down(&mut router, &body(TERMINAL), "Tab"), Decision::Pass);
        // Без панели буква идёт AppKit: в галерее и в пустой рабочей области клавиши компонентов
        // остаются штатными.
        let window = Focus {
            responder: Responder::Body,
            composing: false,
            panel: None,
        };
        assert_eq!(down(&mut router, &window, "x"), Decision::Pass);
        assert_eq!(down(&mut router, &window, "j"), ran("panel.focus.down"));
    }

    #[test]
    fn field_keeps_printable_keys_and_obeys_input_and_global() {
        let mut router = Router::default();
        let field = focus(Responder::Field, FIELD);
        for key in ["j", "s", "Space", "H", "/", "1", "Alt-a"] {
            assert_eq!(down(&mut router, &field, key), Decision::Pass, "{key}");
        }
        for key in [
            "Enter",
            "Esc",
            "Tab",
            "Backspace",
            "Left",
            "Cmd-c",
            "Ctrl-w",
        ] {
            assert_eq!(down(&mut router, &field, key), Decision::Pass, "{key}");
        }
        assert!(
            router.pending().is_empty(),
            "префикс `navigation` в поле не копится"
        );
        assert_eq!(
            down(&mut router, &field, "Cmd-Alt-t"),
            ran("ui.appearance.cycle")
        );
        // Поле с вводом — `Input`, даже если режим панели ещё не обновлён.
        assert_eq!(router.mode(&FIELD), Mode::Navigation);
        assert_eq!(down(&mut router, &field, "Ctrl-\\"), ran("mode.navigation"));
        assert_eq!(router.exit_armed(), Some(&FIELD));
    }

    #[test]
    fn mode_action_from_menu_or_cli_resolves_its_target_mode() {
        let mut router = Router::default();
        assert_eq!(
            router.target_mode(&FIELD, true, MODE_NAVIGATION),
            Some(Mode::Navigation)
        );
        assert_eq!(
            router.target_mode(&FIELD, true, MODE_INPUT),
            Some(Mode::Input)
        );
        assert_eq!(
            router.target_mode(&FIELD, true, MODE_TOGGLE),
            Some(Mode::Input)
        );
        router.set_mode(FIELD, Mode::Input);
        assert_eq!(
            router.target_mode(&FIELD, true, MODE_TOGGLE),
            Some(Mode::Navigation)
        );
        // Панель без получателя ввода остаётся в `Navigation`.
        assert_eq!(
            router.target_mode(&LIST, false, MODE_NAVIGATION),
            Some(Mode::Navigation)
        );
        assert_eq!(router.target_mode(&LIST, false, MODE_INPUT), None);
        assert_eq!(router.target_mode(&LIST, false, MODE_TOGGLE), None);
        assert_eq!(router.target_mode(&FIELD, true, "layout.zoom"), None);
    }

    #[test]
    fn focused_title_button_keeps_space_and_enter() {
        let mut router = Router::default();
        // У панели с вводом `Enter` на кнопке не становится `mode.input`.
        let button = focus(Responder::Button, FIELD);
        assert_eq!(down(&mut router, &button, "Space"), Decision::Pass);
        assert_eq!(down(&mut router, &button, "Enter"), Decision::Pass);
        assert!(router.pending().is_empty(), "лидер с кнопки не срабатывает");
        assert_eq!(router.mode(&FIELD), Mode::Navigation);
        // Остальные клавиши действуют как в теле панели.
        assert_eq!(down(&mut router, &button, "Ctrl-w"), Decision::Swallow);
        assert_eq!(down(&mut router, &button, "z"), ran("layout.zoom"));
        assert_eq!(down(&mut router, &button, "Shift-Enter"), Decision::Pass);
    }

    #[test]
    fn focused_tab_strip_keeps_space_and_enter_and_the_leader_works_from_the_body() {
        let mut router = Router::default();
        let strip = focus(Responder::TabStrip, LIST);
        assert_eq!(down(&mut router, &strip, "Space"), Decision::Pass);
        // `Enter` полосы — подтверждение вкладки, а не `list.activate` панели.
        assert_eq!(down(&mut router, &strip, "Enter"), Decision::Pass);
        assert!(router.pending().is_empty());
        assert_eq!(down(&mut router, &strip, "L"), ran("tab.next"));

        assert_eq!(down(&mut router, &body(LIST), "Space"), Decision::Swallow);
        assert_eq!(down(&mut router, &body(LIST), "z"), Decision::Swallow);
        assert_eq!(
            router.pending(),
            [chord("Space"), chord("z")],
            "префикс лидера из тела"
        );
        assert_eq!(
            down(&mut router, &body(LIST), "j"),
            ran("layout.toggle.bottom")
        );
        // После набранного префикса `Space` полосе не отдаётся: он завершает или сбрасывает набор.
        assert_eq!(down(&mut router, &strip, "Ctrl-w"), Decision::Swallow);
        assert_eq!(down(&mut router, &strip, "Space"), Decision::Swallow);
        assert!(router.pending().is_empty());
    }

    #[test]
    fn composition_takes_every_key() {
        let mut router = Router::default();
        router.set_mode(FIELD, Mode::Input);
        let mut composing = focus(Responder::Field, FIELD);
        composing.composing = true;
        for key in ["j", "Enter", "Esc", "Cmd-0", "Ctrl-\\", "Ctrl-w"] {
            assert_eq!(down(&mut router, &composing, key), Decision::Pass, "{key}");
        }
        assert_eq!(router.mode(&FIELD), Mode::Input);
        assert_eq!(router.exit_armed(), None);
        assert!(router.pending().is_empty());
    }

    #[test]
    fn key_up_and_modifiers_never_run_an_action() {
        let mut router = Router::default();
        for key in ["j", "Cmd-q", "Ctrl-\\", "Enter"] {
            assert_eq!(router.key_up(chord(key).key), Decision::Pass, "{key}");
        }
        assert_eq!(
            down(&mut router, &body(TERMINAL), "Ctrl-w"),
            Decision::Swallow
        );
        assert_eq!(router.key_up(chord("w").key), Decision::Swallow);
        assert_eq!(router.flags_changed(), Decision::Pass);
        assert_eq!(
            router.pending(),
            [chord("Ctrl-w")],
            "отпускание префикс не трогает"
        );
        assert_eq!(down(&mut router, &body(TERMINAL), "q"), ran("panel.close"));
    }

    #[test]
    fn key_up_follows_its_key_down() {
        let mut router = Router::default();
        let j = chord("j").key;
        let i = chord("i").key;
        let up = chord("Up").key;

        // Нажатие исполнило действие: отпускание получателю не идёт, и только один раз.
        assert_eq!(
            down(&mut router, &body(TERMINAL), "j"),
            ran("panel.focus.down")
        );
        assert_eq!(
            held(&mut router, &body(TERMINAL), "j"),
            ran("panel.focus.down")
        );
        assert_eq!(router.key_up(j), Decision::Swallow);
        assert_eq!(router.key_up(j), Decision::Pass);

        // Клавиша без привязки идёт получателю обеими половинами.
        assert_eq!(down(&mut router, &body(TERMINAL), "Up"), Decision::Pass);
        assert_eq!(router.key_up(up), Decision::Pass);

        // Клавиша вернула панель в `Input`: её автоповтор уже печатает, отпускание — нет.
        assert_eq!(down(&mut router, &body(FIELD), "i"), ran("mode.input"));
        assert_eq!(router.mode(&FIELD), Mode::Input);
        assert_eq!(held(&mut router, &body(FIELD), "i"), Decision::Pass);
        assert_eq!(router.key_up(i), Decision::Swallow);
        // Следующее нажатие той же клавиши — обычный ввод с отпусканием.
        assert_eq!(down(&mut router, &body(FIELD), "i"), Decision::Pass);
        assert_eq!(router.key_up(i), Decision::Pass);

        // Отпускание не пришло: новое пропущенное нажатие клавишу забывает.
        router.set_mode(FIELD, Mode::Navigation);
        assert_eq!(down(&mut router, &body(FIELD), "i"), ran("mode.input"));
        assert_eq!(down(&mut router, &body(FIELD), "i"), Decision::Pass);
        assert_eq!(router.key_up(i), Decision::Pass);

        // Приложение ушло с удержанной клавишей: её отпускание могло не прийти.
        assert_eq!(
            down(&mut router, &body(TERMINAL), "j"),
            ran("panel.focus.down")
        );
        router.cancel(Cancel::Resigned);
        assert_eq!(router.key_up(j), Decision::Pass);
        // Остальные события окна удержанную клавишу помнят.
        for cause in [
            Cancel::Responder,
            Cancel::Focus,
            Cancel::MouseDown,
            Cancel::ScrollWheel,
            Cancel::Overlay,
        ] {
            assert_eq!(
                down(&mut router, &body(TERMINAL), "j"),
                ran("panel.focus.down"),
                "{cause:?}"
            );
            router.cancel(cause);
            assert_eq!(router.key_up(j), Decision::Swallow, "{cause:?}");
        }
    }

    #[test]
    fn exit_chord_leaves_input_and_the_second_one_replays_the_pair() {
        let mut router = Router::default();
        router.set_mode(FIELD, Mode::Input);
        let backslash = chord("\\").key;

        assert_eq!(
            down(&mut router, &body(FIELD), "Ctrl-\\"),
            ran("mode.navigation")
        );
        assert_eq!(router.mode(&FIELD), Mode::Navigation);
        assert_eq!(router.exit_armed(), Some(&FIELD));
        assert_eq!(
            held(&mut router, &body(FIELD), "Ctrl-\\"),
            Decision::Swallow
        );
        assert_eq!(router.key_up(backslash), Decision::Swallow);
        // Модификатор между аккордами ожидание не снимает.
        assert_eq!(router.flags_changed(), Decision::Pass);

        assert_eq!(
            down(&mut router, &body(FIELD), "Ctrl-\\"),
            Decision::Replay {
                receiver: FIELD,
                event: "Ctrl-\\",
            }
        );
        assert_eq!(router.mode(&FIELD), Mode::Input);
        assert_eq!(router.exit_armed(), None);
        // Автоповтор второго аккорда ничего не взводит и режим не меняет.
        assert_eq!(
            held(&mut router, &body(FIELD), "Ctrl-\\"),
            Decision::Swallow
        );
        assert_eq!(router.mode(&FIELD), Mode::Input);
        assert_eq!(router.exit_armed(), None);
        assert_eq!(router.key_up(backslash), Decision::Swallow);
        assert_eq!(router.key_up(backslash), Decision::Pass);
    }

    #[test]
    fn another_key_or_a_window_event_cancels_the_exit_chord() {
        let mut router = Router::default();
        let cancels = [
            Cancel::Responder,
            Cancel::Focus,
            Cancel::MouseDown,
            Cancel::ScrollWheel,
            Cancel::Overlay,
            Cancel::Resigned,
        ];
        for cause in cancels {
            router.set_mode(FIELD, Mode::Input);
            assert_eq!(
                down(&mut router, &body(FIELD), "Ctrl-\\"),
                ran("mode.navigation")
            );
            assert!(router.cancel(cause), "{cause:?}");
            assert!(!router.cancel(cause), "{cause:?}: снято один раз");
            // Без ожидания аккорд — обычная привязка `navigation`: пары получателю нет.
            assert_eq!(
                down(&mut router, &body(FIELD), "Ctrl-\\"),
                ran("mode.toggle")
            );
            assert_eq!(router.mode(&FIELD), Mode::Input);
            assert_eq!(router.exit_armed(), None);
        }

        router.set_mode(FIELD, Mode::Input);
        assert_eq!(
            down(&mut router, &body(FIELD), "Ctrl-\\"),
            ran("mode.navigation")
        );
        assert_eq!(
            down(&mut router, &body(FIELD), "j"),
            ran("panel.focus.down")
        );
        assert_eq!(router.exit_armed(), None);
        assert_eq!(router.mode(&FIELD), Mode::Navigation);
    }

    #[test]
    fn window_events_drop_the_prefix_except_scrolling() {
        let mut router = Router::default();
        assert_eq!(
            down(&mut router, &body(TERMINAL), "Ctrl-w"),
            Decision::Swallow
        );
        assert!(!router.cancel(Cancel::ScrollWheel));
        assert_eq!(router.pending(), [chord("Ctrl-w")]);
        assert!(!router.cancel(Cancel::Focus));
        assert!(router.pending().is_empty());
    }

    #[test]
    fn mode_bindings_act_only_on_a_panel_with_an_input_receiver() {
        let mut router = Router::default();
        // Без получателя ввода привязка клавишу не занимает: буква глохнет, `Enter` и аккорд
        // выхода идут AppKit.
        assert_eq!(down(&mut router, &body(TERMINAL), "i"), Decision::Swallow);
        assert_eq!(down(&mut router, &body(TERMINAL), "Enter"), Decision::Pass);
        assert_eq!(
            down(&mut router, &body(TERMINAL), "Ctrl-\\"),
            Decision::Pass
        );
        assert_eq!(router.mode(&TERMINAL), Mode::Navigation);

        assert_eq!(down(&mut router, &body(FIELD), "i"), ran("mode.input"));
        assert_eq!(router.mode(&FIELD), Mode::Input);
        // Следующая клавиша судится уже по новому режиму.
        assert_eq!(down(&mut router, &body(FIELD), "i"), Decision::Pass);
        router.set_mode(FIELD, Mode::Navigation);
        assert_eq!(down(&mut router, &body(FIELD), "Enter"), ran("mode.input"));
        assert_eq!(router.mode(&FIELD), Mode::Input);
        // Режим у каждой панели свой.
        assert_eq!(router.mode(&LIST), Mode::Navigation);
        assert_eq!(down(&mut router, &body(LIST), "j"), ran("list.down"));
    }

    #[test]
    fn without_a_key_window_only_global_bindings_act() {
        let mut router = Router::default();
        let nothing = Focus {
            responder: Responder::None,
            composing: false,
            panel: None,
        };
        assert_eq!(down(&mut router, &nothing, "Cmd-q"), ran("app.quit"));
        assert_eq!(down(&mut router, &nothing, "j"), Decision::Pass);
        assert_eq!(down(&mut router, &nothing, "Ctrl-w"), Decision::Pass);
        assert!(router.pending().is_empty());
    }

    #[test]
    fn closed_panel_takes_its_mode_and_its_exit_chord_away() {
        let mut router = Router::default();
        router.set_mode(FIELD, Mode::Input);
        router.set_mode(LIST, Mode::Input);
        assert_eq!(
            down(&mut router, &body(FIELD), "Ctrl-\\"),
            ran("mode.navigation")
        );
        router.closed(&LIST);
        assert_eq!(router.exit_armed(), Some(&FIELD), "чужое ожидание остаётся");
        assert_eq!(router.mode(&LIST), Mode::Navigation);
        router.set_mode(FIELD, Mode::Input);
        router.closed(&FIELD);
        assert_eq!(router.exit_armed(), None);
        assert_eq!(router.mode(&FIELD), Mode::Navigation);
    }

    /// Правило «Режим панели»: новая и восстановленная панели начинают в `Navigation`, панель,
    /// открытая с автоматическим вводом в поле, сразу в `Input`.
    #[test]
    fn new_and_restored_panels_start_in_navigation_and_autofocus_gives_input() {
        // Новая панель: маршрутизатор о ней не знает.
        let mut router = Router::default();
        assert_eq!(router.mode(&TERMINAL), Mode::Navigation);
        assert_eq!(router.mode(&FIELD), Mode::Navigation);
        assert_eq!(
            down(&mut router, &body(TERMINAL), "h"),
            ran("panel.focus.left")
        );

        // Восстановленная панель: режимы в базу не пишутся, новый процесс начинает с пустых, даже
        // если прежний оставил её в `Input`.
        router.set_mode(FIELD, Mode::Input);
        let restored = Router::default();
        assert_eq!(restored.mode(&FIELD), Mode::Navigation);
        assert_eq!(restored.exit_armed(), None);

        // Открытие с вводом в поле: `Input` без клавиши, печатаемое идёт полю.
        let mut opened = Router::default();
        assert!(opened.follow_field(Some(&FIELD)));
        assert_eq!(opened.mode(&FIELD), Mode::Input);
        let field = focus(Responder::Field, FIELD);
        assert_eq!(down(&mut opened, &field, "h"), Decision::Pass);
    }

    #[test]
    fn only_living_receivers_keep_their_mode_and_exit_chord() {
        let mut router = Router::default();
        router.set_mode(FIELD, Mode::Input);
        router.set_mode(LIST, Mode::Input);
        down(&mut router, &body(FIELD), "Ctrl-\\");
        router.retain(|receiver| *receiver == FIELD);
        assert_eq!(router.exit_armed(), Some(&FIELD));
        assert_eq!(router.mode(&LIST), Mode::Navigation);
        router.retain(|receiver| *receiver == LIST);
        assert_eq!(router.exit_armed(), None);
    }

    #[test]
    fn mode_follows_the_field_and_only_a_foreign_change_disarms() {
        let mut router = Router::default();
        assert!(!router.follow_field(None), "ввода нет и режимов нет");
        // Поле панели получило ввод: открытие с автофокусом, `Tab` или щелчок.
        assert!(router.follow_field(Some(&FIELD)));
        assert_eq!(router.mode(&FIELD), Mode::Input);
        assert!(
            !router.follow_field(Some(&FIELD)),
            "повтор ничего не меняет"
        );

        // Аккорд выхода сам увёл ввод из поля: режим уже `Navigation`, ожидание остаётся.
        let field = focus(Responder::Field, FIELD);
        assert_eq!(down(&mut router, &field, "Ctrl-\\"), ran("mode.navigation"));
        assert!(!router.follow_field(None));
        assert_eq!(router.exit_armed(), Some(&FIELD));

        // Ввод ушёл в чужое поле: режимы переставлены, ожидание снято.
        assert!(router.follow_field(Some(&LIST)));
        assert_eq!(router.exit_armed(), None);
        assert_eq!(router.mode(&FIELD), Mode::Navigation);
        assert_eq!(router.mode(&LIST), Mode::Input);
        assert_eq!(
            router.inputs().collect::<Vec<_>>(),
            [&LIST],
            "тон режима «ВВОД» получает одна панель"
        );

        // Повтор пары вернул `Input` получателю: поле с вводом ожидание уже не трогает.
        let mut router = Router::default();
        router.follow_field(Some(&FIELD));
        down(&mut router, &field, "Ctrl-\\");
        assert!(matches!(
            down(&mut router, &body(FIELD), "Ctrl-\\"),
            Decision::Replay {
                receiver: FIELD,
                ..
            }
        ));
        assert!(!router.follow_field(Some(&FIELD)));
        assert_eq!(router.mode(&FIELD), Mode::Input);
    }

    /// Таблица с привязками наложений из `keymap.toml`.
    fn overlay_keymap() -> Keymap {
        let file = r#"
[global]
"Cmd-k" = "palette.open"
[navigation]
"<leader> <leader>" = "palette.open"
"f" = "hints.open"
":" = "cmdline.open"
"#;
        let any = |_: &Context, _: &str, _: Option<&Value>| Ok(());
        let base = keymap();
        let panels = [
            PanelKeys {
                plugin: "demo-list".to_owned(),
                list_keys: true,
                defaults: Vec::new(),
            },
            PanelKeys {
                plugin: "demo-native".to_owned(),
                list_keys: false,
                defaults: Vec::new(),
            },
        ];
        let table = base.apply(Some(file), &panels, &any).unwrap();
        assert!(table.diagnostics().is_empty(), "{:?}", table.diagnostics());
        table
    }

    fn press(
        router: &mut Router,
        table: &Keymap,
        focus: &Focus<u8>,
        key: &'static str,
    ) -> Decision<u8, &'static str> {
        router.key_down(table, focus, chord(key), false, key)
    }

    fn opened(kind: OverlayKind) -> Decision<u8, &'static str> {
        ran(kind.keymap_name())
    }

    #[test]
    fn opening_chord_opens_the_modal_layer_inside_the_decision() {
        let table = overlay_keymap();
        let mut router = Router::default();
        assert_eq!(
            press(&mut router, &table, &body(LIST), "f"),
            opened(OverlayKind::Hints)
        );
        assert_eq!(
            router.modal().map(|modal| modal.kind),
            Some(OverlayKind::Hints)
        );
        // Следующая быстрая клавиша уже идёт наложению, а не списку под ним.
        assert_eq!(
            press(&mut router, &table, &body(LIST), "a"),
            Decision::Overlay {
                kind: OverlayKind::Hints,
                chord: chord("a"),
            }
        );
        // `Space Space` открывает палитру из `Navigation`; закрытие возвращает источник.
        assert!(matches!(
            press(&mut router, &table, &body(LIST), "Esc"),
            Decision::Close(_)
        ));
        assert_eq!(
            press(&mut router, &table, &body(LIST), "Space"),
            Decision::Swallow
        );
        assert_eq!(
            press(&mut router, &table, &body(LIST), "Space"),
            opened(OverlayKind::Palette)
        );
        assert!(router.pending().is_empty());
    }

    #[test]
    fn keys_under_an_open_overlay_never_reach_the_layer_below() {
        let table = overlay_keymap();
        let mut router = Router::default();
        press(&mut router, &table, &body(TERMINAL), "Cmd-k");
        for key in ["j", "Ctrl-w", "l", "i", "Enter", "Ctrl-\\", "Space"] {
            assert!(
                matches!(
                    press(&mut router, &table, &body(TERMINAL), key),
                    Decision::Overlay { .. }
                ),
                "{key}"
            );
        }
        // Глобальные аккорды зон и оформления поглощаются, наложение их не получает.
        for key in ["Cmd-0", "Cmd-j", "Cmd-Alt-0", "Cmd-Alt-t"] {
            assert_eq!(
                press(&mut router, &table, &body(TERMINAL), key),
                Decision::Swallow,
                "{key}"
            );
        }
        assert!(
            router.pending().is_empty(),
            "префикс под палитрой не копится"
        );
        assert_eq!(router.exit_armed(), None);
        assert_eq!(router.mode(&TERMINAL), Mode::Navigation);
        // `Cmd-q` и `Cmd-w` действуют и при открытом наложении, наложение остаётся.
        assert_eq!(
            press(&mut router, &table, &body(TERMINAL), "Cmd-q"),
            ran("app.quit")
        );
        assert_eq!(
            press(&mut router, &table, &body(TERMINAL), "Cmd-w"),
            ran("window.close")
        );
        assert_eq!(
            router.modal().map(|modal| modal.kind),
            Some(OverlayKind::Palette)
        );
    }

    #[test]
    fn overlay_field_keeps_its_text_and_gives_control_keys_to_the_overlay() {
        let table = overlay_keymap();
        let mut router = Router::default();
        press(&mut router, &table, &body(LIST), "Cmd-k");
        let field = Focus {
            responder: Responder::OverlayField,
            composing: false,
            panel: None,
        };
        // Пробел и буквы — текст запроса: `Space Space` палитру не закрывает и не открывает.
        for key in ["Space", "Space", "j", "f", "Backspace", "Left", "Cmd-a"] {
            assert_eq!(
                press(&mut router, &table, &field, key),
                Decision::Pass,
                "{key}"
            );
        }
        // Глобальный аккорд из поля наложения не уходит в AppKit: его поймал бы пункт меню.
        for key in ["Cmd-0", "Cmd-j", "Cmd-Alt-0", "Cmd-Alt-t"] {
            assert_eq!(
                press(&mut router, &table, &field, key),
                Decision::Swallow,
                "{key}"
            );
        }
        for key in [
            "Up",
            "Down",
            "Enter",
            "Tab",
            "Shift-Tab",
            "Ctrl-n",
            "Ctrl-p",
            "Ctrl-j",
            "Ctrl-k",
        ] {
            assert!(
                matches!(
                    press(&mut router, &table, &field, key),
                    Decision::Overlay { .. }
                ),
                "{key}"
            );
        }
        // При тексте в поле повторный `Cmd-k` палитру не закрывает, при пустом — закрывает.
        router.set_typed(true);
        assert!(matches!(
            press(&mut router, &table, &field, "Cmd-k"),
            Decision::Overlay { .. }
        ));
        router.set_typed(false);
        assert!(matches!(
            press(&mut router, &table, &field, "Cmd-k"),
            Decision::Close(_)
        ));
        assert_eq!(router.modal(), None);
    }

    /// Поле командной строки в окне владельца: текст — полю, `Enter` — строке, `Esc` закрывает.
    #[test]
    fn cmdline_field_types_text_and_gives_enter_to_the_line() {
        let table = overlay_keymap();
        let mut router = Router::default();
        assert_eq!(
            press(&mut router, &table, &body(LIST), ":"),
            opened(OverlayKind::Cmdline)
        );
        let field = Focus {
            responder: Responder::OverlayField,
            composing: false,
            panel: None,
        };
        for key in ["z", "o", "Space", ":", "f", "Backspace"] {
            assert_eq!(
                press(&mut router, &table, &field, key),
                Decision::Pass,
                "{key}"
            );
        }
        assert!(matches!(
            press(&mut router, &table, &field, "Enter"),
            Decision::Overlay {
                kind: OverlayKind::Cmdline,
                ..
            }
        ));
        assert!(matches!(
            press(&mut router, &table, &field, "Esc"),
            Decision::Close(_)
        ));
        assert_eq!(router.modal(), None);
    }

    #[test]
    fn closing_returns_the_source_mode_and_the_field() {
        let table = overlay_keymap();
        let mut router = Router::default();
        // Палитра из поля во `Input`: поле наложения забирает ввод, режим источника ждёт.
        router.follow_field(Some(&FIELD));
        let field = focus(Responder::Field, FIELD);
        assert_eq!(
            press(&mut router, &table, &field, "Cmd-k"),
            opened(OverlayKind::Palette)
        );
        assert!(!router.follow_field(None), "согласование ждёт закрытия");
        assert_eq!(router.mode(&FIELD), Mode::Input);
        // Режим, сменённый под наложением (`mode.*` из CLI или меню), закрытие возвращает.
        router.set_mode(FIELD, Mode::Navigation);
        let Decision::Close(modal) = press(&mut router, &table, &field, "Esc") else {
            panic!("Esc закрывает палитру");
        };
        assert_eq!(
            modal.source,
            OverlaySource {
                receiver: Some(FIELD),
                mode: Mode::Input,
                field: true,
            }
        );
        assert_eq!(router.mode(&FIELD), Mode::Input);

        // Из списка в `Navigation` — обратно в `Navigation`.
        let mut router = Router::default();
        assert_eq!(
            press(&mut router, &table, &body(LIST), ":"),
            opened(OverlayKind::Cmdline)
        );
        router.set_mode(LIST, Mode::Input);
        let modal = router.close().unwrap();
        assert_eq!(router.mode(&LIST), Mode::Navigation);
        assert_eq!(modal.source.receiver, Some(LIST));
        assert_eq!(modal.source.mode, Mode::Navigation);
        assert!(!modal.source.field);
        assert_eq!(router.close(), None, "закрыто один раз");
    }

    #[test]
    fn opening_drops_the_prefix_and_the_exit_chord() {
        let table = overlay_keymap();
        let mut router = Router::default();
        router.set_mode(FIELD, Mode::Input);
        assert_eq!(
            press(&mut router, &table, &body(FIELD), "Ctrl-\\"),
            ran("mode.navigation")
        );
        assert_eq!(router.exit_armed(), Some(&FIELD));
        assert_eq!(
            press(&mut router, &table, &body(FIELD), "Ctrl-w"),
            Decision::Swallow
        );
        // Открытие не клавишей (CLI, кнопка заголовка) снимает то же самое.
        assert!(router.open(OverlayKind::Palette, &body(FIELD)));
        assert_eq!(router.exit_armed(), None);
        assert!(router.pending().is_empty());
        assert!(
            !router.open(OverlayKind::Palette, &body(FIELD)),
            "уже открыта"
        );
        // Другое наложение заменяет открытое и наследует его источник.
        assert!(router.open(OverlayKind::Hints, &body(LIST)));
        assert_eq!(
            router
                .modal()
                .map(|modal| (modal.kind, modal.source.receiver)),
            Some((OverlayKind::Hints, Some(FIELD)))
        );
    }

    #[test]
    fn composition_keeps_its_keys_even_with_an_overlay() {
        let table = overlay_keymap();
        let mut router = Router::default();
        router.open(OverlayKind::Cmdline, &body(LIST));
        let mut composing = focus(Responder::OverlayField, LIST);
        composing.composing = true;
        assert_eq!(
            press(&mut router, &table, &composing, "Esc"),
            Decision::Pass
        );
        assert_eq!(
            press(&mut router, &table, &composing, "Enter"),
            Decision::Pass
        );
        assert!(router.modal().is_some());
    }

    #[test]
    fn closed_source_panel_is_not_restored() {
        let table = overlay_keymap();
        let mut router = Router::default();
        press(&mut router, &table, &body(LIST), "Cmd-k");
        router.closed(&LIST);
        assert_eq!(router.close().unwrap().source.receiver, None);

        press(&mut router, &table, &body(FIELD), "Cmd-k");
        router.retain(|receiver| *receiver != FIELD);
        assert_eq!(router.close().unwrap().source.receiver, None);
        assert_eq!(router.modal(), None);
    }

    #[test]
    fn key_up_of_an_overlay_key_stays_with_the_overlay() {
        let table = overlay_keymap();
        let mut router = Router::default();
        press(&mut router, &table, &body(LIST), "f");
        press(&mut router, &table, &body(LIST), "a");
        assert_eq!(router.key_up(chord("a").key), Decision::Swallow);
        press(&mut router, &table, &body(LIST), "Esc");
        assert_eq!(router.key_up(chord("Esc").key), Decision::Swallow);
    }
}
