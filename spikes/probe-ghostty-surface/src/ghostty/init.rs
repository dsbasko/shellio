//! Контракт процесса `ghostty_init`: ровно один вызов до `NSApplication`, до рабочих потоков пробы
//! и до журналирования из потоков. `argv` — собственный минимальный массив из имени программы,
//! чтобы флаги пробы не стали действиями или настройками ghostty; строки и массив указателей живут
//! до конца процесса.
//!
//! До и после вызова записываются переменные языка, эффективные категории locale и число потоков:
//! `ghostty_init` вызывает `setlocale(LC_ALL, "")` и при отсутствующем или пустом `LANG` выбирает
//! язык через Cocoa (`src/os/locale.zig`).

use std::collections::BTreeMap;
use std::ffi::{CStr, c_char, c_int};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;
use shellio_sys_ghostty as sys;

use crate::clock;

static CALLED: AtomicBool = AtomicBool::new(false);

/// Категории locale, которые читаются до и после.
const CATEGORIES: [(c_int, &str); 7] = [
    (libc::LC_ALL, "LC_ALL"),
    (libc::LC_COLLATE, "LC_COLLATE"),
    (libc::LC_CTYPE, "LC_CTYPE"),
    (libc::LC_MESSAGES, "LC_MESSAGES"),
    (libc::LC_MONETARY, "LC_MONETARY"),
    (libc::LC_NUMERIC, "LC_NUMERIC"),
    (libc::LC_TIME, "LC_TIME"),
];

/// Состояние процесса, которое меняет `ghostty_init`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ProcessState {
    /// `LANG`, `LANGUAGE` и `LC_*` из окружения процесса.
    pub language_env: BTreeMap<String, String>,
    /// Эффективные категории locale по `setlocale(категория, NULL)`.
    pub locale: BTreeMap<String, String>,
    pub threads: Option<i32>,
}

impl ProcessState {
    fn read() -> ProcessState {
        ProcessState {
            language_env: language_vars(std::env::vars_os().map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                )
            })),
            locale: CATEGORIES
                .iter()
                .filter_map(|(category, name)| {
                    // SAFETY: запрос с NULL только читает текущую категорию; потоков, которые
                    // меняют locale, в этот момент нет.
                    let current = unsafe { libc::setlocale(*category, std::ptr::null()) };
                    // SAFETY: `setlocale` вернул NULL или строку библиотеки C с нулём.
                    let value = unsafe { c_str(current) }?;
                    Some(((*name).to_owned(), value))
                })
                .collect(),
            threads: thread_count(),
        }
    }
}

/// Итог единственного вызова `ghostty_init`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct InitRecord {
    pub code: i32,
    pub argv: Vec<String>,
    pub before: ProcessState,
    pub after: ProcessState,
    pub duration_ms: f64,
}

impl InitRecord {
    /// Изменения переменных языка, которые сделал `ghostty_init`.
    pub fn env_changes(&self) -> Vec<String> {
        env_changes(&self.before.language_env, &self.after.language_env)
    }
}

/// Вызывает `ghostty_init` один раз за процесс. Второй вызов — ошибка без обращения к библиотеке.
pub fn init_once(program: &str) -> Result<InitRecord, String> {
    if CALLED.swap(true, Ordering::AcqRel) {
        return Err("ghostty_init уже вызван в этом процессе".to_owned());
    }
    let name = super::c_string("имя программы", program)?;
    // Массив и строка нужны libghostty до конца процесса: `global.zig` хранит `argv`.
    let name: &'static CStr = Box::leak(name.into_boxed_c_str());
    let argv: &'static mut [*mut c_char; 2] =
        Box::leak(Box::new([name.as_ptr().cast_mut(), std::ptr::null_mut()]));
    let before = ProcessState::read();
    let started = clock::uptime_ns();
    // SAFETY: один вызов за процесс до потоков пробы; `argv` из одной строки с нулевым
    // завершением живёт до конца процесса.
    let code = unsafe { sys::ghostty_init(1, argv.as_mut_ptr()) };
    let finished = clock::uptime_ns();
    Ok(InitRecord {
        code,
        argv: vec![program.to_owned()],
        before,
        after: ProcessState::read(),
        duration_ms: clock::interval_ms(started, finished),
    })
}

/// Переменные языка из окружения.
pub fn language_vars(env: impl Iterator<Item = (String, String)>) -> BTreeMap<String, String> {
    env.filter(|(name, _)| name == "LANG" || name == "LANGUAGE" || name.starts_with("LC_"))
        .collect()
}

/// Изменения: `+ИМЯ=значение`, `-ИМЯ`, `~ИМЯ=старое→новое`.
pub fn env_changes(
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut changes = Vec::new();
    for (name, value) in after {
        match before.get(name) {
            None => changes.push(format!("+{name}={value}")),
            Some(old) if old != value => changes.push(format!("~{name}={old}→{value}")),
            Some(_) => {}
        }
    }
    for name in before.keys() {
        if !after.contains_key(name) {
            changes.push(format!("-{name}"));
        }
    }
    changes
}

/// Число потоков процесса по `proc_pidinfo(PROC_PIDTASKINFO)`.
pub fn thread_count() -> Option<i32> {
    let mut info = std::mem::MaybeUninit::<libc::proc_taskinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_taskinfo>() as c_int;
    // SAFETY: буфер размера `size` живёт на время вызова.
    let written = unsafe {
        libc::proc_pidinfo(
            libc::getpid(),
            libc::PROC_PIDTASKINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written != size {
        return None;
    }
    // SAFETY: вызов заполнил структуру целиком.
    Some(unsafe { info.assume_init() }.pti_threadnum)
}

/// # Safety
/// `ptr` — NULL или строка с нулём.
unsafe fn c_str(ptr: *const c_char) -> Option<String> {
    // SAFETY: контракт передаётся вызывающему.
    unsafe { super::copy_c_str(ptr) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn language_vars_keep_only_language() {
        let env = [
            ("HOME", "/h"),
            ("LANG", "en_US.UTF-8"),
            ("LANGUAGE", "en"),
            ("LC_NUMERIC", "C"),
            ("LCX", "no"),
        ]
        .map(|(name, value)| (name.to_owned(), value.to_owned()));
        assert_eq!(
            language_vars(env.into_iter()),
            map(&[
                ("LANG", "en_US.UTF-8"),
                ("LANGUAGE", "en"),
                ("LC_NUMERIC", "C")
            ])
        );
    }

    #[test]
    fn env_changes_list_added_changed_and_removed() {
        let before = map(&[("LANG", "en_US.UTF-8"), ("LC_ALL", "C")]);
        let after = map(&[("LANG", "ru_RU.UTF-8"), ("LC_CTYPE", "UTF-8")]);
        assert_eq!(
            env_changes(&before, &after),
            [
                "~LANG=en_US.UTF-8→ru_RU.UTF-8",
                "+LC_CTYPE=UTF-8",
                "-LC_ALL"
            ]
        );
        assert!(env_changes(&before, &before).is_empty());
    }

    #[test]
    fn thread_count_sees_this_process() {
        assert!(thread_count().is_some_and(|threads| threads >= 1));
    }
}
