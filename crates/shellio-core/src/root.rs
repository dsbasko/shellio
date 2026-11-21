//! Корни проектов: вид, ключ уникальности и порт их разрешения.

use std::fmt;
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
}

impl RootError {
    /// Стабильная причина, которая уходит в `invalid_argument`.
    pub const fn reason(&self) -> &'static str {
        match self {
            Self::NotAbsolute { .. } => "path_not_absolute",
            Self::NotFound { .. } => "path_not_found",
            Self::NotADirectory { .. } => "not_a_directory",
        }
    }

    /// Путь, о котором идёт речь.
    pub fn path(&self) -> &Path {
        match self {
            Self::NotAbsolute { path } | Self::NotFound { path } | Self::NotADirectory { path } => {
                path
            }
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
        }
    }
}

impl std::error::Error for RootError {}

/// Порт разрешения корня. Блокирующий: вызывается вне актора и главного потока.
pub trait RootResolver: Send + Sync {
    /// Разрешить путь, указанный пользователем.
    fn resolve(&self, path: &Path) -> Result<ResolvedRoot, RootError>;
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
}
