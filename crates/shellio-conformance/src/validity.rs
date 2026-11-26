//! Годность паспорта (ADR-0037, «Паспорт и годность»): штамп `prototype/`, SHA-256 канонических
//! сцены и рецепта, ячейка, SHA-256 закрепления браузера и номер схемы съёмщика.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::Error;
use crate::fields::ElementKind;
use crate::mask::Mask;
use crate::passport::{Cell, Passport, Validity, canonical_json};
use crate::recipe::Recipe;
use crate::registry::Registry;
use crate::scene::Scene;

/// Номер схемы паспорта и съёмщика. Растёт при любом изменении сбора.
pub const SNAP_SCHEMA: u32 = 7;

/// Файлы, которые штамп `prototype/` не учитывает: служебные файлы Finder.
const IGNORED_NAMES: &[&str] = &[".DS_Store"];

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// SHA-256 байтов в шестнадцатеричной записи.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// SHA-256 канонической записи JSON-текста.
pub fn canonical_sha256(place: &str, text: &str) -> Result<String, Error> {
    let value: Value = serde_json::from_str(text).map_err(|e| Error::new(place, e.to_string()))?;
    Ok(sha256_hex(canonical_json(&value)?.as_bytes()))
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        if IGNORED_NAMES.iter().any(|n| name == *n) {
            continue;
        }
        let kind = entry.file_type()?;
        if kind.is_dir() {
            collect(root, &path, out)?;
        } else if kind.is_file() {
            let rel = path
                .strip_prefix(root)
                .map_err(io::Error::other)?
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            out.push((rel, path));
        } else {
            return Err(io::Error::other(format!(
                "{}: в штампе учитываются только файлы и каталоги",
                path.display()
            )));
        }
    }
    Ok(())
}

/// Штамп каталога: SHA-256 по отсортированным парам «относительный путь, байты». Каждая пара
/// входит длиной и байтами пути, затем длиной и байтами файла, поэтому границы пар однозначны.
pub fn tree_stamp(dir: &Path) -> io::Result<String> {
    let mut files = Vec::new();
    collect(dir, dir, &mut files)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut hasher = Sha256::new();
    for (rel, path) in files {
        let bytes = fs::read(&path)?;
        hasher.update((rel.len() as u64).to_be_bytes());
        hasher.update(rel.as_bytes());
        hasher.update((bytes.len() as u64).to_be_bytes());
        hasher.update(&bytes);
    }
    Ok(hex(&hasher.finalize()))
}

/// Входы годности, общие для всех паспортов: корень репозитория и его штампы.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inputs {
    pub root: PathBuf,
    pub prototype: String,
    pub pin: String,
    pub schema: u32,
}

impl Inputs {
    /// Считает штамп `prototype/` и сумму `scripts/conformance/pin` по корню репозитория.
    pub fn from_root(root: &Path) -> Result<Self, Error> {
        let prototype = tree_stamp(&root.join("prototype"))
            .map_err(|e| Error::new("prototype/", e.to_string()))?;
        let pin_path = root.join("scripts/conformance/pin");
        let pin = fs::read(&pin_path)
            .map(|bytes| sha256_hex(&bytes))
            .map_err(|e| Error::new(pin_path.display().to_string(), e.to_string()))?;
        Ok(Self {
            root: root.to_path_buf(),
            prototype,
            pin,
            schema: SNAP_SCHEMA,
        })
    }

    pub fn scene_path(&self, scene: &str) -> PathBuf {
        self.root
            .join("conformance/scenes")
            .join(format!("{scene}.json"))
    }

    pub fn recipe_path(&self, recipe: &str) -> PathBuf {
        self.root
            .join("conformance/recipes")
            .join(format!("{recipe}.json"))
    }

    pub fn passport_path(&self, state: &str, cell: Cell) -> PathBuf {
        self.root
            .join("conformance/passports")
            .join(state)
            .join(format!("{}.json", cell.name()))
    }

    /// Ожидаемые поля годности паспорта состояния с рецептом `recipe` в ячейке `cell`. Сцена и
    /// рецепт разбираются и проверяются: негодный вход — ошибка, а не устаревший паспорт.
    pub fn expected(&self, recipe: &str, cell: Cell) -> Result<Validity, Error> {
        let recipe_path = self.recipe_path(recipe);
        let recipe_place = recipe_path.display().to_string();
        let recipe_text = read(&recipe_path)?;
        let parsed = Recipe::parse(&recipe_place, &recipe_text)?;
        if !parsed.allows(cell) {
            return Err(Error::new(
                &recipe_place,
                format!("ячейка {cell} расходится с осью, закреплённой рецептом"),
            ));
        }
        let scene_path = self.scene_path(&parsed.scene);
        let scene_place = scene_path.display().to_string();
        let scene_text = read(&scene_path)?;
        let scene = Scene::parse(&scene_place, &scene_text)?;
        parsed
            .expected(&scene)
            .map_err(|reason| Error::new(&recipe_place, reason))?;
        Ok(Validity {
            prototype: self.prototype.clone(),
            scene: canonical_sha256(&scene_place, &scene_text)?,
            recipe: canonical_sha256(&recipe_place, &recipe_text)?,
            cell,
            pin: self.pin.clone(),
            schema: self.schema,
        })
    }
}

fn read(path: &Path) -> Result<String, Error> {
    fs::read_to_string(path).map_err(|e| Error::new(path.display().to_string(), e.to_string()))
}

/// Поля годности, которые расходятся с ожидаемыми.
pub fn stale_fields(have: &Validity, want: &Validity) -> Vec<&'static str> {
    let mut out = Vec::new();
    if have.prototype != want.prototype {
        out.push("prototype");
    }
    if have.scene != want.scene {
        out.push("scene");
    }
    if have.recipe != want.recipe {
        out.push("recipe");
    }
    if have.cell != want.cell {
        out.push("cell");
    }
    if have.pin != want.pin {
        out.push("pin");
    }
    if have.schema != want.schema {
        out.push("schema");
    }
    out
}

/// Неисправность паспорта состояния в ячейке.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    Missing,
    /// Паспорт не разбирается, нарушает свои правила или записан не канонически.
    Broken(String),
    Stale(Vec<&'static str>),
}

/// Строка отчёта годности.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub state: String,
    pub cell: Cell,
    pub path: PathBuf,
    pub problem: Problem,
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let what = match &self.problem {
            Problem::Missing => "паспорта нет".to_owned(),
            Problem::Broken(reason) => format!("паспорт негоден: {reason}"),
            Problem::Stale(fields) => format!("паспорт устарел: {}", fields.join(", ")),
        };
        write!(
            f,
            "{} [{}] {}: {what}",
            self.state,
            self.cell,
            self.path.display()
        )
    }
}

/// Проверяет паспорт каждой ячейки каждого состояния реестра. Ошибка входа (сцена, рецепт) —
/// `Err`, неисправные паспорта — список.
pub fn check_registry(inputs: &Inputs, registry: &Registry) -> Result<Vec<Finding>, Error> {
    let mut findings = Vec::new();
    for state in &registry.states {
        let recipe_path = inputs.recipe_path(&state.recipe);
        let recipe_place = recipe_path.display().to_string();
        let recipe = Recipe::parse(&recipe_place, &read(&recipe_path)?)?;
        // Годность хеширует сцену рецепта, поэтому сцена строки состояния обязана совпадать с ней.
        if recipe.scene != state.scene {
            return Err(Error::new(
                &recipe_place,
                format!(
                    "состояние «{}» называет сцену «{}», а рецепт — «{}»",
                    state.id, state.scene, recipe.scene
                ),
            ));
        }
        for cell in state.cells() {
            let want = inputs.expected(&state.recipe, cell)?;
            let path = inputs.passport_path(&state.id, cell);
            let problem = match fs::read_to_string(&path) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => Some(Problem::Missing),
                Err(e) => Some(Problem::Broken(e.to_string())),
                Ok(text) => judge(&path, &text, &want),
            };
            if let Some(problem) = problem {
                findings.push(Finding {
                    state: state.id.clone(),
                    cell,
                    path,
                    problem,
                });
            }
        }
    }
    Ok(findings)
}

fn judge(path: &Path, text: &str, want: &Validity) -> Option<Problem> {
    let passport = match Passport::parse(&path.display().to_string(), text) {
        Ok(p) => p,
        Err(e) => return Some(Problem::Broken(e.reason)),
    };
    match passport.to_canonical() {
        Ok(canonical) if canonical == text => {}
        Ok(_) => return Some(Problem::Broken("запись не каноническая".to_owned())),
        Err(e) => return Some(Problem::Broken(e.reason)),
    }
    if let Err(reason) = check_masks(path, &passport) {
        return Some(Problem::Broken(reason));
    }
    let stale = stale_fields(&passport.validity, want);
    (!stale.is_empty()).then_some(Problem::Stale(stale))
}

/// Маски значков рядом с паспортом (`<ячейка>.icons/<ключ>.png`): ровно по одной на видимый
/// значок, каждая разбирается.
fn check_masks(path: &Path, passport: &Passport) -> Result<(), String> {
    let want: BTreeSet<String> = passport
        .elements
        .iter()
        .filter(|e| e.kind == ElementKind::Icon && e.visible)
        .map(|e| e.ck.clone())
        .collect();
    let dir = path.with_extension("icons");
    let mut have = BTreeSet::new();
    if dir.exists() {
        for entry in fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))? {
            let entry = entry.map_err(|e| e.to_string())?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if IGNORED_NAMES.contains(&name.as_str()) {
                continue;
            }
            let ck = name
                .strip_suffix(".png")
                .ok_or_else(|| format!("{}: лишний файл {name}", dir.display()))?;
            let bytes = fs::read(entry.path()).map_err(|e| e.to_string())?;
            Mask::read_png(&bytes).map_err(|e| format!("маска {name}: {e}"))?;
            have.insert(ck.to_owned());
        }
    }
    if let Some(ck) = want.difference(&have).next() {
        return Err(format!("нет маски значка {ck}"));
    }
    if let Some(ck) = have.difference(&want).next() {
        return Err(format!("маска {ck} без видимого значка"));
    }
    Ok(())
}
