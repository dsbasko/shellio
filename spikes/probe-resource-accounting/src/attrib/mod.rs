//! Атрибуция процессов панелям и служебной группе «Git» (ADR-0011, план P0.8, «Solution Overview»).
//!
//! Источники: запись владения от хоста (хост и программы сессии, лидеры группы и сессии
//! нагрузки), дерево потомков по родителю от корней (процессы поверхности вида, хост, программы,
//! дочерние `git` общей работы) и члены сессии и группы записи (`getsid`, `pgid` списка). Корень
//! и ключ записи совпадают с записью списка целиком, `(pid, p_start)`: PID, который держит другой
//! процесс, не находится. Итог владельца — объединение находок по ключу, у каждого члена —
//! источники находки. Работает по одному списку прохода и отдаёт множество ключей для
//! [`crate::snapshot::Snapshot::take`] и [`crate::snapshot::summarize`].
//!
//! Процесс, найденный группе «Git», панелям не начисляется, даже если его нашла запросившая
//! панель. Панели одной сессии делят её процессы, уникальное множество ([`Attribution::all`])
//! содержит каждый процесс один раз. Ненайденный корень, PID в чужих руках и отказ `getsid`
//! возвращаются заметками, а не пропадают.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::proc::{Listed, ReadError, StartTime};
use crate::snapshot::Key;

/// Владелец итога.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum Owner {
    /// Служебная группа «Git»: общая работа репозитория.
    Git,
    Panel(String),
}

/// Источник находки.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// Ключ записи владения: хост или программа.
    Record,
    /// Корень, переданный видом (процесс поверхности) или исполнителем общей работы.
    Root,
    /// Потомок корня или ключа записи по родителю.
    Tree,
    /// `getsid` равен PID лидера сессии записи.
    Session,
    /// `pgid` списка равен PID лидера группы записи.
    Group,
}

/// Запись владения от хоста (ADR-0004, ADR-0005): хост, программы, лидеры группы и сессии
/// нагрузки. Все ключи — `(pid, p_start)`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerRecord {
    pub host: Key,
    pub programs: Vec<Key>,
    /// `pgid` членов группы равен его PID.
    pub group: Key,
    /// `getsid` членов сессии равен его PID.
    pub session: Key,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Panel {
    pub id: String,
    /// Процессы поверхности вида (`login`, attach-клиент); у закрытого вида — пусто.
    pub surface: Vec<Key>,
    /// Запись владения сессии панели.
    pub record: Option<OwnerRecord>,
}

/// Общая работа репозитория: дочерний `git` и запросившие её панели. Запрос только
/// записывается, итог панели он не меняет.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitWork {
    pub child: Key,
    pub requested_by: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims {
    pub panels: Vec<Panel>,
    pub git: Vec<GitWork>,
}

/// Роль ключа, по которому искали.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Surface,
    Host,
    Program,
    Git,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Leader {
    Group,
    Session,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Note {
    /// Ключа нет в списке: процесс вышел.
    Exited { owner: Owner, role: Role, key: Key },
    /// PID ключа держит другой процесс.
    Reused {
        owner: Owner,
        role: Role,
        key: Key,
        listed: StartTime,
    },
    /// PID лидера группы или сессии записи держит другой процесс: номер может принадлежать
    /// чужой группе или сессии, совпадения по нему не учитываются.
    LeaderReused {
        owner: Owner,
        leader: Leader,
        key: Key,
        listed: StartTime,
    },
    /// `getsid` отказал: принадлежность по сессии не проверена.
    SessionUnknown { key: Key, error: ReadError },
    /// Процесс, найденный панели, начислен группе «Git».
    Ceded { key: Key, from: Owner },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub key: Key,
    pub comm: String,
    pub origins: BTreeSet<Origin>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Share {
    pub owner: Owner,
    /// Запросившие работу панели; только у группы «Git».
    pub requested_by: BTreeSet<String>,
    pub members: Vec<Member>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attribution {
    /// Группа «Git» и каждая панель, по порядку владельцев.
    pub shares: Vec<Share>,
    pub notes: Vec<Note>,
}

impl Attribution {
    pub fn share(&self, owner: &Owner) -> Option<&Share> {
        self.shares.iter().find(|share| &share.owner == owner)
    }

    pub fn members(&self, owner: &Owner) -> BTreeSet<Key> {
        self.share(owner)
            .map(|share| share.members.iter().map(|member| member.key).collect())
            .unwrap_or_default()
    }

    pub fn member(&self, owner: &Owner, key: Key) -> Option<&Member> {
        self.share(owner)?
            .members
            .iter()
            .find(|member| member.key == key)
    }

    /// Уникальное множество всех владельцев: каждый процесс один раз.
    pub fn all(&self) -> BTreeSet<Key> {
        self.shares
            .iter()
            .flat_map(|share| share.members.iter().map(|member| member.key))
            .collect()
    }
}

type Found = BTreeMap<Key, BTreeMap<Owner, BTreeSet<Origin>>>;

fn add(found: &mut Found, key: Key, owner: &Owner, origin: Origin) {
    found
        .entry(key)
        .or_default()
        .entry(owner.clone())
        .or_default()
        .insert(origin);
}

/// Состояние ключа в списке: на месте, вышел или PID у другого процесса.
fn locate<'a>(
    by_pid: &BTreeMap<i32, &'a Listed>,
    key: Key,
) -> Result<&'a Listed, Option<StartTime>> {
    match by_pid.get(&key.pid) {
        None => Err(None),
        Some(listed) if listed.start != key.start => Err(Some(listed.start)),
        Some(listed) => Ok(listed),
    }
}

fn open(shares: &mut BTreeMap<Owner, Share>, owner: Owner) -> &mut Share {
    shares.entry(owner.clone()).or_insert_with(|| Share {
        owner,
        requested_by: BTreeSet::new(),
        members: Vec::new(),
    })
}

/// Атрибуция по одному списку прохода. `sid_of` — `getsid`; вызывается не больше раза на PID и
/// только при записи владения с действующей сессией, PID 0 не передаётся: `getsid(0)` отвечает
/// за вызывающий процесс.
pub fn attribute(
    listing: &[Listed],
    claims: &Claims,
    mut sid_of: impl FnMut(i32) -> Result<i32, ReadError>,
) -> Attribution {
    let by_pid: BTreeMap<i32, &Listed> = listing.iter().map(|l| (l.pid, l)).collect();
    let mut children: BTreeMap<i32, Vec<&Listed>> = BTreeMap::new();
    for listed in listing.iter().filter(|l| l.pid != l.ppid) {
        children.entry(listed.ppid).or_default().push(listed);
    }

    let mut roots: Vec<(Owner, Role, Key)> = Vec::new();
    for work in &claims.git {
        roots.push((Owner::Git, Role::Git, work.child));
    }
    for panel in &claims.panels {
        let owner = Owner::Panel(panel.id.clone());
        roots.extend(
            panel
                .surface
                .iter()
                .map(|key| (owner.clone(), Role::Surface, *key)),
        );
        if let Some(record) = &panel.record {
            roots.push((owner.clone(), Role::Host, record.host));
            roots.extend(
                record
                    .programs
                    .iter()
                    .map(|key| (owner.clone(), Role::Program, *key)),
            );
        }
    }

    let mut found = Found::new();
    let mut notes = Vec::new();
    for (owner, role, key) in roots {
        let root = match locate(&by_pid, key) {
            Ok(root) => root,
            Err(None) => {
                notes.push(Note::Exited { owner, role, key });
                continue;
            }
            Err(Some(listed)) => {
                notes.push(Note::Reused {
                    owner,
                    role,
                    key,
                    listed,
                });
                continue;
            }
        };
        let origin = match role {
            Role::Host | Role::Program => Origin::Record,
            Role::Surface | Role::Git => Origin::Root,
        };
        add(&mut found, key, &owner, origin);
        let mut visited = BTreeSet::from([root.pid]);
        let mut stack = vec![root.pid];
        while let Some(pid) = stack.pop() {
            for child in children.get(&pid).into_iter().flatten() {
                if visited.insert(child.pid) {
                    add(&mut found, Key::of(child), &owner, Origin::Tree);
                    stack.push(child.pid);
                }
            }
        }
    }

    let mut sessions: Vec<(Owner, i32)> = Vec::new();
    for panel in &claims.panels {
        let Some(record) = &panel.record else {
            continue;
        };
        let owner = Owner::Panel(panel.id.clone());
        for (leader, key) in [
            (Leader::Group, record.group),
            (Leader::Session, record.session),
        ] {
            if let Err(Some(listed)) = locate(&by_pid, key) {
                notes.push(Note::LeaderReused {
                    owner: owner.clone(),
                    leader,
                    key,
                    listed,
                });
                continue;
            }
            match leader {
                Leader::Group => {
                    for member in listing.iter().filter(|l| l.pgid == key.pid) {
                        add(&mut found, Key::of(member), &owner, Origin::Group);
                    }
                }
                Leader::Session => sessions.push((owner.clone(), key.pid)),
            }
        }
    }
    if !sessions.is_empty() {
        for listed in listing.iter().filter(|l| l.pid != 0) {
            match sid_of(listed.pid) {
                Ok(sid) => {
                    for (owner, _) in sessions.iter().filter(|(_, leader)| *leader == sid) {
                        add(&mut found, Key::of(listed), owner, Origin::Session);
                    }
                }
                Err(error) => notes.push(Note::SessionUnknown {
                    key: Key::of(listed),
                    error,
                }),
            }
        }
    }

    let mut shares: BTreeMap<Owner, Share> = BTreeMap::new();
    open(&mut shares, Owner::Git).requested_by = claims
        .git
        .iter()
        .flat_map(|work| work.requested_by.iter().cloned())
        .collect();
    for panel in &claims.panels {
        open(&mut shares, Owner::Panel(panel.id.clone()));
    }
    for (key, mut owners) in found {
        if let Some(origins) = owners.remove(&Owner::Git) {
            notes.extend(owners.into_keys().map(|from| Note::Ceded { key, from }));
            owners = BTreeMap::from([(Owner::Git, origins)]);
        }
        let comm = by_pid
            .get(&key.pid)
            .map(|listed| listed.comm.clone())
            .unwrap_or_default();
        for (owner, origins) in owners {
            open(&mut shares, owner).members.push(Member {
                key,
                comm: comm.clone(),
                origins,
            });
        }
    }

    Attribution {
        shares: shares.into_values().collect(),
        notes,
    }
}

#[cfg(test)]
mod tests;
