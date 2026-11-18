//! Сценарий `reject-verify` (задача 4): ранняя часть обязательной клетки A2 — отказ проверки
//! кандидата каждым видом до `intent`. Установленный N — копия поколения 1 в
//! `<state>/install/`. Части:
//!
//! 1. `accept-control` — кандидаты поколения 2 и `incompatible` приняты: без принятого контроля
//!    отказы ниже ничего не доказывают. Совместимость `incompatible` решает задача 7;
//! 2. `tampered` — отказ по целостности, код 1, причина называет запечатанный ресурс;
//! 3. `foreign` — отказ по требованию, код 3, тот же идентификатор и CN, другой лист; окружение
//!    подписи вокруг временной идентичности не изменилось;
//! 4. `wrong-id` — отказ по требованию, код 3, закреплённый лист, другой идентификатор;
//! 5. `installed-untouched` — установленный N после всех проверок прежний, `<state>/update/` нет.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::Serialize;

use crate::generation::{
    self, Accepted, BUNDLE_ID, BundleEntry, Expected, FOREIGN_FILE, ForeignIdentity, LAYOUT_FILE,
    Mismatch, Observation, Rejection, Run, SIGNER_CN, Verify, WRONG_ID,
};
use crate::launch;
use crate::results::{self, Diagnostics, Environment, Report};

use super::{PartSpec, Verdict, bundles, required_cell};

/// Срок процесса сценария.
pub const DEADLINE: Duration = Duration::from_secs(120);
pub const CELL: &str = "A2";
pub const DIR: &str = "reject-verify";
pub const EVIDENCE: &str = "reject-verify/evidence.json";
pub const LAYOUT_ARTIFACT: &str = "reject-verify/bundles.json";
pub const FOREIGN_ARTIFACT: &str = "reject-verify/foreign-identity.json";
/// Каталог установленного N внутри `SHELLIO_STATE_DIR`.
pub const INSTALL_DIR: &str = "install";
/// Каталог журнала обновления внутри `SHELLIO_STATE_DIR` (раздел плана «Протокол замены»).
pub const UPDATE_DIR: &str = "update";
/// Кандидаты по порядку проверки.
pub const CANDIDATES: [&str; 5] = ["gen-2", "incompatible", "tampered", "foreign", "wrong-id"];
const INSTALLED_GENERATION: u32 = 1;
const COMPLETION: &str =
    "все вызовы codesign, shasum и plutil завершились; иначе ошибка исполнения";

pub const CONTROL: PartSpec = PartSpec {
    name: "accept-control",
    stimulus: "проверка кандидатов поколения 2 и incompatible при установленном поколении 1",
    oracle: "оба приняты: код 0, поколения 2 и 3, CDHash равен измеренному при сборке бандла",
};
pub const TAMPERED: PartSpec = PartSpec {
    name: "tampered",
    stimulus: "проверка кандидата tampered",
    oracle: "отказ по целостности: код 1, причина codesign называет запечатанный ресурс",
};
pub const FOREIGN: PartSpec = PartSpec {
    name: "foreign",
    stimulus: "проверка кандидата foreign",
    oracle: "отказ по требованию: код 3, идентификатор прежний, первый Authority — Shellio Dev, извлечённый лист не закреплённый; requirement.txt, список поиска и keychain по умолчанию не изменились",
};
pub const WRONG: PartSpec = PartSpec {
    name: "wrong-id",
    stimulus: "проверка кандидата wrong-id",
    oracle: "отказ по требованию: код 3, лист закреплённый, идентификатор <id>.wrong-id",
};
pub const INSTALLED: PartSpec = PartSpec {
    name: "installed-untouched",
    stimulus: "установленный N после всех проверок",
    oracle: "проходит проверку, CDHash и SHA-256 всех файлов прежние, каталога <state>/update/ нет",
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CandidateFacts {
    pub name: String,
    pub app: String,
    /// CDHash бандла, измеренный `run.sh` при сборке.
    pub built_cdhash: String,
    pub observation: Observation,
    pub verdict: Result<Accepted, Rejection>,
}

impl CandidateFacts {
    fn cdhash(&self) -> Option<&str> {
        self.observation
            .details
            .as_ref()
            .ok()
            .map(|details| details.cdhash.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct InstalledFacts {
    pub app: String,
    pub observation: Observation,
    pub files: BTreeMap<String, String>,
}

impl InstalledFacts {
    fn cdhash(&self) -> Option<&str> {
        self.observation
            .details
            .as_ref()
            .ok()
            .map(|details| details.cdhash.as_str())
    }
}

#[derive(Debug, Serialize)]
struct Evidence<'a> {
    leaf: &'a str,
    expected_requirement: String,
    installed_before: Option<&'a InstalledFacts>,
    candidates: &'a [CandidateFacts],
    installed_after: Option<&'a InstalledFacts>,
    update_dir_exists: Option<bool>,
    runs: &'a [Run],
}

/// Убирает установленную копию из каталога состояния при любом исходе.
struct Guard(PathBuf);

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn find<'a>(candidates: &'a [CandidateFacts], name: &str) -> Option<&'a CandidateFacts> {
    candidates.iter().find(|facts| facts.name == name)
}

pub fn judge_control(candidates: &[CandidateFacts]) -> Verdict {
    let mut problems = Vec::new();
    let mut facts = Vec::new();
    for (name, generation) in [("gen-2", 2), ("incompatible", 3)] {
        let Some(candidate) = find(candidates, name) else {
            problems.push(format!("нет кандидата {name}"));
            continue;
        };
        match &candidate.verdict {
            Ok(accepted)
                if accepted.generation == generation
                    && accepted.cdhash == candidate.built_cdhash
                    && accepted.identifier == BUNDLE_ID =>
            {
                facts.push(format!(
                    "{name} принят: поколение {generation}, CDHash {}",
                    accepted.cdhash
                ));
            }
            Ok(accepted) => problems.push(format!(
                "{name} принят не тем: {accepted:?}, ожидается поколение {generation} и CDHash {}",
                candidate.built_cdhash
            )),
            Err(rejection) => problems.push(format!("{name} отвергнут: {rejection:?}")),
        }
    }
    Verdict::of(problems, facts.join("; "))
}

pub fn judge_tampered(candidates: &[CandidateFacts], tampered: Option<&BundleEntry>) -> Verdict {
    let Some(candidate) = find(candidates, "tampered") else {
        return Verdict::of(vec!["нет кандидата tampered".to_owned()], String::new());
    };
    let mut problems = Vec::new();
    let reason = match &candidate.verdict {
        Err(Rejection::Integrity { reason }) => reason.clone(),
        other => {
            problems.push(format!("ожидался отказ по целостности, итог {other:?}"));
            String::new()
        }
    };
    if !matches!(candidate.observation.verify, Verify::Invalid { .. }) {
        problems.push(format!(
            "codesign --verify: {:?}, ожидается код 1",
            candidate.observation.verify
        ));
    }
    if !reason.contains("sealed resource") {
        problems.push(format!(
            "причина «{reason}» не называет запечатанный ресурс"
        ));
    }
    let gen2 = find(candidates, "gen-2").and_then(CandidateFacts::cdhash);
    let same = candidate.cdhash().is_some() && candidate.cdhash() == gen2;
    let file = tampered
        .and_then(|entry| entry.tampered.as_ref())
        .map(|info| {
            format!(
                "{}: SHA-256 {} → {}",
                info.file, info.sha256_before, info.sha256_after
            )
        })
        .unwrap_or_else(|| "сведений о подмене в bundles.json нет".to_owned());
    Verdict::of(
        problems,
        format!(
            "отказ по целостности, код 1: «{reason}»; {file}; CDHash кандидата {:?} {} CDHash поколения 2 — CDHash без проверки целостности поколение не определяет",
            candidate.cdhash(),
            if same {
                "равен"
            } else {
                "не равен"
            }
        ),
    )
}

pub fn judge_foreign(
    candidates: &[CandidateFacts],
    leaf: &str,
    identity: &ForeignIdentity,
) -> Verdict {
    let Some(candidate) = find(candidates, "foreign") else {
        return Verdict::of(vec!["нет кандидата foreign".to_owned()], String::new());
    };
    let mut problems = Vec::new();
    match &candidate.verdict {
        Err(Rejection::Requirement { mismatch, .. }) if mismatch == &[Mismatch::Signer] => {}
        other => problems.push(format!(
            "ожидался отказ по требованию с другим листом, итог {other:?}"
        )),
    }
    if candidate.observation.verify != Verify::Unsatisfied {
        problems.push(format!(
            "codesign --verify: {:?}, ожидается код 3",
            candidate.observation.verify
        ));
    }
    let details = candidate.observation.details.as_ref().ok();
    if details.map(|details| details.identifier.as_str()) != Some(BUNDLE_ID) {
        problems.push(format!(
            "идентификатор {:?}, ожидается {BUNDLE_ID}",
            details.map(|details| &details.identifier)
        ));
    }
    if details.and_then(|details| details.authorities.first().map(String::as_str))
        != Some(SIGNER_CN)
    {
        problems.push(format!(
            "Authority {:?}, ожидается тот же CN {SIGNER_CN}",
            details.map(|details| &details.authorities)
        ));
    }
    let observed = candidate.observation.leaf.as_deref();
    if observed == Some(leaf) || observed != Some(identity.leaf.as_str()) {
        problems.push(format!(
            "извлечённый лист {observed:?}, ожидается лист временной идентичности {}",
            identity.leaf
        ));
    }
    let changes = identity.changes();
    if !changes.is_empty() {
        problems.push(format!(
            "временная идентичность изменила: {}",
            changes.join(", ")
        ));
    }
    Verdict::of(
        problems,
        format!(
            "отказ по требованию, код 3: идентификатор {BUNDLE_ID}, Authority {SIGNER_CN}, лист {} вместо {leaf}; requirement.txt, список поиска ({} keychain) и keychain по умолчанию прежние во время подписи и после удаления временной keychain",
            identity.leaf,
            identity.search_list_after.len()
        ),
    )
}

pub fn judge_wrong_id(candidates: &[CandidateFacts], leaf: &str) -> Verdict {
    let Some(candidate) = find(candidates, "wrong-id") else {
        return Verdict::of(vec!["нет кандидата wrong-id".to_owned()], String::new());
    };
    let mut problems = Vec::new();
    match &candidate.verdict {
        Err(Rejection::Requirement { mismatch, .. }) if mismatch == &[Mismatch::Identifier] => {}
        other => problems.push(format!(
            "ожидался отказ по требованию с другим идентификатором, итог {other:?}"
        )),
    }
    if candidate.observation.verify != Verify::Unsatisfied {
        problems.push(format!(
            "codesign --verify: {:?}, ожидается код 3",
            candidate.observation.verify
        ));
    }
    let identifier = candidate
        .observation
        .details
        .as_ref()
        .ok()
        .map(|details| details.identifier.as_str());
    if identifier != Some(WRONG_ID) {
        problems.push(format!(
            "идентификатор {identifier:?}, ожидается {WRONG_ID}"
        ));
    }
    if candidate.observation.leaf.as_deref() != Some(leaf) {
        problems.push(format!(
            "извлечённый лист {:?}, ожидается закреплённый {leaf}",
            candidate.observation.leaf
        ));
    }
    Verdict::of(
        problems,
        format!("отказ по требованию, код 3: лист {leaf} закреплённый, идентификатор {WRONG_ID}"),
    )
}

pub fn judge_installed(
    before: &InstalledFacts,
    after: &InstalledFacts,
    update_dir_exists: bool,
) -> Verdict {
    let mut problems = Vec::new();
    for (when, facts) in [("до", before), ("после", after)] {
        if facts.observation.verify != Verify::Satisfied {
            problems.push(format!(
                "установленный N {when} проверок: {:?}",
                facts.observation.verify
            ));
        }
    }
    if before.cdhash().is_none() || before.cdhash() != after.cdhash() {
        problems.push(format!(
            "CDHash установленного N {:?} → {:?}",
            before.cdhash(),
            after.cdhash()
        ));
    }
    if before.files != after.files {
        let changed: Vec<&String> = before
            .files
            .keys()
            .chain(after.files.keys())
            .filter(|name| before.files.get(*name) != after.files.get(*name))
            .collect();
        problems.push(format!("файлы установленного N изменились: {changed:?}"));
    }
    if update_dir_exists {
        problems.push(format!("появился каталог <state>/{UPDATE_DIR}/"));
    }
    Verdict::of(
        problems,
        format!(
            "установленный N проходит проверку до и после, CDHash {:?}, SHA-256 {} файлов прежние, <state>/{UPDATE_DIR}/ нет",
            after.cdhash(),
            after.files.len()
        ),
    )
}

fn installed(
    app: &Path,
    requirement: &str,
    prefix: &Path,
    runs: &mut Vec<Run>,
) -> Result<InstalledFacts, String> {
    Ok(InstalledFacts {
        app: app.display().to_string(),
        observation: generation::observe(app, requirement, prefix, runs)?,
        files: generation::digest_tree(app, runs)?,
    })
}

fn install(source: &Path, dest: &Path) -> Result<(), String> {
    let parent = dest
        .parent()
        .ok_or_else(|| format!("у {} нет каталога", dest.display()))?;
    fs::create_dir_all(parent).map_err(|err| format!("{}: {err}", parent.display()))?;
    let status = Command::new("/usr/bin/ditto")
        .args(["--norsrc", "--noextattr", "--noacl"])
        .arg(source)
        .arg(dest)
        .status()
        .map_err(|err| format!("ditto: {err}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "ditto {} {}: {status}",
            source.display(),
            dest.display()
        ))
    }
}

pub fn run(out: &Path, environment: &Environment) -> Result<Report, String> {
    let state_dir = environment
        .process
        .state_dir
        .as_deref()
        .map(PathBuf::from)
        .ok_or_else(|| format!("предусловие: нет {}", launch::STATE_DIR))?;
    let (bundle_dir, layout) = bundles(environment)?;
    let dir = out.join(DIR);
    let certs = dir.join("certs");
    fs::create_dir_all(&certs).map_err(|err| format!("{}: {err}", certs.display()))?;
    fs::copy(bundle_dir.join(LAYOUT_FILE), out.join(LAYOUT_ARTIFACT))
        .map_err(|err| format!("копия {LAYOUT_FILE}: {err}"))?;
    fs::copy(bundle_dir.join(FOREIGN_FILE), out.join(FOREIGN_ARTIFACT))
        .map_err(|err| format!("копия {FOREIGN_FILE}: {err}"))?;
    let identity = fs::read_to_string(out.join(FOREIGN_ARTIFACT))
        .map_err(|err| format!("{FOREIGN_FILE}: {err}"))
        .and_then(|text| ForeignIdentity::parse(&text))
        .map_err(|err| format!("предусловие: {err}"))?;
    let leaf = layout.leaf.as_str();
    let expected = Expected {
        identifier: BUNDLE_ID,
        leaf,
        installed: INSTALLED_GENERATION,
    };
    let requirement = expected.requirement();
    let install_root = state_dir.join(INSTALL_DIR);
    let installed_app = install_root.join(generation::APP_NAME);
    let guard = Guard(install_root.clone());
    let mut runs = Vec::new();
    let mut before = None;
    let mut after = None;
    let mut candidates = Vec::new();
    let mut update_dir_exists = None;
    let walked = (|| -> Result<(), String> {
        install(&layout.get("gen-1")?.app, &installed_app)?;
        let facts = installed(
            &installed_app,
            &requirement,
            &certs.join("installed-before-"),
            &mut runs,
        )?;
        match &facts.observation.manifest {
            Ok(manifest) if manifest.generation == INSTALLED_GENERATION => {}
            other => {
                return Err(format!(
                    "стенд: установленный N — не поколение {INSTALLED_GENERATION}: {other:?}"
                ));
            }
        }
        before = Some(facts);
        for name in CANDIDATES {
            let entry = layout.get(name)?;
            let observation = generation::observe(
                &entry.app,
                &requirement,
                &certs.join(format!("{name}-")),
                &mut runs,
            )?;
            let verdict = generation::check_candidate(&expected, &observation);
            candidates.push(CandidateFacts {
                name: name.to_owned(),
                app: entry.app.display().to_string(),
                built_cdhash: entry.cdhash.clone(),
                observation,
                verdict,
            });
        }
        after = Some(installed(
            &installed_app,
            &requirement,
            &certs.join("installed-after-"),
            &mut runs,
        )?);
        update_dir_exists = Some(state_dir.join(UPDATE_DIR).exists());
        Ok(())
    })();
    drop(guard);
    let written = results::write_json(
        &dir,
        "evidence.json",
        &Evidence {
            leaf,
            expected_requirement: requirement.clone(),
            installed_before: before.as_ref(),
            candidates: &candidates,
            installed_after: after.as_ref(),
            update_dir_exists,
            runs: &runs,
        },
    );
    walked?;
    written?;
    let (Some(before), Some(after), Some(update_dir_exists)) = (before, after, update_dir_exists)
    else {
        return Err("сценарий не собрал факты установленного N".to_owned());
    };
    let artifacts = [EVIDENCE, LAYOUT_ARTIFACT, FOREIGN_ARTIFACT];
    let cells = [
        (CONTROL, judge_control(&candidates)),
        (
            TAMPERED,
            judge_tampered(&candidates, layout.get("tampered").ok()),
        ),
        (FOREIGN, judge_foreign(&candidates, leaf, &identity)),
        (WRONG, judge_wrong_id(&candidates, leaf)),
        (
            INSTALLED,
            judge_installed(&before, &after, update_dir_exists),
        ),
    ]
    .into_iter()
    .map(|(spec, verdict)| required_cell(CELL, &spec, COMPLETION, verdict, &artifacts))
    .collect();
    Ok(Report {
        cells,
        diagnostics: Diagnostics {
            notes: vec![format!(
                "требование кандидата: {requirement}; установленный N — копия поколения 1 в <state>/{INSTALL_DIR}/"
            )],
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generation::tests::saved::*;
    use crate::generation::tests::{expected, foreign_identity, observation};
    use crate::generation::{Details, Tampered, check_candidate};
    use crate::results::Status;

    fn candidate(name: &str, observation: Observation, built: &str) -> CandidateFacts {
        CandidateFacts {
            name: name.to_owned(),
            app: format!("/tmp/bundle/{name}/probe-update.app"),
            built_cdhash: built.to_owned(),
            verdict: check_candidate(&expected(INSTALLED_GENERATION), &observation),
            observation,
        }
    }

    /// Кандидаты прогона задачи 4 из сохранённых выводов.
    fn candidates() -> Vec<CandidateFacts> {
        vec![
            candidate(
                "gen-2",
                observation(
                    0,
                    GEN2_VERIFY,
                    GEN2_DETAILS,
                    GEN2_DESIGNATED,
                    Some(PINNED),
                    2,
                ),
                GEN2_CDHASH,
            ),
            candidate(
                "incompatible",
                observation(
                    0,
                    GEN2_VERIFY,
                    GEN2_DETAILS,
                    GEN2_DESIGNATED,
                    Some(PINNED),
                    3,
                ),
                GEN2_CDHASH,
            ),
            candidate(
                "tampered",
                observation(
                    1,
                    TAMPERED_VERIFY,
                    TAMPERED_DETAILS,
                    GEN2_DESIGNATED,
                    Some(PINNED),
                    2,
                ),
                GEN2_CDHASH,
            ),
            candidate(
                "foreign",
                observation(
                    3,
                    FOREIGN_VERIFY,
                    FOREIGN_DETAILS,
                    FOREIGN_DESIGNATED,
                    Some(FOREIGN_LEAF),
                    2,
                ),
                GEN2_CDHASH,
            ),
            candidate(
                "wrong-id",
                observation(3, WRONG_ID_VERIFY, WRONG_ID_DETAILS, "", Some(PINNED), 2),
                GEN2_CDHASH,
            ),
        ]
    }

    fn tampered_entry() -> BundleEntry {
        BundleEntry {
            name: "tampered".to_owned(),
            generation: 2,
            app: PathBuf::from("/tmp/bundle/tampered/probe-update.app"),
            identifier: BUNDLE_ID.to_owned(),
            signer_leaf: PINNED.to_owned(),
            cdhash: GEN2_CDHASH.to_owned(),
            tampered: Some(Tampered {
                file: generation::MANIFEST_PATH.to_owned(),
                sha256_before: "a".repeat(64),
                sha256_after: "b".repeat(64),
            }),
        }
    }

    fn installed_facts() -> InstalledFacts {
        let mut observation = observation(
            0,
            GEN2_VERIFY,
            GEN2_DETAILS,
            GEN2_DESIGNATED,
            Some(PINNED),
            1,
        );
        if let Ok(details) = &mut observation.details {
            *details = Details {
                cdhash: "c".repeat(40),
                ..details.clone()
            };
        }
        InstalledFacts {
            app: "/tmp/state/install/probe-update.app".to_owned(),
            observation,
            files: BTreeMap::from([
                ("Contents/Info.plist".to_owned(), "1".repeat(64)),
                ("Contents/MacOS/probe-update-app".to_owned(), "2".repeat(64)),
            ]),
        }
    }

    fn find_mut<'a>(candidates: &'a mut [CandidateFacts], name: &str) -> &'a mut CandidateFacts {
        candidates
            .iter_mut()
            .find(|facts| facts.name == name)
            .unwrap()
    }

    #[test]
    fn saved_run_passes_every_part() {
        let facts = candidates();
        let installed = installed_facts();
        for verdict in [
            judge_control(&facts),
            judge_tampered(&facts, Some(&tampered_entry())),
            judge_foreign(&facts, PINNED, &foreign_identity()),
            judge_wrong_id(&facts, PINNED),
            judge_installed(&installed, &installed.clone(), false),
        ] {
            assert_eq!(verdict.status, Status::Pass, "{}", verdict.reason);
        }
        let tampered = judge_tampered(&facts, Some(&tampered_entry()));
        assert!(tampered.reason.contains("равен"), "{}", tampered.reason);
    }

    #[test]
    fn rejected_or_misread_control_fails() {
        let mut facts = candidates();
        find_mut(&mut facts, "gen-2").built_cdhash = "d".repeat(40);
        assert_eq!(judge_control(&facts).status, Status::Fail);
        let mut facts = candidates();
        let tampered = find_mut(&mut facts, "tampered").clone();
        let control = find_mut(&mut facts, "incompatible");
        control.verdict = tampered.verdict;
        assert_eq!(judge_control(&facts).status, Status::Fail);
        assert_eq!(judge_control(&facts[2..]).status, Status::Fail);
    }

    #[test]
    fn accepted_bad_candidates_fail_their_parts() {
        let mut facts = candidates();
        let accepted = find_mut(&mut facts, "gen-2").verdict.clone();
        for name in ["tampered", "foreign", "wrong-id"] {
            find_mut(&mut facts, name).verdict = accepted.clone();
        }
        assert_eq!(
            judge_tampered(&facts, Some(&tampered_entry())).status,
            Status::Fail
        );
        assert_eq!(
            judge_foreign(&facts, PINNED, &foreign_identity()).status,
            Status::Fail
        );
        assert_eq!(judge_wrong_id(&facts, PINNED).status, Status::Fail);
    }

    #[test]
    fn wrong_reason_of_rejection_fails() {
        let facts = candidates();
        // Отказ foreign как wrong-id и наоборот: вид отказа обязан совпасть.
        let mut swapped = facts.clone();
        let foreign = find_mut(&mut swapped, "foreign").verdict.clone();
        let wrong = find_mut(&mut swapped, "wrong-id").verdict.clone();
        find_mut(&mut swapped, "foreign").verdict = wrong;
        find_mut(&mut swapped, "wrong-id").verdict = foreign;
        assert_eq!(
            judge_foreign(&swapped, PINNED, &foreign_identity()).status,
            Status::Fail
        );
        assert_eq!(judge_wrong_id(&swapped, PINNED).status, Status::Fail);
        // Лист foreign, равный закреплённому, — не та идентичность.
        let mut same_leaf = facts.clone();
        find_mut(&mut same_leaf, "foreign").observation.leaf = Some(PINNED.to_owned());
        assert_eq!(
            judge_foreign(&same_leaf, PINNED, &foreign_identity()).status,
            Status::Fail
        );
        // Изменённое окружение подписи роняет часть foreign.
        let mut changed = foreign_identity();
        changed
            .search_list_during
            .push("/t/foreign.keychain-db".to_owned());
        assert_eq!(judge_foreign(&facts, PINNED, &changed).status, Status::Fail);
        // Код 3 без причины целостности у tampered — не та проверка.
        let mut unsatisfied = facts;
        find_mut(&mut unsatisfied, "tampered").observation.verify = Verify::Unsatisfied;
        assert_eq!(
            judge_tampered(&unsatisfied, Some(&tampered_entry())).status,
            Status::Fail
        );
    }

    #[test]
    fn touched_installation_fails() {
        let before = installed_facts();
        let mut files = before.clone();
        files
            .files
            .insert("Contents/MacOS/probe-update-app".to_owned(), "3".repeat(64));
        assert_eq!(judge_installed(&before, &files, false).status, Status::Fail);
        let mut cdhash = before.clone();
        if let Ok(details) = &mut cdhash.observation.details {
            details.cdhash = "e".repeat(40);
        }
        assert_eq!(
            judge_installed(&before, &cdhash, false).status,
            Status::Fail
        );
        let mut broken = before.clone();
        broken.observation.verify = Verify::Invalid {
            reason: "a sealed resource is missing or invalid".to_owned(),
        };
        assert_eq!(
            judge_installed(&before, &broken, false).status,
            Status::Fail
        );
        assert_eq!(
            judge_installed(&before, &before.clone(), true).status,
            Status::Fail
        );
    }

    #[test]
    fn parts_become_valid_required_cells() {
        let facts = candidates();
        let installed = installed_facts();
        for (spec, verdict) in [
            (CONTROL, judge_control(&facts)),
            (TAMPERED, judge_tampered(&facts, None)),
            (FOREIGN, judge_foreign(&facts, PINNED, &foreign_identity())),
            (WRONG, judge_wrong_id(&facts, PINNED)),
            (
                INSTALLED,
                judge_installed(&installed, &installed.clone(), false),
            ),
        ] {
            let cell = required_cell(
                CELL,
                &spec,
                COMPLETION,
                verdict,
                &[EVIDENCE, LAYOUT_ARTIFACT, FOREIGN_ARTIFACT],
            );
            assert!(cell.problems().is_empty(), "{:?}", cell.problems());
        }
    }
}
