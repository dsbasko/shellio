//! Доступность вида терминала (H13): текст атрибутов доступности по `ghostty_surface_read_text`,
//! выделение в единицах UTF-16 и таймер обновления демонстрации.
//!
//! Текст — видимая область одним чтением от левого верхнего до правого нижнего угла. Форматтер
//! ghostty склеивает мягкий перенос в одну строку, не отдаёт пустые клетки в конце строки и пустые
//! строки внизу. `AXValue` дополнительно срезает пробелы в конце каждой строки и пустые строки в
//! конце и ограничен [`VALUE_LIMIT`] единицами UTF-16 без разрыва суррогатной пары ([`Value`]).
//!
//! Выделение. `ghostty_surface_read_selection` отдаёт текст S и линейные смещения клеток
//! (`строка × столбцы + столбец`), а не смещения в тексте: пустые клетки, широкие символы, переводы
//! строк и мягкий перенос их разводят. Начало выводится из чтений: P — текст от левого верхнего
//! угла до последней клетки выделения, он префикс текста видимой области V и оканчивается на S,
//! поэтому S начинается в V с `len16(P) − len16(S)` ([`selection_range`]). Нарушение любого условия
//! — выделение не отдаётся, расхождение записывается. Без выделения точка вставки стоит в конце
//! текста: курсор терминала в смещение не переводится.
//!
//! Текст читается только по запросу клиента доступности и по обновлению, которое зовёт стенд после
//! квитанции или таймер демонстрации; снимок живёт [`SNAPSHOT_TTL_NS`], чтобы серия атрибутов
//! одного запроса не читала экран заново. Таймер демонстрации ([`enable_demo`]) запускается первым
//! запросом доступности при включённом VoiceOver и останавливается, когда VoiceOver выключен.

use std::cell::{Cell, RefCell};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};

use block2::RcBlock;
use objc2::rc::{Retained, Weak};
use objc2_app_kit::NSWorkspace;
use objc2_foundation::{NSRunLoop, NSRunLoopCommonModes, NSTimer};
use serde::Serialize;

use crate::stats;
use crate::utf16::{self, Range16};
use crate::view::SurfaceView;

/// Предел `AXValue` в единицах UTF-16.
pub const VALUE_LIMIT: usize = 100_000;
/// Срок снимка текста: серия атрибутов одного запроса клиента читает экран один раз.
pub const SNAPSHOT_TTL_NS: u64 = 100_000_000;
/// Период таймера обновления доступности в демонстрации.
pub const DEMO_REFRESH_S: f64 = 0.5;
/// Предел записей журнала доступности вида.
const LOG_LIMIT: usize = 2000;

/// Строка текста видимой области: границы в исходном тексте и в `AXValue`, в единицах UTF-16.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Line {
    raw_start: usize,
    raw_len: usize,
    start: usize,
    len: usize,
}

/// Текст `AXValue` и перевод смещений исходного текста видимой области в смещения в нём.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Value {
    pub text: String,
    pub len16: usize,
    /// Текст обрезан по [`VALUE_LIMIT`].
    pub truncated: bool,
    lines: Vec<Line>,
}

impl Value {
    pub fn new(raw: &str, limit: usize) -> Value {
        let parts: Vec<&str> = raw.split('\n').collect();
        let kept = parts
            .iter()
            .rposition(|part| !part.trim_end_matches(' ').is_empty())
            .map_or(0, |last| last + 1);
        let mut text = String::new();
        let mut len16 = 0;
        let mut truncated = false;
        let mut lines = Vec::with_capacity(parts.len());
        let mut raw_start = 0;
        for (index, part) in parts.iter().enumerate() {
            let raw_len = utf16::len(part);
            let mut line = Line {
                raw_start,
                raw_len,
                start: len16,
                len: 0,
            };
            raw_start += raw_len + 1;
            if index >= kept || truncated {
                lines.push(line);
                continue;
            }
            if index > 0 {
                if len16 + 1 > limit {
                    truncated = true;
                    lines.push(line);
                    continue;
                }
                text.push('\n');
                len16 += 1;
                line.start = len16;
            }
            for ch in part.trim_end_matches(' ').chars() {
                if len16 + ch.len_utf16() > limit {
                    truncated = true;
                    break;
                }
                text.push(ch);
                len16 += ch.len_utf16();
                line.len += ch.len_utf16();
            }
            lines.push(line);
        }
        Value {
            text,
            len16,
            truncated,
            lines,
        }
    }

    /// Смещение в `AXValue` для смещения исходного текста: внутри срезанного хвоста строки — конец
    /// строки, за обрезкой и за концом — конец текста.
    pub fn map(&self, raw16: usize) -> usize {
        self.lines
            .iter()
            .find(|line| raw16 <= line.raw_start + line.raw_len)
            .map_or(self.len16, |line| {
                let inside = raw16.saturating_sub(line.raw_start).min(line.len);
                (line.start + inside).min(self.len16)
            })
    }

    pub fn map_range(&self, raw: Range16) -> Option<Range16> {
        let end = raw.end()?;
        let start = self.map(raw.location);
        Some(Range16::new(start, self.map(end).saturating_sub(start)))
    }

    /// Номер строки смещения; смещение за концом — последняя строка.
    pub fn line_for_index(&self, index: usize) -> usize {
        let mut units = 0;
        let mut line = 0;
        for ch in self.text.chars() {
            if units >= index {
                break;
            }
            if ch == '\n' {
                line += 1;
            }
            units += ch.len_utf16();
        }
        line
    }

    /// Диапазон строки без перевода строки.
    pub fn range_for_line(&self, line: usize) -> Option<Range16> {
        let mut start = 0;
        for (index, part) in self.text.split('\n').enumerate() {
            let len = utf16::len(part);
            if index == line {
                return Some(Range16::new(start, len));
            }
            start += len + 1;
        }
        None
    }

    /// Подстрока диапазона UTF-16; диапазон за концом или с половиной суррогатной пары — `None`.
    pub fn substring(&self, range: Range16) -> Option<String> {
        utf16::range_to_utf8(&self.text, range).map(|bytes| self.text[bytes].to_owned())
    }
}

/// Последняя клетка выделения — столбец и строка видимой области по линейным смещениям клеток
/// `ghostty_text_s` (конец включительно).
pub fn end_cell(offset_start: u32, offset_len: u32, columns: u16) -> Option<(u32, u32)> {
    if columns == 0 {
        return None;
    }
    let end = offset_start.checked_add(offset_len)?;
    let columns = u32::from(columns);
    Some((end % columns, end / columns))
}

/// Почему выделение не переведено в диапазон.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mismatch {
    /// У выделения нет части в видимой области.
    NotInViewport,
    /// Смещения клеток не дают клетку видимой области.
    EndCell,
    /// Чтение до последней клетки не прочитано.
    PrefixUnread,
    /// Чтение до последней клетки не префикс текста видимой области.
    PrefixNotInViewport,
    /// Чтение до последней клетки не оканчивается текстом выделения.
    SelectionNotAtPrefixEnd,
}

/// Выделение в исходном тексте видимой области в единицах UTF-16 по тексту видимой области,
/// чтению до последней клетки выделения и тексту выделения.
pub fn selection_range(viewport: &str, prefix: &str, selection: &str) -> Result<Range16, Mismatch> {
    if !viewport.starts_with(prefix) {
        return Err(Mismatch::PrefixNotInViewport);
    }
    if !prefix.ends_with(selection) {
        return Err(Mismatch::SelectionNotAtPrefixEnd);
    }
    let length = utf16::len(selection);
    Ok(Range16::new(utf16::len(prefix) - length, length))
}

/// Что прочитано у поверхности для одного снимка.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reading {
    pub viewport: String,
    /// Текст выделения и его диапазон в исходном тексте или причина, почему диапазона нет.
    pub selection: Option<Result<(String, Range16), Mismatch>>,
    /// Вызовы `ghostty_surface_read_text` и `ghostty_surface_read_selection`.
    pub read_calls: u32,
}

/// Снимок текста и выделения для атрибутов доступности.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub value: Value,
    /// Выделение в `AXValue`; `None` — выделения нет или оно не переведено.
    pub selection: Option<Range16>,
    pub mismatch: Option<Mismatch>,
}

impl Snapshot {
    pub fn new(reading: &Reading) -> Snapshot {
        let value = Value::new(&reading.viewport, VALUE_LIMIT);
        let (selection, mismatch) = match &reading.selection {
            None => (None, None),
            Some(Ok((_, raw))) => match value.map_range(*raw) {
                Some(range) => (Some(range), None),
                None => (None, Some(Mismatch::EndCell)),
            },
            Some(Err(mismatch)) => (None, Some(*mismatch)),
        };
        Snapshot {
            value,
            selection,
            mismatch,
        }
    }

    /// `AXSelectedTextRange`: выделение или точка вставки в конце текста.
    pub fn selected_range(&self) -> Range16 {
        self.selection.unwrap_or(Range16::new(self.value.len16, 0))
    }

    pub fn selected_text(&self) -> Option<String> {
        self.selection
            .filter(|range| range.length > 0)
            .and_then(|range| self.value.substring(range))
    }

    pub fn signature(&self) -> Signature {
        Signature {
            text: self.value.text.clone(),
            selected: self.selected_range(),
        }
    }
}

/// Что видел клиент доступности при последнем обновлении.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    pub text: String,
    pub selected: Range16,
}

/// Изменилось ли значение и выделение с прошлого обновления; первое обновление меняет значение.
pub fn changes(previous: Option<&Signature>, next: &Signature) -> (bool, bool) {
    match previous {
        None => (true, false),
        Some(previous) => (
            previous.text != next.text,
            previous.selected != next.selected,
        ),
    }
}

/// Итог обновления доступности вида.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Refresh {
    /// Вид — элемент доступности: в окне, видим и с поверхностью.
    pub present: bool,
    pub value_changed: bool,
    pub selection_changed: bool,
}

/// Сборка снимка: длительность, вызовы чтения и повод.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Build {
    pub at_ns: u64,
    pub ms: f64,
    pub read_calls: u32,
    pub cause: String,
    /// Запрос пришёл не от стенда: другой клиент доступности в системе.
    pub external: bool,
    pub value_len16: usize,
    pub truncated: bool,
    pub selection: Option<Range16>,
    pub mismatch: Option<Mismatch>,
}

/// Уведомление доступности, отправленное видом.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Post {
    pub at_ns: u64,
    pub notification: &'static str,
    pub reason: String,
}

/// Журнал доступности вида: фактические запросы, сборки и уведомления.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Log {
    pub label: String,
    /// Запросы атрибутов текста; `external` — не от стенда.
    pub requests: u64,
    pub external_requests: u64,
    /// Запросы к виду, который не элемент доступности: ответ пустой, текст не читался.
    pub absent_requests: u64,
    pub cache_hits: u64,
    pub read_calls: u64,
    pub builds: Vec<Build>,
    pub refreshes: u64,
    /// Обновления вида, который не элемент доступности: уведомлений нет.
    pub absent_refreshes: u64,
    pub posts: Vec<Post>,
}

impl Log {
    pub fn push_build(&mut self, build: Build) {
        self.read_calls += u64::from(build.read_calls);
        if self.builds.len() < LOG_LIMIT {
            self.builds.push(build);
        }
    }

    pub fn push_post(&mut self, post: Post) {
        if self.posts.len() < LOG_LIMIT {
            self.posts.push(post);
        }
    }

    /// Сборки по запросам клиента вне стенда.
    pub fn external_builds(&self) -> usize {
        self.builds.iter().filter(|build| build.external).count()
    }
}

/// Запросы доступности не на главном потоке: AXUIElement API к собственному процессу зовёт
/// методы вида в потоке клиента. Такой запрос получает пустой ответ и состояние вида не трогает.
static OFF_MAIN: AtomicU64 = AtomicU64::new(0);

/// Запрос пришёл на главном потоке; иначе он считается в [`off_main_requests`].
pub fn on_main() -> bool {
    // SAFETY: функция без аргументов.
    let main = unsafe { libc::pthread_main_np() } != 0;
    if !main {
        OFF_MAIN.fetch_add(1, Ordering::Relaxed);
    }
    main
}

pub fn off_main_requests() -> u64 {
    OFF_MAIN.load(Ordering::Relaxed)
}

thread_local! {
    /// Запрос клиента доступности стенда в работе: запросы, на которые в это время отвечает
    /// главный поток, — от стенда.
    static STAND_CLIENT: Cell<bool> = const { Cell::new(false) };
    static DEMO: RefCell<Option<Demo>> = const { RefCell::new(None) };
}

/// Клиент доступности стенда начал или закончил запрос.
pub fn stand_client(active: bool) {
    STAND_CLIENT.with(|client| client.set(active));
}

pub fn from_stand() -> bool {
    STAND_CLIENT.with(Cell::get)
}

/// Таймер обновления доступности демонстрации.
struct Demo {
    views: Vec<Weak<SurfaceView>>,
    timer: Option<Retained<NSTimer>>,
    starts: u64,
    ticks: u64,
    tick_ms: Vec<f64>,
}

fn say(line: &str) {
    println!("демонстрация: {line}");
}

/// Вид демонстрации получает обновление доступности по таймеру, пока включён VoiceOver.
pub fn enable_demo(view: &SurfaceView) {
    DEMO.with(|demo| {
        let mut demo = demo.borrow_mut();
        let demo = demo.get_or_insert_with(|| Demo {
            views: Vec::new(),
            timer: None,
            starts: 0,
            ticks: 0,
            tick_ms: Vec::new(),
        });
        demo.views.push(Weak::new(view));
    });
    say(&format!(
        "доступность: таймер обновления с периодом {DEMO_REFRESH_S} с запускается первым запросом доступности при включённом VoiceOver"
    ));
}

fn voice_over_enabled() -> bool {
    NSWorkspace::sharedWorkspace().isVoiceOverEnabled()
}

/// Запрос клиента доступности: в демонстрации при включённом VoiceOver запускает таймер.
pub fn client_request() {
    let idle = DEMO.with(|demo| {
        demo.borrow()
            .as_ref()
            .is_some_and(|demo| demo.timer.is_none())
    });
    if !idle || !voice_over_enabled() {
        return;
    }
    let block = RcBlock::new(move |timer: NonNull<NSTimer>| {
        // SAFETY: таймер жив, пока вызывается его блок.
        demo_tick(unsafe { timer.as_ref() });
    });
    // SAFETY: таймер добавляется в цикл главного потока и снимается там же, поэтому блок
    // вызывается и освобождается только на главном потоке.
    let timer =
        unsafe { NSTimer::timerWithTimeInterval_repeats_block(DEMO_REFRESH_S, true, &block) };
    // SAFETY: `NSRunLoopCommonModes` — неизменяемая константа Foundation.
    unsafe { NSRunLoop::currentRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };
    let starts = DEMO.with(|demo| {
        demo.borrow_mut().as_mut().map_or(0, |demo| {
            demo.timer = Some(timer);
            demo.starts += 1;
            demo.starts
        })
    });
    say(&format!(
        "доступность: VoiceOver включён, таймер обновления запущен ({starts}-й раз), период {DEMO_REFRESH_S} с"
    ));
}

fn demo_tick(timer: &NSTimer) {
    if !voice_over_enabled() {
        timer.invalidate();
        let summary = DEMO.with(|demo| {
            demo.borrow_mut().as_mut().map(|demo| {
                demo.timer = None;
                let median = stats::summarize(&demo.tick_ms).ok().map(|s| s.p50);
                (demo.ticks, median)
            })
        });
        if let Some((ticks, median)) = summary {
            say(&format!(
                "доступность: VoiceOver выключен, таймер остановлен; тиков {ticks}, медиана тика {median:?} мс"
            ));
        }
        return;
    }
    let views: Vec<Retained<SurfaceView>> = DEMO.with(|demo| {
        demo.borrow_mut().as_mut().map_or(Vec::new(), |demo| {
            demo.views.retain(|view| view.load().is_some());
            demo.views.iter().filter_map(Weak::load).collect()
        })
    });
    let started = crate::clock::uptime_ns();
    for view in &views {
        view.a11y_refresh("demo-timer");
    }
    let ms = crate::clock::interval_ms(started, crate::clock::uptime_ns());
    DEMO.with(|demo| {
        if let Some(demo) = demo.borrow_mut().as_mut() {
            demo.ticks += 1;
            if demo.tick_ms.len() < LOG_LIMIT {
                demo.tick_ms.push(ms);
            }
        }
    });
}

/// Остановка таймера при выходе из демонстрации.
pub fn shutdown_demo() {
    if let Some(demo) = DEMO.with(|demo| demo.borrow_mut().take())
        && let Some(timer) = demo.timer
    {
        timer.invalidate();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_trims_trailing_spaces_and_blank_lines() {
        let value = Value::new("ab  \n\nжук 🙂   \n   \n", VALUE_LIMIT);
        assert_eq!(value.text, "ab\n\nжук 🙂");
        assert_eq!(value.len16, utf16::len("ab\n\nжук 🙂"));
        assert!(!value.truncated);
        assert_eq!(value.range_for_line(2), Some(Range16::new(4, 6)));
        assert_eq!(value.range_for_line(3), None);
        assert_eq!(Value::new("", VALUE_LIMIT).text, "");
        assert_eq!(Value::new("   \n  ", VALUE_LIMIT).text, "");
        assert_eq!(
            Value::new("  a", VALUE_LIMIT).text,
            "  a",
            "ведущие пробелы остаются"
        );
    }

    #[test]
    fn value_limit_cuts_on_a_character_boundary() {
        // «a🙂» — три единицы: предел 2 не режет суррогатную пару.
        let value = Value::new("a🙂b", 2);
        assert_eq!(value.text, "a");
        assert!(value.truncated);
        let value = Value::new("ab\ncd", 2);
        assert_eq!(
            value.text, "ab",
            "перевод строки за пределом не добавляется"
        );
        assert!(value.truncated);
        let value = Value::new("ab\ncd", 4);
        assert_eq!(value.text, "ab\nc");
        assert_eq!(
            value.map(utf16::len("ab\ncd")),
            4,
            "за обрезкой — конец текста"
        );
    }

    #[test]
    fn raw_offsets_map_through_trimmed_lines() {
        let raw = "ab  \nжук 🙂   \nx";
        let value = Value::new(raw, VALUE_LIMIT);
        assert_eq!(value.text, "ab\nжук 🙂\nx");
        assert_eq!(value.map(0), 0);
        assert_eq!(value.map(2), 2);
        assert_eq!(value.map(3), 2, "внутри срезанных пробелов — конец строки");
        assert_eq!(value.map(4), 2, "перевод строки — конец строки");
        let second = utf16::len("ab  \n");
        assert_eq!(value.map(second), 3);
        assert_eq!(value.map(second + utf16::len("жук ")), 3 + 4);
        let third = utf16::len("ab  \nжук 🙂   \n");
        assert_eq!(value.map(third), utf16::len("ab\nжук 🙂\n"));
        assert_eq!(value.map(9999), value.len16);
    }

    #[test]
    fn selection_is_found_from_prefix_and_text() {
        let viewport = "a11y-q\nжук 中 🙂 тест\nконец   ";
        let prefix = "a11y-q\nжук 中 🙂 тест";
        let range = selection_range(viewport, prefix, "тест").unwrap();
        assert_eq!(range, Range16::new(utf16::len("a11y-q\nжук 中 🙂 "), 4));
        assert_eq!(range.location, 7 + 9, "эмодзи — две единицы, 中 — одна");
        let across = selection_range(viewport, "a11y-q\nжук", "q\nжук").unwrap();
        assert_eq!(across, Range16::new(5, 5));
        let empty = selection_range(viewport, "a11y", "").unwrap();
        assert_eq!(empty, Range16::new(4, 0));
        assert_eq!(
            selection_range(viewport, "a11y-x", "x"),
            Err(Mismatch::PrefixNotInViewport)
        );
        assert_eq!(
            selection_range(viewport, "a11y-q", "тест"),
            Err(Mismatch::SelectionNotAtPrefixEnd)
        );
    }

    #[test]
    fn selection_maps_into_the_trimmed_value() {
        let viewport = "ab  \nжук 🙂 тест  \nx";
        let prefix = "ab  \nжук 🙂 тест  ";
        let raw = selection_range(viewport, prefix, "тест  ").unwrap();
        let snapshot = Snapshot::new(&Reading {
            viewport: viewport.to_owned(),
            selection: Some(Ok(("тест  ".to_owned(), raw))),
            read_calls: 3,
        });
        let start = utf16::len("ab\nжук 🙂 ");
        assert_eq!(snapshot.selection, Some(Range16::new(start, 4)));
        assert_eq!(snapshot.selected_text().as_deref(), Some("тест"));
        assert_eq!(snapshot.selected_range(), Range16::new(start, 4));
    }

    #[test]
    fn no_selection_puts_the_insertion_point_at_the_end() {
        let snapshot = Snapshot::new(&Reading {
            viewport: "ab\ncd  ".to_owned(),
            selection: None,
            read_calls: 1,
        });
        assert_eq!(snapshot.selected_range(), Range16::new(5, 0));
        assert_eq!(snapshot.selected_text(), None);
        let failed = Snapshot::new(&Reading {
            viewport: "ab".to_owned(),
            selection: Some(Err(Mismatch::NotInViewport)),
            read_calls: 2,
        });
        assert_eq!(failed.selection, None);
        assert_eq!(failed.mismatch, Some(Mismatch::NotInViewport));
        assert_eq!(failed.selected_range(), Range16::new(2, 0));
    }

    #[test]
    fn end_cell_from_linear_offsets() {
        assert_eq!(end_cell(0, 0, 80), Some((0, 0)));
        assert_eq!(end_cell(80 + 10, 3, 80), Some((13, 1)));
        assert_eq!(end_cell(5, 80, 80), Some((5, 1)), "конец включительно");
        assert_eq!(end_cell(1, 2, 0), None);
        assert_eq!(end_cell(u32::MAX, 1, 80), None);
    }

    #[test]
    fn lines_index_and_ranges() {
        let value = Value::new("ab\n\nжук 🙂\n", VALUE_LIMIT);
        assert_eq!(value.text, "ab\n\nжук 🙂");
        assert_eq!(value.line_for_index(0), 0);
        assert_eq!(
            value.line_for_index(2),
            0,
            "перевод строки — конец своей строки"
        );
        assert_eq!(value.line_for_index(3), 1);
        assert_eq!(value.line_for_index(4), 2);
        assert_eq!(value.line_for_index(999), 2);
        assert_eq!(value.range_for_line(0), Some(Range16::new(0, 2)));
        assert_eq!(value.range_for_line(1), Some(Range16::new(3, 0)));
        assert_eq!(value.range_for_line(2), Some(Range16::new(4, 6)));
        assert_eq!(value.range_for_line(3), None);
        assert_eq!(value.substring(Range16::new(8, 2)).as_deref(), Some("🙂"));
        assert_eq!(value.substring(Range16::new(8, 1)), None, "половина пары");
        assert_eq!(value.substring(Range16::new(9, 5)), None);
    }

    #[test]
    fn changes_compare_text_and_selection() {
        let a = Signature {
            text: "ab".to_owned(),
            selected: Range16::new(2, 0),
        };
        assert_eq!(changes(None, &a), (true, false));
        assert_eq!(changes(Some(&a), &a), (false, false));
        let selected = Signature {
            selected: Range16::new(0, 2),
            ..a.clone()
        };
        assert_eq!(changes(Some(&a), &selected), (false, true));
        let text = Signature {
            text: "abc".to_owned(),
            selected: Range16::new(3, 0),
        };
        assert_eq!(changes(Some(&a), &text), (true, true));
    }
}
