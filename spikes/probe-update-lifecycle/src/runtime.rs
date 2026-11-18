//! `runtime/<N>/` (ADR-0004, план P0.5 «D1. Рантаймы»): копии бинарей поколения, из которых
//! владелец запускает хосты. Хост сессии живёт на рантайме своего поколения, поэтому замена
//! бандла его не трогает.
//!
//! Подготовка идёт через каталог подготовки `runtime/.prep-<N>`: копия каждого бинаря из
//! `Contents/MacOS/` по манифесту, `fsync`, побайтная сверка с бандлом, проверка подписи части
//! её собственным требованием (`--deep -R` бандла требование к вложенному коду не применяет,
//! задача 4), `fsync` каталога и переименование без перезаписи (`RENAME_EXCL`). Оставшийся
//! каталог подготовки — прерванная подготовка, он удаляется и подготовка повторяется. Готовый
//! `runtime/<N>/` не перезаписывается: совпадающий с бандлом переиспользуется, несовпадающий —
//! явная ошибка. Подготовку выполняет только владелец под `shellio.lock`. Посреди подготовки
//! (первый бинарь скопирован) стоит барьер стенда: на нём сценарий `runtimes` убивает владельца.
//!
//! Новая сессия берёт хост и нагрузку только из готового рантайма поколения владельца
//! ([`for_new_session`]): прежний каталог с живыми хостами и оставшийся после отката более новый
//! она не берёт.
//!
//! Уборка рантаймов — поведение P5.2; здесь только кандидаты: готовые каталоги не своего
//! поколения, из которых не запущен ни один живой хост.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::barrier::{self, Point};
use crate::generation::{self, Part, Verify};
use crate::sys;

pub const RUNTIME_DIR: &str = "runtime";
const PREP_PREFIX: &str = ".prep-";

/// Шов проверки подписи части: в `make check` — подставной, у владельца — [`Codesign`].
pub trait PartChecker {
    fn check(&self, path: &Path, part: Part) -> Result<(), String>;
}

/// `codesign --verify --strict -R=<требование части>`.
pub struct Codesign {
    /// Основа идентификаторов: идентификатор бандла.
    pub base: String,
    /// Закреплённый SHA-1 листового сертификата.
    pub leaf: String,
}

impl PartChecker for Codesign {
    fn check(&self, path: &Path, part: Part) -> Result<(), String> {
        let requirement = generation::requirement(&part.identifier(&self.base), &self.leaf);
        let mut runs = Vec::new();
        match generation::verify(path, &requirement, &mut runs)? {
            Verify::Satisfied => Ok(()),
            other => Err(format!(
                "{}: подпись {} с требованием «{requirement}»: {:?}",
                path.display(),
                other.name(),
                runs.last().map(|run| run.stderr.trim().to_owned())
            )),
        }
    }
}

pub fn root(state: &Path) -> PathBuf {
    state.join(RUNTIME_DIR)
}

pub fn ready_dir(state: &Path, generation: u32) -> PathBuf {
    root(state).join(generation.to_string())
}

/// Каталог подготовки `runtime/.prep-<N>`.
pub fn prep_dir(state: &Path, generation: u32) -> PathBuf {
    root(state).join(format!("{PREP_PREFIX}{generation}"))
}

/// Путь бинаря части в рантайме.
pub fn binary(state: &Path, generation: u32, part: Part) -> PathBuf {
    ready_dir(state, generation).join(part.file_name())
}

/// Как получен готовый каталог.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    /// Уже был и совпал с бандлом.
    Reused,
    /// Подготовлен сейчас.
    Created,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Prepared {
    pub dir: PathBuf,
    pub outcome: Outcome,
    /// Удалённый каталог прерванной подготовки.
    pub recovered: Option<PathBuf>,
}

fn io_err(what: &Path, err: io::Error) -> String {
    format!("{}: {err}", what.display())
}

/// Совпадение файла с источником байт в байт.
fn same_bytes(copy: &Path, source: &Path) -> Result<bool, String> {
    let copy = fs::read(copy).map_err(|err| io_err(copy, err))?;
    let source = fs::read(source).map_err(|err| io_err(source, err))?;
    Ok(copy == source)
}

/// Готовый каталог содержит ровно бинари рантайма, каждый совпадает с бандлом и проходит
/// проверку подписи своей части.
fn check_ready(dir: &Path, bundle: &Path, checker: &dyn PartChecker) -> Result<(), String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .map_err(|err| io_err(dir, err))?
        .map(|entry| {
            entry
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .map_err(|err| io_err(dir, err))
        })
        .collect::<Result<_, _>>()?;
    names.sort();
    let mut expected: Vec<String> = Part::RUNTIME
        .iter()
        .map(|part| part.file_name().to_owned())
        .collect();
    expected.sort();
    if names != expected {
        return Err(format!(
            "{}: файлы {names:?}, ожидаются {expected:?}",
            dir.display()
        ));
    }
    for part in Part::RUNTIME {
        let copy = dir.join(part.file_name());
        if !same_bytes(&copy, &part.path(bundle))? {
            return Err(format!(
                "{} не совпадает с {}",
                copy.display(),
                part.path(bundle).display()
            ));
        }
        checker.check(&copy, part)?;
    }
    Ok(())
}

/// Подготовка `runtime/<generation>/` из бандла `bundle` (раздел модуля).
pub fn prepare(
    state: &Path,
    generation: u32,
    bundle: &Path,
    checker: &dyn PartChecker,
) -> Result<Prepared, String> {
    let root = root(state);
    fs::create_dir_all(&root).map_err(|err| io_err(&root, err))?;
    let prep = prep_dir(state, generation);
    let recovered = match fs::symlink_metadata(&prep) {
        Ok(_) => {
            fs::remove_dir_all(&prep).map_err(|err| io_err(&prep, err))?;
            sys::sync_dir(&root).map_err(|err| io_err(&root, err))?;
            Some(prep.clone())
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => None,
        Err(err) => return Err(io_err(&prep, err)),
    };
    let ready = ready_dir(state, generation);
    if fs::symlink_metadata(&ready).is_ok() {
        check_ready(&ready, bundle, checker).map_err(|reason| {
            format!("готовый рантайм не совпадает с бандлом, перезапись запрещена: {reason}")
        })?;
        return Ok(Prepared {
            dir: ready,
            outcome: Outcome::Reused,
            recovered,
        });
    }
    fs::create_dir(&prep).map_err(|err| io_err(&prep, err))?;
    for (index, part) in Part::RUNTIME.into_iter().enumerate() {
        if index == 1 {
            barrier::reach(state, Point::RuntimePartial);
        }
        let source = part.path(bundle);
        let copy = prep.join(part.file_name());
        fs::copy(&source, &copy)
            .map_err(|err| format!("копия {} в {}: {err}", source.display(), copy.display()))?;
        fs::File::open(&copy)
            .and_then(|file| file.sync_all())
            .map_err(|err| io_err(&copy, err))?;
        if !same_bytes(&copy, &source)? {
            return Err(format!(
                "{}: копия не совпадает с {}",
                copy.display(),
                source.display()
            ));
        }
        checker.check(&copy, part)?;
    }
    sys::sync_dir(&prep).map_err(|err| io_err(&prep, err))?;
    sys::rename_exclusive(&prep, &ready).map_err(|err| {
        format!(
            "{} → {}: {err}; готовый каталог не перезаписывается",
            prep.display(),
            ready.display()
        )
    })?;
    sys::sync_dir(&root).map_err(|err| io_err(&root, err))?;
    Ok(Prepared {
        dir: ready,
        outcome: Outcome::Created,
        recovered,
    })
}

/// Бинари новой сессии.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionBinaries {
    pub host: PathBuf,
    pub fixture: PathBuf,
}

/// Выбор рантайма новой сессии среди готовых `ready` в `root`: только каталог поколения
/// владельца `generation`. Наибольший готовый номер или прежний каталог с живыми хостами не
/// подставляются; без своего готового каталога сессия не создаётся.
pub fn choose_for_session(
    root: &Path,
    ready: &[u32],
    generation: u32,
) -> Result<SessionBinaries, String> {
    if !ready.contains(&generation) {
        return Err(format!(
            "{}: runtime/{generation} не готов (готовы {ready:?}), новая сессия не создаётся",
            root.display()
        ));
    }
    let dir = root.join(generation.to_string());
    Ok(SessionBinaries {
        host: dir.join(Part::Host.file_name()),
        fixture: dir.join(Part::Fixture.file_name()),
    })
}

/// Рантайм новой сессии по готовым каталогам на диске ([`choose_for_session`]).
pub fn for_new_session(state: &Path, generation: u32) -> Result<SessionBinaries, String> {
    choose_for_session(&root(state), &ready_generations(state)?, generation)
}

/// Готовые рантаймы по номерам поколений; каталоги подготовки и посторонние имена не входят.
pub fn ready_generations(state: &Path) -> Result<Vec<u32>, String> {
    let root = root(state);
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(io_err(&root, err)),
    };
    let mut found = Vec::new();
    for entry in entries {
        let name = entry
            .map_err(|err| io_err(&root, err))?
            .file_name()
            .to_string_lossy()
            .into_owned();
        if let Ok(generation) = name.parse::<u32>()
            && generation.to_string() == name
        {
            found.push(generation);
        }
    }
    found.sort_unstable();
    Ok(found)
}

/// Кандидаты на уборку: готовые рантаймы не поколения `current`, из которых не запущен ни один
/// живой хост (`hosts` — пути исполняемых файлов живых хостов по `proc_pidpath`).
pub fn cleanup_candidates(root: &Path, ready: &[u32], current: u32, hosts: &[PathBuf]) -> Vec<u32> {
    ready
        .iter()
        .copied()
        .filter(|generation| *generation != current)
        .filter(|generation| {
            let dir = root.join(generation.to_string());
            !hosts.iter().any(|host| host.starts_with(&dir))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Bench {
        dir: PathBuf,
        state: PathBuf,
        bundle: PathBuf,
    }

    impl Bench {
        fn new(name: &str) -> Bench {
            let dir = std::env::temp_dir().join(format!(
                "psu-runtime-{name}-{}-{}",
                std::process::id(),
                sys::uptime_ns()
            ));
            let state = dir.join("state");
            let bundle = dir.join("probe-update.app");
            fs::create_dir_all(&state).unwrap();
            fs::create_dir_all(bundle.join("Contents/MacOS")).unwrap();
            for part in Part::ALL {
                fs::write(part.path(&bundle), format!("binary {}", part.name())).unwrap();
            }
            Bench { dir, state, bundle }
        }
    }

    impl Drop for Bench {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    /// Подставная проверка: часть с содержимым `bad` не проходит; запоминает проверенное.
    #[derive(Default)]
    struct Fake {
        checked: RefCell<Vec<(String, Part)>>,
    }

    impl PartChecker for Fake {
        fn check(&self, path: &Path, part: Part) -> Result<(), String> {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            self.checked.borrow_mut().push((name, part));
            if fs::read(path).unwrap() == b"bad" {
                Err(format!("{}: подпись не проходит", path.display()))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn prepares_once_and_reuses_a_matching_ready_dir() {
        let bench = Bench::new("fresh");
        let fake = Fake::default();
        let first = prepare(&bench.state, 1, &bench.bundle, &fake).unwrap();
        assert_eq!(first.outcome, Outcome::Created);
        assert_eq!(first.recovered, None);
        assert_eq!(first.dir, ready_dir(&bench.state, 1));
        for part in Part::RUNTIME {
            assert_eq!(
                fs::read(binary(&bench.state, 1, part)).unwrap(),
                fs::read(part.path(&bench.bundle)).unwrap()
            );
        }
        let checked: Vec<Part> = fake.checked.borrow().iter().map(|(_, p)| *p).collect();
        assert_eq!(checked, Part::RUNTIME);
        assert!(!binary(&bench.state, 1, Part::App).exists());
        assert!(!prep_dir(&bench.state, 1).exists());
        let second = prepare(&bench.state, 1, &bench.bundle, &fake).unwrap();
        assert_eq!(second.outcome, Outcome::Reused);
        assert_eq!(ready_generations(&bench.state).unwrap(), [1]);
    }

    #[test]
    fn interrupted_preparation_is_recovered() {
        let bench = Bench::new("interrupted");
        let prep = prep_dir(&bench.state, 2);
        fs::create_dir_all(&prep).unwrap();
        fs::write(prep.join(Part::Host.file_name()), b"half").unwrap();
        let prepared = prepare(&bench.state, 2, &bench.bundle, &Fake::default()).unwrap();
        assert_eq!(prepared.outcome, Outcome::Created);
        assert_eq!(prepared.recovered, Some(prep.clone()));
        assert!(!prep.exists());
        assert_eq!(
            fs::read(binary(&bench.state, 2, Part::Host)).unwrap(),
            b"binary host"
        );
        assert_eq!(ready_generations(&bench.state).unwrap(), [2]);
    }

    #[test]
    fn ready_dir_that_differs_is_never_overwritten() {
        let bench = Bench::new("differs");
        prepare(&bench.state, 1, &bench.bundle, &Fake::default()).unwrap();
        let host = binary(&bench.state, 1, Part::Host);
        fs::write(&host, b"changed").unwrap();
        let err = prepare(&bench.state, 1, &bench.bundle, &Fake::default()).unwrap_err();
        assert!(err.contains("перезапись запрещена"), "{err}");
        assert_eq!(fs::read(&host).unwrap(), b"changed");
        fs::write(&host, b"binary host").unwrap();
        fs::write(ready_dir(&bench.state, 1).join("extra"), b"x").unwrap();
        let err = prepare(&bench.state, 1, &bench.bundle, &Fake::default()).unwrap_err();
        assert!(err.contains("extra"), "{err}");
        // Пустой готовый каталог тоже не заменяется.
        fs::create_dir_all(ready_dir(&bench.state, 3)).unwrap();
        assert!(prepare(&bench.state, 3, &bench.bundle, &Fake::default()).is_err());
        assert_eq!(fs::read_dir(ready_dir(&bench.state, 3)).unwrap().count(), 0);
    }

    #[test]
    fn failed_signature_leaves_no_ready_dir() {
        let bench = Bench::new("signature");
        fs::write(Part::Fixture.path(&bench.bundle), b"bad").unwrap();
        let err = prepare(&bench.state, 1, &bench.bundle, &Fake::default()).unwrap_err();
        assert!(err.contains("подпись"), "{err}");
        assert!(!ready_dir(&bench.state, 1).exists());
        // Оставшийся каталог подготовки — прерванная подготовка следующего запуска.
        assert!(prep_dir(&bench.state, 1).exists());
        fs::write(Part::Fixture.path(&bench.bundle), b"binary fixture").unwrap();
        let prepared = prepare(&bench.state, 1, &bench.bundle, &Fake::default()).unwrap();
        assert_eq!(prepared.recovered, Some(prep_dir(&bench.state, 1)));
    }

    #[test]
    fn cleanup_candidates_follow_live_hosts() {
        let root = PathBuf::from("/s/runtime");
        let host1 = root.join("1").join("probe-update-host");
        let host2 = root.join("2").join("probe-update-host");
        assert_eq!(
            cleanup_candidates(&root, &[1, 2], 2, std::slice::from_ref(&host1)),
            Vec::<u32>::new()
        );
        assert_eq!(cleanup_candidates(&root, &[1, 2], 2, &[]), [1]);
        assert_eq!(cleanup_candidates(&root, &[1, 2], 2, &[host2]), [1]);
        // Своё поколение кандидатом не бывает, «10» не начинается с «1/».
        let host10 = root.join("10").join("probe-update-host");
        assert_eq!(cleanup_candidates(&root, &[1, 10], 10, &[host10]), [1]);
        assert_eq!(cleanup_candidates(&root, &[1], 1, &[]), Vec::<u32>::new());
    }

    #[test]
    fn new_session_takes_the_owner_generation_only() {
        let root = PathBuf::from("/s/runtime");
        let own = |generation: u32| SessionBinaries {
            host: root.join(generation.to_string()).join("probe-update-host"),
            fixture: root
                .join(generation.to_string())
                .join("probe-update-fixture"),
        };
        // N+1 при живом хосте из прежнего рантайма: своя копия, не прежняя.
        assert_eq!(choose_for_session(&root, &[1, 2], 2).unwrap(), own(2));
        // После отката N+1 остаётся его готовый каталог: N не берёт наибольший номер.
        assert_eq!(choose_for_session(&root, &[1, 2, 3], 2).unwrap(), own(2));
        assert_eq!(choose_for_session(&root, &[1, 3], 1).unwrap(), own(1));
        // Своего готового каталога нет — отказ, а не прежний или более новый каталог.
        for (ready, generation) in [(&[1][..], 2), (&[][..], 1), (&[2, 3][..], 1)] {
            let err = choose_for_session(&root, ready, generation).unwrap_err();
            assert!(err.contains(&format!("runtime/{generation}")), "{err}");
        }
    }

    #[test]
    fn new_session_ignores_preparation_until_ready() {
        let bench = Bench::new("session");
        assert!(for_new_session(&bench.state, 2).is_err(), "рантаймов нет");
        fs::create_dir_all(prep_dir(&bench.state, 2)).unwrap();
        prepare(&bench.state, 1, &bench.bundle, &Fake::default()).unwrap();
        let err = for_new_session(&bench.state, 2).unwrap_err();
        assert!(
            err.contains("runtime/2"),
            "каталог подготовки не готов: {err}"
        );
        prepare(&bench.state, 2, &bench.bundle, &Fake::default()).unwrap();
        let chosen = for_new_session(&bench.state, 2).unwrap();
        assert_eq!(chosen.host, binary(&bench.state, 2, Part::Host));
        assert_eq!(chosen.fixture, binary(&bench.state, 2, Part::Fixture));
        assert!(chosen.host.is_file() && chosen.fixture.is_file());
        assert_eq!(
            for_new_session(&bench.state, 1).unwrap().host,
            binary(&bench.state, 1, Part::Host)
        );
    }

    #[test]
    fn ready_generations_skip_preparation_and_strangers() {
        let bench = Bench::new("list");
        let root = root(&bench.state);
        for name in ["1", "12", ".prep-3", "x", "01"] {
            fs::create_dir_all(root.join(name)).unwrap();
        }
        assert_eq!(ready_generations(&bench.state).unwrap(), [1, 12]);
    }
}
