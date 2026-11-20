//! Файл песочниц `~/.config/shellio-probe-review/sandbox.toml` (вне git): облачные хосты, пути
//! репозиториев и имена элементов Keychain. Только перечисленные в нём репозитории пригодны для
//! записи; локальный GitLab CE настраивает сама проба, его в файле нет.
//!
//! ```toml
//! [keychain]                            # необязательно
//! service = "shellio-probe-review"      # по умолчанию
//!
//! [github_cloud]
//! repo = "<владелец>/shellio-review-sandbox"
//! author = "github-author"              # аккаунт элемента Keychain, по умолчанию
//! reviewer = "github-reviewer"          # по умолчанию
//!
//! [gitlab_cloud]
//! repo = "<пространство имён>/shellio-review-sandbox"
//! author = "gitlab-author"
//! reviewer = "gitlab-reviewer"
//! ```
//!
//! Разбирается подмножество TOML: таблицы `[имя]`, строки `ключ = "значение"` или
//! `ключ = 'значение'`, комментарии `#` и пустые строки. Всё остальное, неизвестная таблица или
//! ключ и повтор — ошибка разбора с номером строки: наполовину разобранный файл не даёт ни одного
//! репозитория.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::results::Column;

pub const DEFAULT_SERVICE: &str = "shellio-probe-review";
/// Путь файла относительно `$HOME`.
pub const RELATIVE_PATH: &str = ".config/shellio-probe-review/sandbox.toml";

/// `$HOME/.config/shellio-probe-review/sandbox.toml`; без `HOME` — `None`.
pub fn default_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(|home| PathBuf::from(home).join(RELATIVE_PATH))
}

/// Облачный хост песочницы.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostEntry {
    pub column: Column,
    /// Путь репозитория: `владелец/имя` у GitHub, `пространство имён/…/проект` у GitLab.
    pub repo: String,
    /// Аккаунт элемента Keychain с токеном автора.
    pub author: String,
    /// Аккаунт элемента Keychain с токеном ревьюера.
    pub reviewer: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sandbox {
    /// Сервис элементов Keychain.
    pub service: String,
    hosts: BTreeMap<&'static str, HostEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SandboxError {
    /// Файла нет: клетки облачных хостов — `not_run`.
    Missing,
    /// Файл есть, но не читается.
    Unreadable(String),
    /// Файл не разобран: строка (с 1) и причина.
    Parse { line: usize, reason: String },
}

impl SandboxError {
    /// Причина `not_run` клеток хоста, если ошибка — отсутствие предусловия.
    pub fn not_run_reason(&self) -> Option<String> {
        match self {
            SandboxError::Missing => Some("нет sandbox.toml".to_owned()),
            SandboxError::Unreadable(_) | SandboxError::Parse { .. } => None,
        }
    }
}

impl fmt::Display for SandboxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SandboxError::Missing => f.write_str("нет sandbox.toml"),
            SandboxError::Unreadable(err) => write!(f, "sandbox.toml не читается: {err}"),
            SandboxError::Parse { line, reason } => {
                write!(f, "sandbox.toml, строка {line}: {reason}")
            }
        }
    }
}

impl std::error::Error for SandboxError {}

impl Sandbox {
    pub fn load(path: &Path) -> Result<Sandbox, SandboxError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Sandbox::parse(&text),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Err(SandboxError::Missing),
            Err(err) => Err(SandboxError::Unreadable(err.kind().to_string())),
        }
    }

    pub fn parse(text: &str) -> Result<Sandbox, SandboxError> {
        let tables = parse_tables(text)?;
        let mut service = DEFAULT_SERVICE.to_owned();
        let mut hosts = BTreeMap::new();
        for table in tables {
            let parse_err = |reason: String| SandboxError::Parse {
                line: table.line,
                reason,
            };
            match table.name.as_str() {
                "keychain" => {
                    let mut keys = table.keys;
                    if let Some(value) = keys.remove("service") {
                        check_account("service", &value).map_err(parse_err)?;
                        service = value;
                    }
                    reject_unknown(&table.name, keys).map_err(parse_err)?;
                }
                "github_cloud" | "gitlab_cloud" => {
                    let column = if table.name == "github_cloud" {
                        Column::GithubCloud
                    } else {
                        Column::GitlabCloud
                    };
                    let mut keys = table.keys;
                    let repo = keys
                        .remove("repo")
                        .ok_or_else(|| parse_err(format!("в [{}] нет repo", table.name)))?;
                    check_repo(column, &repo).map_err(parse_err)?;
                    let (author_default, reviewer_default) = match column {
                        Column::GithubCloud => ("github-author", "github-reviewer"),
                        Column::Fake | Column::GitlabCloud | Column::GitlabCe | Column::Ghes => {
                            ("gitlab-author", "gitlab-reviewer")
                        }
                    };
                    let author = keys
                        .remove("author")
                        .unwrap_or_else(|| author_default.to_owned());
                    let reviewer = keys
                        .remove("reviewer")
                        .unwrap_or_else(|| reviewer_default.to_owned());
                    check_account("author", &author).map_err(parse_err)?;
                    check_account("reviewer", &reviewer).map_err(parse_err)?;
                    if author == reviewer {
                        return Err(parse_err(
                            "author и reviewer — один элемент Keychain: роли разные".to_owned(),
                        ));
                    }
                    reject_unknown(&table.name, keys).map_err(parse_err)?;
                    hosts.insert(
                        column.name(),
                        HostEntry {
                            column,
                            repo,
                            author,
                            reviewer,
                        },
                    );
                }
                "gitlab_ce" => {
                    return Err(parse_err(
                        "[gitlab_ce] не задаётся: локальный GitLab CE настраивает проба".to_owned(),
                    ));
                }
                other => return Err(parse_err(format!("неизвестная таблица [{other}]"))),
            }
        }
        Ok(Sandbox { service, hosts })
    }

    /// Хост столбца; нет хоста — причина `not_run` его клеток.
    pub fn host(&self, column: Column) -> Result<&HostEntry, String> {
        self.hosts
            .get(column.name())
            .ok_or_else(|| format!("нет хоста {} в sandbox.toml", column.name()))
    }

    /// Пригоден ли репозиторий для записи: путь перечислен у хоста столбца. GitHub и GitLab не
    /// различают регистр пути, поэтому и сравнение без учёта регистра ASCII.
    pub fn lists(&self, column: Column, repo: &str) -> bool {
        self.hosts
            .get(column.name())
            .is_some_and(|host| host.repo.eq_ignore_ascii_case(repo))
    }
}

fn reject_unknown(table: &str, keys: BTreeMap<String, String>) -> Result<(), String> {
    match keys.keys().next() {
        Some(key) => Err(format!("неизвестный ключ {key} в [{table}]")),
        None => Ok(()),
    }
}

fn check_account(key: &str, value: &str) -> Result<(), String> {
    if value.is_empty() || value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!("{key}: пустое значение или пробел"));
    }
    Ok(())
}

fn check_repo(column: Column, repo: &str) -> Result<(), String> {
    let segments: Vec<&str> = repo.split('/').collect();
    let shape = match column {
        Column::GithubCloud => segments.len() == 2,
        Column::Fake | Column::GitlabCloud | Column::GitlabCe | Column::Ghes => segments.len() >= 2,
    };
    let chars = segments.iter().all(|segment| {
        !segment.is_empty()
            && !segment.starts_with('.')
            && segment
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    });
    if shape && chars {
        Ok(())
    } else {
        Err(format!(
            "repo в [{}]: ожидается {}",
            column.name(),
            match column {
                Column::GithubCloud => "владелец/имя",
                Column::Fake | Column::GitlabCloud | Column::GitlabCe | Column::Ghes =>
                    "пространство имён/проект",
            }
        ))
    }
}

struct Table {
    name: String,
    /// Строка заголовка таблицы.
    line: usize,
    keys: BTreeMap<String, String>,
}

fn parse_tables(text: &str) -> Result<Vec<Table>, SandboxError> {
    let mut tables: Vec<Table> = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let line = index + 1;
        let err = |reason: String| SandboxError::Parse { line, reason };
        let content = raw.trim();
        if content.is_empty() || content.starts_with('#') {
            continue;
        }
        if let Some(rest) = content.strip_prefix('[') {
            let (name, tail) = rest
                .split_once(']')
                .ok_or_else(|| err("заголовок таблицы без ]".to_owned()))?;
            let name = name.trim();
            if !is_bare_key(name) {
                return Err(err(format!("имя таблицы «{name}» не простой ключ")));
            }
            check_tail(tail).map_err(err)?;
            if tables.iter().any(|table| table.name == name) {
                return Err(err(format!("таблица [{name}] повторяется")));
            }
            tables.push(Table {
                name: name.to_owned(),
                line,
                keys: BTreeMap::new(),
            });
            continue;
        }
        let (key, rest) = content
            .split_once('=')
            .ok_or_else(|| err("ожидается [таблица] или ключ = \"строка\"".to_owned()))?;
        let key = key.trim();
        if !is_bare_key(key) {
            return Err(err(format!("ключ «{key}» не простой ключ")));
        }
        let (value, tail) = parse_string(rest.trim_start()).map_err(err)?;
        check_tail(tail).map_err(err)?;
        let table = tables
            .last_mut()
            .ok_or_else(|| err(format!("ключ {key} вне таблицы")))?;
        if table.keys.insert(key.to_owned(), value).is_some() {
            return Err(err(format!("ключ {key} повторяется в [{}]", table.name)));
        }
    }
    Ok(tables)
}

fn is_bare_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
}

/// После значения или заголовка — только пробелы и комментарий.
fn check_tail(tail: &str) -> Result<(), String> {
    let tail = tail.trim_start();
    if tail.is_empty() || tail.starts_with('#') {
        Ok(())
    } else {
        Err(format!("лишнее после значения: «{tail}»"))
    }
}

/// Строка TOML в начале текста: базовая `"…"` с экранированием `\"` и `\\` или литеральная
/// `'…'`. Возвращает значение и остаток.
fn parse_string(text: &str) -> Result<(String, &str), String> {
    let mut chars = text.char_indices();
    match chars.next() {
        Some((_, '\'')) => {
            let body = &text[1..];
            let end = body
                .find('\'')
                .ok_or_else(|| "литеральная строка без закрывающей '".to_owned())?;
            let value = &body[..end];
            if value.chars().any(char::is_control) {
                return Err("управляющий символ в строке".to_owned());
            }
            Ok((value.to_owned(), &body[end + 1..]))
        }
        Some((_, '"')) => {
            let mut value = String::new();
            let mut escaped = false;
            for (index, c) in chars {
                if escaped {
                    match c {
                        '"' | '\\' => value.push(c),
                        other => return Err(format!("экранирование \\{other} не поддерживается")),
                    }
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == '"' {
                    return Ok((value, &text[index + 1..]));
                } else if c.is_control() {
                    return Err("управляющий символ в строке".to_owned());
                } else {
                    value.push(c);
                }
            }
            Err("строка без закрывающей \"".to_owned())
        }
        _ => Err("значение — только строка в кавычках".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"
# песочницы пробы P0.7
[keychain]
service = "probe-svc"   # свой сервис

[github_cloud]
repo = "owner-a/sandbox"
author = "gh-a"
reviewer = 'gh-r'

[gitlab_cloud]
repo = "group/sub/sandbox"
"#;

    fn parse_error(text: &str) -> (usize, String) {
        match Sandbox::parse(text) {
            Err(SandboxError::Parse { line, reason }) => (line, reason),
            other => panic!("ожидалась ошибка разбора, получено {other:?}"),
        }
    }

    #[test]
    fn parses_hosts_service_and_defaults() -> Result<(), SandboxError> {
        let sandbox = Sandbox::parse(FULL)?;
        assert_eq!(sandbox.service, "probe-svc");
        assert_eq!(
            sandbox.host(Column::GithubCloud),
            Ok(&HostEntry {
                column: Column::GithubCloud,
                repo: "owner-a/sandbox".to_owned(),
                author: "gh-a".to_owned(),
                reviewer: "gh-r".to_owned(),
            })
        );
        assert_eq!(
            sandbox.host(Column::GitlabCloud),
            Ok(&HostEntry {
                column: Column::GitlabCloud,
                repo: "group/sub/sandbox".to_owned(),
                author: "gitlab-author".to_owned(),
                reviewer: "gitlab-reviewer".to_owned(),
            })
        );
        assert!(sandbox.lists(Column::GithubCloud, "Owner-A/Sandbox"));
        assert!(!sandbox.lists(Column::GithubCloud, "owner-a/other"));
        assert!(!sandbox.lists(Column::GitlabCloud, "owner-a/sandbox"));
        assert!(!sandbox.lists(Column::GitlabCe, "group/sub/sandbox"));
        Ok(())
    }

    #[test]
    fn default_service_and_missing_host_is_not_run() -> Result<(), SandboxError> {
        let sandbox = Sandbox::parse("[gitlab_cloud]\nrepo = \"g/p\"\n")?;
        assert_eq!(sandbox.service, DEFAULT_SERVICE);
        assert_eq!(
            sandbox.host(Column::GithubCloud),
            Err("нет хоста github_cloud в sandbox.toml".to_owned())
        );
        let empty = Sandbox::parse("# пусто\n\n")?;
        assert!(empty.host(Column::GitlabCloud).is_err());
        Ok(())
    }

    #[test]
    fn missing_file_is_not_run_and_parse_error_is_not() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let missing = Sandbox::load(&dir.path().join("sandbox.toml"));
        assert_eq!(missing, Err(SandboxError::Missing));
        assert_eq!(
            SandboxError::Missing.not_run_reason().as_deref(),
            Some("нет sandbox.toml")
        );
        let path = dir.path().join("broken.toml");
        std::fs::write(&path, "[github_cloud]\nrepo = owner/x\n").map_err(|e| e.to_string())?;
        let broken = Sandbox::load(&path);
        assert!(matches!(broken, Err(SandboxError::Parse { line: 2, .. })));
        assert_eq!(broken.err().and_then(|e| e.not_run_reason()), None);
        Ok(())
    }

    #[test]
    fn rejects_everything_outside_the_subset() {
        let cases: [(&str, usize); 14] = [
            ("repo = \"a/b\"\n", 1),
            ("[github_cloud]\nrepo = \"a/b\"\n[github_cloud]\n", 3),
            ("[github_cloud]\nrepo = \"a/b\"\nrepo = \"c/d\"\n", 3),
            ("[github_cloud]\nrepo = \"a/b\"\ntoken = \"x\"\n", 1),
            ("[github_cloud]\nauthor = \"a\"\n", 1),
            ("[github_cloud]\nrepo = \"a/b/c\"\n", 1),
            ("[github_cloud]\nrepo = \"a/../b\"\n", 1),
            ("[gitlab_cloud]\nrepo = \"single\"\n", 1),
            ("[gitlab_ce]\nrepo = \"a/b\"\n", 1),
            ("[ghes]\n", 1),
            ("[github_cloud]\nrepo = \"a/b\" extra\n", 2),
            ("[github_cloud]\nrepo = \"a\\nb\"\n", 2),
            ("[github_cloud]\nrepo = [\"a/b\"]\n", 2),
            (
                "[github_cloud]\nrepo = \"a/b\"\nauthor = \"same\"\nreviewer = \"same\"\n",
                1,
            ),
        ];
        for (text, line) in cases {
            let (got, reason) = parse_error(text);
            assert_eq!(got, line, "{text:?}: {reason}");
        }
        let (_, reason) = parse_error("[github_cloud]\nrepo = \"a/b\nauthor = \"x\"\n");
        assert!(
            reason.contains("лишнее") || reason.contains("закрывающей"),
            "{reason}"
        );
        let (_, reason) = parse_error("[dotted.name]\n");
        assert!(reason.contains("простой ключ"), "{reason}");
    }

    #[test]
    fn escapes_in_basic_strings() -> Result<(), SandboxError> {
        let sandbox = Sandbox::parse("[keychain]\nservice = \"svc\\\\x\"\n")?;
        assert_eq!(sandbox.service, "svc\\x");
        Ok(())
    }

    #[test]
    fn default_path_is_under_home() {
        if let Some(path) = default_path() {
            assert!(path.ends_with(RELATIVE_PATH));
        }
    }
}
