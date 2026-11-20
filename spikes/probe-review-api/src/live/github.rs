//! Жизненный цикл клетки на GitHub: refs, Contents API, pulls и `PENDING` review своего аккаунта.

use serde_json::{Value, json};

use crate::github::API_VERSION;
use crate::model::Provider;
use crate::transport::{Method, Request, Transport, percent_encode};

use super::guard::{GuardError, Guarded, RepoInfo};
use super::repo::{
    Account, FileChange, LiveError, LiveRepo, base64, call, id_of, json, pages, str_of, u64_of,
};

const ACCEPT: &str = "application/vnd.github+json";
const PER_PAGE: usize = 100;
pub const PULL_BODY: &str = "Проба P0.7: PR и его ветки удаляются после прогона клетки.";

/// Репозиторий GitHub от имени роли, чей токен держит транспорт сторожа.
pub struct GitHubRepo<'a, T> {
    guard: &'a Guarded<T>,
    owner: String,
}

impl<'a, T: Transport> GitHubRepo<'a, T> {
    pub fn new(guard: &'a Guarded<T>) -> Result<GitHubRepo<'a, T>, String> {
        let target = guard.target();
        if target.provider != Provider::GitHub {
            return Err("сторож не GitHub".to_owned());
        }
        match target.repo.split_once('/') {
            Some((owner, name)) if !owner.is_empty() && !name.is_empty() && !name.contains('/') => {
                Ok(GitHubRepo {
                    guard,
                    owner: owner.to_owned(),
                })
            }
            _ => Err("путь репозитория GitHub — владелец/имя".to_owned()),
        }
    }

    fn request(&self, method: Method, url: String) -> Request {
        Request::new(method, url)
            .header("accept", ACCEPT)
            .header("x-github-api-version", API_VERSION)
    }

    fn repo(&self, method: Method, path: &str) -> Request {
        self.request(method, format!("{}{path}", self.guard.target().repo_url()))
    }
}

/// Сегменты пути по отдельности в процентной кодировке, `/` между ними сохраняется.
fn segments(path: &str) -> String {
    path.split('/')
        .map(percent_encode)
        .collect::<Vec<_>>()
        .join("/")
}

impl<T: Transport> LiveRepo for GitHubRepo<'_, T> {
    fn pull_word(&self) -> &'static str {
        "PR"
    }

    fn check(&self) -> Result<RepoInfo, GuardError> {
        self.guard.check()
    }

    fn whoami(&self) -> Result<Account, LiveError> {
        let request = self.request(Method::Get, format!("{}/user", self.guard.target().rest));
        let user = json(self.guard, &request)?;
        Ok(Account {
            login: str_of(&user, "/login")?,
            id: u64_of(&user, "/id")?,
        })
    }

    fn branch_head(&self, branch: &str) -> Result<Option<String>, LiveError> {
        let request = self.repo(Method::Get, &format!("/git/ref/heads/{}", segments(branch)));
        match json(self.guard, &request) {
            Ok(value) => str_of(&value, "/object/sha").map(Some),
            Err(err) if err.status() == Some(404) => Ok(None),
            Err(err) => Err(err),
        }
    }

    fn create_branch(&self, name: &str, from_sha: &str) -> Result<(), LiveError> {
        let request = self
            .repo(Method::Post, "/git/refs")
            .json(&json!({"ref": format!("refs/heads/{name}"), "sha": from_sha}));
        call(self.guard, &request).map(|_| ())
    }

    fn commit_file(&self, branch: &str, change: &FileChange<'_>) -> Result<String, LiveError> {
        let path = format!("/contents/{}", segments(change.path));
        let mut body = json!({
            "message": change.message,
            "content": base64(change.content.as_bytes()),
            "branch": branch,
        });
        if !change.create {
            let current = self.repo(
                Method::Get,
                &format!("{path}?ref={}", percent_encode(branch)),
            );
            let blob = str_of(&json(self.guard, &current)?, "/sha")?;
            body["sha"] = Value::String(blob);
        }
        let response = json(self.guard, &self.repo(Method::Put, &path).json(&body))?;
        str_of(&response, "/commit/sha")
    }

    fn open_pull(
        &self,
        head: &str,
        base: &str,
        title: &str,
        _reviewer: Option<&Account>,
    ) -> Result<u64, LiveError> {
        let request = self.repo(Method::Post, "/pulls").json(&json!({
            "title": title,
            "head": head,
            "base": base,
            "body": PULL_BODY,
        }));
        u64_of(&json(self.guard, &request)?, "/number")
    }

    fn open_pulls_from(&self, head: &str) -> Result<Vec<u64>, LiveError> {
        let filter = percent_encode(&format!("{}:{head}", self.owner));
        pages(self.guard, |page| {
            self.repo(
                Method::Get,
                &format!("/pulls?state=open&head={filter}&per_page={PER_PAGE}&page={page}"),
            )
        })?
        .iter()
        .map(|pull| u64_of(pull, "/number"))
        .collect()
    }

    fn pull_is_open(&self, number: u64) -> Result<bool, LiveError> {
        let pull = json(
            self.guard,
            &self.repo(Method::Get, &format!("/pulls/{number}")),
        )?;
        Ok(str_of(&pull, "/state")? == "open")
    }

    fn close_pull(&self, number: u64) -> Result<(), LiveError> {
        let request = self
            .repo(Method::Patch, &format!("/pulls/{number}"))
            .json(&json!({"state": "closed"}));
        let pull = json(self.guard, &request)?;
        match str_of(&pull, "/state")?.as_str() {
            "closed" => Ok(()),
            other => Err(LiveError::Unexpected(format!("PR после закрытия: {other}"))),
        }
    }

    fn delete_branch(&self, name: &str) -> Result<(), LiveError> {
        let request = self.repo(
            Method::Delete,
            &format!("/git/refs/heads/{}", segments(name)),
        );
        call(self.guard, &request).map(|_| ())
    }

    fn own_drafts(&self, number: u64) -> Result<Vec<String>, LiveError> {
        // `PENDING` review видит только его владелец: в ответе токена — только свои.
        pages(self.guard, |page| {
            self.repo(
                Method::Get,
                &format!("/pulls/{number}/reviews?per_page={PER_PAGE}&page={page}"),
            )
        })?
        .iter()
        .filter(|review| review.get("state").and_then(Value::as_str) == Some("PENDING"))
        .map(|review| id_of(review, "/id"))
        .collect()
    }

    fn delete_draft(&self, number: u64, id: &str) -> Result<(), LiveError> {
        let request = self.repo(
            Method::Delete,
            &format!("/pulls/{number}/reviews/{}", percent_encode(id)),
        );
        call(self.guard, &request).map(|_| ())
    }
}
