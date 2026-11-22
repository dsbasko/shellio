//! Профиль: каталоги состояния и конфигурации, вычисленные из окружения (ADR-0018).

use std::ffi::OsString;
use std::fmt;
use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

/// Переменная, перенаправляющая состояние и конфигурацию в один каталог.
pub const STATE_DIR_VAR: &str = "SHELLIO_STATE_DIR";

const HOME_VAR: &str = "HOME";
const STATE_DIR_MODE: u32 = 0o700;

/// Каталоги профиля shellio.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    state_dir: PathBuf,
    config_dir: PathBuf,
    explicit: bool,
}

impl Profile {
    /// Профиль по переданному окружению. `SHELLIO_STATE_DIR` важнее `HOME`:
    /// заданная переменная с неверным значением — ошибка, а не переход на настоящий каталог.
    pub fn resolve(env: impl Fn(&str) -> Option<OsString>) -> Result<Self, ProfileError> {
        if let Some(value) = env(STATE_DIR_VAR) {
            if value.is_empty() {
                return Err(ProfileError::StateDirEmpty);
            }
            let dir = PathBuf::from(value);
            if !dir.is_absolute() {
                return Err(ProfileError::StateDirNotAbsolute { value: dir });
            }
            return Ok(Self::isolated(dir));
        }
        let home = env(HOME_VAR).ok_or(ProfileError::HomeMissing)?;
        if home.is_empty() {
            return Err(ProfileError::HomeEmpty);
        }
        let home = PathBuf::from(home);
        if !home.is_absolute() {
            return Err(ProfileError::HomeNotAbsolute { value: home });
        }
        Ok(Self {
            state_dir: home.join("Library/Application Support/shellio"),
            config_dir: home.join(".config/shellio"),
            explicit: false,
        })
    }

    /// Профиль по окружению процесса. Единственное место, где хранилище читает окружение.
    pub fn from_process_env() -> Result<Self, ProfileError> {
        Self::resolve(|name| std::env::var_os(name))
    }

    /// Профиль в указанном каталоге с раскладкой `SHELLIO_STATE_DIR`. Каталог задан явно.
    pub fn isolated(dir: impl Into<PathBuf>) -> Self {
        let state_dir = dir.into();
        let config_dir = state_dir.join("config");
        Self {
            state_dir,
            config_dir,
            explicit: true,
        }
    }

    /// Каталог задан явно (`SHELLIO_STATE_DIR` или [`Profile::isolated`]), а не выведен из `HOME`.
    /// Временность каталога признак не доказывает.
    pub fn is_explicit(&self) -> bool {
        self.explicit
    }

    /// Каталог состояния.
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// Каталог конфигурации. В P1.1 не создаётся.
    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    /// Путь базы состояния.
    pub fn state_db(&self) -> PathBuf {
        self.state_dir.join("state.db")
    }

    /// Каталог резервных копий базы перед миграцией.
    pub fn backups_dir(&self) -> PathBuf {
        self.state_dir.join("backups")
    }

    /// Создать каталог состояния с правами `0700`, если его нет, вместе с недостающими
    /// родителями. Права уже существующего каталога не меняются.
    pub fn ensure_state_dir(&self) -> Result<(), ProfileError> {
        let path = &self.state_dir;
        match DirBuilder::new()
            .recursive(true)
            .mode(STATE_DIR_MODE)
            .create(path)
        {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotADirectory => {
                Err(ProfileError::NotADirectory {
                    path: occupied_ancestor(path),
                })
            }
            Err(error) => match fs::metadata(path) {
                Ok(metadata) if !metadata.is_dir() => {
                    Err(ProfileError::NotADirectory { path: path.clone() })
                }
                _ => Err(ProfileError::Io {
                    path: path.clone(),
                    source: error,
                }),
            },
        }
    }
}

/// Ближайший существующий предок пути, который не каталог; иначе сам путь.
fn occupied_ancestor(path: &Path) -> PathBuf {
    path.ancestors()
        .find(|ancestor| fs::metadata(ancestor).is_ok_and(|metadata| !metadata.is_dir()))
        .unwrap_or(path)
        .to_path_buf()
}

/// Ошибка профиля.
#[derive(Debug)]
pub enum ProfileError {
    /// `HOME` не задан, а `SHELLIO_STATE_DIR` нет.
    HomeMissing,
    /// `HOME` пуст.
    HomeEmpty,
    /// `HOME` не абсолютный.
    HomeNotAbsolute {
        /// Значение переменной.
        value: PathBuf,
    },
    /// `SHELLIO_STATE_DIR` задан пустым.
    StateDirEmpty,
    /// `SHELLIO_STATE_DIR` не абсолютный.
    StateDirNotAbsolute {
        /// Значение переменной.
        value: PathBuf,
    },
    /// Путь каталога состояния или его предка занят не каталогом.
    NotADirectory {
        /// Путь, занятый не каталогом.
        path: PathBuf,
    },
    /// Каталог состояния не создан.
    Io {
        /// Каталог состояния.
        path: PathBuf,
        /// Ошибка ввода-вывода.
        source: io::Error,
    },
}

impl fmt::Display for ProfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HomeMissing => write!(f, "не задан {HOME_VAR}, а {STATE_DIR_VAR} нет"),
            Self::HomeEmpty => write!(f, "{HOME_VAR} пуст"),
            Self::HomeNotAbsolute { value } => {
                write!(f, "{HOME_VAR}={} не абсолютный путь", value.display())
            }
            Self::StateDirEmpty => write!(f, "{STATE_DIR_VAR} задан пустым"),
            Self::StateDirNotAbsolute { value } => {
                write!(f, "{STATE_DIR_VAR}={} не абсолютный путь", value.display())
            }
            Self::NotADirectory { path } => {
                write!(f, "путь {} занят не каталогом", path.display())
            }
            Self::Io { path, source } => {
                write!(
                    f,
                    "каталог состояния {} не создан: {source}",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for ProfileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::HomeMissing
            | Self::HomeEmpty
            | Self::HomeNotAbsolute { .. }
            | Self::StateDirEmpty
            | Self::StateDirNotAbsolute { .. }
            | Self::NotADirectory { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    type Vars<'a> = &'a [(&'a str, &'a str)];

    fn resolve_with(vars: Vars<'_>) -> (Result<Profile, ProfileError>, Vec<String>) {
        let read = RefCell::new(Vec::new());
        let result = Profile::resolve(|name| {
            assert_ne!(name, "XDG_CONFIG_HOME", "профиль не читает XDG_CONFIG_HOME");
            read.borrow_mut().push(name.to_owned());
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(value))
        });
        (result, read.into_inner())
    }

    fn profile(state: &str, config: &str, explicit: bool) -> Profile {
        Profile {
            state_dir: PathBuf::from(state),
            config_dir: PathBuf::from(config),
            explicit,
        }
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn environment_table() {
        let home_layout = profile(
            "/Users/test/Library/Application Support/shellio",
            "/Users/test/.config/shellio",
            false,
        );
        let state_layout = profile("/tmp/shellio-state", "/tmp/shellio-state/config", true);
        let cases: [(&str, Vars<'_>, Result<Profile, &str>); 9] = [
            (
                "без SHELLIO_STATE_DIR",
                &[("HOME", "/Users/test")],
                Ok(home_layout.clone()),
            ),
            (
                "XDG_CONFIG_HOME игнорируется",
                &[("HOME", "/Users/test"), ("XDG_CONFIG_HOME", "/xdg")],
                Ok(home_layout),
            ),
            (
                "абсолютный SHELLIO_STATE_DIR",
                &[
                    ("SHELLIO_STATE_DIR", "/tmp/shellio-state"),
                    ("HOME", "/Users/test"),
                ],
                Ok(state_layout.clone()),
            ),
            (
                "абсолютный SHELLIO_STATE_DIR без HOME",
                &[("SHELLIO_STATE_DIR", "/tmp/shellio-state")],
                Ok(state_layout),
            ),
            (
                "пустой SHELLIO_STATE_DIR при верном HOME",
                &[("SHELLIO_STATE_DIR", ""), ("HOME", "/Users/test")],
                Err("state_dir_empty"),
            ),
            (
                "относительный SHELLIO_STATE_DIR при верном HOME",
                &[("SHELLIO_STATE_DIR", "state"), ("HOME", "/Users/test")],
                Err("state_dir_not_absolute:state"),
            ),
            ("без HOME", &[], Err("home_missing")),
            ("пустой HOME", &[("HOME", "")], Err("home_empty")),
            (
                "относительный HOME",
                &[("HOME", "home/test")],
                Err("home_not_absolute:home/test"),
            ),
        ];
        for (name, vars, expected) in cases {
            let (result, _) = resolve_with(vars);
            let actual = result.map_err(|error| match error {
                ProfileError::HomeMissing => "home_missing".to_owned(),
                ProfileError::HomeEmpty => "home_empty".to_owned(),
                ProfileError::HomeNotAbsolute { value } => {
                    format!("home_not_absolute:{}", value.display())
                }
                ProfileError::StateDirEmpty => "state_dir_empty".to_owned(),
                ProfileError::StateDirNotAbsolute { value } => {
                    format!("state_dir_not_absolute:{}", value.display())
                }
                other => format!("unexpected:{other}"),
            });
            assert_eq!(actual, expected.map_err(str::to_owned), "{name}");
        }
    }

    #[test]
    fn only_state_dir_and_home_are_read() {
        let (_, read) = resolve_with(&[("HOME", "/Users/test"), ("XDG_CONFIG_HOME", "/xdg")]);
        assert_eq!(read, ["SHELLIO_STATE_DIR", "HOME"]);
        let (_, read) = resolve_with(&[("SHELLIO_STATE_DIR", "/tmp/shellio-state")]);
        assert_eq!(read, ["SHELLIO_STATE_DIR"]);
    }

    #[test]
    fn explicit_only_when_state_dir_is_given() {
        let (given, _) = resolve_with(&[
            ("SHELLIO_STATE_DIR", "/tmp/shellio-state"),
            ("HOME", "/Users/test"),
        ]);
        assert!(given.unwrap().is_explicit(), "заданный SHELLIO_STATE_DIR");
        let (absent, _) = resolve_with(&[("HOME", "/Users/test")]);
        assert!(!absent.unwrap().is_explicit(), "профиль из HOME");
        let (empty, _) = resolve_with(&[("SHELLIO_STATE_DIR", ""), ("HOME", "/Users/test")]);
        assert!(
            matches!(empty, Err(ProfileError::StateDirEmpty)),
            "пустой SHELLIO_STATE_DIR не становится профилем из HOME: {empty:?}"
        );
        assert!(Profile::isolated("/tmp/shellio-state").is_explicit());
    }

    #[test]
    fn isolated_matches_state_dir_variable() {
        let (resolved, _) = resolve_with(&[("SHELLIO_STATE_DIR", "/tmp/shellio-state")]);
        assert_eq!(resolved.unwrap(), Profile::isolated("/tmp/shellio-state"));
    }

    #[test]
    fn database_and_backups_live_in_state_dir() {
        let profile = Profile::isolated("/tmp/shellio-state");
        assert_eq!(profile.state_dir(), Path::new("/tmp/shellio-state"));
        assert_eq!(profile.config_dir(), Path::new("/tmp/shellio-state/config"));
        assert_eq!(profile.state_db(), Path::new("/tmp/shellio-state/state.db"));
        assert_eq!(
            profile.backups_dir(),
            Path::new("/tmp/shellio-state/backups")
        );
    }

    #[test]
    fn new_state_dir_gets_owner_only_mode() {
        let temp = tempfile::tempdir().unwrap();
        let profile = Profile::isolated(temp.path().join("state"));
        profile.ensure_state_dir().unwrap();
        assert_eq!(mode(profile.state_dir()), 0o700);
        assert!(
            !profile.config_dir().exists(),
            "каталог конфигурации не создаётся"
        );
        profile.ensure_state_dir().unwrap();
        assert_eq!(mode(profile.state_dir()), 0o700);
    }

    #[test]
    fn missing_parents_are_created_owner_only() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("a");
        let profile = Profile::isolated(parent.join("b"));
        profile.ensure_state_dir().unwrap();
        assert_eq!(mode(&parent), 0o700);
        assert_eq!(mode(profile.state_dir()), 0o700);
    }

    #[test]
    fn existing_state_dir_keeps_its_mode() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("state");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(dir.join("state.db"), b"data").unwrap();
        Profile::isolated(&dir).ensure_state_dir().unwrap();
        assert_eq!(mode(&dir), 0o755);
        assert_eq!(fs::read(dir.join("state.db")).unwrap(), b"data");
    }

    #[test]
    fn state_dir_taken_by_file_is_not_a_directory() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("state");
        fs::write(&file, b"file").unwrap();
        let error = Profile::isolated(&file).ensure_state_dir().unwrap_err();
        assert!(
            matches!(&error, ProfileError::NotADirectory { path } if *path == file),
            "{error:?}"
        );
        assert_eq!(fs::read(&file).unwrap(), b"file");
    }

    #[test]
    fn file_in_the_middle_of_state_path_is_not_a_directory() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("file");
        fs::write(&file, b"file").unwrap();
        let error = Profile::isolated(file.join("state"))
            .ensure_state_dir()
            .unwrap_err();
        assert!(
            matches!(&error, ProfileError::NotADirectory { path } if *path == file),
            "{error:?}"
        );
        assert_eq!(fs::read(&file).unwrap(), b"file");
    }

    #[test]
    fn symlink_to_directory_is_accepted() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o750)).unwrap();
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        Profile::isolated(&link).ensure_state_dir().unwrap();
        assert_eq!(mode(&target), 0o750);
    }
}
