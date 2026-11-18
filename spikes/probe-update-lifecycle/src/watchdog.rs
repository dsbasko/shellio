//! Сторож помощника (задача 8; план P0.5, «Протокол замены», «Сторож помощника»). Отпустив
//! блокировку после `launched`, помощник следит за журналом (`kqueue` `EVFILT_VNODE` на
//! `<state>/update`) и за процессом с ключом владельца (`NOTE_EXIT`).
//!
//! Решение ([`decide`]) — чистая функция журнала, ключа владельца, его жизни и времени:
//!
//! - попытка закончена, если журнал больше не в `launched` этой попытки (`committed`, `cleaning`,
//!   `idle` с итогом этой попытки, любое другое состояние или другая попытка) — помощник выходит,
//!   поэтому пропущенное короткое `committed` к сигналу не приводит;
//! - сигнал только при трёх условиях, проверенных перечитыванием журнала непосредственно перед
//!   ним: попытка та же, состояние `launched`, ключ владельца в журнале совпадает с живым
//!   процессом; и только после срока;
//! - владелец завершился до срока без подтверждения — откат без ожидания срока.
//!
//! Время берётся до чтения журнала: решение `Kill` значит, что журнал был в `launched` уже после
//! срока, поэтому `committed`, записанный до срока, сторож всегда видит. Подтверждение, проверившее
//! срок до него, а записанное после, может опоздать к чтению сторожа: тогда владелец снят после
//! `committed`, и сторож под блокировкой доводит уборку и запускает установленный N+1, а не
//! оставляет приложение остановленным. Запускает он N+1, только если под блокировкой журнал ещё в
//! `committed` или `cleaning` этой попытки: `idle` с итогом «подтверждена» значит, что попытку довёл
//! другой процесс, взявший блокировку раньше сторожа, и запуск открыл бы приложение вслед за выходом
//! пользователя ([`rollback`]).
//!
//! Откат идёт под блокировкой после перечитывания журнала. Без записанного решения сторож не
//! действует.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::journal::{self, Journal, State, Store};
use crate::kq;
use crate::observer::{self, ProcessKey};
use crate::owner::{self, Lock};
use crate::recover::Next;
use crate::update::{self, event};

/// Сколько сторож ждёт выхода владельца после `SIGKILL`.
pub const KILL_WAIT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "kebab-case")]
pub enum Action {
    /// Попытка закончена или ушла из `launched`: выйти.
    Finished { why: String },
    /// Ждать события или срока.
    Wait,
    /// Срок истёк, владелец жив и не подтвердился: `SIGKILL` ровно ему, затем откат.
    Kill,
    /// Владелец завершился без подтверждения: откат.
    Rollback,
    /// Журнал противоречит тому, что знает помощник: ничего не делать.
    Stand { why: String },
}

/// Решение сторожа (раздел модуля).
pub fn decide(
    journal: Option<&Journal>,
    attempt: &str,
    owner: ProcessKey,
    owner_alive: bool,
    now_ms: u64,
) -> Action {
    let Some(journal) = journal else {
        return Action::Stand {
            why: "журнала нет".to_owned(),
        };
    };
    if journal.attempt != attempt {
        return Action::Finished {
            why: format!("журнал другой попытки {}", journal.attempt),
        };
    }
    if journal.state != State::Launched {
        return Action::Finished {
            why: format!("попытка в {}", journal.state.name()),
        };
    }
    if journal.owner != Some(owner) {
        return Action::Stand {
            why: format!(
                "ключ владельца в журнале {:?}, помощник записал {owner:?}",
                journal.owner
            ),
        };
    }
    if !owner_alive {
        return Action::Rollback;
    }
    match journal.deadline_ms {
        Some(deadline) if now_ms >= deadline => Action::Kill,
        Some(_) => Action::Wait,
        None => Action::Stand {
            why: "в launched нет срока".to_owned(),
        },
    }
}

/// Итог сторожа.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub summary: String,
    /// Запустить установленный бандл: N после отката или N+1, если владелец снят после
    /// `committed`.
    pub relaunch: bool,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

fn alive(key: ProcessKey) -> bool {
    observer::identify(key).is_ok_and(|found| found.is_alive())
}

/// Сторож попытки `attempt` с владельцем `owner` до её окончания.
pub fn watch(
    store: &Store,
    attempt: &str,
    owner: ProcessKey,
    deadline_ms: u64,
    me: ProcessKey,
) -> Result<Outcome, String> {
    let mut queue = kq::Queue::new()?;
    queue.watch_dir(&store.state().join(journal::UPDATE_DIR))?;
    queue.watch_exit(owner.pid)?;
    loop {
        // До чтения журнала: см. раздел модуля.
        let now = now_ms();
        let journal = store.read().map_err(|err| err.to_string())?;
        let action = decide(journal.as_ref(), attempt, owner, alive(owner), now);
        match action {
            Action::Finished { why } => {
                event("watch-finished", json!({ "why": why }));
                return Ok(Outcome {
                    summary: format!("попытка закончена без сторожа: {why}"),
                    relaunch: false,
                });
            }
            Action::Stand { why } => {
                event("watch-stand", json!({ "why": why }));
                return Err(format!("сторож не действует: {why}"));
            }
            Action::Wait => {
                let left = deadline_ms.saturating_sub(now_ms());
                let until = Instant::now() + Duration::from_millis(left);
                queue.wait(until)?;
            }
            Action::Kill => {
                event("watch-kill", json!({ "owner": owner }));
                // SAFETY: сигнал процессу, ключ которого только что сверен с журналом и ядром.
                if unsafe { libc::kill(owner.pid, libc::SIGKILL) } != 0 {
                    let err = std::io::Error::last_os_error();
                    if err.raw_os_error() != Some(libc::ESRCH) {
                        return Err(format!("SIGKILL {}: {err}", owner.pid));
                    }
                }
                let left = crate::launch::wait_exit(&[owner.pid], KILL_WAIT);
                if !left.is_empty() && alive(owner) {
                    return Err(format!(
                        "владелец {} не вышел за {} с после SIGKILL",
                        owner.pid,
                        KILL_WAIT.as_secs()
                    ));
                }
                return rollback(store, attempt, me, true, "срок истёк, владелец снят");
            }
            Action::Rollback => {
                event("watch-owner-exited", json!({ "owner": owner }));
                return rollback(
                    store,
                    attempt,
                    me,
                    false,
                    "владелец завершился без подтверждения",
                );
            }
        }
    }
}

/// Подтверждение попытки `attempt` записано, а уборка не доведена: `committed` или `cleaning`.
fn confirmed(journal: Option<&Journal>, attempt: &str) -> bool {
    journal.is_some_and(|journal| {
        journal.attempt == attempt && matches!(journal.state, State::Committed | State::Cleaning)
    })
}

/// Под блокировкой: если журнал всё ещё в `launched` этой попытки — откат по таблице
/// восстановления. Если владелец снят (`killed`), а журнал под блокировкой в `committed` или
/// `cleaning` этой попытки — уборка по таблице и запуск установленного N+1: уборку никто не довёл,
/// значит, владельца после снятия никто не принял. Иначе ничего.
fn rollback(
    store: &Store,
    attempt: &str,
    me: ProcessKey,
    killed: bool,
    why: &str,
) -> Result<Outcome, String> {
    let lock = Lock::acquire(&owner::lock_path(store.state()))
        .map_err(|err| format!("блокировка: {err}"))?;
    let journal = store.read().map_err(|err| err.to_string())?;
    if killed && confirmed(journal.as_ref(), attempt) {
        let journal = journal.ok_or("журнала нет")?;
        let options = update::mode(&journal)?;
        let report = update::recover_now(store, &options, me)?;
        let done = store.read().map_err(|err| err.to_string())?;
        drop(lock);
        if !done
            .as_ref()
            .is_some_and(|done| update::committed(done, attempt))
        {
            return Err(format!("после уборки снятого владельца журнал {done:?}"));
        }
        event(
            "watch-killed-committed",
            json!({ "why": why, "steps": format!("{:?}", report.steps) }),
        );
        return Ok(Outcome {
            summary: format!(
                "{why} после committed: уборка {:?}, N+1 запущен",
                report.steps
            ),
            relaunch: true,
        });
    }
    let Some(journal) =
        journal.filter(|journal| journal.attempt == attempt && journal.state == State::Launched)
    else {
        drop(lock);
        event("watch-yield", json!({ "why": why }));
        return Ok(Outcome {
            summary: format!("{why}, но под блокировкой попытка уже не в launched: ничего"),
            relaunch: false,
        });
    };
    let options = update::mode(&journal)?;
    let report = update::recover_now(store, &options, me)?;
    drop(lock);
    event(
        "watch-rolled-back",
        json!({ "why": why, "steps": format!("{:?}", report.steps) }),
    );
    Ok(Outcome {
        summary: format!("{why}: откат {:?}", report.steps),
        relaunch: report.next == Next::LaunchInstalled,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::Outcome as Result_;
    use crate::journal::tests::{ATTEMPT, DEADLINE, OWNER, journal_in};
    use std::path::Path;

    const BEFORE: u64 = DEADLINE - 1;

    fn launched() -> Journal {
        journal_in(State::Launched, Path::new("/i/probe-update.app"))
    }

    fn is_signal(action: &Action) -> bool {
        matches!(action, Action::Kill)
    }

    #[test]
    fn attempt_is_finished_once_the_journal_left_launched_of_this_attempt() {
        for state in [State::Committed, State::Cleaning, State::RollbackIntent] {
            let journal = journal_in(state, Path::new("/i/probe-update.app"));
            assert!(
                matches!(
                    decide(Some(&journal), ATTEMPT, OWNER, true, DEADLINE + 1),
                    Action::Finished { .. }
                ),
                "{}",
                state.name()
            );
        }
        let mut idle = journal_in(State::Idle, Path::new("/i/probe-update.app"));
        idle.outcome = Some(Result_::Committed);
        assert!(matches!(
            decide(Some(&idle), ATTEMPT, OWNER, true, DEADLINE + 1),
            Action::Finished { .. }
        ));
        let mut other = launched();
        other.attempt = "ffff".to_owned();
        assert!(matches!(
            decide(Some(&other), ATTEMPT, OWNER, true, DEADLINE + 1),
            Action::Finished { .. }
        ));
    }

    #[test]
    fn confirmed_attempt_is_committed_or_cleaning_only() {
        let at = |state| journal_in(state, Path::new("/i/probe-update.app"));
        assert!(confirmed(Some(&at(State::Committed)), ATTEMPT));
        assert!(confirmed(Some(&at(State::Cleaning)), ATTEMPT));
        let mut idle = at(State::Idle);
        idle.outcome = Some(Result_::Committed);
        assert!(
            !confirmed(Some(&idle), ATTEMPT),
            "уборку довёл другой процесс: сторож не запускает N+1"
        );
        idle.outcome = Some(Result_::RolledBack);
        assert!(!confirmed(Some(&idle), ATTEMPT), "откат — не подтверждение");
        for state in [State::Launched, State::RollbackIntent, State::Swapped] {
            assert!(!confirmed(Some(&at(state)), ATTEMPT), "{}", state.name());
        }
        let mut other = at(State::Committed);
        other.attempt = "ffff".to_owned();
        assert!(!confirmed(Some(&other), ATTEMPT), "другая попытка");
        assert!(!confirmed(None, ATTEMPT));
    }

    #[test]
    fn signal_needs_same_attempt_launched_and_live_owner_key() {
        let journal = launched();
        assert_eq!(
            decide(Some(&journal), ATTEMPT, OWNER, true, DEADLINE),
            Action::Kill,
            "все три условия и срок"
        );
        assert_eq!(
            decide(Some(&journal), ATTEMPT, OWNER, true, BEFORE),
            Action::Wait,
            "до срока сигнала нет"
        );
        // Попытка не та.
        let mut other = journal.clone();
        other.attempt = "ffff".to_owned();
        assert!(!is_signal(&decide(
            Some(&other),
            ATTEMPT,
            OWNER,
            true,
            DEADLINE
        )));
        // Состояние не launched: короткое committed, пропущенное сторожем, тоже.
        for state in [
            State::Committed,
            State::Cleaning,
            State::Idle,
            State::Swapped,
        ] {
            let mut moved = journal.clone();
            moved.state = state;
            assert!(!is_signal(&decide(
                Some(&moved),
                ATTEMPT,
                OWNER,
                true,
                DEADLINE
            )));
        }
        // Ключ владельца в журнале не тот.
        let stranger = ProcessKey {
            pid: OWNER.pid,
            start_us: OWNER.start_us + 1,
        };
        assert!(matches!(
            decide(Some(&journal), ATTEMPT, stranger, true, DEADLINE),
            Action::Stand { .. }
        ));
        // Владелец не жив: откат, а не сигнал.
        assert_eq!(
            decide(Some(&journal), ATTEMPT, OWNER, false, DEADLINE),
            Action::Rollback
        );
        assert_eq!(
            decide(Some(&journal), ATTEMPT, OWNER, false, BEFORE),
            Action::Rollback,
            "выход владельца до срока — откат без ожидания срока"
        );
        assert!(matches!(
            decide(None, ATTEMPT, OWNER, true, DEADLINE),
            Action::Stand { .. }
        ));
    }
}
