//! Самопроверка во временной неподписанной раскладке `shellio.app` из ресурсов опубликованного набора.
//!
//! Неподписанная значит без идентичности `Shellio Dev`: бинарь копируется вместе с подписью линковщика.
//! Набор только читается, блокировку держит цель `make`, которая запускает тесты.

// allow-unwrap-in-tests не распространяется на вспомогательные функции интеграционного теста.
#![allow(clippy::unwrap_used)]

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{self, Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Переменные, которые самопроверка получает временными каталогами; после выхода каталоги пусты.
const ISOLATED: [&str; 6] = [
    "HOME",
    "XDG_CONFIG_HOME",
    "XDG_STATE_HOME",
    "XDG_CACHE_HOME",
    "XDG_DATA_HOME",
    "SHELLIO_STATE_DIR",
];

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = env::temp_dir().join(format!(
            "shellio-layout-{}-{}",
            process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir(&path).unwrap();
        // Пути в выводе разрешены, а /var — ссылка на /private/var.
        Self(fs::canonicalize(&path).unwrap())
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn workspace_root() -> PathBuf {
    fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")).unwrap()
}

// Корень набора выбирается так же, как в build.rs.
fn vendor_root() -> PathBuf {
    match env::var_os("SHELLIO_GHOSTTY_VENDOR") {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => workspace_root().join("vendor/ghostty"),
    }
}

fn stamp_value(key: &str) -> String {
    let stamp = fs::read_to_string(vendor_root().join("stamp")).unwrap();
    stamp
        .lines()
        .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
        .unwrap_or_else(|| panic!("в штампе нет {key}"))
        .to_owned()
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let kind = entry.file_type().unwrap();
        let target = to.join(entry.file_name());
        if kind.is_dir() {
            copy_dir(&entry.path(), &target);
        } else if kind.is_file() {
            fs::copy(entry.path(), &target).unwrap();
        } else {
            panic!("в наборе неожиданный элемент {}", entry.path().display());
        }
    }
}

/// Раскладывает `shellio.app` в `root` по разделу Technical Details и возвращает путь бинаря.
fn layout(root: &Path, with_terminfo: bool) -> PathBuf {
    let vendor = vendor_root();
    let contents = root.join("shellio.app/Contents");
    let exe = contents.join("MacOS/shellio");
    fs::create_dir_all(exe.parent().unwrap()).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_shellio"), &exe).unwrap();
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("Info.plist"),
        contents.join("Info.plist"),
    )
    .unwrap();
    let resources = contents.join("Resources");
    copy_dir(&vendor.join("share/ghostty"), &resources.join("ghostty"));
    if with_terminfo {
        copy_dir(&vendor.join("share/terminfo"), &resources.join("terminfo"));
    }
    fs::create_dir_all(resources.join("licenses")).unwrap();
    fs::copy(
        vendor.join("LICENSE"),
        resources.join("licenses/ghostty.txt"),
    )
    .unwrap();
    exe
}

fn isolated_dirs(root: &Path) -> Vec<(&'static str, PathBuf)> {
    ISOLATED
        .iter()
        .map(|&name| {
            let dir = root.join("isolated").join(name);
            fs::create_dir_all(&dir).unwrap();
            (name, dir)
        })
        .collect()
}

fn run_isolated(exe: &Path, root: &Path, dirs: &[(&str, PathBuf)]) -> Output {
    Command::new(exe)
        .arg("--self-check")
        .env_clear()
        .envs(dirs.iter().map(|(name, dir)| (name, dir)))
        .current_dir(root)
        .output()
        .unwrap()
}

fn assert_empty(dirs: &[(&str, PathBuf)]) {
    for (name, dir) in dirs {
        let entries: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert!(entries.is_empty(), "{name} не пуст: {entries:?}");
    }
}

fn plutil_extract(key: &str, file: &Path) -> String {
    let output = Command::new("/usr/bin/plutil")
        .env_clear()
        .args(["-extract", key, "raw", "-o", "-"])
        .arg(file)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "plutil -extract {key} {}: {}",
        file.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .trim_end_matches('\n')
        .to_owned()
}

#[test]
fn self_check_passes_in_isolated_bundle() {
    let tmp = TempDir::new();
    let exe = layout(&tmp.0, true);
    let dirs = isolated_dirs(&tmp.0);

    let output = run_isolated(&exe, &tmp.0, &dirs);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");
    assert!(stderr.is_empty(), "stderr: {stderr}");
    let line = stdout.strip_suffix('\n').unwrap();
    assert!(!line.contains('\n'), "вывод не одной строкой: {stdout}");

    let resources = tmp.0.join("shellio.app/Contents/Resources");
    let ghostty = resources.join("ghostty");
    let terminfo = resources.join("terminfo");
    let version = stamp_value("ghostty_version_string");
    assert_eq!(
        line,
        format!(
            "{{\"app_version\":\"{}\",\"ghostty_version\":\"{version}\",\
             \"ghostty_build_mode\":\"ReleaseFast\",\
             \"ghostty_resources\":\"{}\",\"terminfo\":\"{}\"}}",
            env!("CARGO_PKG_VERSION"),
            ghostty.display(),
            terminfo.display()
        )
    );

    // Скрипты бандла читают итог через plutil, поэтому строка должна им разбираться.
    let json = tmp.0.join("self-check.json");
    fs::write(&json, &stdout).unwrap();
    for (key, value) in [
        ("app_version", env!("CARGO_PKG_VERSION").to_owned()),
        ("ghostty_version", version),
        ("ghostty_build_mode", "ReleaseFast".to_owned()),
        ("ghostty_resources", ghostty.display().to_string()),
        ("terminfo", terminfo.display().to_string()),
    ] {
        assert_eq!(plutil_extract(key, &json), value, "{key}");
    }

    assert_empty(&dirs);
}

#[test]
fn self_check_follows_link_to_bundle_binary() {
    let tmp = TempDir::new();
    let exe = layout(&tmp.0, true);
    let link = tmp.0.join("bin/shellio");
    fs::create_dir_all(link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&exe, &link).unwrap();
    let dirs = isolated_dirs(&tmp.0);

    let output = run_isolated(&link, &tmp.0, &dirs);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");
    let json = tmp.0.join("self-check.json");
    fs::write(&json, &output.stdout).unwrap();
    let resources = tmp.0.join("shellio.app/Contents/Resources");
    assert_eq!(
        plutil_extract("ghostty_resources", &json),
        resources.join("ghostty").display().to_string()
    );
    assert_eq!(
        plutil_extract("terminfo", &json),
        resources.join("terminfo").display().to_string()
    );

    assert_empty(&dirs);
}

#[test]
fn self_check_fails_without_terminfo() {
    let tmp = TempDir::new();
    let exe = layout(&tmp.0, false);
    let dirs = isolated_dirs(&tmp.0);

    let output = run_isolated(&exe, &tmp.0, &dirs);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "stderr: {stderr}");
    assert!(output.stdout.is_empty());
    let terminfo = tmp.0.join("shellio.app/Contents/Resources/terminfo");
    assert!(
        stderr.contains(&format!("нет каталога terminfo {}", terminfo.display())),
        "stderr: {stderr}"
    );

    assert_empty(&dirs);
}

#[test]
fn info_plist_template_is_valid() {
    let plist = Path::new(env!("CARGO_MANIFEST_DIR")).join("Info.plist");
    let lint = Command::new("/usr/bin/plutil")
        .env_clear()
        .arg("-lint")
        .arg(&plist)
        .output()
        .unwrap();
    assert!(
        lint.status.success(),
        "{}",
        String::from_utf8_lossy(&lint.stdout)
    );

    let config = fs::read_to_string(workspace_root().join(".cargo/config.toml")).unwrap();
    let deployment_target = config
        .lines()
        .find_map(|line| line.strip_prefix("MACOSX_DEPLOYMENT_TARGET = { value = \""))
        .and_then(|rest| rest.split_once('"'))
        .map(|(value, _)| value)
        .unwrap();
    for (key, value) in [
        ("CFBundleIdentifier", "io.github.dsbasko.shellio"),
        ("CFBundleExecutable", "shellio"),
        ("CFBundlePackageType", "APPL"),
        ("LSMinimumSystemVersion", deployment_target),
        ("LSUIElement", "true"),
    ] {
        assert_eq!(plutil_extract(key, &plist), value, "{key}");
    }
}
