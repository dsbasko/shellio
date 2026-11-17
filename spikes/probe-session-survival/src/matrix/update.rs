//! Чистая часть сценария `update` (план P0.4, задача 16, клетка G3): поколения подписанного
//! бандла пробы, раскладка `runtime/<поколение>/` и выбор бинарей хоста и клиента, подготовка
//! рантайма и обмен бандлов. Водитель с GUI — `surface::update` под функцией `surface`.
//!
//! Хост и attach-клиент запускаются не из бандла, а из `runtime/<N>/`, куда их копирует поколение
//! N до запуска хоста: замена бандла не трогает файлы живого хоста. Рантайм готовится в
//! `.staging-<N>-<pid>`, сверяется побайтно с бандлом и подписью и только потом получает имя
//! поколения; готовый каталог не перезаписывается.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::host::HOST_NAME;
use crate::host::attach::ATTACH_NAME;
use crate::matrix::Candidate;
use crate::sys;

pub const CELL: &str = "G3";
/// Манифест поколений в каталоге процесса; его пишет `run.sh`.
pub const MANIFEST_FILE: &str = "generations.json";
/// Сырой артефакт клетки.
pub const ARTIFACT: &str = "update-g3.json";
/// Сцена `update` в `scenes/bridge.json`.
pub const SCENE: &str = "update";
pub const BUNDLE_ID: &str = "io.github.dsbasko.shellio.probe.session-survival";
pub const SURFACE_NAME: &str = "probe-session-surface";
pub const ZMX_NAME: &str = "zmx";
const STAGING_PREFIX: &str = ".staging-";

/// Вспомогательный бинарь бандла, который поколение копирует в свой рантайм.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Helper {
    Host,
    Attach,
    Zmx,
}

impl Helper {
    /// Рантайм поколения общий для сессий обоих кандидатов.
    pub const ALL: [Helper; 3] = [Helper::Host, Helper::Attach, Helper::Zmx];

    pub fn file_name(self) -> &'static str {
        match self {
            Helper::Host => HOST_NAME,
            Helper::Attach => ATTACH_NAME,
            Helper::Zmx => ZMX_NAME,
        }
    }

    /// Идентификатор подписи бинаря внутри бандла.
    pub fn identifier(self) -> String {
        let suffix = match self {
            Helper::Host => "host",
            Helper::Attach => "attach",
            Helper::Zmx => "zmx",
        };
        format!("{BUNDLE_ID}.{suffix}")
    }

    /// Бинарь, который запускает сессию кандидата и остаётся её хостом.
    pub fn host_of(candidate: Candidate) -> Helper {
        match candidate {
            Candidate::Zmx => Helper::Zmx,
            Candidate::Rust => Helper::Host,
        }
    }

    /// Бинарь, которым поколение открывает сессию кандидата.
    pub fn client_of(candidate: Candidate) -> Helper {
        match candidate {
            Candidate::Zmx => Helper::Zmx,
            Candidate::Rust => Helper::Attach,
        }
    }
}

/// Поколение бандла и его путь.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationBundle {
    pub generation: u32,
    pub app: PathBuf,
}

/// `generations.json`: установленный бандл, который сценарий заменяет, корень рантаймов и
/// подписанные поколения.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub installed: PathBuf,
    pub runtime: PathBuf,
    pub generations: Vec<GenerationBundle>,
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Manifest, String> {
        let manifest: Manifest =
            serde_json::from_str(text).map_err(|err| format!("{MANIFEST_FILE}: {err}"))?;
        let mut numbers: Vec<u32> = manifest
            .generations
            .iter()
            .map(|bundle| bundle.generation)
            .collect();
        numbers.sort_unstable();
        numbers.dedup();
        if numbers.len() != manifest.generations.len() || numbers.contains(&0) {
            return Err(format!(
                "{MANIFEST_FILE}: поколения должны быть различными и больше нуля"
            ));
        }
        if manifest
            .generations
            .iter()
            .any(|bundle| bundle.app == manifest.installed)
        {
            return Err(format!(
                "{MANIFEST_FILE}: установленный бандл совпадает с бандлом поколения"
            ));
        }
        Ok(manifest)
    }

    pub fn bundle(&self, generation: u32) -> Result<&Path, String> {
        self.generations
            .iter()
            .find(|bundle| bundle.generation == generation)
            .map(|bundle| bundle.app.as_path())
            .ok_or_else(|| format!("{MANIFEST_FILE}: нет поколения {generation}"))
    }
}

/// Бинарь внутри бандла.
pub fn bundled(app: &Path, name: &str) -> PathBuf {
    app.join("Contents").join("MacOS").join(name)
}

/// Бандл, которому принадлежит исполняемый файл `exe`, если он лежит в `Contents/MacOS`.
pub fn bundle_of(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let app = contents.parent()?;
    let ok = macos.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && app.extension()? == "app";
    ok.then(|| app.to_path_buf())
}

pub fn info_plist(app: &Path) -> PathBuf {
    app.join("Contents").join("Info.plist")
}

/// Поколение бандла — `CFBundleVersion` по `plutil -extract … raw`: целое больше нуля.
pub fn parse_generation(raw: &str) -> Option<u32> {
    raw.trim()
        .parse::<u32>()
        .ok()
        .filter(|generation| *generation > 0)
}

/// CDHash из `codesign -d --verbose=2`.
pub fn parse_cdhash(details: &str) -> Option<String> {
    details
        .lines()
        .find_map(|line| line.strip_prefix("CDHash="))
        .map(str::trim)
        .filter(|hash| !hash.is_empty() && hash.chars().all(|c| c.is_ascii_hexdigit()))
        .map(str::to_owned)
}

/// SHA-1 листового сертификата из designated requirement (`codesign -d -r-`).
pub fn parse_leaf(requirement: &str) -> Option<String> {
    let rest = requirement.split("certificate leaf = H\"").nth(1)?;
    let leaf = rest.split('"').next()?;
    (leaf.len() == 40 && leaf.chars().all(|c| c.is_ascii_hexdigit())).then(|| leaf.to_owned())
}

/// Требование подписи: идентификатор и листовой сертификат.
pub fn requirement(identifier: &str, leaf: &str) -> String {
    format!("identifier \"{identifier}\" and certificate leaf = H\"{leaf}\"")
}

/// Поколения с готовым рантаймом по именам каталогов: только числа больше нуля, каталоги
/// подготовки и чужие имена не считаются.
pub fn generations<'a>(names: impl IntoIterator<Item = &'a str>) -> Vec<u32> {
    let mut found: Vec<u32> = names
        .into_iter()
        .filter(|name| name.bytes().all(|b| b.is_ascii_digit()))
        .filter_map(parse_generation)
        .collect();
    found.sort_unstable();
    found.dedup();
    found
}

/// Поколение рантайма для процесса поколения `installed`: ровно его рантайм. Откат на другое
/// поколение запустил бы новую сессию чужим кодом, поэтому без готового рантайма — отказ.
pub fn select(installed: u32, prepared: &[u32]) -> Result<u32, String> {
    if prepared.contains(&installed) {
        Ok(installed)
    } else {
        Err(format!(
            "рантайм поколения {installed} не подготовлен, готовы: {prepared:?}"
        ))
    }
}

/// Итог подготовки рантайма поколения.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Prepared {
    pub generation: u32,
    pub dir: PathBuf,
    /// Каталог уже был и совпал с бандлом побайтно.
    pub reused: bool,
    pub files: Vec<String>,
}

/// Раскладка `runtime/`: `runtime/<N>/<бинарь>` и каталоги подготовки `.staging-<N>-<pid>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Runtime {
    root: PathBuf,
}

impl Runtime {
    pub fn new(root: &Path) -> Runtime {
        Runtime {
            root: root.to_path_buf(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn dir(&self, generation: u32) -> PathBuf {
        self.root.join(generation.to_string())
    }

    pub fn helper(&self, generation: u32, helper: Helper) -> PathBuf {
        self.dir(generation).join(helper.file_name())
    }

    /// Бинарь, из которого поколение запускает хост новой сессии кандидата.
    pub fn host_binary(&self, candidate: Candidate, generation: u32) -> PathBuf {
        self.helper(generation, Helper::host_of(candidate))
    }

    /// Бинарь, которым поколение открывает сессию кандидата.
    pub fn client_binary(&self, candidate: Candidate, generation: u32) -> PathBuf {
        self.helper(generation, Helper::client_of(candidate))
    }

    fn staging(&self, generation: u32) -> PathBuf {
        self.root.join(format!(
            "{STAGING_PREFIX}{generation}-{}",
            std::process::id()
        ))
    }

    /// Поколения с готовым рантаймом.
    pub fn prepared(&self) -> Result<Vec<u32>, String> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(format!("{}: {err}", self.root.display())),
        };
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|err| format!("{}: {err}", self.root.display()))?;
            if entry.path().is_dir() {
                names.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        Ok(generations(names.iter().map(String::as_str)))
    }

    /// Рантайм поколения `generation` из бинарей бандла `app`. Копии сверяются с бандлом
    /// побайтно и проверяются `verify` до того, как каталог получит имя поколения. Готовый
    /// каталог не перезаписывается: совпал с бандлом — используется, нет — отказ.
    pub fn prepare(
        &self,
        generation: u32,
        app: &Path,
        verify: impl Fn(&Path, Helper) -> Result<(), String>,
    ) -> Result<Prepared, String> {
        let dir = self.dir(generation);
        let files: Vec<String> = Helper::ALL
            .iter()
            .map(|helper| helper.file_name().to_owned())
            .collect();
        if dir.exists() {
            for helper in Helper::ALL {
                same_bytes(
                    &bundled(app, helper.file_name()),
                    &dir.join(helper.file_name()),
                )?;
                verify(&dir.join(helper.file_name()), helper)?;
            }
            let extra = extra_files(&dir, &files)?;
            if !extra.is_empty() {
                return Err(format!("{}: лишние файлы {extra:?}", dir.display()));
            }
            return Ok(Prepared {
                generation,
                dir,
                reused: true,
                files,
            });
        }
        let staging = self.staging(generation);
        if staging.exists() {
            fs::remove_dir_all(&staging).map_err(|err| format!("{}: {err}", staging.display()))?;
        }
        fs::create_dir_all(&staging).map_err(|err| format!("{}: {err}", staging.display()))?;
        let staged = (|| {
            for helper in Helper::ALL {
                let source = bundled(app, helper.file_name());
                let target = staging.join(helper.file_name());
                fs::copy(&source, &target)
                    .map_err(|err| format!("{} → {}: {err}", source.display(), target.display()))?;
                same_bytes(&source, &target)?;
                verify(&target, helper)?;
            }
            fs::rename(&staging, &dir)
                .map_err(|err| format!("{} → {}: {err}", staging.display(), dir.display()))
        })();
        if let Err(reason) = staged {
            let _ = fs::remove_dir_all(&staging);
            return Err(reason);
        }
        Ok(Prepared {
            generation,
            dir,
            reused: false,
            files,
        })
    }
}

fn same_bytes(expected: &Path, actual: &Path) -> Result<(), String> {
    let read = |path: &Path| fs::read(path).map_err(|err| format!("{}: {err}", path.display()));
    if read(expected)? == read(actual)? {
        Ok(())
    } else {
        Err(format!(
            "{} не совпадает с {}",
            actual.display(),
            expected.display()
        ))
    }
}

fn extra_files(dir: &Path, known: &[String]) -> Result<Vec<String>, String> {
    let mut extra = Vec::new();
    for entry in fs::read_dir(dir).map_err(|err| format!("{}: {err}", dir.display()))? {
        let name = entry
            .map_err(|err| format!("{}: {err}", dir.display()))?
            .file_name()
            .to_string_lossy()
            .into_owned();
        if !known.contains(&name) {
            extra.push(name);
        }
    }
    Ok(extra)
}

/// Установка подготовленного бандла `staged` на место `installed` атомарным обменом; прежний
/// бандл оказывается на месте `staged` и удаляется. Подписанные файлы не перезаписываются на
/// месте: запущенные из прежнего бандла процессы держат свои файлы.
pub fn install(installed: &Path, staged: &Path) -> Result<(), String> {
    sys::swap_paths(staged, installed).map_err(|err| {
        format!(
            "обмен {} и {}: {err}",
            staged.display(),
            installed.display()
        )
    })?;
    fs::remove_dir_all(staged).map_err(|err| format!("{}: {err}", staged.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(label: &str) -> Scratch {
            let dir =
                std::env::temp_dir().join(format!("pss-update-{label}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("временный каталог");
            Scratch(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn fake_app(root: &Path, name: &str, marker: &str) -> PathBuf {
        let app = root.join(name).join("probe-session-surface.app");
        let macos = app.join("Contents").join("MacOS");
        fs::create_dir_all(&macos).expect("бандл");
        for helper in Helper::ALL {
            fs::write(
                macos.join(helper.file_name()),
                format!("{marker}:{}", helper.file_name()),
            )
            .expect("бинарь");
        }
        app
    }

    fn accept(_: &Path, _: Helper) -> Result<(), String> {
        Ok(())
    }

    #[test]
    fn runtime_layout_keeps_binaries_outside_the_bundle() {
        let runtime = Runtime::new(Path::new("/p/runtime"));
        assert_eq!(runtime.dir(2), Path::new("/p/runtime/2"));
        assert_eq!(
            runtime.host_binary(Candidate::Rust, 1),
            Path::new("/p/runtime/1/probe-session-host")
        );
        assert_eq!(
            runtime.client_binary(Candidate::Rust, 1),
            Path::new("/p/runtime/1/probe-session-attach")
        );
        assert_eq!(
            runtime.host_binary(Candidate::Zmx, 3),
            Path::new("/p/runtime/3/zmx")
        );
        assert_eq!(
            runtime.client_binary(Candidate::Zmx, 3),
            Path::new("/p/runtime/3/zmx")
        );
        let app = Path::new("/p/bundle/probe-session-surface.app");
        for candidate in Candidate::ALL {
            assert!(!runtime.host_binary(candidate, 1).starts_with(app));
        }
        assert_eq!(
            bundled(app, HOST_NAME),
            Path::new("/p/bundle/probe-session-surface.app/Contents/MacOS/probe-session-host")
        );
    }

    #[test]
    fn helpers_have_own_signing_identifiers() {
        let ids: Vec<String> = Helper::ALL
            .iter()
            .map(|helper| helper.identifier())
            .collect();
        assert_eq!(
            ids,
            [
                "io.github.dsbasko.shellio.probe.session-survival.host",
                "io.github.dsbasko.shellio.probe.session-survival.attach",
                "io.github.dsbasko.shellio.probe.session-survival.zmx",
            ]
        );
    }

    #[test]
    fn generation_dirs_ignore_staging_and_foreign_names() {
        let names = [".staging-2-77", "2", "1", "x1", "01", "0", "10", "-3", "2"];
        assert_eq!(generations(names), [1, 2, 10]);
    }

    #[test]
    fn selection_takes_exactly_the_installed_generation() {
        assert_eq!(select(2, &[1, 2]), Ok(2));
        assert_eq!(select(1, &[1, 2]), Ok(1));
        assert!(select(3, &[1, 2]).is_err(), "без отката на поколение 2");
        assert!(select(1, &[]).is_err());
    }

    #[test]
    fn bundle_of_executable_requires_contents_macos() {
        assert_eq!(
            bundle_of(Path::new("/b/x.app/Contents/MacOS/x")),
            Some(PathBuf::from("/b/x.app"))
        );
        assert_eq!(bundle_of(Path::new("/b/target/release/x")), None);
        assert_eq!(bundle_of(Path::new("/b/x/Contents/MacOS/x")), None);
    }

    #[test]
    fn codesign_outputs_are_parsed() {
        assert_eq!(parse_generation("2\n"), Some(2));
        assert_eq!(parse_generation("0"), None);
        assert_eq!(parse_generation("1.2"), None);
        let details = "Executable=/x\nIdentifier=io.x\nCDHash=ab12cd\nSignature size=9\n";
        assert_eq!(parse_cdhash(details).as_deref(), Some("ab12cd"));
        assert_eq!(parse_cdhash("Identifier=io.x\n"), None);
        let leaf = "0123456789ABCDEF0123456789ABCDEF01234567";
        let designated = format!("designated => {}", requirement("io.x", leaf));
        assert_eq!(parse_leaf(&designated).as_deref(), Some(leaf));
        assert_eq!(parse_leaf("designated => identifier \"io.x\""), None);
        assert_eq!(parse_leaf("certificate leaf = H\"12\""), None);
    }

    #[test]
    fn manifest_requires_distinct_generations_apart_from_installed() {
        let good = r#"{"installed":"/b/a.app","runtime":"/r",
            "generations":[{"generation":1,"app":"/g/1/a.app"},{"generation":2,"app":"/g/2/a.app"}]}"#;
        let manifest = Manifest::parse(good).expect("манифест");
        assert_eq!(manifest.bundle(2), Ok(Path::new("/g/2/a.app")));
        assert!(manifest.bundle(3).is_err());
        let repeated = good.replace("\"generation\":2", "\"generation\":1");
        assert!(Manifest::parse(&repeated).is_err());
        let zero = good.replace("\"generation\":1", "\"generation\":0");
        assert!(Manifest::parse(&zero).is_err());
        let same = good.replace("/g/1/a.app", "/b/a.app");
        assert!(Manifest::parse(&same).is_err());
    }

    #[test]
    fn prepare_copies_through_staging_and_reuses_only_identical_dirs() {
        let scratch = Scratch::new("prepare");
        let first = fake_app(&scratch.0, "g1", "one");
        let other = fake_app(&scratch.0, "g1b", "other");
        let runtime = Runtime::new(&scratch.0.join("runtime"));
        assert_eq!(runtime.prepared(), Ok(Vec::new()));
        let made = runtime.prepare(1, &first, accept).expect("рантайм 1");
        assert!(!made.reused);
        assert_eq!(
            fs::read_to_string(runtime.host_binary(Candidate::Rust, 1))
                .ok()
                .as_deref(),
            Some("one:probe-session-host")
        );
        assert_eq!(runtime.prepared(), Ok(vec![1]));
        assert_eq!(select(1, &runtime.prepared().expect("список")), Ok(1));
        let again = runtime.prepare(1, &first, accept).expect("повтор");
        assert!(again.reused);
        assert!(runtime.prepare(1, &other, accept).is_err(), "чужая сборка");
        let names: Vec<String> = fs::read_dir(runtime.root())
            .expect("корень")
            .map(|entry| {
                entry
                    .expect("запись")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names, ["1"], "каталог подготовки убран");
    }

    #[test]
    fn prepare_leaves_no_generation_when_verification_fails() {
        let scratch = Scratch::new("refuse");
        let app = fake_app(&scratch.0, "g1", "one");
        let runtime = Runtime::new(&scratch.0.join("runtime"));
        let refuse = |path: &Path, helper: Helper| match helper {
            Helper::Attach => Err(format!("подпись {}", path.display())),
            _ => Ok(()),
        };
        assert!(runtime.prepare(1, &app, refuse).is_err());
        assert_eq!(runtime.prepared(), Ok(Vec::new()));
        let leftovers = fs::read_dir(runtime.root()).expect("корень").count();
        assert_eq!(leftovers, 0);
        fs::remove_file(bundled(&app, ZMX_NAME)).expect("удаление");
        assert!(
            runtime.prepare(1, &app, accept).is_err(),
            "нет бинаря в бандле"
        );
        assert_eq!(runtime.prepared(), Ok(Vec::new()));
    }

    #[test]
    fn install_swaps_bundles_and_removes_the_previous_one() {
        let scratch = Scratch::new("install");
        let installed = fake_app(&scratch.0, "bundle", "one");
        let staged = fake_app(&scratch.0, "next", "two");
        install(&installed, &staged).expect("установка");
        assert_eq!(
            fs::read_to_string(bundled(&installed, HOST_NAME))
                .ok()
                .as_deref(),
            Some("two:probe-session-host")
        );
        assert!(!staged.exists());
        assert!(install(&installed, &staged).is_err(), "нечего ставить");
    }
}
