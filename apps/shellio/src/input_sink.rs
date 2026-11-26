//! Чистая модель приёмника ввода галереи (ADR-0010, «Уточнено в P1.6 (планирование)»): журнал
//! событий клавиш, которые получил вид приёмника, и поколение самого приёмника.
//!
//! Приёмник принимает клавиши только в режиме `Input`. Клавиша без привязки в `Navigation`
//! доходит до вида обеими половинами: такие события принятыми не считаются.

use std::collections::VecDeque;

use shellio_keymap::{Chord, Mode, Mods};

/// Плагин приёмника в контекстах keymap: привязок `panel.<plugin>` у него нет.
pub const PLUGIN: &str = "gallery-input";

/// Сколько событий хранит журнал: старые вытесняются.
pub const JOURNAL_LIMIT: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyKind {
    Down,
    Up,
}

/// Событие клавиши, дошедшее до вида приёмника.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyRecord {
    pub kind: KeyKind,
    /// Виртуальный код физической клавиши.
    pub key_code: u16,
    pub mods: Mods,
    /// Автоповтор удержанной клавиши.
    pub repeat: bool,
}

impl KeyRecord {
    pub const fn down(chord: Chord, repeat: bool) -> Self {
        Self {
            kind: KeyKind::Down,
            key_code: chord.key.0,
            mods: chord.mods,
            repeat,
        }
    }

    /// Отпускание той же клавиши с теми же модификаторами: так строится вторая половина пары,
    /// которую возвращает второй аккорд выхода.
    pub const fn released(self) -> Self {
        Self {
            kind: KeyKind::Up,
            repeat: false,
            ..self
        }
    }
}

/// Приёмник ввода: поколение и журнал принятых событий. Поколение растёт с каждым новым видом
/// приёмника и не сбрасывается.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputSink {
    generation: u64,
    journal: VecDeque<KeyRecord>,
}

impl Default for InputSink {
    fn default() -> Self {
        Self::new()
    }
}

impl InputSink {
    /// Приёмник первого построения: поколение 1, журнал пуст.
    pub const fn new() -> Self {
        Self {
            generation: 1,
            journal: VecDeque::new(),
        }
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Принятые события нынешнего приёмника по порядку.
    pub fn journal(&self) -> impl ExactSizeIterator<Item = &KeyRecord> {
        self.journal.iter()
    }

    /// Событие, которое получил вид приёмника поколения `generation` при режиме `mode`. `true` —
    /// событие принято и записано. Вид прежнего поколения и режим `Navigation` записи не дают.
    pub fn accept(&mut self, generation: u64, mode: Mode, record: KeyRecord) -> bool {
        if generation != self.generation || mode != Mode::Input {
            return false;
        }
        if self.journal.len() == JOURNAL_LIMIT {
            self.journal.pop_front();
        }
        self.journal.push_back(record);
        true
    }

    /// `gallery.input.reset`: журнал пуст, приёмник и его поколение прежние.
    pub fn reset(&mut self) {
        self.journal.clear();
    }

    /// Вид приёмника построен заново: новое поколение с пустым журналом.
    pub fn rebuilt(&mut self) -> u64 {
        self.generation += 1;
        self.journal.clear();
        self.generation
    }
}

#[cfg(test)]
mod tests {
    use shellio_keymap::{Cancel, KeyCode, Keymap};

    use super::*;
    use crate::key_router::{Action, Decision, Focus, KeyRouter, PanelFocus, Responder};

    /// Окно галереи с приёмником в фокусе: настоящий маршрутизатор с таблицей по умолчанию,
    /// получатель — поколение приёмника, сохранённое событие — запись его нажатия.
    struct Bench {
        keymap: Keymap,
        router: KeyRouter<u64, KeyRecord>,
        sink: InputSink,
    }

    fn chord(name: &str) -> Chord {
        Chord::parse(name).unwrap()
    }

    fn ran(name: &str) -> Decision<u64, KeyRecord> {
        Decision::Run(Action {
            name: name.to_owned(),
            args: None,
        })
    }

    impl Bench {
        /// Приёмник получил фокус: он в `Input`.
        fn focused() -> Self {
            let mut bench = Self {
                keymap: Keymap::defaults(&[]).unwrap(),
                router: KeyRouter::default(),
                sink: InputSink::new(),
            };
            bench.router.set_mode(bench.sink.generation(), Mode::Input);
            bench
        }

        fn mode(&self) -> Mode {
            self.router.mode(&self.sink.generation())
        }

        fn journal(&self) -> Vec<KeyRecord> {
            self.sink.journal().copied().collect()
        }

        /// Событие дошло до вида приёмника нынешнего поколения.
        fn view(&mut self, record: KeyRecord) {
            let generation = self.sink.generation();
            let mode = self.router.mode(&generation);
            self.sink.accept(generation, mode, record);
        }

        /// Нажатие через монитор: пропущенное идёт виду, пару второго аккорда вид получает
        /// напрямую — нажатие и построенное по нему отпускание.
        fn down_as(&mut self, name: &str, repeat: bool) -> Decision<u64, KeyRecord> {
            let chord = chord(name);
            let record = KeyRecord::down(chord, repeat);
            let focus = Focus {
                responder: Responder::Body,
                composing: false,
                panel: Some(PanelFocus {
                    receiver: self.sink.generation(),
                    plugin: PLUGIN.to_owned(),
                    input: true,
                }),
            };
            let decision = self
                .router
                .key_down(&self.keymap, &focus, chord, repeat, record);
            match &decision {
                Decision::Pass => self.view(record),
                Decision::Replay { receiver, event } => {
                    let mode = self.router.mode(receiver);
                    self.sink.accept(*receiver, mode, *event);
                    self.sink.accept(*receiver, mode, event.released());
                }
                Decision::Swallow | Decision::Run(_) => {}
            }
            decision
        }

        fn down(&mut self, name: &str) -> Decision<u64, KeyRecord> {
            self.down_as(name, false)
        }

        /// Отпускание через монитор: виду идёт отпускание клавиши, чьё нажатие он получил.
        fn up(&mut self, name: &str) -> Decision<u64, KeyRecord> {
            let chord = chord(name);
            let decision = self.router.key_up(KeyCode(chord.key.0));
            if decision == Decision::Pass {
                self.view(KeyRecord::down(chord, false).released());
            }
            decision
        }

        /// Вид приёмника построен заново: прежний закрыт, новый получил фокус.
        fn rebuild(&mut self) {
            self.router.closed(&self.sink.generation());
            let generation = self.sink.rebuilt();
            self.router.set_mode(generation, Mode::Input);
        }
    }

    const EXIT: &str = "Ctrl-\\";

    #[test]
    fn input_keys_are_journaled_in_order_and_reset_keeps_the_generation() {
        let mut bench = Bench::focused();
        assert_eq!(bench.sink.generation(), 1);
        assert_eq!(bench.journal(), []);

        assert_eq!(bench.down("a"), Decision::Pass);
        assert_eq!(bench.down_as("a", true), Decision::Pass);
        assert_eq!(bench.up("a"), Decision::Pass);
        let a = chord("a");
        assert_eq!(
            bench.journal(),
            [
                KeyRecord::down(a, false),
                KeyRecord::down(a, true),
                KeyRecord::down(a, false).released(),
            ]
        );

        bench.sink.reset();
        assert_eq!(bench.journal(), []);
        assert_eq!(bench.sink.generation(), 1);
        assert_eq!(bench.mode(), Mode::Input);
    }

    #[test]
    fn first_exit_chord_reaches_nobody_and_the_second_returns_one_pair() {
        let mut bench = Bench::focused();

        assert_eq!(bench.down(EXIT), ran("mode.navigation"));
        assert_eq!(bench.mode(), Mode::Navigation);
        assert_eq!(bench.down_as(EXIT, true), Decision::Swallow);
        assert_eq!(bench.up(EXIT), Decision::Swallow);
        assert_eq!(bench.journal(), []);

        let Decision::Replay { receiver, event } = bench.down(EXIT) else {
            panic!("второй аккорд возвращает пару");
        };
        assert_eq!(receiver, bench.sink.generation());
        assert_eq!(event, KeyRecord::down(chord(EXIT), false));
        assert_eq!(bench.mode(), Mode::Input);
        // Автоповтор и физическое отпускание второго аккорда к паре ничего не добавляют.
        assert_eq!(bench.down_as(EXIT, true), Decision::Swallow);
        assert_eq!(bench.up(EXIT), Decision::Swallow);

        let exit = chord(EXIT);
        assert_eq!(
            bench.journal(),
            [
                KeyRecord {
                    kind: KeyKind::Down,
                    key_code: exit.key.0,
                    mods: Mods {
                        ctrl: true,
                        ..Mods::NONE
                    },
                    repeat: false,
                },
                KeyRecord {
                    kind: KeyKind::Up,
                    key_code: exit.key.0,
                    mods: Mods {
                        ctrl: true,
                        ..Mods::NONE
                    },
                    repeat: false,
                },
            ]
        );
    }

    #[test]
    fn cancelled_exit_chord_returns_nothing() {
        // Другая клавиша: её нажатие исполняет действие `Navigation`, отпускание поглощено.
        let mut bench = Bench::focused();
        assert_eq!(bench.down(EXIT), ran("mode.navigation"));
        assert_eq!(bench.up(EXIT), Decision::Swallow);
        assert_eq!(bench.down("j"), ran("panel.focus.down"));
        assert_eq!(bench.up("j"), Decision::Swallow);
        // Клавиша без привязки в `Navigation` проходит к виду обеими половинами.
        assert_eq!(bench.down("Up"), Decision::Pass);
        assert_eq!(bench.up("Up"), Decision::Pass);
        assert_eq!(bench.mode(), Mode::Navigation);
        assert_eq!(bench.journal(), []);
        // Ожидание снято: следующий аккорд — обычная привязка `navigation`, пары нет. Режим уже
        // `Input`, но отпускание клавиши, чьё нажатие приёмник не получил, до него не доходит.
        assert_eq!(bench.down(EXIT), ran("mode.toggle"));
        assert_eq!(bench.mode(), Mode::Input);
        assert_eq!(bench.up(EXIT), Decision::Swallow);
        assert_eq!(bench.journal(), []);

        // Возврат в `Input` клавишей `i`: в журнале нет ни её нажатия, ни отпускания.
        let mut bench = Bench::focused();
        assert_eq!(bench.down(EXIT), ran("mode.navigation"));
        assert_eq!(bench.up(EXIT), Decision::Swallow);
        assert_eq!(bench.down("j"), ran("panel.focus.down"));
        assert_eq!(bench.up("j"), Decision::Swallow);
        assert_eq!(bench.down("i"), ran("mode.input"));
        assert_eq!(bench.mode(), Mode::Input);
        assert_eq!(bench.up("i"), Decision::Swallow);
        assert_eq!(bench.journal(), []);
        // Следующая клавиша — обычный ввод обеими половинами.
        assert_eq!(bench.down("i"), Decision::Pass);
        assert_eq!(bench.up("i"), Decision::Pass);
        assert_eq!(bench.journal().len(), 2);

        for cause in [
            Cancel::Responder,
            Cancel::Focus,
            Cancel::MouseDown,
            Cancel::ScrollWheel,
            Cancel::Overlay,
            Cancel::Resigned,
        ] {
            let mut bench = Bench::focused();
            assert_eq!(bench.down(EXIT), ran("mode.navigation"), "{cause:?}");
            assert_eq!(bench.up(EXIT), Decision::Swallow, "{cause:?}");
            assert!(bench.router.cancel(cause), "{cause:?}");
            assert_eq!(bench.down(EXIT), ran("mode.toggle"), "{cause:?}");
            assert_eq!(bench.journal(), [], "{cause:?}");
        }
    }

    #[test]
    fn closed_receiver_takes_its_pair_away_and_the_new_one_starts_empty() {
        let mut bench = Bench::focused();
        assert_eq!(bench.down("a"), Decision::Pass);
        assert_eq!(bench.down(EXIT), ran("mode.navigation"));
        assert_eq!(bench.up(EXIT), Decision::Swallow);

        bench.rebuild();
        assert_eq!(bench.sink.generation(), 2);
        assert_eq!(bench.journal(), []);
        // Аккорд нового приёмника — первый: пара прежнего не возвращается никому.
        assert_eq!(bench.down(EXIT), ran("mode.navigation"));
        assert_eq!(bench.up(EXIT), Decision::Swallow);
        assert_eq!(bench.journal(), []);
        assert!(matches!(
            bench.down(EXIT),
            Decision::Replay { receiver: 2, .. }
        ));
        assert_eq!(bench.journal().len(), 2);
    }

    #[test]
    fn stale_view_and_navigation_mode_are_not_journaled() {
        let mut sink = InputSink::new();
        let key = KeyRecord::down(chord("a"), false);
        assert!(!sink.accept(1, Mode::Navigation, key));
        assert!(sink.accept(1, Mode::Input, key));
        assert_eq!(sink.rebuilt(), 2);
        // Событие вида прежнего поколения, пришедшее после перестройки.
        assert!(!sink.accept(1, Mode::Input, key));
        assert!(sink.accept(2, Mode::Input, key));
        assert_eq!(sink.journal().len(), 1);
    }

    #[test]
    fn journal_keeps_the_last_events_only() {
        let mut sink = InputSink::new();
        let first = KeyRecord::down(chord("a"), false);
        let rest = KeyRecord::down(chord("b"), false);
        assert!(sink.accept(1, Mode::Input, first));
        for _ in 0..JOURNAL_LIMIT {
            assert!(sink.accept(1, Mode::Input, rest));
        }
        assert_eq!(sink.journal().len(), JOURNAL_LIMIT);
        assert!(sink.journal().all(|record| *record == rest));
    }
}
