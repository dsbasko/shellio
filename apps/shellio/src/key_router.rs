//! Чистая часть маршрутизатора клавиш (ADR-0010, «Уточнено в P1.6 (планирование)»): решение о
//! нажатии по действующей таблице, режиму панели с фокусом и тому, кто держит ввод в окне.
//!
//! Монитор событий отвечает AppKit сразу, а действие исполняется позже, поэтому всё, от чего
//! зависит следующее решение, меняется внутри самого решения: набранный префикс, режим панели и
//! автомат аккорда выхода. На одно событие приходится не больше одного действия.
//!
//! Модальный слой наложений встаёт перед таблицей вместе с самими наложениями.

use std::collections::BTreeMap;

use serde_json::Value;
use shellio_keymap::{Cancel, Chord, Context, ExitChord, KeyCode, KeyDown, Keymap, Lookup, Mode};

/// Перевести панель с фокусом в `Navigation`.
pub const MODE_NAVIGATION: &str = "mode.navigation";
/// Перевести панель с фокусом в `Input`.
pub const MODE_INPUT: &str = "mode.input";
/// Сменить режим панели с фокусом.
pub const MODE_TOGGLE: &str = "mode.toggle";

const SPACE: KeyCode = KeyCode(0x31);
const ENTER: KeyCode = KeyCode(0x24);

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
}

/// `R` — получатель ввода панели, `E` — исходное событие нажатия для повтора.
#[derive(Debug)]
pub struct KeyRouter<R, E> {
    pending: Vec<Chord>,
    exit: ExitChord<R, E>,
    /// Панели в `Input`; остальные в `Navigation`.
    input: BTreeMap<R, ()>,
}

impl<R, E> Default for KeyRouter<R, E> {
    fn default() -> Self {
        Self {
            pending: Vec::new(),
            exit: ExitChord::default(),
            input: BTreeMap::new(),
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
        Responder::None | Responder::Body => false,
    }
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
        if focus.composing {
            return Decision::Pass;
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
            Responder::Body | Responder::Field | Responder::TabStrip | Responder::Button => {
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
        Decision::Run(Action {
            name: action.to_owned(),
            args,
        })
    }

    /// Отпускание клавиши действий не исполняет: `Swallow` — отпускание аккорда выхода, его пару
    /// получателю доставляет повтор.
    pub fn key_up(&mut self, key: KeyCode) -> Decision<R, E> {
        if self.exit.key_up(key) {
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
            Cancel::Responder
            | Cancel::Focus
            | Cancel::MouseDown
            | Cancel::Overlay
            | Cancel::Resigned => self.pending.clear(),
        }
        self.exit.cancel(cause)
    }

    /// Панель закрыта: её режим и её ожидание второго аккорда забыты.
    pub fn closed(&mut self, receiver: &R) {
        self.input.remove(receiver);
        self.exit.receiver_closed(receiver);
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
        assert_eq!(router.key_up(chord("w").key), Decision::Pass);
        assert_eq!(router.flags_changed(), Decision::Pass);
        assert_eq!(
            router.pending(),
            [chord("Ctrl-w")],
            "отпускание префикс не трогает"
        );
        assert_eq!(down(&mut router, &body(TERMINAL), "q"), ran("panel.close"));
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
}
