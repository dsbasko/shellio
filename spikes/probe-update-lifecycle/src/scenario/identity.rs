//! Сценарий `identity` (задача 4): статическая часть обязательной клетки A1 — подписи и CDHash
//! поколений 1 и 2 на диске. Части:
//!
//! 1. `signatures` — бандл и каждый из пяти бинарей обоих поколений проходят
//!    `codesign --verify --strict --deep -R` со своим требованием, подпись не ad-hoc, первый
//!    `Authority` — `Shellio Dev`, SHA-1 извлечённого листа равен закреплённому;
//! 2. `distinct-code` — SHA-256 и CDHash `probe-update-app` и `probe-update-host` и CDHash бандлов
//!    различны; совпадение остальных бинарей записывается;
//! 3. `stable-requirement` — идентификатор и designated requirement каждой части одинаковы в
//!    обоих поколениях и равны ожидаемым;
//! 4. `generation-reports` — номер поколения и диапазоны протокола одинаковы у манифеста,
//!    `CFBundleVersion` и обеих заглушек, запущенных из бандла.
//!
//! `--deep -R` применяет требование только к внешнему бандлу, вложенный код он проверяет на
//! целостность, поэтому каждая часть сверяется отдельно со своим требованием. Перед частями
//! стенд проверяет кандидата `incompatible` и бандл `.denied` под их требованиями: сломанный
//! стенд — ошибка исполнения, а не `fail` A1.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;

use crate::generation::{
    self, BUNDLE_ID, DENIED_ID, Details, LAYOUT_FILE, Manifest, Part, Run, SIGNER_CN, StubReport,
    Verify,
};
use crate::proto::Versions;
use crate::results::{self, Diagnostics, Environment, Report};

use super::{PartSpec, Verdict, bundles, required_cell};

/// Срок процесса сценария.
pub const DEADLINE: Duration = Duration::from_secs(120);
pub const CELL: &str = "A1";
pub const DIR: &str = "identity";
pub const EVIDENCE: &str = "identity/evidence.json";
pub const LAYOUT_ARTIFACT: &str = "identity/bundles.json";
const COMPLETION: &str =
    "все вызовы codesign, shasum, plutil и заглушек завершились; иначе ошибка исполнения";

/// Ожидаемое поколение (раздел плана «Поколения и кандидаты»).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Generation {
    pub name: &'static str,
    pub generation: u32,
    pub host: Versions,
    pub attach: Versions,
}

pub const GENERATIONS: [Generation; 2] = [
    Generation {
        name: "gen-1",
        generation: 1,
        host: Versions::new(1, 1),
        attach: Versions::new(1, 1),
    },
    Generation {
        name: "gen-2",
        generation: 2,
        host: Versions::new(1, 2),
        attach: Versions::new(1, 2),
    },
];

pub const SIGNATURES: PartSpec = PartSpec {
    name: "signatures",
    stimulus: "поколения 1 и 2: бандл и каждый из пяти бинарей",
    oracle: "codesign --verify --strict --deep -R со своим требованием — код 0; подпись не ad-hoc; первый Authority — Shellio Dev; SHA-1 извлечённого листа равен закреплённому",
};
pub const DISTINCT: PartSpec = PartSpec {
    name: "distinct-code",
    stimulus: "сравнение поколений 1 и 2",
    oracle: "SHA-256 и CDHash probe-update-app и probe-update-host различны, CDHash бандлов различны",
};
pub const STABLE: PartSpec = PartSpec {
    name: "stable-requirement",
    stimulus: "сравнение поколений 1 и 2",
    oracle: "идентификатор и designated requirement каждой части одинаковы в обоих поколениях и равны ожидаемым",
};
pub const REPORTS: PartSpec = PartSpec {
    name: "generation-reports",
    stimulus: "манифест, CFBundleVersion, probe-update-app --report и probe-update-helper --report из бандла",
    oracle: "номер поколения одинаков у манифеста, CFBundleVersion и обеих заглушек; диапазоны манифеста и заглушек равны ожидаемым; исполняемый путь заглушки внутри бандла",
};

/// Подписанный код: бандл или его часть.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Signed {
    pub path: String,
    pub identifier: String,
    pub requirement: String,
    pub verify: Verify,
    pub details: Result<Details, String>,
    pub designated: Option<String>,
    pub leaf: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PartFacts {
    pub part: Part,
    pub signed: Signed,
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StubFacts {
    pub part: Part,
    pub report: Result<StubReport, String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct BundleFacts {
    pub name: String,
    pub app: String,
    pub bundle: Signed,
    pub parts: Vec<PartFacts>,
    pub manifest: Result<Manifest, String>,
    pub bundle_version: Option<u32>,
    pub reports: Vec<StubFacts>,
}

impl BundleFacts {
    fn part(&self, part: Part) -> Option<&PartFacts> {
        self.parts.iter().find(|facts| facts.part == part)
    }
}

#[derive(Debug, Serialize)]
struct Evidence<'a> {
    leaf: &'a str,
    generations: &'a [BundleFacts],
    stand: &'a [(String, Signed, Result<Manifest, String>)],
    runs: &'a [Run],
}

fn signed(
    path: &Path,
    identifier: &str,
    leaf: &str,
    prefix: &Path,
    runs: &mut Vec<Run>,
) -> Result<Signed, String> {
    let requirement = generation::requirement(identifier, leaf);
    Ok(Signed {
        path: path.display().to_string(),
        identifier: identifier.to_owned(),
        verify: generation::verify(path, &requirement, runs)?,
        details: generation::details(path, runs)?,
        designated: generation::designated(path, runs)?,
        leaf: generation::leaf(path, prefix, runs)?,
        requirement,
    })
}

fn stub(app: &Path, part: Part, tag: &str, runs: &mut Vec<Run>) -> Result<StubFacts, String> {
    let call = generation::run(
        &part.path(app).display().to_string(),
        &["--report".to_owned(), "--tag".to_owned(), tag.to_owned()],
    )?;
    let report = if call.code == Some(0) {
        serde_json::from_str::<StubReport>(call.stdout.trim())
            .map_err(|err| format!("отчёт {}: {err}", part.file_name()))
    } else {
        Err(format!(
            "{} --report: код {:?}: {}",
            part.file_name(),
            call.code,
            call.stderr.trim()
        ))
    };
    runs.push(call);
    Ok(StubFacts { part, report })
}

/// Факты бандла поколения: подпись бандла и частей, SHA-256 частей, манифест, `CFBundleVersion`
/// и отчёты заглушек.
fn collect(
    name: &str,
    app: &Path,
    leaf: &str,
    certs: &Path,
    tag: &str,
    runs: &mut Vec<Run>,
) -> Result<BundleFacts, String> {
    let bundle = signed(
        app,
        BUNDLE_ID,
        leaf,
        &certs.join(format!("{name}-bundle-")),
        runs,
    )?;
    let paths: Vec<PathBuf> = Part::ALL.iter().map(|part| part.path(app)).collect();
    let digests = generation::sha256(&paths, runs)?;
    let mut parts = Vec::new();
    for (part, sha256) in Part::ALL.into_iter().zip(digests) {
        let prefix = certs.join(format!("{name}-{}-", part.name()));
        parts.push(PartFacts {
            part,
            signed: signed(
                &part.path(app),
                &part.identifier(BUNDLE_ID),
                leaf,
                &prefix,
                runs,
            )?,
            sha256,
        });
    }
    let mut reports = Vec::new();
    for part in [Part::App, Part::Helper] {
        reports.push(stub(app, part, tag, runs)?);
    }
    Ok(BundleFacts {
        name: name.to_owned(),
        app: app.display().to_string(),
        bundle,
        parts,
        manifest: Manifest::read(app),
        bundle_version: generation::bundle_version(app, runs)?,
        reports,
    })
}

/// Нарушения подписи одной части: код проверки, ad-hoc, `Authority`, лист.
pub fn signature_problems(label: &str, signed: &Signed, leaf: &str) -> Vec<String> {
    let mut problems = Vec::new();
    if signed.verify != Verify::Satisfied {
        problems.push(format!(
            "{label}: codesign --verify -R «{}»: {:?}",
            signed.requirement, signed.verify
        ));
    }
    match &signed.details {
        Ok(details) => {
            if details.adhoc {
                problems.push(format!("{label}: подпись ad-hoc"));
            }
            if details.authorities.first().map(String::as_str) != Some(SIGNER_CN) {
                problems.push(format!(
                    "{label}: Authority {:?}, ожидается {SIGNER_CN}",
                    details.authorities
                ));
            }
        }
        Err(err) => problems.push(format!("{label}: codesign -d: {err}")),
    }
    if signed.leaf.as_deref() != Some(leaf) {
        problems.push(format!(
            "{label}: SHA-1 извлечённого листа {:?}, закреплён {leaf}",
            signed.leaf
        ));
    }
    problems
}

fn cdhash(signed: &Signed) -> Option<&str> {
    signed
        .details
        .as_ref()
        .ok()
        .map(|details| details.cdhash.as_str())
}

pub fn judge_signatures(generations: &[BundleFacts], leaf: &str) -> Verdict {
    let mut problems = Vec::new();
    let mut checked = 0;
    for facts in generations {
        problems.extend(signature_problems(
            &format!("{} бандл", facts.name),
            &facts.bundle,
            leaf,
        ));
        checked += 1;
        for part in &facts.parts {
            problems.extend(signature_problems(
                &format!("{} {}", facts.name, part.part.file_name()),
                &part.signed,
                leaf,
            ));
            checked += 1;
        }
    }
    if checked != GENERATIONS.len() * (1 + Part::ALL.len()) {
        problems.push(format!("проверено {checked} подписей вместо 12"));
    }
    Verdict::of(
        problems,
        format!(
            "{checked} подписей двух поколений: код 0 со своим требованием, не ad-hoc, Authority {SIGNER_CN}, лист {leaf}"
        ),
    )
}

pub fn judge_distinct(generations: &[BundleFacts]) -> Verdict {
    let [first, second] = generations else {
        return Verdict::of(
            vec![format!("поколений {}, ожидается 2", generations.len())],
            String::new(),
        );
    };
    let mut problems = Vec::new();
    let mut facts = Vec::new();
    match (cdhash(&first.bundle), cdhash(&second.bundle)) {
        (Some(a), Some(b)) if a != b => facts.push(format!("CDHash бандлов {a} и {b}")),
        (a, b) => problems.push(format!("CDHash бандлов не различны: {a:?} и {b:?}")),
    }
    for part in [Part::App, Part::Host] {
        let (Some(a), Some(b)) = (first.part(part), second.part(part)) else {
            problems.push(format!("нет фактов {}", part.file_name()));
            continue;
        };
        if a.sha256 == b.sha256 {
            problems.push(format!(
                "SHA-256 {} одинаков: {}",
                part.file_name(),
                a.sha256
            ));
        }
        match (cdhash(&a.signed), cdhash(&b.signed)) {
            (Some(x), Some(y)) if x != y => {}
            (x, y) => problems.push(format!(
                "CDHash {} не различны: {x:?} и {y:?}",
                part.file_name()
            )),
        }
    }
    let same: Vec<&str> = [Part::Helper, Part::Attach, Part::Fixture]
        .into_iter()
        .filter(|part| {
            matches!((first.part(*part), second.part(*part)), (Some(a), Some(b)) if a.sha256 == b.sha256)
        })
        .map(Part::file_name)
        .collect();
    facts.push("SHA-256 и CDHash probe-update-app и probe-update-host различны".to_owned());
    facts.push(if same.is_empty() {
        "остальные бинари тоже различаются".to_owned()
    } else {
        format!("совпадают байт в байт: {}", same.join(", "))
    });
    Verdict::of(problems, facts.join("; "))
}

pub fn judge_requirements(generations: &[BundleFacts], leaf: &str) -> Verdict {
    let mut problems = Vec::new();
    let mut checked = 0;
    let mut check = |label: String, signed_list: Vec<&Signed>, identifier: String| {
        let expected = generation::requirement(&identifier, leaf);
        for signed in &signed_list {
            let actual = signed
                .details
                .as_ref()
                .ok()
                .map(|details| details.identifier.as_str());
            if actual != Some(identifier.as_str()) {
                problems.push(format!(
                    "{label}: идентификатор {actual:?}, ожидается {identifier}"
                ));
            }
            if signed.designated.as_deref() != Some(expected.as_str()) {
                problems.push(format!(
                    "{label}: designated requirement {:?}, ожидается «{expected}»",
                    signed.designated
                ));
            }
        }
        checked += 1;
    };
    check(
        "бандл".to_owned(),
        generations.iter().map(|facts| &facts.bundle).collect(),
        BUNDLE_ID.to_owned(),
    );
    for part in Part::ALL {
        check(
            part.file_name().to_owned(),
            generations
                .iter()
                .filter_map(|facts| facts.part(part).map(|facts| &facts.signed))
                .collect(),
            part.identifier(BUNDLE_ID),
        );
    }
    for facts in generations {
        if facts.parts.len() != Part::ALL.len() {
            problems.push(format!("{}: частей {}", facts.name, facts.parts.len()));
        }
    }
    Verdict::of(
        problems,
        format!(
            "{checked} частей: идентификатор и designated requirement одинаковы в поколениях 1 и 2 и равны «identifier \"<идентификатор части>\" and certificate leaf = H\"{leaf}\"»"
        ),
    )
}

pub fn judge_reports(generations: &[BundleFacts]) -> Verdict {
    let mut problems = Vec::new();
    let mut facts_text = Vec::new();
    if generations.len() != GENERATIONS.len() {
        problems.push(format!("поколений {}, ожидается 2", generations.len()));
    }
    for (facts, expected) in generations.iter().zip(GENERATIONS) {
        let label = facts.name.as_str();
        match &facts.manifest {
            Ok(manifest) => {
                if manifest.generation != expected.generation
                    || manifest.host_versions != expected.host
                    || manifest.attach_versions != expected.attach
                {
                    problems.push(format!(
                        "{label}: манифест {} {}/{}, ожидается {} {}/{}",
                        manifest.generation,
                        manifest.host_versions,
                        manifest.attach_versions,
                        expected.generation,
                        expected.host,
                        expected.attach
                    ));
                }
            }
            Err(err) => problems.push(format!("{label}: манифест: {err}")),
        }
        if facts.bundle_version != Some(expected.generation) {
            problems.push(format!(
                "{label}: CFBundleVersion {:?}, ожидается {}",
                facts.bundle_version, expected.generation
            ));
        }
        for part in [Part::App, Part::Helper] {
            let Some(stub) = facts.reports.iter().find(|stub| stub.part == part) else {
                problems.push(format!("{label}: нет отчёта {}", part.file_name()));
                continue;
            };
            match &stub.report {
                Ok(report) => {
                    if report.binary != part.file_name()
                        || report.generation != expected.generation
                        || report.host_versions != expected.host
                        || report.attach_versions != expected.attach
                    {
                        problems.push(format!("{label}: отчёт {report:?} не равен ожидаемому"));
                    }
                    let inside = same_path(
                        Path::new(&report.executable),
                        &part.path(Path::new(&facts.app)),
                    );
                    if !inside {
                        problems.push(format!(
                            "{label}: исполняемый путь {} не {} внутри бандла",
                            report.executable,
                            part.relative()
                        ));
                    }
                }
                Err(err) => problems.push(format!("{label}: {err}")),
            }
        }
        facts_text.push(format!(
            "{label}: поколение {}, хост {}, attach {}",
            expected.generation, expected.host, expected.attach
        ));
    }
    Verdict::of(
        problems,
        format!(
            "манифест, CFBundleVersion и обе заглушки согласны — {}",
            facts_text.join("; ")
        ),
    )
}

/// Один и тот же файл: пути равны или равны после разрешения ссылок.
fn same_path(actual: &Path, expected: &Path) -> bool {
    actual == expected
        || actual
            .canonicalize()
            .ok()
            .zip(expected.canonicalize().ok())
            .is_some_and(|(actual, expected)| actual == expected)
}

/// Стенд: кандидат `incompatible` и бандл `.denied` под своими требованиями.
fn stand_problems(stand: &[(String, Signed, Result<Manifest, String>)]) -> Vec<String> {
    let mut problems = Vec::new();
    for (name, signed, manifest) in stand {
        if signed.verify != Verify::Satisfied {
            problems.push(format!("{name}: {:?}", signed.verify));
        }
        let expected = match name.as_str() {
            "incompatible" => (3, Versions::new(2, 2)),
            _ => (1, Versions::new(1, 1)),
        };
        match manifest {
            Ok(manifest)
                if manifest.generation == expected.0 && manifest.host_versions == expected.1 => {}
            other => problems.push(format!(
                "{name}: манифест {other:?}, ожидается поколение {} и {}",
                expected.0, expected.1
            )),
        }
    }
    problems
}

pub fn run(out: &Path, environment: &Environment) -> Result<Report, String> {
    let nonce = environment
        .process
        .nonce
        .as_deref()
        .ok_or("предусловие: нет nonce")?;
    let (bundle_dir, layout) = bundles(environment)?;
    let dir = out.join(DIR);
    let certs = dir.join("certs");
    fs::create_dir_all(&certs).map_err(|err| format!("{}: {err}", certs.display()))?;
    fs::copy(bundle_dir.join(LAYOUT_FILE), out.join(LAYOUT_ARTIFACT))
        .map_err(|err| format!("копия {LAYOUT_FILE}: {err}"))?;
    let leaf = layout.leaf.as_str();
    let mut runs = Vec::new();
    let mut generations = Vec::new();
    let mut stand = Vec::new();
    let walked = (|| -> Result<(), String> {
        for expected in GENERATIONS {
            let entry = layout.get(expected.name)?;
            generations.push(collect(
                expected.name,
                &entry.app,
                leaf,
                &certs,
                nonce,
                &mut runs,
            )?);
        }
        for (name, identifier) in [("incompatible", BUNDLE_ID), ("denied", DENIED_ID)] {
            let entry = layout.get(name)?;
            let prefix = certs.join(format!("{name}-bundle-"));
            let facts = signed(&entry.app, identifier, leaf, &prefix, &mut runs)?;
            stand.push((name.to_owned(), facts, Manifest::read(&entry.app)));
        }
        Ok(())
    })();
    let written = results::write_json(
        &dir,
        "evidence.json",
        &Evidence {
            leaf,
            generations: &generations,
            stand: &stand,
            runs: &runs,
        },
    );
    walked?;
    written?;
    let broken = stand_problems(&stand);
    if !broken.is_empty() {
        return Err(format!("стенд: {}", broken.join("; ")));
    }
    let artifacts = [EVIDENCE, LAYOUT_ARTIFACT];
    let cells = [
        (SIGNATURES, judge_signatures(&generations, leaf)),
        (DISTINCT, judge_distinct(&generations)),
        (STABLE, judge_requirements(&generations, leaf)),
        (REPORTS, judge_reports(&generations)),
    ]
    .into_iter()
    .map(|(spec, verdict)| required_cell(CELL, &spec, COMPLETION, verdict, &artifacts))
    .collect();
    Ok(Report {
        cells,
        diagnostics: Diagnostics {
            notes: vec![format!(
                "бандлы {}; кандидат incompatible и бандл .denied проходят проверку под своими требованиями",
                bundle_dir.display()
            )],
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generation::tests::saved::{GEN2_CDHASH, PINNED};
    use crate::results::Status;

    fn signed_ok(identifier: &str, cdhash: &str) -> Signed {
        Signed {
            path: format!("/tmp/bundle/{identifier}"),
            identifier: identifier.to_owned(),
            requirement: generation::requirement(identifier, PINNED),
            verify: Verify::Satisfied,
            details: Ok(Details {
                identifier: identifier.to_owned(),
                cdhash: cdhash.to_owned(),
                authorities: vec![SIGNER_CN.to_owned()],
                adhoc: false,
                team: None,
                format: None,
            }),
            designated: Some(generation::requirement(identifier, PINNED)),
            leaf: Some(PINNED.to_owned()),
        }
    }

    fn hash(seed: u32, len: usize) -> String {
        format!("{seed:0>len$x}")
    }

    fn bundle(expected: &Generation) -> BundleFacts {
        let app = format!("/tmp/bundle/{}/probe-update.app", expected.name);
        let seed = expected.generation * 100;
        BundleFacts {
            name: expected.name.to_owned(),
            app: app.clone(),
            bundle: signed_ok(BUNDLE_ID, &hash(seed, 40)),
            parts: Part::ALL
                .iter()
                .enumerate()
                .map(|(index, part)| PartFacts {
                    part: *part,
                    signed: signed_ok(
                        &part.identifier(BUNDLE_ID),
                        &hash(seed + index as u32 + 1, 40),
                    ),
                    sha256: hash(seed + index as u32 + 1, 64),
                })
                .collect(),
            manifest: Ok(crate::generation::tests::manifest(
                expected.generation,
                expected.host,
                expected.attach,
            )),
            bundle_version: Some(expected.generation),
            reports: [Part::App, Part::Helper]
                .into_iter()
                .map(|part| StubFacts {
                    part,
                    report: Ok(StubReport {
                        binary: part.file_name().to_owned(),
                        generation: expected.generation,
                        host_versions: expected.host,
                        attach_versions: expected.attach,
                        executable: format!("{app}/{}", part.relative()),
                    }),
                })
                .collect(),
        }
    }

    fn generations() -> Vec<BundleFacts> {
        GENERATIONS.iter().map(bundle).collect()
    }

    fn part_mut(facts: &mut BundleFacts, part: Part) -> &mut PartFacts {
        facts
            .parts
            .iter_mut()
            .find(|facts| facts.part == part)
            .unwrap()
    }

    #[test]
    fn matching_generations_pass_every_part() {
        let facts = generations();
        for verdict in [
            judge_signatures(&facts, PINNED),
            judge_distinct(&facts),
            judge_requirements(&facts, PINNED),
            judge_reports(&facts),
        ] {
            assert_eq!(verdict.status, Status::Pass, "{}", verdict.reason);
        }
    }

    #[test]
    fn each_signature_defect_fails() {
        let mut facts = generations();
        part_mut(&mut facts[1], Part::Fixture).signed.leaf = Some("f".repeat(40));
        let verdict = judge_signatures(&facts, PINNED);
        assert_eq!(verdict.status, Status::Fail);
        assert!(
            verdict.reason.contains("probe-update-fixture"),
            "{}",
            verdict.reason
        );

        let mut facts = generations();
        facts[0].bundle.verify = Verify::Unsatisfied;
        if let Ok(details) = &mut part_mut(&mut facts[0], Part::Host).signed.details {
            details.adhoc = true;
            details.authorities = vec!["Other".to_owned()];
        }
        let verdict = judge_signatures(&facts, PINNED);
        assert_eq!(verdict.status, Status::Fail);
        assert!(verdict.reason.contains("ad-hoc") && verdict.reason.contains("Authority"));
    }

    #[test]
    fn identical_code_fails_distinct() {
        let mut facts = generations();
        let first = part_mut(&mut facts[0], Part::Host).sha256.clone();
        part_mut(&mut facts[1], Part::Host).sha256 = first;
        assert_eq!(judge_distinct(&facts).status, Status::Fail);
        let mut facts = generations();
        let first = facts[0].bundle.clone();
        facts[1].bundle = first;
        assert_eq!(judge_distinct(&facts).status, Status::Fail);
        // Совпадение вспомогательных бинарей допустимо и записывается.
        let mut facts = generations();
        let first = part_mut(&mut facts[0], Part::Attach).sha256.clone();
        part_mut(&mut facts[1], Part::Attach).sha256 = first;
        let verdict = judge_distinct(&facts);
        assert_eq!(verdict.status, Status::Pass);
        assert!(
            verdict.reason.contains("probe-update-attach"),
            "{}",
            verdict.reason
        );
        assert_eq!(judge_distinct(&facts[..1]).status, Status::Fail);
    }

    #[test]
    fn requirement_drift_fails() {
        let mut facts = generations();
        facts[1].bundle.designated = Some(generation::requirement(BUNDLE_ID, &"f".repeat(40)));
        assert_eq!(judge_requirements(&facts, PINNED).status, Status::Fail);
        let mut facts = generations();
        if let Ok(details) = &mut part_mut(&mut facts[0], Part::Helper).signed.details {
            details.identifier = BUNDLE_ID.to_owned();
        }
        assert_eq!(judge_requirements(&facts, PINNED).status, Status::Fail);
    }

    #[test]
    fn disagreeing_generation_reports_fail() {
        let mut facts = generations();
        facts[1].bundle_version = Some(1);
        assert_eq!(judge_reports(&facts).status, Status::Fail);
        let mut facts = generations();
        if let Ok(report) = &mut facts[0].reports[1].report {
            report.generation = 2;
        }
        assert_eq!(judge_reports(&facts).status, Status::Fail);
        let mut facts = generations();
        if let Ok(report) = &mut facts[1].reports[0].report {
            report.executable = "/tmp/elsewhere/probe-update-app".to_owned();
        }
        assert_eq!(judge_reports(&facts).status, Status::Fail);
        let mut facts = generations();
        facts[0].manifest = Ok(crate::generation::tests::manifest(
            1,
            Versions::new(1, 2),
            Versions::new(1, 1),
        ));
        assert_eq!(judge_reports(&facts).status, Status::Fail);
        let mut facts = generations();
        facts[1].reports[1].report = Err("код 1".to_owned());
        assert_eq!(judge_reports(&facts).status, Status::Fail);
    }

    #[test]
    fn broken_stand_bundles_are_reported() {
        let manifest =
            |generation, host| Ok(crate::generation::tests::manifest(generation, host, host));
        let ok = vec![
            (
                "incompatible".to_owned(),
                signed_ok(BUNDLE_ID, GEN2_CDHASH),
                manifest(3, Versions::new(2, 2)),
            ),
            (
                "denied".to_owned(),
                signed_ok(DENIED_ID, GEN2_CDHASH),
                manifest(1, Versions::new(1, 1)),
            ),
        ];
        assert!(stand_problems(&ok).is_empty());
        let mut broken = ok.clone();
        broken[0].2 = manifest(2, Versions::new(1, 2));
        broken[1].1.verify = Verify::Unsatisfied;
        assert_eq!(stand_problems(&broken).len(), 2);
    }

    #[test]
    fn parts_become_valid_required_cells() {
        let facts = generations();
        for (spec, verdict) in [
            (SIGNATURES, judge_signatures(&facts, PINNED)),
            (DISTINCT, judge_distinct(&facts)),
            (STABLE, judge_requirements(&facts, PINNED)),
            (REPORTS, judge_reports(&facts)),
        ] {
            let cell = required_cell(
                CELL,
                &spec,
                COMPLETION,
                verdict,
                &[EVIDENCE, LAYOUT_ARTIFACT],
            );
            assert!(cell.problems().is_empty(), "{:?}", cell.problems());
        }
    }
}
