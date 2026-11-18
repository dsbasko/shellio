//! Поколения бандла пробы и проверка кандидата (задача 4, клетки A1 и A2).
//!
//! `run.sh bundle` собирает каждое поколение своим вызовом cargo, раскладывает бандлы, подписывает
//! их и пишет `bundles.json` ([`Layout`]). Манифест поколения ([`Manifest`]) печатает сам
//! `probe-update-app --manifest` этого поколения, поэтому он не расходится со скомпилированными
//! константами.
//!
//! Проверка кандидата ([`check_candidate`]) решает только по коду выхода
//! `codesign --verify --strict --deep -R=<требование>` и CDHash. `Authority=` и текст designated
//! requirement решения не меняют: у временной идентичности тот же CN, а требование пишет сам
//! подписант. Вид отказа уточняется по фактам подписи: идентификатору и SHA-1 листового
//! сертификата, извлечённого из подписи. Разбор выводов чистый и идёт в `make check` на
//! сохранённых выводах; вызовы `codesign` — только в сценариях.
//!
//! Разбор CDHash и листа из требования — по образцу `spikes/probe-session-survival/src/matrix/
//! update.rs` (`parse_cdhash`, `parse_leaf`, `requirement`), расширен идентификатором,
//! `Authority`, признаком ad-hoc и классификацией кода выхода проверки.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use serde::{Deserialize, Serialize};

use crate::proto::{ATTACH_VERSIONS, HOST_VERSIONS, Versions};
use crate::{EXIT_USAGE, attach, fixture, host};

/// Идентификатор бандла пробы и основа идентификаторов его частей.
pub const BUNDLE_ID: &str = "io.github.dsbasko.shellio.probe.update-lifecycle";
/// Идентификатор кандидата `wrong-id`.
pub const WRONG_ID: &str = "io.github.dsbasko.shellio.probe.update-lifecycle.wrong-id";
/// Идентификатор бандла-контроля TCC и Keychain.
pub const DENIED_ID: &str = "io.github.dsbasko.shellio.probe.update-lifecycle.denied";
/// Имя бандла поколения и кандидатов.
pub const APP_NAME: &str = "probe-update.app";
pub const APP_BIN: &str = "probe-update-app";
pub const HELPER_BIN: &str = "probe-update-helper";
pub const MANIFEST_PATH: &str = "Contents/Resources/generation.json";
pub const INFO_PLIST: &str = "Contents/Info.plist";
/// Раскладка бандлов в каталоге `SHELLIO_PROBE_BUNDLES`; её пишет `run.sh bundle`.
pub const LAYOUT_FILE: &str = "bundles.json";
/// Сверка окружения подписи вокруг временной идентичности `foreign`.
pub const FOREIGN_FILE: &str = "foreign-identity.json";
/// CN сертификата подписи и первая строка `Authority=`.
pub const SIGNER_CN: &str = "Shellio Dev";
const CODESIGN: &str = "/usr/bin/codesign";
const SHASUM: &str = "/usr/bin/shasum";
const PLUTIL: &str = "/usr/bin/plutil";

/// Бандлы `bundles.json` по именам.
pub const BUNDLES: [&str; 7] = [
    "gen-1",
    "gen-2",
    "incompatible",
    "tampered",
    "foreign",
    "wrong-id",
    "denied",
];

/// Исполняемая часть бандла: исполняемый файл бандла и вспомогательные бинари.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Part {
    App,
    Helper,
    Host,
    Attach,
    Fixture,
}

impl Part {
    pub const ALL: [Part; 5] = [
        Part::App,
        Part::Helper,
        Part::Host,
        Part::Attach,
        Part::Fixture,
    ];
    /// Бинари, которые поколение копирует в `runtime/<N>/`.
    pub const RUNTIME: [Part; 3] = [Part::Host, Part::Attach, Part::Fixture];

    pub fn name(self) -> &'static str {
        match self {
            Part::App => "app",
            Part::Helper => "helper",
            Part::Host => "host",
            Part::Attach => "attach",
            Part::Fixture => "fixture",
        }
    }

    pub fn file_name(self) -> &'static str {
        match self {
            Part::App => APP_BIN,
            Part::Helper => HELPER_BIN,
            Part::Host => host::HOST_NAME,
            Part::Attach => attach::ATTACH_NAME,
            Part::Fixture => fixture::FIXTURE_NAME,
        }
    }

    /// Путь внутри бандла.
    pub fn relative(self) -> String {
        format!("Contents/MacOS/{}", self.file_name())
    }

    pub fn path(self, app: &Path) -> PathBuf {
        app.join(self.relative())
    }

    /// Идентификатор подписи части при основе `base`: исполняемый файл несёт подпись бандла.
    pub fn identifier(self, base: &str) -> String {
        match self {
            Part::App => base.to_owned(),
            other => format!("{base}.{}", other.name()),
        }
    }
}

/// `Contents/Resources/generation.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub generation: u32,
    pub host_versions: Versions,
    pub attach_versions: Versions,
    /// Бинари для `runtime/<N>/`: пути внутри бандла.
    pub runtime: Vec<String>,
}

impl Manifest {
    /// Манифест этой сборки.
    pub fn current() -> Manifest {
        Manifest {
            generation: crate::GENERATION,
            host_versions: HOST_VERSIONS,
            attach_versions: ATTACH_VERSIONS,
            runtime: Part::RUNTIME.iter().map(|part| part.relative()).collect(),
        }
    }

    pub fn parse(text: &str) -> Result<Manifest, String> {
        let manifest: Manifest =
            serde_json::from_str(text).map_err(|err| format!("{MANIFEST_PATH}: {err}"))?;
        if manifest.generation == 0 {
            return Err(format!("{MANIFEST_PATH}: поколение 0"));
        }
        for (what, versions) in [
            ("хоста", manifest.host_versions),
            ("attach", manifest.attach_versions),
        ] {
            if !versions.is_valid() {
                return Err(format!(
                    "{MANIFEST_PATH}: диапазон протокола {what} {versions} пуст"
                ));
            }
        }
        let expected: Vec<String> = Part::RUNTIME.iter().map(|part| part.relative()).collect();
        if manifest.runtime != expected {
            return Err(format!(
                "{MANIFEST_PATH}: бинари runtime {:?}, ожидаются {expected:?}",
                manifest.runtime
            ));
        }
        Ok(manifest)
    }

    pub fn read(app: &Path) -> Result<Manifest, String> {
        let path = app.join(MANIFEST_PATH);
        let text = fs::read_to_string(&path).map_err(|err| format!("{}: {err}", path.display()))?;
        Manifest::parse(&text)
    }
}

/// Факты подписи из `codesign -d --verbose=4`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Details {
    pub identifier: String,
    pub cdhash: String,
    /// Цепочка `Authority=` от листа; у самоподписанного сертификата одна строка.
    pub authorities: Vec<String>,
    pub adhoc: bool,
    pub team: Option<String>,
    pub format: Option<String>,
}

fn is_hex(text: &str, len: usize) -> bool {
    text.len() == len && text.chars().all(|c| c.is_ascii_hexdigit())
}

/// Разбор `codesign -d --verbose=4` (stderr и stdout вместе). Пути в выводе не читаются.
pub fn parse_details(text: &str) -> Result<Details, String> {
    let mut details = Details::default();
    let (mut identifier, mut cdhash) = (None, None);
    for line in text.lines() {
        let line = line.trim_end();
        if line.ends_with("code object is not signed at all") {
            return Err("код не подписан".to_owned());
        }
        if let Some(value) = line.strip_prefix("Identifier=") {
            identifier = Some(value.trim().to_owned());
        } else if let Some(value) = line.strip_prefix("CDHash=") {
            cdhash = Some(value.trim().to_ascii_lowercase());
        } else if let Some(value) = line.strip_prefix("Authority=") {
            details.authorities.push(value.trim().to_owned());
        } else if let Some(value) = line.strip_prefix("TeamIdentifier=") {
            let value = value.trim();
            details.team = (value != "not set" && !value.is_empty()).then(|| value.to_owned());
        } else if let Some(value) = line.strip_prefix("Format=") {
            details.format = Some(value.trim().to_owned());
        } else if line == "Signature=adhoc"
            || (line.starts_with("CodeDirectory ") && line.contains("adhoc"))
        {
            details.adhoc = true;
        }
    }
    details.identifier = identifier
        .filter(|identifier| !identifier.is_empty())
        .ok_or("в выводе codesign -d нет Identifier")?;
    details.cdhash = cdhash
        .filter(|hash| is_hex(hash, 40))
        .ok_or("в выводе codesign -d нет CDHash из 40 шестнадцатеричных знаков")?;
    Ok(details)
}

/// Designated requirement из `codesign -d -r-`.
pub fn parse_designated(text: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.trim().strip_prefix("designated => "))
        .map(|requirement| requirement.trim().to_owned())
        .filter(|requirement| !requirement.is_empty())
}

/// SHA-1 листового сертификата из требования, строчными.
pub fn parse_leaf(requirement: &str) -> Option<String> {
    let rest = requirement.split("certificate leaf = H\"").nth(1)?;
    let leaf = rest.split('"').next()?;
    is_hex(leaf, 40).then(|| leaf.to_ascii_lowercase())
}

/// Требование подписи: идентификатор и листовой сертификат.
pub fn requirement(identifier: &str, leaf: &str) -> String {
    format!(
        "identifier \"{identifier}\" and certificate leaf = H\"{}\"",
        leaf.to_ascii_lowercase()
    )
}

/// SHA-1 из строки `shasum -a 1` или `shasum -a 256`: первое слово нужной длины.
pub fn parse_digest(text: &str, len: usize) -> Option<String> {
    let word = text.split_whitespace().next()?;
    is_hex(word, len).then(|| word.to_ascii_lowercase())
}

/// Итог `codesign --verify --strict --deep -R=<требование>` по коду выхода (`man codesign`,
/// DIAGNOSTICS).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "kebab-case")]
pub enum Verify {
    /// Код 0: подпись цела, требование выполнено.
    Satisfied,
    /// Код 3: подпись цела, требование не выполнено.
    Unsatisfied,
    /// Код 1: подпись не цела или её нет.
    Invalid { reason: String },
    /// Другой код или сигнал: ошибка вызова, кандидат не проверен.
    Failed { code: Option<i32>, reason: String },
}

impl Verify {
    pub fn name(&self) -> &'static str {
        match self {
            Verify::Satisfied => "satisfied",
            Verify::Unsatisfied => "unsatisfied",
            Verify::Invalid { .. } => "invalid",
            Verify::Failed { .. } => "failed",
        }
    }
}

/// stderr без пустых строк одной строкой.
fn condense(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("; ")
}

pub fn classify_verify(code: Option<i32>, stderr: &str) -> Verify {
    match code {
        Some(0) => Verify::Satisfied,
        Some(3) => Verify::Unsatisfied,
        Some(1) => Verify::Invalid {
            reason: condense(stderr),
        },
        code => Verify::Failed {
            code,
            reason: condense(stderr),
        },
    }
}

/// Наблюдение кандидата: всё, что проверка читает с диска.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    /// Требование, с которым шла проверка.
    pub requirement: String,
    pub verify: Verify,
    pub details: Result<Details, String>,
    pub designated: Option<String>,
    /// SHA-1 листового сертификата, извлечённого из подписи.
    pub leaf: Option<String>,
    pub manifest: Result<Manifest, String>,
    /// `CFBundleVersion`.
    pub bundle_version: Option<u32>,
}

/// Чего проверка ждёт от кандидата.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Expected<'a> {
    pub identifier: &'a str,
    /// Закреплённый SHA-1 листового сертификата.
    pub leaf: &'a str,
    /// Поколение установленного N.
    pub installed: u32,
}

impl Expected<'_> {
    pub fn requirement(&self) -> String {
        requirement(self.identifier, self.leaf)
    }
}

/// Принятый кандидат: его поколение и CDHash, которые запишет журнал.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Accepted {
    pub generation: u32,
    pub cdhash: String,
    pub identifier: String,
}

/// Какая часть требования не выполнена, по фактам подписи.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mismatch {
    /// Идентификатор подписи не ожидаемый.
    Identifier,
    /// Листовой сертификат не закреплённый.
    Signer,
}

/// Причина отказа кандидату.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Rejection {
    /// Код 1: подпись не цела или её нет.
    Integrity { reason: String },
    /// Код 3: подпись цела, требование не выполнено.
    Requirement {
        identifier: Option<String>,
        leaf: Option<String>,
        mismatch: Vec<Mismatch>,
    },
    /// Требование выполнено, но манифест или `CFBundleVersion` не годятся.
    Manifest { reason: String },
    /// Поколение кандидата не новее установленного.
    NotNewer { installed: u32, candidate: u32 },
    /// Проверка не состоялась: ошибка вызова или противоречивые факты.
    Unverified { reason: String },
}

impl Rejection {
    pub fn kind(&self) -> &'static str {
        match self {
            Rejection::Integrity { .. } => "integrity",
            Rejection::Requirement { .. } => "requirement",
            Rejection::Manifest { .. } => "manifest",
            Rejection::NotNewer { .. } => "not-newer",
            Rejection::Unverified { .. } => "unverified",
        }
    }
}

/// Решение о кандидате по наблюдению. Принимается только кандидат с кодом 0 при ожидаемом
/// требовании, разобранным CDHash, годным манифестом, `CFBundleVersion`, равным поколению
/// манифеста, и поколением новее установленного.
pub fn check_candidate(expected: &Expected, observed: &Observation) -> Result<Accepted, Rejection> {
    let wanted = expected.requirement();
    if observed.requirement != wanted {
        return Err(Rejection::Unverified {
            reason: format!(
                "проверка шла с требованием «{}», ожидается «{wanted}»",
                observed.requirement
            ),
        });
    }
    match &observed.verify {
        Verify::Satisfied => {}
        Verify::Invalid { reason } => {
            return Err(Rejection::Integrity {
                reason: reason.clone(),
            });
        }
        Verify::Unsatisfied => {
            let identifier = observed
                .details
                .as_ref()
                .ok()
                .map(|details| details.identifier.clone());
            let mut mismatch = Vec::new();
            if identifier.as_deref() != Some(expected.identifier) {
                mismatch.push(Mismatch::Identifier);
            }
            if observed.leaf.as_deref() != Some(expected.leaf.to_ascii_lowercase().as_str()) {
                mismatch.push(Mismatch::Signer);
            }
            return Err(Rejection::Requirement {
                identifier,
                leaf: observed.leaf.clone(),
                mismatch,
            });
        }
        Verify::Failed { code, reason } => {
            return Err(Rejection::Unverified {
                reason: format!("codesign --verify: код {code:?}: {reason}"),
            });
        }
    }
    let details = observed
        .details
        .as_ref()
        .map_err(|err| Rejection::Unverified {
            reason: format!("подпись прошла проверку, но не прочитана: {err}"),
        })?;
    let manifest = observed
        .manifest
        .as_ref()
        .map_err(|err| Rejection::Manifest {
            reason: err.clone(),
        })?;
    if observed.bundle_version != Some(manifest.generation) {
        return Err(Rejection::Manifest {
            reason: format!(
                "CFBundleVersion {:?} не равен поколению манифеста {}",
                observed.bundle_version, manifest.generation
            ),
        });
    }
    if manifest.generation <= expected.installed {
        return Err(Rejection::NotNewer {
            installed: expected.installed,
            candidate: manifest.generation,
        });
    }
    Ok(Accepted {
        generation: manifest.generation,
        cdhash: details.cdhash.clone(),
        identifier: details.identifier.clone(),
    })
}

/// Изменённый после подписи файл кандидата `tampered`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tampered {
    /// Путь внутри бандла.
    pub file: String,
    pub sha256_before: String,
    pub sha256_after: String,
}

/// Бандл в `bundles.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleEntry {
    pub name: String,
    /// Поколение сборки, из каталога cargo которого взяты бинари.
    pub generation: u32,
    pub app: PathBuf,
    pub identifier: String,
    /// SHA-1 листа подписанта, строчными.
    pub signer_leaf: String,
    /// CDHash бандла по `codesign -d` при сборке; сценарии измеряют его заново.
    pub cdhash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tampered: Option<Tampered>,
}

/// `bundles.json`: закреплённый лист и бандлы стенда.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Layout {
    pub bundle_id: String,
    /// Закреплённый SHA-1 листового сертификата из `scripts/signing/requirement.txt`, строчными.
    pub leaf: String,
    pub bundles: Vec<BundleEntry>,
}

impl Layout {
    pub fn parse(text: &str) -> Result<Layout, String> {
        let layout: Layout =
            serde_json::from_str(text).map_err(|err| format!("{LAYOUT_FILE}: {err}"))?;
        if layout.bundle_id != BUNDLE_ID {
            return Err(format!(
                "{LAYOUT_FILE}: bundle_id {}, ожидается {BUNDLE_ID}",
                layout.bundle_id
            ));
        }
        if !is_hex(&layout.leaf, 40) || layout.leaf != layout.leaf.to_ascii_lowercase() {
            return Err(format!(
                "{LAYOUT_FILE}: лист «{}» не 40 строчных шестнадцатеричных знаков",
                layout.leaf
            ));
        }
        for name in BUNDLES {
            let count = layout
                .bundles
                .iter()
                .filter(|entry| entry.name == name)
                .count();
            if count != 1 {
                return Err(format!("{LAYOUT_FILE}: бандл {name} записан {count} раз"));
            }
        }
        for entry in &layout.bundles {
            if !BUNDLES.contains(&entry.name.as_str()) {
                return Err(format!("{LAYOUT_FILE}: неизвестный бандл {}", entry.name));
            }
            if entry.generation == 0 || !entry.app.is_absolute() {
                return Err(format!(
                    "{LAYOUT_FILE}: бандл {}: поколение 0 или относительный путь",
                    entry.name
                ));
            }
            if !is_hex(&entry.cdhash, 40) || !is_hex(&entry.signer_leaf, 40) {
                return Err(format!(
                    "{LAYOUT_FILE}: бандл {}: CDHash или лист не шестнадцатеричные",
                    entry.name
                ));
            }
        }
        Ok(layout)
    }

    pub fn read(dir: &Path) -> Result<Layout, String> {
        let path = dir.join(LAYOUT_FILE);
        let text = fs::read_to_string(&path).map_err(|err| format!("{}: {err}", path.display()))?;
        Layout::parse(&text)
    }

    pub fn get(&self, name: &str) -> Result<&BundleEntry, String> {
        self.bundles
            .iter()
            .find(|entry| entry.name == name)
            .ok_or_else(|| format!("{LAYOUT_FILE}: нет бандла {name}"))
    }

    /// CDHash бандлов по именам для `environment.generations`.
    pub fn cdhashes(&self) -> BTreeMap<String, String> {
        self.bundles
            .iter()
            .map(|entry| (entry.name.clone(), entry.cdhash.clone()))
            .collect()
    }
}

/// `foreign-identity.json`: сверка окружения подписи до создания временной идентичности, во
/// время подписи (временная keychain ещё жива) и после её удаления. Снимок во время подписи
/// нужен потому, что `security delete-keychain` убирает keychain и из списка поиска: сверка
/// только до и после не увидела бы keychain, добавленную в список на время подписи.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForeignIdentity {
    /// SHA-1 листа временной идентичности, строчными.
    pub leaf: String,
    pub requirement_sha256_before: String,
    pub requirement_sha256_during: String,
    pub requirement_sha256_after: String,
    pub search_list_before: Vec<String>,
    pub search_list_during: Vec<String>,
    pub search_list_after: Vec<String>,
    pub default_keychain_before: String,
    pub default_keychain_during: String,
    pub default_keychain_after: String,
}

impl ForeignIdentity {
    pub fn parse(text: &str) -> Result<ForeignIdentity, String> {
        serde_json::from_str(text).map_err(|err| format!("{FOREIGN_FILE}: {err}"))
    }

    /// Что изменилось во время подписи или после; пусто — окружение подписи прежнее.
    pub fn changes(&self) -> Vec<String> {
        let mut changes = Vec::new();
        for (when, requirement, list, default) in [
            (
                "во время подписи",
                &self.requirement_sha256_during,
                &self.search_list_during,
                &self.default_keychain_during,
            ),
            (
                "после удаления keychain",
                &self.requirement_sha256_after,
                &self.search_list_after,
                &self.default_keychain_after,
            ),
        ] {
            if requirement != &self.requirement_sha256_before {
                changes.push(format!("scripts/signing/requirement.txt {when}"));
            }
            if list != &self.search_list_before {
                changes.push(format!("список поиска keychain {when}"));
            }
            if default != &self.default_keychain_before {
                changes.push(format!("keychain по умолчанию {when}"));
            }
        }
        changes
    }
}

/// Вызов внешней команды с выводом: сырой артефакт сценария.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    pub command: Vec<String>,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Run {
    /// stderr и stdout вместе: `codesign -d` пишет в оба.
    pub fn text(&self) -> String {
        format!("{}{}", self.stderr, self.stdout)
    }
}

/// Запуск команды до выхода; отказ запуска — ошибка исполнения.
pub fn run(program: &str, args: &[String]) -> Result<Run, String> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|err| format!("запуск {program}: {err}"))?;
    let mut command = vec![program.to_owned()];
    command.extend(args.iter().cloned());
    Ok(Run {
        command,
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

fn path_arg(path: &Path) -> String {
    path.display().to_string()
}

/// `codesign --verify --strict --deep -R=<требование>`.
pub fn verify(path: &Path, requirement: &str, runs: &mut Vec<Run>) -> Result<Verify, String> {
    let call = run(
        CODESIGN,
        &[
            "--verify".to_owned(),
            "--strict".to_owned(),
            "--deep".to_owned(),
            format!("-R={requirement}"),
            path_arg(path),
        ],
    )?;
    let verify = classify_verify(call.code, &call.stderr);
    runs.push(call);
    Ok(verify)
}

/// `codesign -d --verbose=4`.
pub fn details(path: &Path, runs: &mut Vec<Run>) -> Result<Result<Details, String>, String> {
    let call = run(
        CODESIGN,
        &["-d".to_owned(), "--verbose=4".to_owned(), path_arg(path)],
    )?;
    let details = parse_details(&call.text());
    runs.push(call);
    Ok(details)
}

/// `codesign -d -r-`.
pub fn designated(path: &Path, runs: &mut Vec<Run>) -> Result<Option<String>, String> {
    let call = run(
        CODESIGN,
        &["-d".to_owned(), "-r-".to_owned(), path_arg(path)],
    )?;
    let designated = parse_designated(&call.text());
    runs.push(call);
    Ok(designated)
}

/// SHA-1 листового сертификата: `codesign -d --extract-certificates=<prefix>` пишет лист в
/// `<prefix>0`. Без подписи — `None`.
pub fn leaf(path: &Path, prefix: &Path, runs: &mut Vec<Run>) -> Result<Option<String>, String> {
    let call = run(
        CODESIGN,
        &[
            "-d".to_owned(),
            format!("--extract-certificates={}", prefix.display()),
            path_arg(path),
        ],
    )?;
    let ok = call.code == Some(0);
    runs.push(call);
    let cert = PathBuf::from(format!("{}0", prefix.display()));
    if !ok || !cert.is_file() {
        return Ok(None);
    }
    let call = run(SHASUM, &["-a".to_owned(), "1".to_owned(), path_arg(&cert)])?;
    let digest = parse_digest(&call.stdout, 40);
    runs.push(call);
    Ok(digest)
}

/// SHA-256 файлов одним вызовом `shasum -a 256`.
pub fn sha256(paths: &[PathBuf], runs: &mut Vec<Run>) -> Result<Vec<String>, String> {
    let mut args = vec!["-a".to_owned(), "256".to_owned()];
    args.extend(paths.iter().map(|path| path_arg(path)));
    let call = run(SHASUM, &args)?;
    if call.code != Some(0) {
        let reason = format!("shasum: код {:?}: {}", call.code, condense(&call.stderr));
        runs.push(call);
        return Err(reason);
    }
    let digests: Vec<String> = call
        .stdout
        .lines()
        .filter_map(|line| parse_digest(line, 64))
        .collect();
    runs.push(call);
    if digests.len() != paths.len() {
        return Err(format!(
            "shasum: {} сумм на {} файлов",
            digests.len(),
            paths.len()
        ));
    }
    Ok(digests)
}

/// `CFBundleVersion` бандла: целое больше нуля.
pub fn bundle_version(app: &Path, runs: &mut Vec<Run>) -> Result<Option<u32>, String> {
    let call = run(
        PLUTIL,
        &[
            "-extract".to_owned(),
            "CFBundleVersion".to_owned(),
            "raw".to_owned(),
            "-o".to_owned(),
            "-".to_owned(),
            path_arg(&app.join(INFO_PLIST)),
        ],
    )?;
    let version = (call.code == Some(0))
        .then(|| call.stdout.trim().parse::<u32>().ok())
        .flatten()
        .filter(|version| *version > 0);
    runs.push(call);
    Ok(version)
}

/// Наблюдение кандидата `app` с требованием `requirement`; извлечённые сертификаты ложатся под
/// `prefix`.
pub fn observe(
    app: &Path,
    requirement: &str,
    prefix: &Path,
    runs: &mut Vec<Run>,
) -> Result<Observation, String> {
    Ok(Observation {
        requirement: requirement.to_owned(),
        verify: verify(app, requirement, runs)?,
        details: details(app, runs)?,
        designated: designated(app, runs)?,
        leaf: leaf(app, prefix, runs)?,
        manifest: Manifest::read(app),
        bundle_version: bundle_version(app, runs)?,
    })
}

/// Файлы дерева относительными путями по порядку; символические ссылки — как файлы.
pub fn files(root: &Path) -> Result<Vec<String>, String> {
    fn walk(root: &Path, dir: &Path, found: &mut Vec<String>) -> Result<(), String> {
        let entries = fs::read_dir(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|err| format!("{}: {err}", dir.display()))?;
            let path = entry.path();
            let kind = entry
                .file_type()
                .map_err(|err| format!("{}: {err}", path.display()))?;
            if kind.is_dir() {
                walk(root, &path, found)?;
            } else {
                let relative = path
                    .strip_prefix(root)
                    .map_err(|err| format!("{}: {err}", path.display()))?;
                found.push(relative.display().to_string());
            }
        }
        Ok(())
    }
    let mut found = Vec::new();
    walk(root, root, &mut found)?;
    found.sort();
    Ok(found)
}

/// SHA-256 каждого файла дерева по относительному пути.
pub fn digest_tree(root: &Path, runs: &mut Vec<Run>) -> Result<BTreeMap<String, String>, String> {
    let names = files(root)?;
    if names.is_empty() {
        return Err(format!("{}: файлов нет", root.display()));
    }
    let paths: Vec<PathBuf> = names.iter().map(|name| root.join(name)).collect();
    let digests = sha256(&paths, runs)?;
    Ok(names.into_iter().zip(digests).collect())
}

/// Путь внутри дерева без выхода наружу.
pub fn is_relative_inside(path: &str) -> bool {
    let path = Path::new(path);
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

/// Отчёт заглушки: поколение и диапазоны этой сборки.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StubReport {
    pub binary: String,
    pub generation: u32,
    pub host_versions: Versions,
    pub attach_versions: Versions,
    pub executable: String,
}

/// Что печатает заглушка.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StubMode {
    Report,
    Manifest,
}

/// Аргументы заглушки: `--report` или (только `probe-update-app`) `--manifest`, необязательный
/// `--tag <nonce>` для внешнего сторожа.
pub fn parse_stub_args(binary: &str, args: &[String]) -> Result<StubMode, String> {
    let mut mode = None;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let next = match arg.as_str() {
            "--report" => StubMode::Report,
            "--manifest" if binary == APP_BIN => StubMode::Manifest,
            "--tag" => {
                rest.next().ok_or("--tag без значения")?;
                continue;
            }
            other => return Err(format!("неизвестный аргумент «{other}»")),
        };
        if mode.replace(next).is_some() {
            return Err("режим задан дважды".to_owned());
        }
    }
    mode.ok_or_else(|| "нужен --report".to_owned())
}

/// Вход заглушек `probe-update-app` и `probe-update-helper` задачи 4: JSON одной строкой.
pub fn stub_main(binary: &str, args: &[String]) -> ExitCode {
    let mode = match parse_stub_args(binary, args) {
        Ok(mode) => mode,
        Err(reason) => {
            crate::say(&format!("{binary}: {reason}"));
            return ExitCode::from(EXIT_USAGE);
        }
    };
    let text = match mode {
        StubMode::Manifest => serde_json::to_string_pretty(&Manifest::current()),
        StubMode::Report => serde_json::to_string(&StubReport {
            binary: binary.to_owned(),
            generation: crate::GENERATION,
            host_versions: HOST_VERSIONS,
            attach_versions: ATTACH_VERSIONS,
            executable: std::env::current_exe()
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
        }),
    };
    match text {
        Ok(text) => {
            let mut stdout = io::stdout().lock();
            if writeln!(stdout, "{text}").is_ok() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(err) => {
            crate::say(&format!("{binary}: {err}"));
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Сохранённые выводы `codesign` прогона задачи 4: пути заменены на `/tmp/bundle` и `/tmp/x`.
    pub(crate) mod saved {
        pub const GEN2_DETAILS: &str = include_str!("testdata/codesign/gen-2.details.txt");
        pub const GEN2_DESIGNATED: &str = include_str!("testdata/codesign/gen-2.designated.txt");
        pub const GEN2_VERIFY: &str = include_str!("testdata/codesign/gen-2.verify.txt");
        pub const GEN1_HOST_DETAILS: &str =
            include_str!("testdata/codesign/gen-1-host.details.txt");
        pub const TAMPERED_DETAILS: &str = include_str!("testdata/codesign/tampered.details.txt");
        pub const TAMPERED_VERIFY: &str = include_str!("testdata/codesign/tampered.verify.txt");
        pub const FOREIGN_DETAILS: &str = include_str!("testdata/codesign/foreign.details.txt");
        pub const FOREIGN_DESIGNATED: &str =
            include_str!("testdata/codesign/foreign.designated.txt");
        pub const FOREIGN_VERIFY: &str = include_str!("testdata/codesign/foreign.verify.txt");
        pub const WRONG_ID_DETAILS: &str = include_str!("testdata/codesign/wrong-id.details.txt");
        pub const WRONG_ID_VERIFY: &str = include_str!("testdata/codesign/wrong-id.verify.txt");
        pub const ADHOC_DETAILS: &str = include_str!("testdata/codesign/adhoc.details.txt");
        pub const UNSIGNED_DETAILS: &str = include_str!("testdata/codesign/unsigned.details.txt");
        pub const PINNED: &str = "406f897fd84d95069d9a50abd81ab09b7afb7e55";
        pub const FOREIGN_LEAF: &str = "f5997ff5d087cb45153d7c3e1ef1fa78b2c983c8";
        pub const GEN2_CDHASH: &str = "7cba408da0471136dd528842d31e584f7e2a39a3";
    }

    use saved::*;

    pub(crate) fn manifest(generation: u32, host: Versions, attach: Versions) -> Manifest {
        Manifest {
            generation,
            host_versions: host,
            attach_versions: attach,
            runtime: Part::RUNTIME.iter().map(|part| part.relative()).collect(),
        }
    }

    /// Наблюдение кандидата из сохранённых выводов с кодом проверки `code`.
    pub(crate) fn observation(
        code: i32,
        verify_stderr: &str,
        details_text: &str,
        designated_text: &str,
        leaf: Option<&str>,
        generation: u32,
    ) -> Observation {
        Observation {
            requirement: requirement(BUNDLE_ID, PINNED),
            verify: classify_verify(Some(code), verify_stderr),
            details: parse_details(details_text),
            designated: parse_designated(designated_text),
            leaf: leaf.map(str::to_owned),
            manifest: Ok(manifest(
                generation,
                Versions::new(1, 2),
                Versions::new(1, 2),
            )),
            bundle_version: Some(generation),
        }
    }

    pub(crate) fn expected(installed: u32) -> Expected<'static> {
        Expected {
            identifier: BUNDLE_ID,
            leaf: PINNED,
            installed,
        }
    }

    fn gen2() -> Observation {
        observation(
            0,
            GEN2_VERIFY,
            GEN2_DETAILS,
            GEN2_DESIGNATED,
            Some(PINNED),
            2,
        )
    }

    #[test]
    fn parts_name_files_and_identifiers() {
        assert_eq!(Part::App.identifier(BUNDLE_ID), BUNDLE_ID);
        assert_eq!(
            Part::Host.identifier(BUNDLE_ID),
            format!("{BUNDLE_ID}.host")
        );
        assert_eq!(
            Part::Fixture.identifier(DENIED_ID),
            format!("{DENIED_ID}.fixture")
        );
        assert_eq!(WRONG_ID, format!("{BUNDLE_ID}.wrong-id"));
        assert_eq!(DENIED_ID, format!("{BUNDLE_ID}.denied"));
        assert_eq!(Part::Host.relative(), "Contents/MacOS/probe-update-host");
        assert_eq!(
            Part::Helper.path(Path::new("/a/b.app")),
            Path::new("/a/b.app/Contents/MacOS/probe-update-helper")
        );
        let names: Vec<&str> = Part::ALL.iter().map(|part| part.file_name()).collect();
        assert_eq!(
            names,
            [
                APP_BIN,
                HELPER_BIN,
                host::HOST_NAME,
                attach::ATTACH_NAME,
                fixture::FIXTURE_NAME
            ]
        );
        assert!(Part::RUNTIME.iter().all(|part| Part::ALL.contains(part)));
    }

    #[test]
    fn manifest_of_this_build_round_trips() {
        let current = Manifest::current();
        assert_eq!(current.generation, crate::GENERATION);
        assert_eq!(current.host_versions, HOST_VERSIONS);
        let text = serde_json::to_string_pretty(&current).unwrap();
        assert_eq!(Manifest::parse(&text), Ok(current.clone()));
        // Кандидат tampered дописывает пробел: файл меняется, разбор — нет.
        assert_eq!(Manifest::parse(&format!("{text} ")), Ok(current));
    }

    #[test]
    fn malformed_manifest_is_refused() {
        let good = manifest(2, Versions::new(1, 2), Versions::new(1, 2));
        let text = |manifest: &Manifest| serde_json::to_string(manifest).unwrap();
        let mut zero = good.clone();
        zero.generation = 0;
        assert!(
            Manifest::parse(&text(&zero))
                .unwrap_err()
                .contains("поколение 0")
        );
        let mut empty = good.clone();
        empty.host_versions = Versions::new(2, 1);
        assert!(Manifest::parse(&text(&empty)).unwrap_err().contains("пуст"));
        let mut runtime = good.clone();
        runtime.runtime.pop();
        assert!(
            Manifest::parse(&text(&runtime))
                .unwrap_err()
                .contains("runtime")
        );
        let extra = text(&good).replacen('{', "{\"extra\": 1, ", 1);
        assert!(Manifest::parse(&extra).is_err());
        assert!(Manifest::parse("не JSON").is_err());
    }

    #[test]
    fn saved_details_are_parsed() {
        let gen2 = parse_details(GEN2_DETAILS).unwrap();
        assert_eq!(gen2.identifier, BUNDLE_ID);
        assert_eq!(gen2.cdhash, GEN2_CDHASH);
        assert_eq!(gen2.authorities, [SIGNER_CN]);
        assert!(!gen2.adhoc);
        assert_eq!(gen2.team, None);
        assert_eq!(
            gen2.format.as_deref(),
            Some("app bundle with Mach-O thin (arm64)")
        );

        let host = parse_details(GEN1_HOST_DETAILS).unwrap();
        assert_eq!(host.identifier, Part::Host.identifier(BUNDLE_ID));
        assert_eq!(host.authorities, [SIGNER_CN]);

        // Чужой сертификат с тем же CN: Authority не отличает foreign от Shellio Dev.
        let foreign = parse_details(FOREIGN_DETAILS).unwrap();
        assert_eq!(foreign.identifier, BUNDLE_ID);
        assert_eq!(foreign.authorities, gen2.authorities);
        assert_ne!(foreign.cdhash, gen2.cdhash);

        assert_eq!(
            parse_details(WRONG_ID_DETAILS).unwrap().identifier,
            WRONG_ID
        );
        // Изменённый ресурс не меняет CDHash: без проверки целостности поколение не определено.
        assert_eq!(parse_details(TAMPERED_DETAILS).unwrap().cdhash, GEN2_CDHASH);
    }

    #[test]
    fn adhoc_and_unsigned_code_is_recognised() {
        let adhoc = parse_details(ADHOC_DETAILS).unwrap();
        assert!(adhoc.adhoc);
        assert_eq!(adhoc.identifier, "io.example.adhoc");
        assert!(adhoc.authorities.is_empty());
        assert_eq!(
            parse_details(UNSIGNED_DETAILS),
            Err("код не подписан".to_owned())
        );
        let flags_only = "Identifier=x\nCodeDirectory v=20400 size=1 flags=0x2(adhoc) hashes=1+1\nCDHash=8e141f8f43b3bca2b7c7a101138a092c782632c1\n";
        assert!(parse_details(flags_only).unwrap().adhoc);
    }

    #[test]
    fn details_need_identifier_and_cdhash() {
        let cdhash = format!("CDHash={GEN2_CDHASH}\n");
        assert!(parse_details(&cdhash).unwrap_err().contains("Identifier"));
        assert!(
            parse_details("Identifier=x\n")
                .unwrap_err()
                .contains("CDHash")
        );
        assert!(parse_details("Identifier=x\nCDHash=abc\n").is_err());
        assert!(
            parse_details("Identifier=\nCDHash=7cba408da0471136dd528842d31e584f7e2a39a3\n")
                .is_err()
        );
        let upper = format!(
            "Identifier=x\nCDHash={}\n",
            GEN2_CDHASH.to_ascii_uppercase()
        );
        assert_eq!(parse_details(&upper).unwrap().cdhash, GEN2_CDHASH);
    }

    #[test]
    fn designated_requirement_and_leaf() {
        let gen2 = parse_designated(GEN2_DESIGNATED).unwrap();
        assert_eq!(gen2, requirement(BUNDLE_ID, PINNED));
        assert_eq!(parse_leaf(&gen2).as_deref(), Some(PINNED));
        let foreign = parse_designated(FOREIGN_DESIGNATED).unwrap();
        assert_eq!(parse_leaf(&foreign).as_deref(), Some(FOREIGN_LEAF));
        assert_ne!(parse_leaf(&foreign), parse_leaf(&gen2));
        assert_eq!(
            requirement("a.b", &PINNED.to_ascii_uppercase()),
            format!("identifier \"a.b\" and certificate leaf = H\"{PINNED}\"")
        );
        assert_eq!(parse_designated("Executable=/x\n"), None);
        assert_eq!(parse_leaf("certificate leaf = H\"abc\""), None);
    }

    #[test]
    fn verify_is_classified_by_exit_code() {
        assert_eq!(classify_verify(Some(0), GEN2_VERIFY), Verify::Satisfied);
        assert_eq!(
            classify_verify(Some(3), FOREIGN_VERIFY),
            Verify::Unsatisfied
        );
        assert_eq!(
            classify_verify(Some(3), WRONG_ID_VERIFY),
            Verify::Unsatisfied
        );
        let Verify::Invalid { reason } = classify_verify(Some(1), TAMPERED_VERIFY) else {
            panic!("код 1 — нарушение целостности");
        };
        assert!(reason.contains("sealed resource"), "{reason}");
        // Решение не зависит от пути в выводе.
        let moved = TAMPERED_VERIFY.replace("/tmp/bundle", "/Volumes/other place");
        assert!(matches!(
            classify_verify(Some(1), &moved),
            Verify::Invalid { .. }
        ));
        assert!(matches!(
            classify_verify(Some(1), UNSIGNED_DETAILS),
            Verify::Invalid { reason } if reason.contains("not signed at all")
        ));
        assert!(matches!(
            classify_verify(Some(2), "codesign: invalid option"),
            Verify::Failed { code: Some(2), .. }
        ));
        assert!(matches!(
            classify_verify(None, ""),
            Verify::Failed { code: None, .. }
        ));
    }

    #[test]
    fn valid_newer_candidate_is_accepted() {
        assert_eq!(
            check_candidate(&expected(1), &gen2()),
            Ok(Accepted {
                generation: 2,
                cdhash: GEN2_CDHASH.to_owned(),
                identifier: BUNDLE_ID.to_owned(),
            })
        );
    }

    #[test]
    fn each_kind_of_bad_candidate_is_rejected() {
        let tampered = observation(
            1,
            TAMPERED_VERIFY,
            TAMPERED_DETAILS,
            GEN2_DESIGNATED,
            Some(PINNED),
            2,
        );
        assert!(matches!(
            check_candidate(&expected(1), &tampered),
            Err(Rejection::Integrity { reason }) if reason.contains("sealed resource")
        ));
        let foreign = observation(
            3,
            FOREIGN_VERIFY,
            FOREIGN_DETAILS,
            FOREIGN_DESIGNATED,
            Some(FOREIGN_LEAF),
            2,
        );
        assert_eq!(
            check_candidate(&expected(1), &foreign),
            Err(Rejection::Requirement {
                identifier: Some(BUNDLE_ID.to_owned()),
                leaf: Some(FOREIGN_LEAF.to_owned()),
                mismatch: vec![Mismatch::Signer],
            })
        );
        let wrong = observation(3, WRONG_ID_VERIFY, WRONG_ID_DETAILS, "", Some(PINNED), 2);
        assert_eq!(
            check_candidate(&expected(1), &wrong),
            Err(Rejection::Requirement {
                identifier: Some(WRONG_ID.to_owned()),
                leaf: Some(PINNED.to_owned()),
                mismatch: vec![Mismatch::Identifier],
            })
        );
        let mut unsigned = gen2();
        unsigned.verify = classify_verify(Some(1), UNSIGNED_DETAILS);
        unsigned.details = parse_details(UNSIGNED_DETAILS);
        assert_eq!(
            check_candidate(&expected(1), &unsigned).unwrap_err().kind(),
            "integrity"
        );
    }

    #[test]
    fn signature_alone_does_not_accept() {
        assert_eq!(
            check_candidate(&expected(2), &gen2()),
            Err(Rejection::NotNewer {
                installed: 2,
                candidate: 2
            })
        );
        let mut broken = gen2();
        broken.manifest = Err("generation.json: нет файла".to_owned());
        assert_eq!(
            check_candidate(&expected(1), &broken).unwrap_err().kind(),
            "manifest"
        );
        let mut version = gen2();
        version.bundle_version = Some(3);
        assert_eq!(
            check_candidate(&expected(1), &version).unwrap_err().kind(),
            "manifest"
        );
        version.bundle_version = None;
        assert_eq!(
            check_candidate(&expected(1), &version).unwrap_err().kind(),
            "manifest"
        );
        let mut unread = gen2();
        unread.details = Err("нет вывода".to_owned());
        assert_eq!(
            check_candidate(&expected(1), &unread).unwrap_err().kind(),
            "unverified"
        );
        let mut other_requirement = gen2();
        other_requirement.requirement = requirement(WRONG_ID, PINNED);
        assert_eq!(
            check_candidate(&expected(1), &other_requirement)
                .unwrap_err()
                .kind(),
            "unverified"
        );
        let mut failed = gen2();
        failed.verify = classify_verify(Some(2), "codesign: invalid option");
        assert_eq!(
            check_candidate(&expected(1), &failed).unwrap_err().kind(),
            "unverified"
        );
    }

    #[test]
    fn shasum_lines_are_parsed() {
        assert_eq!(
            parse_digest(&format!("{PINNED}  /tmp/x/cert0\n"), 40).as_deref(),
            Some(PINNED)
        );
        let sha256 = "428786cce8d7a4c2eb4c8fff99dfe43f7843566ffb9bfd3f58ac58320512ba6f";
        assert_eq!(
            parse_digest(&format!("{sha256}  -"), 64).as_deref(),
            Some(sha256)
        );
        assert_eq!(parse_digest(&format!("{sha256}  -"), 40), None);
        assert_eq!(parse_digest("", 40), None);
    }

    pub(crate) fn layout_json(names: &[&str]) -> String {
        let bundles: Vec<String> = names
            .iter()
            .map(|name| {
                format!(
                    "{{\"name\": \"{name}\", \"generation\": 2, \"app\": \"/tmp/bundle/{name}/probe-update.app\", \"identifier\": \"{BUNDLE_ID}\", \"signer_leaf\": \"{PINNED}\", \"cdhash\": \"{GEN2_CDHASH}\"}}"
                )
            })
            .collect();
        format!(
            "{{\"bundle_id\": \"{BUNDLE_ID}\", \"leaf\": \"{PINNED}\", \"bundles\": [{}]}}",
            bundles.join(", ")
        )
    }

    #[test]
    fn layout_lists_every_bundle_once() {
        let layout = Layout::parse(&layout_json(&BUNDLES)).unwrap();
        assert_eq!(layout.get("foreign").unwrap().generation, 2);
        assert!(layout.get("gen-9").is_err());
        assert_eq!(layout.cdhashes().len(), BUNDLES.len());
        assert!(
            Layout::parse(&layout_json(&BUNDLES[1..]))
                .unwrap_err()
                .contains("gen-1")
        );
        let mut twice = BUNDLES.to_vec();
        twice.push("gen-2");
        assert!(
            Layout::parse(&layout_json(&twice))
                .unwrap_err()
                .contains("gen-2")
        );
        let mut unknown = BUNDLES.to_vec();
        unknown.push("other");
        assert!(
            Layout::parse(&layout_json(&unknown))
                .unwrap_err()
                .contains("other")
        );
        let upper = layout_json(&BUNDLES).replacen(PINNED, &PINNED.to_ascii_uppercase(), 1);
        assert!(Layout::parse(&upper).is_err());
        let relative = layout_json(&BUNDLES).replacen("/tmp/bundle/gen-1", "gen-1", 1);
        assert!(
            Layout::parse(&relative)
                .unwrap_err()
                .contains("относительный")
        );
        let other_id = layout_json(&BUNDLES).replacen(BUNDLE_ID, DENIED_ID, 1);
        assert!(Layout::parse(&other_id).unwrap_err().contains("bundle_id"));
    }

    pub(crate) fn foreign_identity() -> ForeignIdentity {
        ForeignIdentity {
            leaf: FOREIGN_LEAF.to_owned(),
            requirement_sha256_before: "a".repeat(64),
            requirement_sha256_during: "a".repeat(64),
            requirement_sha256_after: "a".repeat(64),
            search_list_before: vec!["/k/login.keychain-db".to_owned()],
            search_list_during: vec!["/k/login.keychain-db".to_owned()],
            search_list_after: vec!["/k/login.keychain-db".to_owned()],
            default_keychain_before: "/k/login.keychain-db".to_owned(),
            default_keychain_during: "/k/login.keychain-db".to_owned(),
            default_keychain_after: "/k/login.keychain-db".to_owned(),
        }
    }

    #[test]
    fn foreign_identity_reports_changes() {
        let same = foreign_identity();
        assert!(same.changes().is_empty());
        let text = serde_json::to_string(&same).unwrap();
        assert_eq!(ForeignIdentity::parse(&text), Ok(same.clone()));
        let mut changed = same.clone();
        changed.requirement_sha256_after = "b".repeat(64);
        changed
            .search_list_after
            .push("/t/foreign.keychain-db".to_owned());
        changed.default_keychain_after = "/t/foreign.keychain-db".to_owned();
        assert_eq!(changed.changes().len(), 3);
        // Keychain в списке поиска только на время подписи: delete-keychain убрал бы её и из
        // списка, сверка до и после её не увидела бы.
        let mut during = same;
        during
            .search_list_during
            .push("/t/foreign.keychain-db".to_owned());
        assert_eq!(
            during.changes(),
            ["список поиска keychain во время подписи"]
        );
    }

    #[test]
    fn stub_arguments() {
        assert_eq!(
            parse_stub_args(APP_BIN, &["--report".to_owned()]),
            Ok(StubMode::Report)
        );
        assert_eq!(
            parse_stub_args(APP_BIN, &["--manifest".to_owned()]),
            Ok(StubMode::Manifest)
        );
        assert_eq!(
            parse_stub_args(
                HELPER_BIN,
                &["--tag".to_owned(), "n1".to_owned(), "--report".to_owned()]
            ),
            Ok(StubMode::Report)
        );
        assert!(parse_stub_args(HELPER_BIN, &["--manifest".to_owned()]).is_err());
        assert!(parse_stub_args(APP_BIN, &[]).is_err());
        assert!(parse_stub_args(APP_BIN, &["--tag".to_owned()]).is_err());
        assert!(
            parse_stub_args(APP_BIN, &["--report".to_owned(), "--manifest".to_owned()]).is_err()
        );
    }

    #[test]
    fn tree_files_are_listed_relative() {
        let root = std::env::temp_dir().join(format!(
            "probe-update-generation-{}-{}",
            std::process::id(),
            crate::sys::uptime_ns()
        ));
        fs::create_dir_all(root.join("Contents/MacOS")).unwrap();
        fs::write(root.join("Contents/Info.plist"), "p").unwrap();
        fs::write(root.join("Contents/MacOS/a"), "a").unwrap();
        assert_eq!(
            files(&root).unwrap(),
            ["Contents/Info.plist", "Contents/MacOS/a"]
        );
        fs::remove_dir_all(&root).unwrap();
        assert!(is_relative_inside("Contents/MacOS/a"));
        assert!(!is_relative_inside("../a"));
        assert!(!is_relative_inside("/a"));
        assert!(!is_relative_inside(""));
    }
}
