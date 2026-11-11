//! Самопроверка бандла: ресурсы ghostty на своих местах, libghostty слинкован и отвечает.
//!
//! Ресурсы ищутся от настоящего пути исполняемого файла так же, как их найдёт ghostty без
//! `GHOSTTY_RESOURCES_DIR` (`src/os/resourcesdir.zig`): по файлу `terminfo/78/xterm-ghostty` в
//! `Contents/Resources` рядом с каталогом `ghostty`. Каталог состояния самопроверка не читает и не пишет.

use std::env;
use std::error::Error as StdError;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use shellio_ghostty::{BuildMode, Info, InfoError};

const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Файл, по которому ghostty на macOS опознаёт каталог ресурсов бандла.
const TERMINFO_SENTINEL: &str = "78/xterm-ghostty";

/// Найденные в бандле ресурсы ghostty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resources {
    pub ghostty: PathBuf,
    pub terminfo: PathBuf,
}

/// Чем раскладка бандла расходится с ожидаемой.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutError {
    NotInBundle(PathBuf),
    MissingGhostty(PathBuf),
    MissingTerminfo(PathBuf),
    MissingXtermGhostty(PathBuf),
}

impl fmt::Display for LayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotInBundle(exe) => write!(
                f,
                "исполняемый файл {} лежит не в Contents/MacOS бандла",
                exe.display()
            ),
            Self::MissingGhostty(path) => {
                write!(f, "нет каталога ресурсов ghostty {}", path.display())
            }
            Self::MissingTerminfo(path) => write!(
                f,
                "нет каталога terminfo {} рядом с ресурсами ghostty",
                path.display()
            ),
            Self::MissingXtermGhostty(path) => write!(
                f,
                "нет файла {}, по нему ghostty находит ресурсы бандла",
                path.display()
            ),
        }
    }
}

impl StdError for LayoutError {}

/// Причина, по которой самопроверка не прошла.
#[derive(Debug)]
pub enum Error {
    CurrentExe(io::Error),
    ResolveExe(PathBuf, io::Error),
    Layout(LayoutError),
    Ghostty(InfoError),
    NonUtf8Path(PathBuf),
    Stdout(io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CurrentExe(err) => {
                write!(f, "не удалось получить путь исполняемого файла: {err}")
            }
            Self::ResolveExe(exe, err) => write!(
                f,
                "не удалось разрешить путь исполняемого файла {}: {err}",
                exe.display()
            ),
            Self::Layout(err) => err.fmt(f),
            Self::Ghostty(err) => err.fmt(f),
            Self::NonUtf8Path(path) => write!(f, "путь {} не в UTF-8", path.display()),
            Self::Stdout(err) => write!(f, "не удалось записать результат в stdout: {err}"),
        }
    }
}

// Причина уже входит в текст ошибки, поэтому `source` не переопределяется: обход цепочки повторил бы её.
impl StdError for Error {}

/// Проверяет бандл и libghostty и возвращает итог одной строкой JSON.
///
/// Путь берётся один раз через `current_exe`; `argv[0]` и `PATH` не используются.
pub fn run() -> Result<String, Error> {
    let exe = env::current_exe().map_err(Error::CurrentExe)?;
    // ghostty разрешает путь через realpath, поэтому ссылка на бинарь ведёт в бандл настоящего файла.
    let exe = fs::canonicalize(&exe).map_err(|err| Error::ResolveExe(exe, err))?;
    let resources = find_resources(&exe).map_err(Error::Layout)?;
    let info = shellio_ghostty::info().map_err(Error::Ghostty)?;
    report(&info, &resources)
}

/// Находит ресурсы ghostty бандла, в `Contents/MacOS` которого лежит `exe`.
pub fn find_resources(exe: &Path) -> Result<Resources, LayoutError> {
    let not_in_bundle = || LayoutError::NotInBundle(exe.to_path_buf());
    let macos = exe.parent().ok_or_else(not_in_bundle)?;
    let contents = macos.parent().ok_or_else(not_in_bundle)?;
    if macos.file_name() != Some(OsStr::new("MacOS"))
        || contents.file_name() != Some(OsStr::new("Contents"))
    {
        return Err(not_in_bundle());
    }

    let resources = contents.join("Resources");
    let ghostty = resources.join("ghostty");
    if !ghostty.is_dir() {
        return Err(LayoutError::MissingGhostty(ghostty));
    }
    let terminfo = resources.join("terminfo");
    if !terminfo.is_dir() {
        return Err(LayoutError::MissingTerminfo(terminfo));
    }
    let sentinel = terminfo.join(TERMINFO_SENTINEL);
    if !sentinel.is_file() {
        return Err(LayoutError::MissingXtermGhostty(sentinel));
    }
    Ok(Resources { ghostty, terminfo })
}

fn report(info: &Info, resources: &Resources) -> Result<String, Error> {
    let fields = [
        ("app_version", APP_VERSION),
        ("ghostty_version", info.version),
        ("ghostty_build_mode", build_mode_name(info.build_mode)),
        ("ghostty_resources", utf8(&resources.ghostty)?),
        ("terminfo", utf8(&resources.terminfo)?),
    ];
    let mut out = String::from("{");
    for (index, (key, value)) in fields.into_iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        push_json_string(&mut out, key);
        out.push(':');
        push_json_string(&mut out, value);
    }
    out.push('}');
    Ok(out)
}

fn build_mode_name(mode: BuildMode) -> &'static str {
    match mode {
        BuildMode::Debug => "Debug",
        BuildMode::ReleaseSafe => "ReleaseSafe",
        BuildMode::ReleaseFast => "ReleaseFast",
        BuildMode::ReleaseSmall => "ReleaseSmall",
    }
}

fn utf8(path: &Path) -> Result<&str, Error> {
    path.to_str()
        .ok_or_else(|| Error::NonUtf8Path(path.to_path_buf()))
}

fn push_json_string(out: &mut String, value: &str) {
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch < '\u{20}' => out.push_str(&format!("\\u{:04x}", u32::from(ch))),
            ch => out.push(ch),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::unix::ffi::OsStrExt;
    use std::process;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = env::temp_dir().join(format!(
                "shellio-self-check-{}-{}",
                process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir(&path).unwrap();
            Self(fs::canonicalize(&path).unwrap())
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Раскладка `shellio.app` во временном каталоге: создаёт перечисленные каталоги (с `/` на конце)
    /// и файлы внутри `Contents/Resources` и возвращает путь исполняемого файла.
    fn bundle(dir: &TempDir, entries: &[&str]) -> PathBuf {
        let contents = dir.0.join("shellio.app/Contents");
        let exe = contents.join("MacOS/shellio");
        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        fs::write(&exe, b"").unwrap();
        for entry in entries {
            let path = contents.join("Resources").join(entry);
            if entry.ends_with('/') {
                fs::create_dir_all(&path).unwrap();
            } else {
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(&path, b"").unwrap();
            }
        }
        exe
    }

    fn resources_dir(dir: &TempDir) -> PathBuf {
        dir.0.join("shellio.app/Contents/Resources")
    }

    #[test]
    fn finds_resources_in_bundle() {
        let dir = TempDir::new();
        let exe = bundle(
            &dir,
            &[
                "ghostty/shell-integration/",
                "ghostty/themes/",
                "terminfo/67/ghostty",
                "terminfo/78/xterm-ghostty",
            ],
        );
        let resources = resources_dir(&dir);
        assert_eq!(
            find_resources(&exe),
            Ok(Resources {
                ghostty: resources.join("ghostty"),
                terminfo: resources.join("terminfo"),
            })
        );
    }

    #[test]
    fn rejects_missing_ghostty() {
        let dir = TempDir::new();
        let exe = bundle(&dir, &["terminfo/78/xterm-ghostty"]);
        assert_eq!(
            find_resources(&exe),
            Err(LayoutError::MissingGhostty(
                resources_dir(&dir).join("ghostty")
            ))
        );
    }

    #[test]
    fn rejects_ghostty_file_instead_of_directory() {
        let dir = TempDir::new();
        let exe = bundle(&dir, &["ghostty", "terminfo/78/xterm-ghostty"]);
        assert_eq!(
            find_resources(&exe),
            Err(LayoutError::MissingGhostty(
                resources_dir(&dir).join("ghostty")
            ))
        );
    }

    #[test]
    fn rejects_terminfo_not_beside_ghostty() {
        let dir = TempDir::new();
        let exe = bundle(
            &dir,
            &["ghostty/themes/", "ghostty/terminfo/78/xterm-ghostty"],
        );
        assert_eq!(
            find_resources(&exe),
            Err(LayoutError::MissingTerminfo(
                resources_dir(&dir).join("terminfo")
            ))
        );
    }

    #[test]
    fn rejects_missing_xterm_ghostty() {
        let dir = TempDir::new();
        let exe = bundle(&dir, &["ghostty/themes/", "terminfo/67/ghostty"]);
        assert_eq!(
            find_resources(&exe),
            Err(LayoutError::MissingXtermGhostty(
                resources_dir(&dir).join("terminfo/78/xterm-ghostty")
            ))
        );
    }

    #[test]
    fn rejects_executable_outside_bundle() {
        let dir = TempDir::new();
        let exe = dir.0.join("target/debug/shellio");
        assert_eq!(find_resources(&exe), Err(LayoutError::NotInBundle(exe)));
        // Каждое из двух имён проверяется отдельно.
        for path in ["X/MacOS/shellio", "shellio.app/Contents/bin/shellio"] {
            let exe = dir.0.join(path);
            assert_eq!(find_resources(&exe), Err(LayoutError::NotInBundle(exe)));
        }
        let root = Path::new("/shellio");
        assert_eq!(
            find_resources(root),
            Err(LayoutError::NotInBundle(root.to_path_buf()))
        );
    }

    #[test]
    fn reports_single_line_json() {
        let info = Info {
            build_mode: BuildMode::ReleaseFast,
            version: "1.4.0",
        };
        let resources = Resources {
            ghostty: PathBuf::from("/A/shellio.app/Contents/Resources/ghostty"),
            terminfo: PathBuf::from("/A/shellio.app/Contents/Resources/terminfo"),
        };
        assert_eq!(
            report(&info, &resources).unwrap(),
            format!(
                "{{\"app_version\":\"{APP_VERSION}\",\"ghostty_version\":\"1.4.0\",\
                 \"ghostty_build_mode\":\"ReleaseFast\",\
                 \"ghostty_resources\":\"/A/shellio.app/Contents/Resources/ghostty\",\
                 \"terminfo\":\"/A/shellio.app/Contents/Resources/terminfo\"}}"
            )
        );
    }

    #[test]
    fn escapes_json_strings() {
        let mut out = String::new();
        push_json_string(&mut out, "a\"b\\c\nd\te\u{1}f\u{1f}ё");
        assert_eq!(out, r#""a\"b\\c\nd\te\u0001f\u001fё""#);
    }

    #[test]
    fn names_every_build_mode() {
        for (mode, name) in [
            (BuildMode::Debug, "Debug"),
            (BuildMode::ReleaseSafe, "ReleaseSafe"),
            (BuildMode::ReleaseFast, "ReleaseFast"),
            (BuildMode::ReleaseSmall, "ReleaseSmall"),
        ] {
            assert_eq!(build_mode_name(mode), name);
        }
    }

    #[test]
    fn rejects_non_utf8_path() {
        let info = Info {
            build_mode: BuildMode::ReleaseFast,
            version: "1.4.0",
        };
        let ghostty = PathBuf::from(OsStr::from_bytes(b"/A/\xff/ghostty"));
        let resources = Resources {
            ghostty: ghostty.clone(),
            terminfo: PathBuf::from("/A/terminfo"),
        };
        assert!(matches!(
            report(&info, &resources),
            Err(Error::NonUtf8Path(path)) if path == ghostty
        ));
    }
}
