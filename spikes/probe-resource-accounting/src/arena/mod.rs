//! Учёт арен (ADR-0011, «Оценка ≈»; план P0.8, «Solution Overview»).
//!
//! Счёт владельца ведёт объявленные байты его арен и кэшей: занятые байты — показываемое
//! значение, ёмкость — диагностика рядом с ним. Ни то ни другое не складывается с footprint
//! процесса и не вычитается из него. Каждая структура держит свою долю счёта через [`Lease`]:
//! рост ёмкости, заполнение, сброс с переиспользованием ёмкости и освобождение меняют счёт на
//! ровно ту долю, которую структура в него внесла, а `Drop` возвращает остаток. Переполнение
//! ёмкости и освобождение сверх свободной ёмкости — ошибка, а не насыщение.
//!
//! [`Arena`] — арена из кусков объявленного размера. Ёмкость куска известна заранее: размер
//! куска арены, а для выделения больше куска — ровно его размер. Поэтому оба числа счёта
//! совпадают с известными размерами точно, без зависимости от политики роста `Vec`.
//!
//! Общий кэш репозитория принадлежит группе «Git» ([`Arenas::shared_git_lease`]): запросившие
//! панели записываются, но байты им не начисляются.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde::Serialize;

use crate::attrib::Owner;

/// Счёт владельца. Занятые байты и ёмкость меняются под одной блокировкой, поэтому чтение
/// никогда не видит занятых больше ёмкости.
#[derive(Debug, Default)]
struct Account {
    totals: Mutex<Usage>,
}

impl Account {
    fn totals(&self) -> MutexGuard<'_, Usage> {
        self.totals.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Учтённые байты владельца на момент чтения.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Usage {
    /// Занятые байты: показываемое значение оценки.
    pub used_bytes: u64,
    /// Ёмкость: диагностика, не показывается вместо занятых.
    pub capacity_bytes: u64,
    /// Живые структуры владельца.
    pub leases: u64,
    /// Панели, запросившие общий кэш; байты им не начисляются.
    pub requested_by: BTreeSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OwnerUsage {
    pub owner: Owner,
    pub usage: Usage,
}

/// Операция, которая нарушила бы `занятые ≤ ёмкость` доли структуры.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArenaError {
    /// Заполнение сверх ёмкости.
    Overfill {
        used: u64,
        capacity: u64,
        bytes: u64,
    },
    /// Освобождение больше свободной ёмкости.
    OverRelease {
        used: u64,
        capacity: u64,
        bytes: u64,
    },
}

impl fmt::Display for ArenaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ArenaError::Overfill {
                used,
                capacity,
                bytes,
            } => write!(
                f,
                "заполнение {bytes} Б сверх ёмкости: занято {used} из {capacity} Б"
            ),
            ArenaError::OverRelease {
                used,
                capacity,
                bytes,
            } => write!(
                f,
                "освобождение {bytes} Б больше свободной ёмкости: занято {used} из {capacity} Б"
            ),
        }
    }
}

/// Доля одной структуры в счёте владельца.
#[derive(Debug)]
pub struct Lease {
    account: Arc<Account>,
    used: u64,
    capacity: u64,
}

impl Lease {
    fn new(account: Arc<Account>) -> Lease {
        account.totals().leases += 1;
        Lease {
            account,
            used: 0,
            capacity: 0,
        }
    }

    /// Рост ёмкости.
    fn grow(&mut self, bytes: u64) {
        self.capacity += bytes;
        self.account.totals().capacity_bytes += bytes;
    }

    /// Заполнение внутри ёмкости.
    fn fill(&mut self, bytes: u64) -> Result<(), ArenaError> {
        if bytes > self.capacity - self.used {
            return Err(ArenaError::Overfill {
                used: self.used,
                capacity: self.capacity,
                bytes,
            });
        }
        self.used += bytes;
        self.account.totals().used_bytes += bytes;
        Ok(())
    }

    /// Сброс занятых: ёмкость остаётся для переиспользования.
    fn reset(&mut self) {
        self.account.totals().used_bytes -= self.used;
        self.used = 0;
    }

    /// Освобождение свободной ёмкости.
    fn release(&mut self, bytes: u64) -> Result<(), ArenaError> {
        if bytes > self.capacity - self.used {
            return Err(ArenaError::OverRelease {
                used: self.used,
                capacity: self.capacity,
                bytes,
            });
        }
        self.capacity -= bytes;
        self.account.totals().capacity_bytes -= bytes;
        Ok(())
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut totals = self.account.totals();
        totals.used_bytes -= self.used;
        totals.capacity_bytes -= self.capacity;
        totals.leases -= 1;
    }
}

/// Счета владельцев.
#[derive(Debug, Default)]
pub struct Arenas {
    accounts: Mutex<BTreeMap<Owner, Arc<Account>>>,
}

impl Arenas {
    pub fn new() -> Arenas {
        Arenas::default()
    }

    fn accounts(&self) -> MutexGuard<'_, BTreeMap<Owner, Arc<Account>>> {
        self.accounts.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn account(&self, owner: &Owner) -> Arc<Account> {
        Arc::clone(self.accounts().entry(owner.clone()).or_default())
    }

    /// Доля новой структуры владельца.
    pub fn lease(&self, owner: &Owner) -> Lease {
        Lease::new(self.account(owner))
    }

    /// Доля общего кэша репозитория: счёт группы «Git», запросившие панели только
    /// записываются, сколько бы их ни было и какая бы ни запросила первой.
    pub fn shared_git_lease(&self, requested_by: &[&str]) -> Lease {
        let account = self.account(&Owner::Git);
        account
            .totals()
            .requested_by
            .extend(requested_by.iter().map(|panel| (*panel).to_owned()));
        Lease::new(account)
    }

    pub fn usage(&self) -> Vec<OwnerUsage> {
        self.accounts()
            .iter()
            .map(|(owner, account)| OwnerUsage {
                owner: owner.clone(),
                usage: account.totals().clone(),
            })
            .collect()
    }

    /// Учтённое владельцу; владелец без структур — нули.
    pub fn usage_of(&self, owner: &Owner) -> Usage {
        self.accounts()
            .get(owner)
            .map(|account| account.totals().clone())
            .unwrap_or_default()
    }
}

/// Кусок арены: объявленная ёмкость и заполненная часть.
#[derive(Debug)]
struct Chunk {
    /// Номер куска в арене, не меняется при освобождении соседних.
    id: u64,
    capacity: usize,
    data: Vec<u8>,
}

/// Место выделения в арене. Действует до сброса арены.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slot {
    generation: u64,
    chunk: u64,
    offset: usize,
    len: usize,
}

/// Арена из кусков с учётом в доле владельца.
///
/// Выделение занимает первый кусок со свободным местом, без места — заводит кусок размера
/// `chunk_bytes` или ровно размера выделения, если оно больше. Сброс освобождает занятое и
/// оставляет куски для переиспользования, освобождение отдаёт пустые куски, `Drop` — всё.
#[derive(Debug)]
pub struct Arena {
    lease: Lease,
    chunk_bytes: usize,
    chunks: Vec<Chunk>,
    next_chunk: u64,
    generation: u64,
}

impl Arena {
    pub fn new(lease: Lease, chunk_bytes: usize) -> Arena {
        Arena {
            lease,
            chunk_bytes,
            chunks: Vec::new(),
            next_chunk: 0,
            generation: 0,
        }
    }

    /// Копия `bytes` в арене. Пустое выделение места не занимает и кусок не заводит.
    pub fn alloc(&mut self, bytes: &[u8]) -> Result<Slot, ArenaError> {
        let len = bytes.len();
        if len == 0 {
            return Ok(Slot {
                generation: self.generation,
                chunk: 0,
                offset: 0,
                len: 0,
            });
        }
        let fits = self
            .chunks
            .iter()
            .position(|chunk| chunk.capacity - chunk.data.len() >= len);
        let index = match fits {
            Some(index) => index,
            None => {
                let capacity = self.chunk_bytes.max(len);
                self.lease.grow(capacity as u64);
                self.chunks.push(Chunk {
                    id: self.next_chunk,
                    capacity,
                    data: Vec::with_capacity(capacity),
                });
                self.next_chunk += 1;
                self.chunks.len() - 1
            }
        };
        self.lease.fill(len as u64)?;
        let chunk = &mut self.chunks[index];
        let offset = chunk.data.len();
        chunk.data.extend_from_slice(bytes);
        Ok(Slot {
            generation: self.generation,
            chunk: chunk.id,
            offset,
            len,
        })
    }

    /// Байты места; `None` — место от прошлого поколения арены.
    pub fn get(&self, slot: Slot) -> Option<&[u8]> {
        if slot.generation != self.generation {
            return None;
        }
        if slot.len == 0 {
            return Some(&[]);
        }
        let index = self
            .chunks
            .binary_search_by_key(&slot.chunk, |chunk| chunk.id)
            .ok()?;
        self.chunks[index]
            .data
            .get(slot.offset..slot.offset + slot.len)
    }

    /// Сброс: занятое освобождается, куски остаются, места прошлого поколения недействительны.
    pub fn reset(&mut self) {
        for chunk in &mut self.chunks {
            chunk.data.clear();
        }
        self.lease.reset();
        self.generation += 1;
    }

    /// Освобождение пустых кусков; возвращает освобождённую ёмкость. Места в непустых кусках
    /// остаются действительными.
    pub fn trim(&mut self) -> Result<u64, ArenaError> {
        let freed: usize = self
            .chunks
            .iter()
            .filter(|chunk| chunk.data.is_empty())
            .map(|chunk| chunk.capacity)
            .sum();
        self.lease.release(freed as u64)?;
        self.chunks.retain(|chunk| !chunk.data.is_empty());
        Ok(freed as u64)
    }
}

#[cfg(test)]
mod tests;
