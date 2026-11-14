//! Буфер обмена поверхности: реестр запросов, которые libghostty держит до ответа, и доска
//! `NSPasteboard`, с которой работает главный поток.
//!
//! Токен — указатель состояния запроса libghostty. Он приходит из двух входов:
//! - `read_clipboard_cb`, вернувший `STARTED`: запрос заносится до возврата из колбэка;
//! - `confirm_read_clipboard_cb`: токен без чтения (запись kitty) или из `complete`, который ушёл в
//!   подтверждение с тем же токеном синхронно, внутри вызова.
//!
//! Поэтому `complete` не освобождает токен окончательно: если подтверждение пришло во время
//! вызова, токен остаётся в реестре до `deny`, иначе библиотека его освободила. Блокировка реестра
//! никогда не держится во время вызова в libghostty: повторный вход из `complete` в подтверждение
//! её не встречает. Закрытие перестаёт принимать запросы и отдаёт ожидающие для отклонения ровно
//! один раз. Поздняя выдача по снятому ID указатель не разыменовывает.

use std::cell::RefCell;
use std::sync::Mutex;

use objc2::msg_send;
use objc2::rc::Retained;
use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
use objc2_foundation::NSString;
use serde::Serialize;
use shellio_sys_ghostty as sys;

/// Указатель состояния запроса libghostty как число: он не разыменовывается в Rust.
pub type Token = usize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct RequestId(pub u64);

/// Ответ колбэка чтения.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadDecision {
    /// `STARTED`: запрос занесён и завершится на следующем проходе главного потока.
    Started(RequestId),
    /// Поверхность закрывается: состояние освобождает сама библиотека.
    Unavailable,
    /// Поддержан только обычный буфер.
    Unsupported,
}

impl ReadDecision {
    pub fn raw(self) -> sys::ghostty_clipboard_read_result_e {
        match self {
            ReadDecision::Started(_) => {
                sys::ghostty_clipboard_read_result_e_GHOSTTY_CLIPBOARD_READ_STARTED
            }
            ReadDecision::Unavailable => {
                sys::ghostty_clipboard_read_result_e_GHOSTTY_CLIPBOARD_READ_UNAVAILABLE
            }
            ReadDecision::Unsupported => {
                sys::ghostty_clipboard_read_result_e_GHOSTTY_CLIPBOARD_READ_UNSUPPORTED
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    /// Ждёт выдачи.
    Read,
    /// Главный поток внутри `complete`.
    Completing,
    /// Ждёт отклонения.
    Confirm,
    /// Главный поток внутри `deny`.
    Denying,
}

#[derive(Debug)]
struct Entry {
    id: RequestId,
    token: Token,
    stage: Stage,
}

/// Счётчики реестра для итога.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Stats {
    pub reads_started: u64,
    pub reads_unavailable: u64,
    pub reads_unsupported: u64,
    pub served: u64,
    /// `complete` вернулся без подтверждения: библиотека освободила состояние.
    pub completed: u64,
    /// `complete` ушёл в подтверждение с тем же токеном.
    pub completed_to_confirm: u64,
    pub confirms: u64,
    /// Подтверждение с токеном, которого в реестре не было (запись kitty без чтения).
    pub confirms_without_read: u64,
    pub denied: u64,
    pub denied_on_close: u64,
    /// Выдача или отклонение по снятому ID: указатель не тронут.
    pub late_skipped: u64,
}

#[derive(Debug, Default)]
struct Inner {
    closed: bool,
    next: u64,
    entries: Vec<Entry>,
    stats: Stats,
}

impl Inner {
    fn add(&mut self, token: Token, stage: Stage) -> RequestId {
        self.next += 1;
        let id = RequestId(self.next);
        self.entries.push(Entry { id, token, stage });
        id
    }

    fn entry(&mut self, id: RequestId) -> Option<&mut Entry> {
        self.entries.iter_mut().find(|entry| entry.id == id)
    }

    fn remove(&mut self, id: RequestId) {
        self.entries.retain(|entry| entry.id != id);
    }
}

/// Реестр запросов одной поверхности. Его держит контекст `userdata`, колбэки зовут его с любого
/// потока, выдачу и отклонение ведёт главный поток.
#[derive(Debug, Default)]
pub struct Requests {
    inner: Mutex<Inner>,
}

/// Что сделала выдача.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Served {
    /// ID снят закрытием: вызова в библиотеку не было.
    Skipped,
    /// Библиотека завершила запрос и освободила состояние.
    Completed,
    /// Запрос ушёл в подтверждение и ждёт отклонения.
    Confirming(RequestId),
}

impl Requests {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Колбэк чтения: решение и запись в реестр до возврата из колбэка.
    pub fn begin_read(&self, token: Token, location: sys::ghostty_clipboard_e) -> ReadDecision {
        let mut inner = self.lock();
        if inner.closed {
            inner.stats.reads_unavailable += 1;
            return ReadDecision::Unavailable;
        }
        if location != sys::ghostty_clipboard_e_GHOSTTY_CLIPBOARD_STANDARD {
            inner.stats.reads_unsupported += 1;
            return ReadDecision::Unsupported;
        }
        inner.stats.reads_started += 1;
        ReadDecision::Started(inner.add(token, Stage::Read))
    }

    /// Колбэк подтверждения. Подтверждение обязано получить ответ, поэтому оно заносится и после
    /// закрытия: его отклоняет последняя проверка перед освобождением поверхности.
    pub fn confirm(&self, token: Token) -> RequestId {
        let mut inner = self.lock();
        inner.stats.confirms += 1;
        let existing = inner
            .entries
            .iter_mut()
            .find(|entry| entry.token == token && entry.stage != Stage::Denying);
        if let Some(entry) = existing {
            let completing = entry.stage == Stage::Completing;
            entry.stage = Stage::Confirm;
            let id = entry.id;
            if completing {
                inner.stats.completed_to_confirm += 1;
            }
            return id;
        }
        inner.stats.confirms_without_read += 1;
        inner.add(token, Stage::Confirm)
    }

    /// Выдача на главном потоке: `complete` вызывается без блокировки реестра.
    pub fn serve(&self, id: RequestId, complete: impl FnOnce(Token)) -> Served {
        let token = {
            let mut inner = self.lock();
            match inner.entry(id) {
                Some(entry) if entry.stage == Stage::Read => {
                    entry.stage = Stage::Completing;
                    entry.token
                }
                _ => {
                    inner.stats.late_skipped += 1;
                    return Served::Skipped;
                }
            }
        };
        complete(token);
        let mut inner = self.lock();
        inner.stats.served += 1;
        match inner.entry(id).map(|entry| entry.stage) {
            Some(Stage::Confirm) => Served::Confirming(id),
            _ => {
                inner.remove(id);
                inner.stats.completed += 1;
                Served::Completed
            }
        }
    }

    /// Отклонение подтверждения на главном потоке, без блокировки во время `deny`. `false` — ID
    /// уже снят.
    pub fn deny(&self, id: RequestId, deny: impl FnOnce(Token)) -> bool {
        let token = {
            let mut inner = self.lock();
            match inner.entry(id) {
                Some(entry) if entry.stage == Stage::Confirm => {
                    entry.stage = Stage::Denying;
                    entry.token
                }
                _ => {
                    inner.stats.late_skipped += 1;
                    return false;
                }
            }
        };
        deny(token);
        let mut inner = self.lock();
        inner.remove(id);
        inner.stats.denied += 1;
        true
    }

    /// Закрытие: новые чтения отвечают «недоступно», ожидающие запросы отклоняются через `deny`
    /// ровно один раз, пока поверхность жива. Возвращает число отклонённых.
    pub fn close(&self, mut deny: impl FnMut(Token)) -> usize {
        let tokens: Vec<Token> = {
            let mut inner = self.lock();
            inner.closed = true;
            let mut taken = Vec::new();
            inner.entries.retain(|entry| {
                let waiting = matches!(entry.stage, Stage::Read | Stage::Confirm);
                if waiting {
                    taken.push(entry.token);
                }
                !waiting
            });
            inner.stats.denied_on_close += taken.len() as u64;
            taken
        };
        for token in &tokens {
            deny(*token);
        }
        tokens.len()
    }

    pub fn pending(&self) -> usize {
        self.lock().entries.len()
    }

    #[cfg(test)]
    pub fn is_closed(&self) -> bool {
        self.lock().closed
    }

    pub fn stats(&self) -> Stats {
        self.lock().stats.clone()
    }
}

/// Текст записи буфера обмена из представлений, скопированных в колбэке.
pub fn plain_text(contents: &[(String, Vec<u8>)]) -> Option<String> {
    contents
        .iter()
        .find(|(mime, _)| mime == "text/plain" || mime.starts_with("text/plain;"))
        .map(|(_, data)| String::from_utf8_lossy(data).into_owned())
}

// ---- доска главного потока ----

/// Доска, с которой работает буфер обмена: сценарии — собственная именованная, демонстрация —
/// общая.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Board {
    General,
    Named(String),
}

thread_local! {
    static BOARD: RefCell<Board> = const { RefCell::new(Board::General) };
}

pub fn use_board(board: Board) {
    BOARD.with(|cell| *cell.borrow_mut() = board);
}

pub fn board() -> Board {
    BOARD.with(|cell| cell.borrow().clone())
}

fn pasteboard() -> Retained<NSPasteboard> {
    match board() {
        Board::General => NSPasteboard::generalPasteboard(),
        Board::Named(name) => NSPasteboard::pasteboardWithName(&NSString::from_str(&name)),
    }
}

/// Текст доски. Вызывается на главном потоке.
pub fn read_text() -> Option<String> {
    // SAFETY: `NSPasteboardTypeString` — неизменяемая константа AppKit.
    let kind = unsafe { NSPasteboardTypeString };
    pasteboard()
        .stringForType(kind)
        .map(|text| text.to_string())
}

/// Заменяет содержимое доски текстом. Вызывается на главном потоке.
pub fn write_text(text: &str) -> bool {
    let board = pasteboard();
    board.clearContents();
    // SAFETY: `NSPasteboardTypeString` — неизменяемая константа AppKit.
    let kind = unsafe { NSPasteboardTypeString };
    board.setString_forType(&NSString::from_str(text), kind)
}

/// Освобождает именованную доску сценария в системе.
pub fn release_named(name: &str) {
    let board = NSPasteboard::pasteboardWithName(&NSString::from_str(name));
    // SAFETY: `releaseGlobally` без аргументов есть у любой доски.
    let () = unsafe { msg_send![&*board, releaseGlobally] };
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    const STANDARD: sys::ghostty_clipboard_e = sys::ghostty_clipboard_e_GHOSTTY_CLIPBOARD_STANDARD;

    fn started(decision: ReadDecision) -> RequestId {
        match decision {
            ReadDecision::Started(id) => id,
            other => panic!("ожидался STARTED, а не {other:?}"),
        }
    }

    #[test]
    fn read_is_registered_before_return_and_served_once() {
        let requests = Requests::default();
        let id = started(requests.begin_read(0xA0, STANDARD));
        assert_eq!(requests.pending(), 1, "занесён до возврата из колбэка");
        let completed = Cell::new(Vec::new());
        let served = requests.serve(id, |token| completed.set(vec![token]));
        assert_eq!(served, Served::Completed);
        assert_eq!(completed.take(), [0xA0]);
        assert_eq!(requests.pending(), 0);
        assert_eq!(
            requests.serve(id, |_| panic!("второй выдачи нет")),
            Served::Skipped
        );
        let stats = requests.stats();
        assert_eq!(
            (stats.served, stats.completed, stats.late_skipped),
            (1, 1, 1)
        );
    }

    #[test]
    fn non_started_results_are_never_completed() {
        let requests = Requests::default();
        let selection = sys::ghostty_clipboard_e_GHOSTTY_CLIPBOARD_SELECTION;
        assert_eq!(requests.begin_read(1, selection), ReadDecision::Unsupported);
        assert_eq!(
            requests.pending(),
            0,
            "не STARTED: состояние освобождает библиотека"
        );
        requests.close(|_| panic!("отклонять нечего"));
        assert_eq!(requests.begin_read(2, STANDARD), ReadDecision::Unavailable);
        assert_eq!(requests.pending(), 0);
        assert_eq!(
            ReadDecision::Unavailable.raw(),
            sys::ghostty_clipboard_read_result_e_GHOSTTY_CLIPBOARD_READ_UNAVAILABLE
        );
    }

    #[test]
    fn confirm_token_without_read_is_denied() {
        let requests = Requests::default();
        let id = requests.confirm(0xB0);
        assert_eq!(requests.stats().confirms_without_read, 1);
        let denied = Cell::new(0);
        assert!(requests.deny(id, |token| denied.set(token)));
        assert_eq!(denied.get(), 0xB0);
        assert!(!requests.deny(id, |_| panic!("второго отклонения нет")));
        assert_eq!(requests.pending(), 0);
    }

    #[test]
    fn complete_that_goes_to_confirm_keeps_the_token_without_holding_the_lock() {
        let requests = Requests::default();
        let id = started(requests.begin_read(0xC0, STANDARD));
        // Повторный вход: `complete` синхронно зовёт подтверждение с тем же токеном. Если бы
        // блокировка держалась во время вызова, тест бы завис.
        let served = requests.serve(id, |token| {
            let again = requests.confirm(token);
            assert_eq!(again, id, "тот же запрос");
        });
        assert_eq!(served, Served::Confirming(id));
        assert_eq!(requests.pending(), 1, "токен не освобождён");
        let stats = requests.stats();
        assert_eq!((stats.completed, stats.completed_to_confirm), (0, 1));
        assert!(requests.deny(id, |token| assert_eq!(token, 0xC0)));
        assert_eq!(requests.pending(), 0);
    }

    #[test]
    fn close_denies_pending_exactly_once_and_late_serve_is_skipped() {
        let requests = Requests::default();
        let read = started(requests.begin_read(1, STANDARD));
        let confirm = requests.confirm(2);
        let denied = RefCell::new(Vec::new());
        assert_eq!(requests.close(|token| denied.borrow_mut().push(token)), 2);
        assert_eq!(*denied.borrow(), [1, 2]);
        assert_eq!(
            requests.close(|_| panic!("второе закрытие ничего не отклоняет")),
            0
        );
        assert_eq!(
            requests.serve(read, |_| panic!("поздняя выдача не трогает указатель")),
            Served::Skipped
        );
        assert!(!requests.deny(confirm, |_| panic!("уже отклонено при закрытии")));
        assert!(requests.is_closed());
        let stats = requests.stats();
        assert_eq!((stats.denied_on_close, stats.late_skipped), (2, 2));
    }

    #[test]
    fn confirm_after_close_is_kept_for_the_final_sweep() {
        let requests = Requests::default();
        requests.close(|_| {});
        let id = requests.confirm(7);
        assert_eq!(requests.pending(), 1);
        let denied = Cell::new(0);
        assert_eq!(requests.close(|token| denied.set(token)), 1);
        assert_eq!(denied.get(), 7);
        assert!(!requests.deny(id, |_| panic!("уже отклонено")));
    }

    #[test]
    fn plain_text_picks_text_representation() {
        let contents = vec![
            ("text/html".to_owned(), b"<b>x</b>".to_vec()),
            ("text/plain".to_owned(), "жук".as_bytes().to_vec()),
        ];
        assert_eq!(plain_text(&contents).as_deref(), Some("жук"));
        assert_eq!(plain_text(&contents[..1]), None);
    }
}
