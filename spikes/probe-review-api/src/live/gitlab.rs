//! Жизненный цикл клетки на GitLab: branches, Commits API, merge requests и `draft_notes` своего
//! аккаунта.

use serde_json::{Value, json};

use crate::model::Provider;
use crate::transport::{Method, Request, Transport, percent_encode};

use super::guard::{GuardError, Guarded, RepoInfo};
use super::repo::{
    Account, FileChange, LiveError, LiveRepo, call, id_of, json, pages, str_of, u64_of,
};

const ACCEPT: &str = "application/json";
const PER_PAGE: usize = 100;
pub const MR_DESCRIPTION: &str = "Проба P0.7: MR и его ветки удаляются после прогона клетки.";

/// Проект GitLab от имени роли, чей токен держит транспорт сторожа.
pub struct GitLabRepo<'a, T> {
    guard: &'a Guarded<T>,
}

impl<'a, T: Transport> GitLabRepo<'a, T> {
    pub fn new(guard: &'a Guarded<T>) -> Result<GitLabRepo<'a, T>, String> {
        let target = guard.target();
        if target.provider != Provider::GitLab {
            return Err("сторож не GitLab".to_owned());
        }
        let segments: Vec<&str> = target.repo.split('/').collect();
        if segments.len() < 2 || segments.iter().any(|segment| segment.is_empty()) {
            return Err("путь проекта GitLab — пространство имён/проект".to_owned());
        }
        Ok(GitLabRepo { guard })
    }

    fn project(&self, method: Method, path: &str) -> Request {
        Request::new(method, format!("{}{path}", self.guard.target().repo_url()))
            .header("accept", ACCEPT)
    }
}

impl<T: Transport> LiveRepo for GitLabRepo<'_, T> {
    fn pull_word(&self) -> &'static str {
        "MR"
    }

    fn check(&self) -> Result<RepoInfo, GuardError> {
        self.guard.check()
    }

    fn whoami(&self) -> Result<Account, LiveError> {
        let request =
            Request::get(format!("{}/user", self.guard.target().rest)).header("accept", ACCEPT);
        let user = json(self.guard, &request)?;
        Ok(Account {
            login: str_of(&user, "/username")?,
            id: u64_of(&user, "/id")?,
        })
    }

    fn branch_head(&self, branch: &str) -> Result<Option<String>, LiveError> {
        let request = self.project(
            Method::Get,
            &format!("/repository/branches/{}", percent_encode(branch)),
        );
        match json(self.guard, &request) {
            Ok(value) => str_of(&value, "/commit/id").map(Some),
            Err(err) if err.status() == Some(404) => Ok(None),
            Err(err) => Err(err),
        }
    }

    fn create_branch(&self, name: &str, from_sha: &str) -> Result<(), LiveError> {
        let request = self
            .project(Method::Post, "/repository/branches")
            .json(&json!({"branch": name, "ref": from_sha}));
        call(self.guard, &request).map(|_| ())
    }

    fn commit_file(&self, branch: &str, change: &FileChange<'_>) -> Result<String, LiveError> {
        let action = if change.create { "create" } else { "update" };
        let request = self
            .project(Method::Post, "/repository/commits")
            .json(&json!({
                "branch": branch,
                "commit_message": change.message,
                "actions": [{
                    "action": action,
                    "file_path": change.path,
                    "content": change.content,
                }],
            }));
        str_of(&json(self.guard, &request)?, "/id")
    }

    fn open_pull(
        &self,
        head: &str,
        base: &str,
        title: &str,
        reviewer: Option<&Account>,
    ) -> Result<u64, LiveError> {
        let mut body = json!({
            "source_branch": head,
            "target_branch": base,
            "title": title,
            "description": MR_DESCRIPTION,
        });
        if let Some(reviewer) = reviewer {
            body["reviewer_ids"] = json!([reviewer.id]);
        }
        let request = self.project(Method::Post, "/merge_requests").json(&body);
        u64_of(&json(self.guard, &request)?, "/iid")
    }

    fn open_pulls_from(&self, head: &str) -> Result<Vec<u64>, LiveError> {
        let branch = percent_encode(head);
        pages(self.guard, |page| {
            self.project(
                Method::Get,
                &format!(
                    "/merge_requests?state=opened&source_branch={branch}&per_page={PER_PAGE}&page={page}"
                ),
            )
        })?
        .iter()
        .map(|mr| u64_of(mr, "/iid"))
        .collect()
    }

    fn pull_is_open(&self, number: u64) -> Result<bool, LiveError> {
        let mr = json(
            self.guard,
            &self.project(Method::Get, &format!("/merge_requests/{number}")),
        )?;
        Ok(str_of(&mr, "/state")? == "opened")
    }

    fn close_pull(&self, number: u64) -> Result<(), LiveError> {
        let request = self
            .project(Method::Put, &format!("/merge_requests/{number}"))
            .json(&json!({"state_event": "close"}));
        let mr = json(self.guard, &request)?;
        match str_of(&mr, "/state")?.as_str() {
            "closed" => Ok(()),
            other => Err(LiveError::Unexpected(format!("MR после закрытия: {other}"))),
        }
    }

    fn delete_branch(&self, name: &str) -> Result<(), LiveError> {
        let request = self.project(
            Method::Delete,
            &format!("/repository/branches/{}", percent_encode(name)),
        );
        call(self.guard, &request).map(|_| ())
    }

    fn own_drafts(&self, number: u64) -> Result<Vec<String>, LiveError> {
        // `draft_notes` видит только автор черновика: в ответе токена — только свои.
        pages(self.guard, |page| {
            self.project(
                Method::Get,
                &format!("/merge_requests/{number}/draft_notes?per_page={PER_PAGE}&page={page}"),
            )
        })?
        .iter()
        .map(|draft: &Value| id_of(draft, "/id"))
        .collect()
    }

    fn delete_draft(&self, number: u64, id: &str) -> Result<(), LiveError> {
        let request = self.project(
            Method::Delete,
            &format!(
                "/merge_requests/{number}/draft_notes/{}",
                percent_encode(id)
            ),
        );
        call(self.guard, &request).map(|_| ())
    }
}
