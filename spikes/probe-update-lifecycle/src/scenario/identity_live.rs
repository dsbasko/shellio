//! Сценарий `identity-live` (задача 8): динамическая часть A1. Обновление поколения 1 до 2 тем же
//! ходом, что `swap`, без остановки N+1. Части:
//!
//! 1. `live-generation` — GUI N+1 сообщает поколение 2; CDHash его процесса из ядра (`csops`)
//!    равен сообщённому и CDHash бандла поколения 2 по `bundles.json`; `proc_pidpath` — исполняемый
//!    файл установленного бандла;
//! 2. `live-same-path-new-code` — по одному и тому же установленному пути работали разные коды:
//!    GUI N сообщил поколение 1 и CDHash поколения 1, N+1 — поколение 2 и CDHash поколения 2;
//!    исполняемый путь у обоих — установленный бандл.
//!
//! Шаги и факты — в `identity-live/steps.json`.

use std::fs;
use std::path::Path;
use std::time::Duration;

use serde_json::json;

use crate::generation::{Layout, Part};
use crate::results::{Cell, Diagnostics, Environment, Report, Requirement};
use crate::sys;

use super::owner_basic::Guard;
use super::swap::{Prepared, prepare};
use super::update_flow::{self, Stand, canonical};
use super::{PartSpec, Verdict};

pub const DEADLINE: Duration = Duration::from_secs(180);
pub const DIR: &str = "identity-live";
pub const STEPS_ARTIFACT: &str = "identity-live/steps.json";
const CELL: &str = "A1";
const COMPLETION: &str = "готовность N+1 в срок шага (30 с); иначе ошибка исполнения";

pub const GENERATION: PartSpec = PartSpec {
    name: "live-generation",
    stimulus: "обновление поколения 1 до 2: update, activate, помощник, N+1 через open -n",
    oracle: "ping N+1: поколение 2; CDHash процесса из ядра = сообщённый = CDHash бандла gen-2 по bundles.json ≠ CDHash gen-1; proc_pidpath N+1 и сообщённый исполняемый файл — установленный бандл",
};
pub const SAME_PATH: PartSpec = PartSpec {
    name: "live-same-path-new-code",
    stimulus: "тот же проход",
    oracle: "GUI N до обновления: поколение 1, CDHash процесса — gen-1, исполняемый файл — установленный бандл; N+1 после: поколение 2, CDHash — gen-2, тот же исполняемый путь",
};

fn part(spec: &PartSpec, verdict: Verdict, artifacts: &[String]) -> Cell {
    let mut all = vec![STEPS_ARTIFACT.to_owned()];
    all.extend(artifacts.iter().cloned());
    Cell {
        id: CELL.to_owned(),
        part: Some(spec.name.to_owned()),
        requirement: Requirement::Required,
        status: verdict.status,
        stimulus: spec.stimulus.to_owned(),
        oracle: spec.oracle.to_owned(),
        completion: COMPLETION.to_owned(),
        reason: verdict.reason,
        artifacts: all,
    }
}

pub fn run(out: &Path, environment: &Environment) -> Result<Report, String> {
    let (state, nonce, layout) = super::stand_inputs(environment)?;
    let dir = out.join(DIR);
    fs::create_dir(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    let mut guard = Guard {
        state: state.clone(),
        apps: Vec::new(),
        sessions: Vec::new(),
        nonce: Some(nonce.clone()),
        armed: true,
    };
    let mut steps = Vec::new();
    let walked = walk(&state, &nonce, &layout, &dir, &mut steps, &mut guard);
    drop(guard);
    if let Err(reason) = &walked {
        steps.push(json!({ "name": "error", "reason": reason }));
    }
    let written = crate::results::write_json(&dir, "steps.json", &steps);
    let cells = walked?;
    written?;
    Ok(Report {
        cells,
        diagnostics: Diagnostics::default(),
    })
}

fn walk(
    state: &Path,
    nonce: &str,
    layout: &Layout,
    out: &Path,
    steps: &mut Vec<serde_json::Value>,
    guard: &mut Guard,
) -> Result<Vec<Cell>, String> {
    let stand = Stand::new(state, nonce, layout, "gen-1")?;
    let gen1 = layout.get("gen-1")?.cdhash.clone();
    let gen2 = layout.get("gen-2")?.cdhash.clone();
    let Prepared {
        gui_index,
        owner_n,
        activating,
        ..
    } = prepare(&stand, layout, "gen-2", guard, out, steps)?;
    let followed =
        update_flow::follow(&stand, guard, gui_index, activating, false, out, "owner-n")?;
    let next = &followed.next;
    let kernel_cdhash = sys::process_cdhash(next.key.pid).ok();
    let kernel_path = sys::process_path(next.key.pid)
        .ok()
        .map(|path| canonical(&path));
    let expected_bin = canonical(&Part::App.path(&stand.installed));
    steps.push(json!({
        "name": "follow",
        "facts": followed.facts(),
        "kernel_cdhash": kernel_cdhash,
        "kernel_path": kernel_path,
        "expected_bin": expected_bin,
    }));
    let artifacts = vec![
        format!("{DIR}/helper-{}", followed.attempt),
        format!("{DIR}/owner-n.stdout"),
    ];

    let mut v = Vec::new();
    if next.generation != 2 {
        v.push(format!("N+1 сообщил поколение {}", next.generation));
    }
    if !(kernel_cdhash.as_deref() == Some(next.cdhash.as_str())
        && next.cdhash == gen2
        && gen2 != gen1)
    {
        v.push(format!(
            "CDHash: ядро {kernel_cdhash:?}, сообщённый {}, gen-2 {gen2}, gen-1 {gen1}",
            next.cdhash
        ));
    }
    if !(kernel_path.as_deref() == Some(expected_bin.as_path())
        && canonical(Path::new(&next.executable)) == expected_bin)
    {
        v.push(format!(
            "исполняемый файл: ядро {kernel_path:?}, сообщённый {}, ожидался {}",
            next.executable,
            expected_bin.display()
        ));
    }
    let generation = Verdict::of(
        v,
        format!(
            "N+1 {}: поколение 2, CDHash процесса {} = gen-2, путь {}",
            next.key.pid,
            &gen2[..8.min(gen2.len())],
            expected_bin.display()
        ),
    );

    let mut v = Vec::new();
    if !(owner_n.generation == 1
        && owner_n.cdhash == gen1
        && canonical(Path::new(&owner_n.executable)) == expected_bin)
    {
        v.push(format!(
            "GUI N: поколение {}, CDHash {}, путь {}",
            owner_n.generation, owner_n.cdhash, owner_n.executable
        ));
    }
    if !(next.generation == 2
        && next.cdhash == gen2
        && canonical(Path::new(&next.executable)) == expected_bin)
    {
        v.push(format!(
            "N+1: поколение {}, CDHash {}, путь {}",
            next.generation, next.cdhash, next.executable
        ));
    }
    let same_path = Verdict::of(
        v,
        format!(
            "путь {}: GUI N {} — поколение 1 ({}), N+1 {} — поколение 2 ({})",
            expected_bin.display(),
            owner_n.key.pid,
            &gen1[..8.min(gen1.len())],
            next.key.pid,
            &gen2[..8.min(gen2.len())]
        ),
    );
    let cells = vec![
        part(&GENERATION, generation, &artifacts),
        part(&SAME_PATH, same_path, &artifacts),
    ];
    let finished = update_flow::finish(&stand, guard, next.key)?;
    steps.push(json!({ "name": "finish", "facts": finished }));
    Ok(cells)
}
