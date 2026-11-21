//! Корни проектов: вид, ключ уникальности и порт их разрешения.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Вид корня проекта.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootKind {
    /// Репозиторий git или его связанная worktree.
    Git,
    /// Обычная папка.
    Folder,
}

impl RootKind {
    /// Имя вида в хранилище и JSON.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Folder => "folder",
        }
    }

    /// Вид по имени из хранилища. Неизвестное имя — `None`.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "git" => Some(Self::Git),
            "folder" => Some(Self::Folder),
            _ => None,
        }
    }
}

/// Ключ уникальности корня: канонический общий git-каталог у репозитория,
/// канонический путь у папки (ADR-0025).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RootKey(String);

impl RootKey {
    /// Ключ из готовой строки.
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    /// Ключ как строка.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RootKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Корень, подготовленный до актора: канонический путь, вид и ключ.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedRoot {
    /// Канонический путь выбранного каталога.
    pub path: PathBuf,
    /// Вид корня.
    pub kind: RootKind,
    /// Ключ уникальности.
    pub key: RootKey,
}

/// Отказ разрешения корня.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RootError {
    /// Путь не абсолютный.
    NotAbsolute { path: PathBuf },
    /// Пути нет.
    NotFound { path: PathBuf },
    /// Путь ведёт не на каталог.
    NotADirectory { path: PathBuf },
    /// Путь или файл, нужный для разрешения, не прочитан: нет прав, петля ссылок и прочее.
    Unreadable { path: PathBuf, message: String },
    /// Путь не в UTF-8: хранилище держит путь и ключ корня строкой.
    NotUtf8 { path: PathBuf },
    /// Файл `.git` или `commondir` не ведёт на существующий каталог git.
    GitLinkInvalid { path: PathBuf },
}

impl RootError {
    /// Стабильная причина, которая уходит в `invalid_argument`.
    pub const fn reason(&self) -> &'static str {
        match self {
            Self::NotAbsolute { .. } => "path_not_absolute",
            Self::NotFound { .. } => "path_not_found",
            Self::NotADirectory { .. } => "not_a_directory",
            Self::Unreadable { .. } => "path_unreadable",
            Self::NotUtf8 { .. } => "path_not_utf8",
            Self::GitLinkInvalid { .. } => "git_link_invalid",
        }
    }

    /// Путь, о котором идёт речь.
    pub fn path(&self) -> &Path {
        match self {
            Self::NotAbsolute { path }
            | Self::NotFound { path }
            | Self::NotADirectory { path }
            | Self::Unreadable { path, .. }
            | Self::NotUtf8 { path }
            | Self::GitLinkInvalid { path } => path,
        }
    }

    fn unreadable(path: &Path, error: &io::Error) -> Self {
        Self::Unreadable {
            path: path.to_path_buf(),
            message: error.to_string(),
        }
    }

    fn git_link_invalid(path: &Path) -> Self {
        Self::GitLinkInvalid {
            path: path.to_path_buf(),
        }
    }
}

impl fmt::Display for RootError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = self.path().display();
        match self {
            Self::NotAbsolute { .. } => write!(f, "путь {path} не абсолютный"),
            Self::NotFound { .. } => write!(f, "пути {path} нет"),
            Self::NotADirectory { .. } => write!(f, "путь {path} ведёт не на каталог"),
            Self::Unreadable { message, .. } => write!(f, "путь {path} не прочитан: {message}"),
            Self::NotUtf8 { .. } => write!(f, "путь {path} не в UTF-8"),
            Self::GitLinkInvalid { .. } => {
                write!(f, "файл {path} не ведёт на существующий каталог git")
            }
        }
    }
}

impl std::error::Error for RootError {}

/// Порт разрешения корня. Блокирующий: вызывается вне актора и главного потока.
pub trait RootResolver: Send + Sync {
    /// Разрешить путь, указанный пользователем.
    fn resolve(&self, path: &Path) -> Result<ResolvedRoot, RootError>;
}

/// Разрешение корня на файловой системе без запуска `git` и без поиска `.git` вверх.
///
/// Путь канонизируется. `.git` каталогом — репозиторий с ключом по этому каталогу. `.git`
/// файлом `gitdir: <x>` — worktree с ключом по `<x>/<commondir>` или по `<x>`, если файла
/// `commondir` нет. Иначе папка с ключом по её каноническому пути.
#[derive(Clone, Copy, Debug, Default)]
pub struct FsRootResolver;

impl RootResolver for FsRootResolver {
    fn resolve(&self, path: &Path) -> Result<ResolvedRoot, RootError> {
        if !path.is_absolute() {
            return Err(RootError::NotAbsolute {
                path: path.to_path_buf(),
            });
        }
        let canonical = fs::canonicalize(path).map_err(|error| match error.kind() {
            // Файл посреди пути: пути, указанного пользователем, нет.
            io::ErrorKind::NotFound | io::ErrorKind::NotADirectory => RootError::NotFound {
                path: path.to_path_buf(),
            },
            _ => RootError::unreadable(path, &error),
        })?;
        let metadata =
            fs::metadata(&canonical).map_err(|error| RootError::unreadable(&canonical, &error))?;
        if !metadata.is_dir() {
            return Err(RootError::NotADirectory {
                path: path.to_path_buf(),
            });
        }
        let (kind, key) = match common_git_dir(&canonical)? {
            Some(git_dir) => (RootKind::Git, git_dir),
            None => (RootKind::Folder, canonical.clone()),
        };
        utf8(&canonical)?;
        Ok(ResolvedRoot {
            key: RootKey::new(utf8(&key)?),
            path: canonical,
            kind,
        })
    }
}

/// Общий git-каталог для канонического каталога `dir` или `None`, если это не корень git.
fn common_git_dir(dir: &Path) -> Result<Option<PathBuf>, RootError> {
    let dot_git = dir.join(".git");
    match fs::metadata(&dot_git) {
        Ok(metadata) if metadata.is_dir() => canonical_dir(&dot_git, &dot_git).map(Some),
        Ok(metadata) if metadata.is_file() => linked_git_dir(dir, &dot_git).map(Some),
        Ok(_) => Ok(None),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(RootError::unreadable(&dot_git, &error)),
    }
}

/// Общий git-каталог worktree по файлу `.git` с содержимым `gitdir: <x>`. Относительный
/// `<x>` отсчитывается от `dir`, относительный `commondir` — от `<x>`, как у git.
fn linked_git_dir(dir: &Path, dot_git: &Path) -> Result<PathBuf, RootError> {
    let text = read_link_file(dot_git)?.ok_or_else(|| RootError::git_link_invalid(dot_git))?;
    let target = text
        .strip_prefix("gitdir: ")
        .map(str::trim_end)
        .filter(|target| !target.is_empty())
        .ok_or_else(|| RootError::git_link_invalid(dot_git))?;
    let git_dir = canonical_dir(&dir.join(target), dot_git)?;
    let commondir = git_dir.join("commondir");
    let Some(text) = read_link_file(&commondir)? else {
        return Ok(git_dir);
    };
    let common = text.trim_end_matches(['\n', '\r']);
    if common.is_empty() {
        return Err(RootError::git_link_invalid(&commondir));
    }
    canonical_dir(&git_dir.join(common), &commondir)
}

/// Текст файла ссылки git. Нет файла — `None`, не обычный файл — ссылка неверна.
fn read_link_file(path: &Path) -> Result<Option<String>, RootError> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => return Err(RootError::git_link_invalid(path)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(RootError::unreadable(path, &error)),
    }
    let bytes = fs::read(path).map_err(|error| RootError::unreadable(path, &error))?;
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| RootError::NotUtf8 {
            path: path.to_path_buf(),
        })
}

/// Канонический путь каталога, на который ведёт ссылка из файла `link`.
fn canonical_dir(path: &Path, link: &Path) -> Result<PathBuf, RootError> {
    match fs::canonicalize(path) {
        Ok(canonical) if canonical.is_dir() => Ok(canonical),
        Ok(_) => Err(RootError::git_link_invalid(link)),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            Err(RootError::git_link_invalid(link))
        }
        Err(error) => Err(RootError::unreadable(path, &error)),
    }
}

fn utf8(path: &Path) -> Result<&str, RootError> {
    path.to_str().ok_or_else(|| RootError::NotUtf8 {
        path: path.to_path_buf(),
    })
}

/// Поддельное разрешение для тестов без файловой системы.
///
/// Относительный путь — [`RootError::NotAbsolute`]. Зарегистрированный путь получает свой
/// результат. Остальные абсолютные пути — папки с ключом, равным пути.
#[cfg(feature = "test-support")]
#[derive(Clone, Debug, Default)]
pub struct FakeRootResolver {
    entries: std::collections::BTreeMap<PathBuf, Result<ResolvedRoot, RootError>>,
}

#[cfg(feature = "test-support")]
impl FakeRootResolver {
    /// Разрешение без зарегистрированных путей.
    pub fn new() -> Self {
        Self::default()
    }

    /// Зарегистрировать результат для пути.
    #[must_use]
    pub fn with(
        mut self,
        path: impl Into<PathBuf>,
        result: Result<ResolvedRoot, RootError>,
    ) -> Self {
        self.entries.insert(path.into(), result);
        self
    }

    /// Зарегистрировать путь как репозиторий git с ключом `key`.
    #[must_use]
    pub fn with_git(self, path: impl Into<PathBuf>, key: impl Into<String>) -> Self {
        let path = path.into();
        let root = ResolvedRoot {
            path: path.clone(),
            kind: RootKind::Git,
            key: RootKey::new(key),
        };
        self.with(path, Ok(root))
    }
}

#[cfg(feature = "test-support")]
impl RootResolver for FakeRootResolver {
    fn resolve(&self, path: &Path) -> Result<ResolvedRoot, RootError> {
        if !path.is_absolute() {
            return Err(RootError::NotAbsolute {
                path: path.to_path_buf(),
            });
        }
        if let Some(result) = self.entries.get(path) {
            return result.clone();
        }
        Ok(ResolvedRoot {
            path: path.to_path_buf(),
            kind: RootKind::Folder,
            key: RootKey::new(path.to_string_lossy()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_names_round_trip() {
        for kind in [RootKind::Git, RootKind::Folder] {
            assert_eq!(RootKind::parse(kind.as_str()), Some(kind));
            assert_eq!(
                serde_json::to_string(&kind).unwrap(),
                format!("\"{}\"", kind.as_str())
            );
        }
        assert_eq!(RootKind::parse("Git"), None);
        assert_eq!(RootKind::parse("svn"), None);
    }

    #[test]
    fn error_reasons_are_stable() {
        let path = PathBuf::from("x");
        let cases = [
            (
                RootError::NotAbsolute { path: path.clone() },
                "path_not_absolute",
            ),
            (RootError::NotFound { path: path.clone() }, "path_not_found"),
            (
                RootError::NotADirectory { path: path.clone() },
                "not_a_directory",
            ),
            (
                RootError::Unreadable {
                    path: path.clone(),
                    message: "отказано в доступе".into(),
                },
                "path_unreadable",
            ),
            (RootError::NotUtf8 { path: path.clone() }, "path_not_utf8"),
            (
                RootError::GitLinkInvalid { path: path.clone() },
                "git_link_invalid",
            ),
        ];
        for (error, reason) in cases {
            assert_eq!(error.reason(), reason);
            assert_eq!(error.path(), path);
            assert!(error.to_string().contains('x'));
        }
    }

    #[test]
    fn fake_resolver_defaults_to_folder_and_honours_entries() {
        let resolver: std::sync::Arc<dyn RootResolver> = std::sync::Arc::new(
            FakeRootResolver::new()
                .with_git("/repo/wt", "/repo/.git")
                .with(
                    "/missing",
                    Err(RootError::NotFound {
                        path: "/missing".into(),
                    }),
                ),
        );

        let folder = resolver.resolve(Path::new("/work/notes")).unwrap();
        assert_eq!(folder.kind, RootKind::Folder);
        assert_eq!(folder.key.as_str(), "/work/notes");
        assert_eq!(folder.path, Path::new("/work/notes"));

        let git = resolver.resolve(Path::new("/repo/wt")).unwrap();
        assert_eq!(git.kind, RootKind::Git);
        assert_eq!(git.key, RootKey::new("/repo/.git"));

        assert_eq!(
            resolver
                .resolve(Path::new("/missing"))
                .unwrap_err()
                .reason(),
            "path_not_found"
        );
        assert_eq!(
            resolver.resolve(Path::new("rel")).unwrap_err().reason(),
            "path_not_absolute"
        );
    }

    #[test]
    fn path_outside_utf8_is_refused() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let path = Path::new(OsStr::from_bytes(b"/src/\xff\xfe"));
        let error = utf8(path).unwrap_err();
        assert_eq!(error.reason(), "path_not_utf8");
        assert_eq!(error.path(), path);
        assert_eq!(utf8(Path::new("/src/проект")).unwrap(), "/src/проект");
    }
}
