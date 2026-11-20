//! Учёт исполняемых участков (ADR-0011, «Оценка ≈»; план P0.8, «Solution Overview»).
//!
//! Участок — один `poll` обёрнутой future ([`Accounted`]) или один запуск обёрнутого замыкания
//! ([`Ledger::blocking`], для `spawn_blocking`). CPU потока читается на входе и выходе участка и
//! начисляется логическому владельцу; между `poll` часы не держатся, поэтому ожидание не
//! начисляется, а future, которую опрашивают разные потоки, складывается из частей на каждом
//! потоке. Стек участков на потоке вычитает из внешнего участка время вложенного: вложенный
//! учтённый участок другого владельца (например, общая работа «Git», которую панель опрашивает
//! у себя) начисляется своему владельцу, а не внешнему. Отказ чтения часов не скрывается: участок
//! не начисляется и считается неизмеренным, а внешний участок, из которого нельзя вычесть
//! вложенный, тоже.

pub mod clock;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};

use serde::Serialize;

use crate::attrib::Owner;
use clock::Clock;

/// Счёт владельца. Начисленное CPU и число участков обновляются на каждом участке без
/// блокировки; части по потокам и смены потока — при завершении обёртки.
#[derive(Debug, Default)]
pub struct Account {
    cpu_ns: AtomicU64,
    spans: AtomicU64,
    unmeasured: AtomicU64,
    migrations: AtomicU64,
    detail: Mutex<Detail>,
}

#[derive(Debug, Default)]
struct Detail {
    per_thread: BTreeMap<u64, u64>,
    requested_by: BTreeSet<String>,
}

impl Account {
    fn detail(&self) -> MutexGuard<'_, Detail> {
        self.detail.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn merge_parts(&self, parts: &[(u64, u64)]) {
        if parts.is_empty() {
            return;
        }
        let mut detail = self.detail();
        for &(thread, ns) in parts {
            *detail.per_thread.entry(thread).or_default() += ns;
        }
    }

    pub fn charge(&self) -> Charge {
        let detail = self.detail();
        Charge {
            cpu_ns: self.cpu_ns.load(Ordering::Relaxed),
            spans: self.spans.load(Ordering::Relaxed),
            unmeasured_spans: self.unmeasured.load(Ordering::Relaxed),
            migrations: self.migrations.load(Ordering::Relaxed),
            per_thread_ns: detail.per_thread.clone(),
            requested_by: detail.requested_by.iter().cloned().collect(),
        }
    }
}

/// Начисленное владельцу на момент чтения.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Charge {
    pub cpu_ns: u64,
    /// Участки: `poll` и запуски замыканий.
    pub spans: u64,
    /// Участки без начисления: отказ чтения часов у участка или у вложенного в него.
    pub unmeasured_spans: u64,
    /// Смены потока между соседними `poll` одной future.
    pub migrations: u64,
    /// Части по номеру потока ([`thread_index`]) у завершённых обёрток.
    pub per_thread_ns: BTreeMap<u64, u64>,
    /// Панели, запросившие общую работу; итог им не начисляется.
    pub requested_by: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OwnerCharge {
    pub owner: Owner,
    pub charge: Charge,
}

/// Номер потока процесса: присваивается при первом участке на потоке, по порядку.
pub fn thread_index() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    thread_local! {
        static INDEX: u64 = NEXT.fetch_add(1, Ordering::Relaxed);
    }
    INDEX.with(|index| *index)
}

/// Открытый участок на стеке потока: CPU вложенных участков и признак вложенного без замера.
struct Frame {
    child_ns: u64,
    tainted: bool,
}

thread_local! {
    static STACK: RefCell<Vec<Frame>> = const { RefCell::new(Vec::new()) };
}

/// Участок на стеке потока. Закрывается в `Drop`, поэтому паника внутри не оставляет кадр.
struct Span<'a, C: Clock> {
    clock: &'a C,
    account: &'a Account,
    start: Option<u64>,
    /// Своё CPU участка, если он измерен.
    own: &'a mut Option<u64>,
}

impl<'a, C: Clock> Span<'a, C> {
    fn enter(clock: &'a C, account: &'a Account, own: &'a mut Option<u64>) -> Span<'a, C> {
        STACK.with(|stack| {
            stack.borrow_mut().push(Frame {
                child_ns: 0,
                tainted: false,
            })
        });
        let start = clock.now_ns();
        Span {
            clock,
            account,
            start,
            own,
        }
    }
}

impl<C: Clock> Drop for Span<'_, C> {
    fn drop(&mut self) {
        let end = self.clock.now_ns();
        let elapsed = match (self.start, end) {
            (Some(start), Some(end)) => Some(end.saturating_sub(start)),
            _ => None,
        };
        let frame = STACK.with(|stack| {
            let mut stack = stack.borrow_mut();
            let frame = stack.pop();
            if let Some(parent) = stack.last_mut() {
                match elapsed {
                    Some(ns) => parent.child_ns += ns,
                    None => parent.tainted = true,
                }
            }
            frame
        });
        self.account.spans.fetch_add(1, Ordering::Relaxed);
        let own = match (elapsed, frame) {
            (Some(ns), Some(frame)) if !frame.tainted => Some(ns.saturating_sub(frame.child_ns)),
            _ => None,
        };
        match own {
            Some(ns) => {
                self.account.cpu_ns.fetch_add(ns, Ordering::Relaxed);
            }
            None => {
                self.account.unmeasured.fetch_add(1, Ordering::Relaxed);
            }
        }
        *self.own = own;
    }
}

fn add_part(parts: &mut Vec<(u64, u64)>, thread: u64, ns: u64) {
    match parts.iter_mut().find(|(t, _)| *t == thread) {
        Some((_, total)) => *total += ns,
        None => parts.push((thread, ns)),
    }
}

/// Future, CPU каждого `poll` которой начисляется владельцу.
pub struct Accounted<F: Future, C: Clock> {
    inner: Pin<Box<F>>,
    clock: C,
    account: Arc<Account>,
    last_thread: Option<u64>,
    migrations: u64,
    parts: Vec<(u64, u64)>,
}

impl<F: Future, C: Clock> Future for Accounted<F, C> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let this = self.get_mut();
        let thread = thread_index();
        if this.last_thread.is_some_and(|last| last != thread) {
            this.migrations += 1;
        }
        this.last_thread = Some(thread);
        let mut own = None;
        let result = {
            let _span = Span::enter(&this.clock, &this.account, &mut own);
            this.inner.as_mut().poll(cx)
        };
        if let Some(ns) = own {
            add_part(&mut this.parts, thread, ns);
        }
        result
    }
}

impl<F: Future, C: Clock> Drop for Accounted<F, C> {
    fn drop(&mut self) {
        self.account
            .migrations
            .fetch_add(self.migrations, Ordering::Relaxed);
        self.account.merge_parts(&self.parts);
    }
}

/// Счета владельцев и часы, которыми их ведут.
pub struct Ledger<C: Clock> {
    clock: C,
    accounts: Mutex<BTreeMap<Owner, Arc<Account>>>,
}

impl<C: Clock> Ledger<C> {
    pub fn new(clock: C) -> Ledger<C> {
        Ledger {
            clock,
            accounts: Mutex::new(BTreeMap::new()),
        }
    }

    /// Счёт владельца; обёртка берёт его один раз, на участках карта не блокируется.
    pub fn account(&self, owner: &Owner) -> Arc<Account> {
        let mut accounts = self.accounts.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(accounts.entry(owner.clone()).or_default())
    }

    pub fn wrap<F: Future>(&self, owner: &Owner, future: F) -> Accounted<F, C> {
        Accounted {
            inner: Box::pin(future),
            clock: self.clock.clone(),
            account: self.account(owner),
            last_thread: None,
            migrations: 0,
            parts: Vec::new(),
        }
    }

    /// Общая работа репозитория: начисляется группе «Git», запросившие панели только
    /// записываются, сколько бы их ни было и какая бы ни опрашивала работу у себя.
    pub fn wrap_shared_git<F: Future>(&self, requested_by: &[&str], future: F) -> Accounted<F, C> {
        let accounted = self.wrap(&Owner::Git, future);
        accounted
            .account
            .detail()
            .requested_by
            .extend(requested_by.iter().map(|panel| (*panel).to_owned()));
        accounted
    }

    /// Замыкание, CPU которого начисляется владельцу; для `spawn_blocking`.
    pub fn blocking<R>(
        &self,
        owner: &Owner,
        work: impl FnOnce() -> R + Send + 'static,
    ) -> impl FnOnce() -> R + Send + 'static {
        let clock = self.clock.clone();
        let account = self.account(owner);
        move || {
            let thread = thread_index();
            let mut own = None;
            let result = {
                let _span = Span::enter(&clock, &account, &mut own);
                work()
            };
            if let Some(ns) = own {
                account.merge_parts(&[(thread, ns)]);
            }
            result
        }
    }

    /// `tokio::task::spawn_blocking` с начислением владельцу.
    pub fn spawn_blocking<R: Send + 'static>(
        &self,
        runtime: &tokio::runtime::Handle,
        owner: &Owner,
        work: impl FnOnce() -> R + Send + 'static,
    ) -> tokio::task::JoinHandle<R> {
        runtime.spawn_blocking(self.blocking(owner, work))
    }

    pub fn charges(&self) -> Vec<OwnerCharge> {
        let accounts = self.accounts.lock().unwrap_or_else(PoisonError::into_inner);
        accounts
            .iter()
            .map(|(owner, account)| OwnerCharge {
                owner: owner.clone(),
                charge: account.charge(),
            })
            .collect()
    }

    pub fn charge(&self, owner: &Owner) -> Charge {
        self.account(owner).charge()
    }
}

#[cfg(test)]
mod tests;
