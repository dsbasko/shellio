//! Заморозка создания сессий, проверка совместимости кандидата с живыми сессиями и отсрочка
//! (задача 7; план P0.5, «Несовместимость», переходы `idle → staged-deferred` и
//! `staged-deferred → staged`).
//!
//! Заморозка — флаг под тем же мьютексом, под которым роль GUI создаёт сессию ([`Gate`]):
//! взявший заморозку ждёт конца начатого создания, а после неё новое создание получает временный
//! отказ, а не ожидание. Поэтому набор сессий на диске к началу проверки окончательный, и между
//! проверкой и записью журнала непроверенная сессия не появится. Заморозка снимается на каждом
//! выходе, кроме записи `staged`: тогда она действует до выхода GUI N ([`Freeze::keep`]).
//!
//! Каждая живая сессия проверяется соединением `probe` с диапазоном attach кандидата из его
//! манифеста: хост отвечает `Welcome` или `Refused`, не меняя лидерства и размера. Решение
//! ([`decide`]) тотально: отказ живой сессии откладывает активацию, живая сессия без ответа или
//! неразобранная запись дают «совместимость не установлена» — попытка тогда не начинается
//! (повторная — не продвигается). Сессия с мёртвым хостом не проверяется: её больше нет.
//!
//! Совместимый кандидат ставится в `staged` (задача 8): под той же заморозкой помощник
//! установленного бандла копируется в `<state>/update/helper/<attempt>/` и проверяется требованием
//! своей части ([`update::stage_helper`]), и только затем пишется журнал. То же на пути
//! `staged-deferred → staged`.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

use crate::generation::{self, Expected, Manifest};
use crate::journal::{self, Journal, STAGED_PREFIX, State, Store};
use crate::owner::{self, Entry, Probe};
use crate::proto::{Hello, Purpose, Versions};
use crate::pty::Winsize;
use crate::recover::{Checker, Codesign, Seen};
use crate::{sys, update};

const DITTO: &str = "/usr/bin/ditto";

/// Причина заморозки на время проверки.
pub const CHECKING: &str = "создание сессий заморожено: идёт проверка совместимости обновления";
/// Причина заморозки после записи `staged`: до выхода GUI N.
pub const STAGED: &str = "создание сессий заморожено: обновление поставлено, GUI перезапускается";

/// Разрешение создавать сессии. Живёт под мьютексом роли GUI, которым сериализовано создание.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Gate {
    frozen: Option<String>,
}

impl Gate {
    /// Можно ли создать сессию; `Err` — причина временного отказа.
    pub fn admit(&self) -> Result<(), String> {
        match &self.frozen {
            None => Ok(()),
            Some(reason) => Err(reason.clone()),
        }
    }

    pub fn frozen(&self) -> Option<&str> {
        self.frozen.as_deref()
    }

    /// Снять оставленную заморозку: попытка, ради которой она держалась, прервана.
    pub fn thaw(&mut self) {
        self.frozen = None;
    }
}

fn lock(gate: &Mutex<Gate>) -> MutexGuard<'_, Gate> {
    gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Взятая заморозка: снимается при выходе из области, если не оставлена [`Freeze::keep`].
pub struct Freeze<'a> {
    gate: &'a Mutex<Gate>,
    kept: bool,
}

impl<'a> Freeze<'a> {
    /// Ждёт конца начатого создания сессии и замораживает. Заморозку держит одна проверка за
    /// раз: вторая — ошибка вызывающего.
    pub fn take(gate: &'a Mutex<Gate>) -> Result<Freeze<'a>, String> {
        let mut state = lock(gate);
        if let Some(reason) = &state.frozen {
            return Err(format!("уже заморожено: {reason}"));
        }
        state.frozen = Some(CHECKING.to_owned());
        Ok(Freeze { gate, kept: false })
    }

    /// Оставить заморозку до выхода процесса с причиной [`STAGED`].
    pub fn keep(mut self) {
        lock(self.gate).frozen = Some(STAGED.to_owned());
        self.kept = true;
    }
}

impl Drop for Freeze<'_> {
    fn drop(&mut self) {
        if !self.kept {
            lock(self.gate).frozen = None;
        }
    }
}

/// Решение о совместимости кандидата с живыми сессиями.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "kebab-case")]
pub enum Decision {
    /// Каждая живая сессия согласовала версию с диапазоном кандидата.
    Compatible { checked: usize },
    /// Хотя бы одна живая сессия отвергла диапазон кандидата: активация откладывается.
    Deferred { reason: String },
    /// Совместимость не установлена: живая сессия не ответила или запись не разобралась.
    Unverified { reason: String },
}

/// Решение по реестру, проверенному диапазоном кандидата.
pub fn decide(entries: &[Entry], problems: &[String]) -> Decision {
    let live: Vec<&Entry> = entries.iter().filter(|entry| entry.host_alive).collect();
    let refused: Vec<String> = live
        .iter()
        .filter_map(|entry| match &entry.probe {
            Probe::Refused { reason } => Some(format!("{}: {reason}", entry.record.session)),
            _ => None,
        })
        .collect();
    let unreachable: Vec<String> = live
        .iter()
        .filter_map(|entry| match &entry.probe {
            Probe::Unreachable { reason } => Some(format!("{}: {reason}", entry.record.session)),
            _ => None,
        })
        .collect();
    if !refused.is_empty() {
        let mut reason = format!(
            "активация отложена: живые сессии несовместимы с кандидатом — {}",
            refused.join("; ")
        );
        if !unreachable.is_empty() {
            reason.push_str(&format!("; не ответили — {}", unreachable.join("; ")));
        }
        return Decision::Deferred { reason };
    }
    let mut unknown = unreachable;
    unknown.extend(
        problems
            .iter()
            .map(|problem| format!("запись не разобрана: {problem}")),
    );
    if !unknown.is_empty() {
        return Decision::Unverified {
            reason: format!(
                "совместимость с живыми сессиями не установлена — {}",
                unknown.join("; ")
            ),
        };
    }
    Decision::Compatible {
        checked: live.len(),
    }
}

/// `Hello` проверки совместимости: назначение `probe`, диапазон attach кандидата, нулевой размер.
pub fn probe_hello(candidate: &Manifest) -> Hello {
    Hello {
        purpose: Purpose::Probe,
        versions: candidate.attach_versions,
        client: format!(
            "{} {} (кандидат {})",
            generation::APP_BIN,
            crate::GENERATION,
            candidate.generation
        ),
        from: 0,
        size: Winsize::cells(0, 0),
    }
}

/// Итог проверки живых сессий: реестр с ответами на `probe` кандидата и решение.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checked {
    pub versions: Versions,
    pub entries: Vec<Entry>,
    pub problems: Vec<String>,
    pub decision: Decision,
}

fn check_sessions(state: &Path, candidate: &Manifest) -> Checked {
    let (entries, problems) = owner::registry_with(state, &probe_hello(candidate));
    let decision = decide(&entries, &problems);
    Checked {
        versions: candidate.attach_versions,
        entries,
        problems,
        decision,
    }
}

/// Что роль GUI знает об установке.
#[derive(Clone, Copy, Debug)]
pub struct Install<'a> {
    pub state: &'a Path,
    pub installed: &'a Path,
    pub identifier: &'a str,
    pub leaf: &'a str,
}

impl Install<'_> {
    fn requirement(&self) -> String {
        generation::requirement(self.identifier, self.leaf)
    }

    fn install_dir(&self) -> Result<&Path, String> {
        self.installed
            .parent()
            .ok_or_else(|| format!("у {} нет каталога", self.installed.display()))
    }
}

/// Ответ на постановку или повторную проверку.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "staging", rename_all = "kebab-case")]
pub enum Staging {
    /// Журнал записан в `staged`; заморозка оставлена до выхода GUI N.
    Staged { attempt: String, checked: Checked },
    /// Журнал в `staged-deferred` с причиной; заморозка снята. `written` — записан ли журнал
    /// этим запросом (повторная проверка, снова отказ, ничего не пишет).
    Deferred {
        attempt: String,
        reason: String,
        written: bool,
        checked: Checked,
    },
}

/// Отказ постановки: журнал не изменён; `retry` — временный.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub reason: String,
    pub retry: bool,
}

impl Refusal {
    fn final_(reason: impl Into<String>) -> Refusal {
        Refusal {
            reason: reason.into(),
            retry: false,
        }
    }

    fn temporary(reason: impl Into<String>) -> Refusal {
        Refusal {
            reason: reason.into(),
            retry: true,
        }
    }
}

/// Каталог кандидата, удаляемый при выходе, если попытка его не оставила.
struct Candidate {
    path: PathBuf,
    kept: bool,
}

impl Drop for Candidate {
    fn drop(&mut self) {
        if !self.kept {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

/// Копия бандла `source` в `dest` через `ditto` с `POSIX_SPAWN_CLOEXEC_DEFAULT`: подпись
/// сохраняется.
fn copy_bundle(source: &Path, dest: &Path) -> Result<(), String> {
    let args: Vec<String> = ["--norsrc", "--noextattr", "--noacl"]
        .into_iter()
        .map(str::to_owned)
        .chain([source.display().to_string(), dest.display().to_string()])
        .collect();
    let run = generation::run(DITTO, &args)?;
    if run.code == Some(0) {
        Ok(())
    } else {
        Err(format!("ditto: {}", run.text().trim()))
    }
}

fn device(path: &Path) -> Result<u64, String> {
    fs::metadata(path)
        .map(|meta| meta.dev())
        .map_err(|err| format!("{}: {err}", path.display()))
}

/// CDHash установленного бандла, прошедшего проверку подписи.
fn installed_cdhash(install: &Install) -> Result<String, String> {
    let checker = Codesign {
        requirement: install.requirement(),
    };
    match checker.check(install.installed)? {
        Seen::Valid { cdhash } => Ok(cdhash),
        other => Err(format!(
            "установленный бандл {}: {other:?}",
            install.installed.display()
        )),
    }
}

/// Проверка кандидата на диске как при постановке задачи 4: требование, CDHash, манифест,
/// `CFBundleVersion` и поколение новее установленного.
fn verify_candidate(
    install: &Install,
    app: &Path,
    attempt: &str,
) -> Result<(generation::Accepted, Manifest), Refusal> {
    let certs = install
        .state
        .join(journal::UPDATE_DIR)
        .join(format!(".certs-{attempt}"));
    fs::create_dir_all(&certs)
        .map_err(|err| Refusal::final_(format!("{}: {err}", certs.display())))?;
    let mut runs = Vec::new();
    let observed = generation::observe(
        app,
        &install.requirement(),
        &certs.join("candidate-"),
        &mut runs,
    );
    let _ = fs::remove_dir_all(&certs);
    let observed = observed.map_err(Refusal::final_)?;
    let expected = Expected {
        identifier: install.identifier,
        leaf: install.leaf,
        installed: crate::GENERATION,
    };
    let accepted = generation::check_candidate(&expected, &observed).map_err(|rejection| {
        Refusal::final_(format!(
            "кандидат отвергнут проверкой ({}): {rejection:?}",
            rejection.kind()
        ))
    })?;
    let manifest = observed
        .manifest
        .map_err(|err| Refusal::final_(format!("манифест кандидата: {err}")))?;
    Ok((accepted, manifest))
}

/// Копия помощника попытки под заморозкой; при отказе каталог помощника удаляется.
fn stage_helper(install: &Install, attempt: &str) -> Result<(), Refusal> {
    update::stage_helper(
        install.state,
        install.installed,
        install.identifier,
        install.leaf,
        attempt,
    )
    .map(|_| ())
    .map_err(|reason| {
        let _ = fs::remove_dir_all(update::helper_dir(install.state, attempt));
        Refusal::final_(reason)
    })
}

/// `idle → staged-deferred` или `idle → staged`: копия кандидата `source` в
/// `.shellio-update-<attempt>/` на томе установки, его проверка и проверка живых сессий под
/// заморозкой. Совместимый кандидат ставится с копией помощника, заморозка остаётся до выхода GUI.
pub fn stage(install: &Install, gate: &Mutex<Gate>, source: &Path) -> Result<Staging, Refusal> {
    let store = Store::new(install.state);
    match store.read() {
        Ok(None) => {}
        Ok(Some(current)) if current.state == State::Idle => {}
        Ok(Some(current)) => {
            return Err(Refusal::final_(format!(
                "попытка {} в {}: новая попытка не начинается (отмена отложенного кандидата — P5.2)",
                current.attempt,
                current.state.name()
            )));
        }
        Err(err) => return Err(Refusal::final_(err.to_string())),
    }
    let install_dir = install.install_dir().map_err(Refusal::final_)?;
    let attempt = sys::random_id();
    let staged = install_dir.join(format!("{STAGED_PREFIX}{attempt}"));
    let freeze = Freeze::take(gate).map_err(Refusal::temporary)?;
    let mut candidate = Candidate {
        path: staged.clone(),
        kept: false,
    };
    copy_bundle(source, &staged).map_err(Refusal::final_)?;
    let (staged_dev, install_dev) = (
        device(&staged).map_err(Refusal::final_)?,
        device(install_dir).map_err(Refusal::final_)?,
    );
    if staged_dev != install_dev {
        return Err(Refusal::final_(format!(
            "кандидат {} не на томе установки",
            staged.display()
        )));
    }
    let (accepted, manifest) = verify_candidate(install, &staged, &attempt)?;
    let from_cdhash = installed_cdhash(install).map_err(Refusal::final_)?;
    let checked = check_sessions(install.state, &manifest);
    match checked.decision.clone() {
        Decision::Deferred { reason } => {
            let mut next = Journal::begin(
                &attempt,
                State::StagedDeferred,
                (crate::GENERATION, &from_cdhash),
                (accepted.generation, &accepted.cdhash),
                install.installed,
            );
            next.reason = Some(reason.clone());
            store
                .begin(&next)
                .map_err(|err| Refusal::final_(err.to_string()))?;
            candidate.kept = true;
            drop(freeze);
            Ok(Staging::Deferred {
                attempt,
                reason,
                written: true,
                checked,
            })
        }
        Decision::Unverified { reason } => Err(Refusal::temporary(reason)),
        Decision::Compatible { .. } => {
            stage_helper(install, &attempt)?;
            let next = Journal::begin(
                &attempt,
                State::Staged,
                (crate::GENERATION, &from_cdhash),
                (accepted.generation, &accepted.cdhash),
                install.installed,
            );
            if let Err(err) = store.begin(&next) {
                let _ = fs::remove_dir_all(update::helper_dir(install.state, &attempt));
                return Err(Refusal::final_(err.to_string()));
            }
            candidate.kept = true;
            freeze.keep();
            Ok(Staging::Staged { attempt, checked })
        }
    }
}

/// `staged-deferred → staged`: под заморозкой повторная проверка подписи кандидата этой попытки
/// по журналу (CDHash и поколение «в») и совместимости. Снова несовместим — журнал не меняется.
pub fn restage(install: &Install, gate: &Mutex<Gate>) -> Result<Staging, Refusal> {
    let store = Store::new(install.state);
    let journal = match store.read() {
        Ok(Some(journal)) if journal.state == State::StagedDeferred => journal,
        Ok(Some(journal)) => {
            return Err(Refusal::final_(format!(
                "попытка {} в {}: отложенной попытки нет",
                journal.attempt,
                journal.state.name()
            )));
        }
        Ok(None) => return Err(Refusal::final_("журнала нет: отложенной попытки нет")),
        Err(err) => return Err(Refusal::final_(err.to_string())),
    };
    let freeze = Freeze::take(gate).map_err(Refusal::temporary)?;
    let staged = journal.staged_dir();
    let checker = Codesign {
        requirement: install.requirement(),
    };
    match checker.check(&staged).map_err(Refusal::final_)? {
        Seen::Valid { cdhash } if cdhash == journal.cdhash_to => {}
        other => {
            return Err(Refusal::final_(format!(
                "кандидат попытки {} ({}) не проходит повторную проверку: {other:?}, ждали CDHash {}",
                journal.attempt,
                staged.display(),
                journal.cdhash_to
            )));
        }
    }
    let manifest = Manifest::read(&staged).map_err(Refusal::final_)?;
    if manifest.generation != journal.to {
        return Err(Refusal::final_(format!(
            "манифест кандидата — поколение {}, журнал ждёт {}",
            manifest.generation, journal.to
        )));
    }
    let checked = check_sessions(install.state, &manifest);
    match checked.decision.clone() {
        Decision::Compatible { .. } => {
            stage_helper(install, &journal.attempt)?;
            store
                .advance(&journal.attempt, &[State::StagedDeferred], |next| {
                    next.state = State::Staged;
                    next.reason = None;
                })
                .map_err(|err| Refusal::final_(err.to_string()))?;
            freeze.keep();
            Ok(Staging::Staged {
                attempt: journal.attempt,
                checked,
            })
        }
        Decision::Deferred { reason } => Ok(Staging::Deferred {
            attempt: journal.attempt,
            reason,
            written: false,
            checked,
        }),
        Decision::Unverified { reason } => Err(Refusal::temporary(reason)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::SessionRecord;
    use crate::observer::ProcessKey;
    use crate::proto::ATTACH_VERSIONS;
    use std::sync::Arc;
    use std::thread;

    fn entry(session: &str, host_alive: bool, probe: Probe) -> Entry {
        Entry {
            record: SessionRecord {
                session: session.to_owned(),
                host: ProcessKey {
                    pid: 10,
                    start_us: 1,
                },
                executable: "/x/runtime/1/probe-update-host".to_owned(),
                workload: ProcessKey {
                    pid: 11,
                    start_us: 1,
                },
                workload_pgid: 11,
                workload_sid: Some(11),
                tty: "/dev/ttys001".to_owned(),
                versions: ATTACH_VERSIONS,
                generation: 1,
                socket: format!("/x/s/{session}.sock"),
            },
            host_alive,
            workload_alive: host_alive,
            probe,
        }
    }

    fn welcome() -> Probe {
        Probe::Welcome { version: 1 }
    }

    fn refused() -> Probe {
        Probe::Refused {
            reason: "несовместимые версии протокола: клиент 2..2, хост 1..1".to_owned(),
        }
    }

    fn unreachable() -> Probe {
        Probe::Unreachable {
            reason: "connect: Connection refused".to_owned(),
        }
    }

    #[test]
    fn refusal_of_a_live_session_defers() {
        let decision = decide(
            &[entry("h1", true, refused()), entry("h2", true, welcome())],
            &[],
        );
        match decision {
            Decision::Deferred { reason } => {
                assert!(reason.contains("h1") && reason.contains("2..2"), "{reason}");
                assert!(!reason.contains("h2"), "{reason}");
            }
            other => panic!("{other:?}"),
        }
        // Отказ важнее неответившей: несовместимость уже установлена, причина называет обе.
        match decide(
            &[
                entry("h1", true, refused()),
                entry("h2", true, unreachable()),
            ],
            &["bad: {".to_owned()],
        ) {
            Decision::Deferred { reason } => assert!(reason.contains("h2"), "{reason}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unanswered_live_session_or_unparsed_record_is_never_compatible() {
        assert!(matches!(
            decide(&[entry("h1", true, unreachable())], &[]),
            Decision::Unverified { .. }
        ));
        match decide(
            &[entry("h1", true, welcome())],
            &["запись h2: EOF".to_owned()],
        ) {
            Decision::Unverified { reason } => assert!(reason.contains("h2"), "{reason}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn dead_sessions_are_skipped_and_no_sessions_is_compatible() {
        assert_eq!(decide(&[], &[]), Decision::Compatible { checked: 0 });
        assert_eq!(
            decide(
                &[
                    entry("gone", false, refused()),
                    entry("lost", false, unreachable()),
                    entry("h1", true, welcome()),
                ],
                &[],
            ),
            Decision::Compatible { checked: 1 }
        );
    }

    #[test]
    fn freeze_refuses_creation_until_released_unless_kept() {
        let gate = Mutex::new(Gate::default());
        assert_eq!(lock(&gate).admit(), Ok(()));
        {
            let freeze = Freeze::take(&gate).unwrap();
            assert_eq!(lock(&gate).admit(), Err(CHECKING.to_owned()));
            assert!(
                Freeze::take(&gate).is_err(),
                "вторая проверка не замораживает"
            );
            drop(freeze);
        }
        assert_eq!(lock(&gate).admit(), Ok(()), "снята при выходе");
        Freeze::take(&gate).unwrap().keep();
        assert_eq!(
            lock(&gate).admit(),
            Err(STAGED.to_owned()),
            "оставлена до выхода"
        );
        assert_eq!(lock(&gate).frozen(), Some(STAGED));
    }

    #[test]
    fn freeze_waits_for_a_creation_in_flight() {
        let gate = Arc::new(Mutex::new(Gate::default()));
        let creating = lock(&gate);
        let (sender, receiver) = std::sync::mpsc::channel();
        let freezer = {
            let gate = Arc::clone(&gate);
            thread::spawn(move || {
                let freeze = Freeze::take(&gate).unwrap();
                sender.send(()).unwrap();
                freeze.keep();
            })
        };
        assert!(
            receiver.try_recv().is_err(),
            "заморозка не взята, пока идёт создание"
        );
        drop(creating);
        receiver.recv().unwrap();
        freezer.join().unwrap();
        assert!(lock(&gate).admit().is_err());
    }

    #[test]
    fn probe_hello_carries_the_candidate_range_without_size() {
        let candidate = Manifest {
            generation: 3,
            host_versions: Versions::new(2, 2),
            attach_versions: Versions::new(2, 2),
            runtime: Vec::new(),
        };
        let hello = probe_hello(&candidate);
        assert_eq!(hello.purpose, Purpose::Probe);
        assert_eq!(hello.versions, Versions::new(2, 2));
        assert_eq!((hello.size.rows, hello.size.cols), (0, 0));
        assert!(hello.client.contains("кандидат 3"), "{}", hello.client);
    }
}
