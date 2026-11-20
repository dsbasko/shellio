//! GitHub для `forge-fake`: подмножество REST reviews и GraphQL по документации
//! (<https://docs.github.com/en/rest/pulls/reviews>, <https://docs.github.com/en/rest/pulls/comments>,
//! <https://docs.github.com/en/graphql/reference/pulls>).
//!
//! Комментарии к существующему `PENDING` добавляются и обновляются через GraphQL
//! `addPullRequestReviewThread` и `updatePullRequestReviewComment`: у REST нет способа добавить
//! комментарий в существующий review (решение по правилу плана, см. отчёт). REST
//! `POST /pulls/{n}/comments` и `PATCH /pulls/comments/{id}` сервер не реализует, вызов даёт 404.
//!
//! Строже живого хоста: Submit с `comments[]` — 422, `body` обязателен для `COMMENT` и
//! `REQUEST_CHANGES` и при Submit, `position` отвергается, второй `PENDING` того же
//! пользователя — 422 (по памяти о живом поведении, проверяется в живых клетках).

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value, json};

use super::{Forge, GraphqlKind, Incoming, graphql_root};
use crate::transport::{Method, Response};

const DOCS: &str = "https://docs.github.com/rest";
const PER_PAGE_DEFAULT: usize = 30;
const PER_PAGE_MAX: usize = 100;
const RATE_LIMIT: u64 = 5000;
/// Ответ `GET /versions` (<https://docs.github.com/en/rest/meta/meta#get-all-api-versions>).
const API_VERSIONS: [&str; 1] = ["2022-11-28"];
/// Мутации, которые реализует сервер; introspection `__type(name: "Mutation")` перечисляет их.
pub const MUTATIONS: [&str; 6] = [
    "addPullRequestReviewThread",
    "markFileAsViewed",
    "resolveReviewThread",
    "unmarkFileAsViewed",
    "unresolveReviewThread",
    "updatePullRequestReviewComment",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Permission {
    Read,
    Write,
}

#[derive(Clone, Debug)]
struct User {
    login: String,
    token: String,
    id: u64,
    permission: Permission,
}

/// Кто создал объект. Ответы API это поле не показывают, его читают оракулы.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    Api,
    /// Другой клиент того же пользователя (браузер), подложен сценарием.
    Planted,
}

impl Origin {
    fn name(self) -> &'static str {
        match self {
            Origin::Api => "api",
            Origin::Planted => "planted",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewState {
    Pending,
    Commented,
    Approved,
    ChangesRequested,
}

impl ReviewState {
    pub fn name(self) -> &'static str {
        match self {
            ReviewState::Pending => "PENDING",
            ReviewState::Commented => "COMMENTED",
            ReviewState::Approved => "APPROVED",
            ReviewState::ChangesRequested => "CHANGES_REQUESTED",
        }
    }

    fn from_event(event: &str) -> Option<ReviewState> {
        match event {
            "COMMENT" => Some(ReviewState::Commented),
            "APPROVE" => Some(ReviewState::Approved),
            "REQUEST_CHANGES" => Some(ReviewState::ChangesRequested),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffSide {
    Left,
    Right,
}

impl DiffSide {
    pub fn name(self) -> &'static str {
        match self {
            DiffSide::Left => "LEFT",
            DiffSide::Right => "RIGHT",
        }
    }

    fn parse(value: &Value) -> Result<Option<DiffSide>, String> {
        match value {
            Value::Null => Ok(None),
            Value::String(side) if side == "LEFT" => Ok(Some(DiffSide::Left)),
            Value::String(side) if side == "RIGHT" => Ok(Some(DiffSide::Right)),
            other => Err(format!("side {other} is not included in the list")),
        }
    }
}

/// Файл коммита PR с `patch` в виде unified diff без заголовков файла.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FakeFile {
    pub path: String,
    pub previous_path: Option<String>,
    pub status: String,
    pub patch: String,
}

impl FakeFile {
    pub fn modified(path: &str, patch: &str) -> FakeFile {
        FakeFile {
            path: path.to_owned(),
            previous_path: None,
            status: "modified".to_owned(),
            patch: patch.to_owned(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FakeCommit {
    pub sha: String,
    pub files: Vec<FakeFile>,
}

/// Строки сторон файла с номером блока. Разбор свой, независимый от модели пробы.
#[derive(Clone, Debug, Default)]
struct FileLines {
    left: BTreeMap<u32, usize>,
    right: BTreeMap<u32, usize>,
}

impl FileLines {
    fn scan(patch: &str) -> FileLines {
        let mut lines = FileLines::default();
        let (mut old, mut new, mut hunk) = (0u32, 0u32, 0usize);
        for line in patch.lines() {
            if let Some(header) = line.strip_prefix("@@ ") {
                hunk += 1;
                let mut parts = header.split_whitespace();
                old = hunk_start(parts.next(), '-');
                new = hunk_start(parts.next(), '+');
                continue;
            }
            if hunk == 0 {
                continue;
            }
            match line.chars().next() {
                Some('+') => {
                    lines.right.insert(new, hunk);
                    new += 1;
                }
                Some('-') => {
                    lines.left.insert(old, hunk);
                    old += 1;
                }
                Some('\\') => {}
                _ => {
                    lines.left.insert(old, hunk);
                    lines.right.insert(new, hunk);
                    old += 1;
                    new += 1;
                }
            }
        }
        lines
    }

    fn hunk(&self, side: DiffSide, line: u32) -> Option<usize> {
        match side {
            DiffSide::Left => self.left.get(&line).copied(),
            DiffSide::Right => self.right.get(&line).copied(),
        }
    }
}

fn hunk_start(part: Option<&str>, sign: char) -> u32 {
    part.and_then(|part| part.strip_prefix(sign))
        .and_then(|range| range.split(',').next())
        .and_then(|start| start.parse().ok())
        .unwrap_or(0)
}

/// Комментарий для создания: `comments[]` REST, вход GraphQL или подложенный сценарием.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommentSpec {
    pub path: String,
    pub body: String,
    pub line: u32,
    pub side: DiffSide,
    pub start_line: Option<u32>,
    pub start_side: Option<DiffSide>,
}

impl CommentSpec {
    pub fn line(path: &str, side: DiffSide, line: u32, body: &str) -> CommentSpec {
        CommentSpec {
            path: path.to_owned(),
            body: body.to_owned(),
            line,
            side,
            start_line: None,
            start_side: None,
        }
    }

    fn from_rest(value: &Value) -> Result<CommentSpec, String> {
        CommentSpec::parse(
            value,
            ["path", "body", "line", "side", "start_line", "start_side"],
        )
    }

    fn from_graphql(value: &Value) -> Result<CommentSpec, String> {
        CommentSpec::parse(
            value,
            ["path", "body", "line", "side", "startLine", "startSide"],
        )
    }

    fn parse(value: &Value, names: [&str; 6]) -> Result<CommentSpec, String> {
        let object = value.as_object().ok_or("comment is not an object")?;
        if object.contains_key("position") {
            return Err("forge-fake: position is closing down and not modeled".to_owned());
        }
        if object
            .get("subjectType")
            .or_else(|| object.get("subject_type"))
            .and_then(Value::as_str)
            .is_some_and(|subject| !subject.eq_ignore_ascii_case("line"))
        {
            return Err("forge-fake: file comments are not modeled".to_owned());
        }
        let text = |name: &str| object.get(name).and_then(Value::as_str).unwrap_or("");
        let number = |name: &str| -> Result<Option<u32>, String> {
            match object.get(name) {
                None | Some(Value::Null) => Ok(None),
                Some(value) => value
                    .as_u64()
                    .and_then(|line| u32::try_from(line).ok())
                    .filter(|line| *line > 0)
                    .map(Some)
                    .ok_or_else(|| format!("{name} is not a positive integer")),
            }
        };
        let path = text(names[0]);
        if path.is_empty() {
            return Err("path can't be blank".to_owned());
        }
        let body = text(names[1]);
        if body.trim().is_empty() {
            return Err("body can't be blank".to_owned());
        }
        let line = number(names[2])?.ok_or("line can't be blank")?;
        let side = DiffSide::parse(object.get(names[3]).unwrap_or(&Value::Null))?
            .unwrap_or(DiffSide::Right);
        let start_line = number(names[4])?;
        let start_side = DiffSide::parse(object.get(names[5]).unwrap_or(&Value::Null))?;
        if start_side.is_some() && start_line.is_none() {
            return Err("start_side requires start_line".to_owned());
        }
        Ok(CommentSpec {
            path: path.to_owned(),
            body: body.to_owned(),
            line,
            side,
            start_line,
            start_side,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Review {
    pub id: u64,
    pub node_id: String,
    pub user: String,
    pub body: String,
    pub state: ReviewState,
    pub commit_id: String,
    pub submitted_at: Option<String>,
    pub origin: Origin,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Comment {
    pub id: u64,
    pub node_id: String,
    pub review_id: u64,
    pub thread_id: u64,
    pub user: String,
    pub body: String,
    pub path: String,
    pub line: u32,
    pub side: DiffSide,
    pub start_line: Option<u32>,
    pub start_side: Option<DiffSide>,
    pub commit_id: String,
    pub in_reply_to: Option<u64>,
    pub origin: Origin,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Thread {
    pub id: u64,
    pub node_id: String,
    pub resolved: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Viewed {
    Viewed,
    Dismissed,
}

#[derive(Clone, Debug)]
struct Pull {
    number: u64,
    node_id: String,
    author: String,
    base_sha: String,
    commits: Vec<(FakeCommit, BTreeMap<String, FileLines>)>,
    reviews: Vec<Review>,
    comments: Vec<Comment>,
    threads: Vec<Thread>,
    viewed: BTreeMap<String, BTreeMap<String, Viewed>>,
}

impl Pull {
    fn head(&self) -> &FakeCommit {
        &self.commits[self.commits.len() - 1].0
    }

    fn lines(&self, sha: &str) -> Option<&BTreeMap<String, FileLines>> {
        self.commits
            .iter()
            .find(|(commit, _)| commit.sha == sha)
            .map(|(_, lines)| lines)
    }

    fn review(&self, id: u64) -> Option<&Review> {
        self.reviews.iter().find(|review| review.id == id)
    }

    fn pending_of(&self, login: &str) -> Option<&Review> {
        self.reviews
            .iter()
            .find(|review| review.user == login && review.state == ReviewState::Pending)
    }

    /// `PENDING` и его комментарии видит только владелец.
    fn review_visible(&self, review: &Review, login: &str) -> bool {
        review.state != ReviewState::Pending || review.user == login
    }

    fn comment_visible(&self, comment: &Comment, login: &str) -> bool {
        self.review(comment.review_id)
            .is_some_and(|review| self.review_visible(review, login))
    }

    fn comment_published(&self, comment: &Comment) -> bool {
        self.review(comment.review_id)
            .is_some_and(|review| review.state != ReviewState::Pending)
    }

    fn validate(&self, sha: &str, spec: &CommentSpec) -> Result<(), String> {
        let files = self
            .lines(sha)
            .ok_or("commit_id is not part of the pull request")?;
        let file = files.get(&spec.path).ok_or("Path could not be resolved")?;
        let end = file
            .hunk(spec.side, spec.line)
            .ok_or("Line could not be resolved")?;
        if let Some(start_line) = spec.start_line {
            let start_side = spec.start_side.unwrap_or(spec.side);
            let start = file
                .hunk(start_side, start_line)
                .ok_or("Start line could not be resolved")?;
            if start != end {
                return Err("start_line must be part of the same hunk as the line".to_owned());
            }
            if start_side == spec.side && start_line >= spec.line {
                return Err("start_line must precede the end line".to_owned());
            }
        }
        Ok(())
    }
}

/// Состояние GitHub одного репозитория.
#[derive(Clone, Debug)]
pub struct GithubFake {
    owner: String,
    name: String,
    private: bool,
    default_branch: String,
    users: Vec<User>,
    pulls: BTreeMap<u64, Pull>,
    next_id: u64,
    clock: u64,
    remaining: u64,
    /// Мутации, которых нет в схеме хоста: introspection их не показывает, вызов — `undefinedField`.
    removed: BTreeSet<String>,
}

/// Маршрут REST.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
    User,
    Versions,
    Graphql,
    Repo,
    Pull(u64),
    PullCommits(u64),
    PullFiles(u64),
    Reviews(u64),
    Review(u64, u64),
    ReviewEvents(u64, u64),
    ReviewComments(u64, u64),
    PullComments(u64),
    Replies(u64, u64),
}

impl Route {
    fn template(self) -> &'static str {
        match self {
            Route::User => "/user",
            Route::Versions => "/versions",
            Route::Graphql => "/graphql",
            Route::Repo => "/repos/{owner}/{repo}",
            Route::Pull(_) => "/repos/{owner}/{repo}/pulls/{number}",
            Route::PullCommits(_) => "/repos/{owner}/{repo}/pulls/{number}/commits",
            Route::PullFiles(_) => "/repos/{owner}/{repo}/pulls/{number}/files",
            Route::Reviews(_) => "/repos/{owner}/{repo}/pulls/{number}/reviews",
            Route::Review(..) => "/repos/{owner}/{repo}/pulls/{number}/reviews/{review_id}",
            Route::ReviewEvents(..) => {
                "/repos/{owner}/{repo}/pulls/{number}/reviews/{review_id}/events"
            }
            Route::ReviewComments(..) => {
                "/repos/{owner}/{repo}/pulls/{number}/reviews/{review_id}/comments"
            }
            Route::PullComments(_) => "/repos/{owner}/{repo}/pulls/{number}/comments",
            Route::Replies(..) => {
                "/repos/{owner}/{repo}/pulls/{number}/comments/{comment_id}/replies"
            }
        }
    }
}

/// Ошибка GraphQL: ответ 200 с `errors` и, при наличии, `data`.
struct GqlError {
    kind: &'static str,
    message: String,
}

fn gql_error(kind: &'static str, message: impl Into<String>) -> GqlError {
    GqlError {
        kind,
        message: message.into(),
    }
}

impl GithubFake {
    /// Приватный репозиторий `owner/name` с веткой по умолчанию `main`.
    pub fn new(owner: &str, name: &str) -> GithubFake {
        GithubFake {
            owner: owner.to_owned(),
            name: name.to_owned(),
            private: true,
            default_branch: "main".to_owned(),
            users: Vec::new(),
            pulls: BTreeMap::new(),
            next_id: 1000,
            clock: 0,
            remaining: RATE_LIMIT,
            removed: BTreeSet::new(),
        }
    }

    /// Хост без мутации (старая редакция GHES): introspection её не показывает, вызов даёт
    /// `undefinedField`.
    pub fn remove_mutation(&mut self, name: &str) {
        self.removed.insert(name.to_owned());
    }

    pub fn set_private(&mut self, private: bool) {
        self.private = private;
    }

    pub fn add_user(&mut self, login: &str, token: &str, permission: Permission) {
        let id = self.id();
        self.users.push(User {
            login: login.to_owned(),
            token: token.to_owned(),
            id,
            permission,
        });
    }

    pub fn add_pull(&mut self, number: u64, author: &str, base_sha: &str, head: FakeCommit) {
        let lines = lines_of(&head);
        self.pulls.insert(
            number,
            Pull {
                number,
                node_id: format!("PR_fake{number}"),
                author: author.to_owned(),
                base_sha: base_sha.to_owned(),
                commits: vec![(head, lines)],
                reviews: Vec::new(),
                comments: Vec::new(),
                threads: Vec::new(),
                viewed: BTreeMap::new(),
            },
        );
    }

    /// Новый head PR. Просмотренные файлы, чей `patch` изменился или исчез, становятся
    /// `DISMISSED`.
    pub fn push_head(&mut self, number: u64, head: FakeCommit) -> Result<(), String> {
        let pull = self.pulls.get_mut(&number).ok_or("нет PR")?;
        if pull.lines(&head.sha).is_some() {
            return Err(format!("коммит {} уже есть в PR", head.sha));
        }
        let previous = pull.head().files.clone();
        for viewed in pull.viewed.values_mut() {
            for (path, state) in viewed.iter_mut() {
                let before = previous.iter().find(|file| &file.path == path);
                let after = head.files.iter().find(|file| &file.path == path);
                if before.map(|file| &file.patch) != after.map(|file| &file.patch) {
                    *state = Viewed::Dismissed;
                }
            }
        }
        let lines = lines_of(&head);
        pull.commits.push((head, lines));
        Ok(())
    }

    pub fn head_sha(&self, number: u64) -> Option<String> {
        self.pulls.get(&number).map(|pull| pull.head().sha.clone())
    }

    pub fn pull_node_id(&self, number: u64) -> Option<String> {
        self.pulls.get(&number).map(|pull| pull.node_id.clone())
    }

    /// `PENDING` другого клиента того же пользователя (браузер): создаётся в обход HTTP,
    /// счётчики вызовов не меняются. Возвращает ID review и комментариев.
    pub fn plant_pending(
        &mut self,
        number: u64,
        login: &str,
        body: &str,
        comments: &[CommentSpec],
    ) -> Result<(u64, Vec<u64>), String> {
        let pull = self.pulls.get(&number).ok_or("нет PR")?;
        if pull.pending_of(login).is_some() {
            return Err(format!("у {login} уже есть PENDING"));
        }
        let sha = pull.head().sha.clone();
        for spec in comments {
            pull.validate(&sha, spec)?;
        }
        let review = self.insert_review(number, login, body, None, sha, Origin::Planted);
        let ids = comments
            .iter()
            .map(|spec| self.insert_comment(number, review.id, login, spec, None, Origin::Planted))
            .collect();
        Ok((review.id, ids))
    }

    pub fn reviews(&self, number: u64) -> Vec<Review> {
        self.pulls
            .get(&number)
            .map(|pull| pull.reviews.clone())
            .unwrap_or_default()
    }

    pub fn comments(&self, number: u64) -> Vec<Comment> {
        self.pulls
            .get(&number)
            .map(|pull| pull.comments.clone())
            .unwrap_or_default()
    }

    pub fn threads(&self, number: u64) -> Vec<Thread> {
        self.pulls
            .get(&number)
            .map(|pull| pull.threads.clone())
            .unwrap_or_default()
    }

    pub fn viewed(&self, number: u64, login: &str) -> BTreeMap<String, Viewed> {
        self.pulls
            .get(&number)
            .and_then(|pull| pull.viewed.get(login).cloned())
            .unwrap_or_default()
    }

    /// Серверное состояние для `result.json` (до записи на диск — через очиститель).
    pub fn snapshot(&self) -> Value {
        let pulls: Vec<Value> = self
            .pulls
            .values()
            .map(|pull| {
                json!({
                    "number": pull.number,
                    "author": pull.author,
                    "base_sha": pull.base_sha,
                    "commits": pull.commits.iter().map(|(c, _)| c.sha.clone()).collect::<Vec<_>>(),
                    "reviews": pull.reviews.iter().map(|r| json!({
                        "id": r.id,
                        "node_id": r.node_id,
                        "user": r.user,
                        "state": r.state.name(),
                        "body": r.body,
                        "commit_id": r.commit_id,
                        "origin": r.origin.name(),
                    })).collect::<Vec<_>>(),
                    "comments": pull.comments.iter().map(|c| json!({
                        "id": c.id,
                        "node_id": c.node_id,
                        "review_id": c.review_id,
                        "thread_id": c.thread_id,
                        "user": c.user,
                        "body": c.body,
                        "path": c.path,
                        "line": c.line,
                        "side": c.side.name(),
                        "start_line": c.start_line,
                        "start_side": c.start_side.map(DiffSide::name),
                        "commit_id": c.commit_id,
                        "in_reply_to": c.in_reply_to,
                        "origin": c.origin.name(),
                    })).collect::<Vec<_>>(),
                    "threads": pull.threads.iter().map(|t| json!({
                        "id": t.id,
                        "node_id": t.node_id,
                        "resolved": t.resolved,
                    })).collect::<Vec<_>>(),
                    "viewed": pull.viewed.iter().map(|(login, files)| (login.clone(), json!(
                        files.iter().map(|(path, state)| (path.clone(), json!(viewed_name(*state))))
                            .collect::<Map<_, _>>()
                    ))).collect::<Map<_, _>>(),
                })
            })
            .collect();
        json!({ "pulls": pulls })
    }

    fn id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn now(&mut self) -> String {
        self.clock += 1;
        let seconds = self.clock;
        format!(
            "2026-01-01T{:02}:{:02}:{:02}Z",
            (seconds / 3600) % 24,
            (seconds / 60) % 60,
            seconds % 60
        )
    }

    fn insert_review(
        &mut self,
        number: u64,
        login: &str,
        body: &str,
        state: Option<ReviewState>,
        commit_id: String,
        origin: Origin,
    ) -> Review {
        let id = self.id();
        let submitted_at = state.map(|_| self.now());
        let review = Review {
            id,
            node_id: format!("PRR_fake{id}"),
            user: login.to_owned(),
            body: body.to_owned(),
            state: state.unwrap_or(ReviewState::Pending),
            commit_id,
            submitted_at,
            origin,
        };
        if let Some(pull) = self.pulls.get_mut(&number) {
            pull.reviews.push(review.clone());
        }
        review
    }

    /// Комментарий; без `reply_to` начинает новый тред.
    fn insert_comment(
        &mut self,
        number: u64,
        review_id: u64,
        login: &str,
        spec: &CommentSpec,
        reply_to: Option<&Comment>,
        origin: Origin,
    ) -> u64 {
        let id = self.id();
        let thread_id = match reply_to {
            Some(parent) => parent.thread_id,
            None => self.id(),
        };
        let Some(pull) = self.pulls.get_mut(&number) else {
            return id;
        };
        let commit_id = pull
            .review(review_id)
            .map(|review| review.commit_id.clone())
            .unwrap_or_default();
        if reply_to.is_none() {
            pull.threads.push(Thread {
                id: thread_id,
                node_id: format!("PRRT_fake{thread_id}"),
                resolved: false,
            });
        }
        pull.comments.push(Comment {
            id,
            node_id: format!("PRRC_fake{id}"),
            review_id,
            thread_id,
            user: login.to_owned(),
            body: spec.body.clone(),
            path: spec.path.clone(),
            line: spec.line,
            side: spec.side,
            start_line: spec.start_line,
            start_side: spec
                .start_line
                .map(|_| spec.start_side.unwrap_or(spec.side)),
            commit_id,
            in_reply_to: reply_to.map(|parent| parent.id),
            origin,
        });
        id
    }

    fn user_by_token(&self, token: &str) -> Option<&User> {
        self.users.iter().find(|user| user.token == token)
    }

    fn route(&self, path: &str) -> Option<Route> {
        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let number = |s: &str| s.parse::<u64>().ok();
        match segments[..] {
            ["user"] => Some(Route::User),
            ["versions"] => Some(Route::Versions),
            ["graphql"] => Some(Route::Graphql),
            ["repos", owner, repo, ref rest @ ..] => {
                if !owner.eq_ignore_ascii_case(&self.owner)
                    || !repo.eq_ignore_ascii_case(&self.name)
                {
                    return None;
                }
                match *rest {
                    [] => Some(Route::Repo),
                    ["pulls", n] => Some(Route::Pull(number(n)?)),
                    ["pulls", n, "commits"] => Some(Route::PullCommits(number(n)?)),
                    ["pulls", n, "files"] => Some(Route::PullFiles(number(n)?)),
                    ["pulls", n, "reviews"] => Some(Route::Reviews(number(n)?)),
                    ["pulls", n, "reviews", r] => Some(Route::Review(number(n)?, number(r)?)),
                    ["pulls", n, "reviews", r, "events"] => {
                        Some(Route::ReviewEvents(number(n)?, number(r)?))
                    }
                    ["pulls", n, "reviews", r, "comments"] => {
                        Some(Route::ReviewComments(number(n)?, number(r)?))
                    }
                    ["pulls", n, "comments"] => Some(Route::PullComments(number(n)?)),
                    ["pulls", n, "comments", c, "replies"] => {
                        Some(Route::Replies(number(n)?, number(c)?))
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    fn rate_headers(&mut self, response: Response, resource: &str) -> Response {
        self.remaining = self.remaining.saturating_sub(1);
        response
            .with_header("x-ratelimit-limit", RATE_LIMIT.to_string())
            .with_header("x-ratelimit-remaining", self.remaining.to_string())
            .with_header(
                "x-ratelimit-used",
                (RATE_LIMIT - self.remaining).to_string(),
            )
            .with_header("x-ratelimit-reset", "1767229200")
            .with_header("x-ratelimit-resource", resource)
    }

    fn dispatch(&mut self, request: &Incoming) -> Response {
        let Some(route) = self.route(&request.path) else {
            return not_found();
        };
        let token = request.bearer();
        let Some(user) = token.and_then(|token| self.user_by_token(token)).cloned() else {
            return match token {
                Some(_) => message(401, "Bad credentials"),
                None => message(401, "Requires authentication"),
            };
        };
        let method = request.method;
        match (route, method) {
            (Route::User, Method::Get) => ok(json!({
                "login": user.login,
                "id": user.id,
                "node_id": format!("U_fake{}", user.id),
            })),
            (Route::Versions, Method::Get) => ok(json!(API_VERSIONS)),
            (Route::Graphql, Method::Post) => self.graphql(request, &user),
            (Route::Repo, Method::Get) => ok(self.repo_json(&user)),
            (Route::Pull(n), Method::Get) => self.with_pull(n, |pull| ok(pull_json(pull))),
            (Route::PullCommits(n), Method::Get) => self.with_pull(n, |pull| {
                let commits: Vec<Value> = pull
                    .commits
                    .iter()
                    .map(|(commit, _)| json!({"sha": commit.sha}))
                    .collect();
                paged(request, &commits)
            }),
            (Route::PullFiles(n), Method::Get) => self.with_pull(n, |pull| {
                let files: Vec<Value> = pull.head().files.iter().map(file_json).collect();
                paged(request, &files)
            }),
            (Route::Reviews(n), Method::Get) => self.with_pull(n, |pull| {
                let reviews: Vec<Value> = pull
                    .reviews
                    .iter()
                    .filter(|review| pull.review_visible(review, &user.login))
                    .map(|review| review_json(pull, review))
                    .collect();
                paged(request, &reviews)
            }),
            (Route::Reviews(n), Method::Post) => self.create_review(request, &user, n),
            (Route::Review(n, r), Method::Get) => {
                self.with_visible_review(n, r, &user, |pull, review| ok(review_json(pull, review)))
            }
            (Route::Review(n, r), Method::Put) => self.update_review(request, &user, n, r),
            (Route::Review(n, r), Method::Delete) => self.delete_review(&user, n, r),
            (Route::ReviewEvents(n, r), Method::Post) => self.submit_review(request, &user, n, r),
            (Route::ReviewComments(n, r), Method::Get) => {
                self.with_visible_review(n, r, &user, |pull, review| {
                    let comments: Vec<Value> = pull
                        .comments
                        .iter()
                        .filter(|comment| comment.review_id == review.id)
                        .map(|comment| comment_json(pull, comment))
                        .collect();
                    paged(request, &comments)
                })
            }
            (Route::PullComments(n), Method::Get) => self.with_pull(n, |pull| {
                let comments: Vec<Value> = pull
                    .comments
                    .iter()
                    .filter(|comment| pull.comment_published(comment))
                    .map(|comment| comment_json(pull, comment))
                    .collect();
                paged(request, &comments)
            }),
            (Route::Replies(n, c), Method::Post) => self.reply(request, &user, n, c),
            _ => not_found(),
        }
    }

    fn repo_json(&self, user: &User) -> Value {
        json!({
            "id": 1,
            "node_id": "R_fake1",
            "name": self.name,
            "full_name": format!("{}/{}", self.owner, self.name),
            "private": self.private,
            "visibility": if self.private { "private" } else { "public" },
            "owner": {"login": self.owner},
            "default_branch": self.default_branch,
            "permissions": {
                "admin": false,
                "push": user.permission == Permission::Write,
                "pull": true,
            },
        })
    }

    fn with_pull(&self, number: u64, f: impl FnOnce(&Pull) -> Response) -> Response {
        match self.pulls.get(&number) {
            Some(pull) => f(pull),
            None => not_found(),
        }
    }

    fn with_visible_review(
        &self,
        number: u64,
        review_id: u64,
        user: &User,
        f: impl FnOnce(&Pull, &Review) -> Response,
    ) -> Response {
        let Some(pull) = self.pulls.get(&number) else {
            return not_found();
        };
        match pull.review(review_id) {
            Some(review) if pull.review_visible(review, &user.login) => f(pull, review),
            _ => not_found(),
        }
    }

    fn create_review(&mut self, request: &Incoming, user: &User, number: u64) -> Response {
        let body = match request.json() {
            Ok(Value::Object(body)) => body,
            _ => return message(400, "Problems parsing JSON"),
        };
        let Some(pull) = self.pulls.get(&number) else {
            return not_found();
        };
        let state = match body.get("event") {
            None | Some(Value::Null) => None,
            Some(Value::String(event)) if event.is_empty() => None,
            Some(Value::String(event)) => match ReviewState::from_event(event) {
                Some(state) => Some(state),
                None => return unprocessable("event is not included in the list"),
            },
            Some(_) => return unprocessable("event is not a string"),
        };
        if pull.pending_of(&user.login).is_some() {
            return unprocessable("User can only have one pending review per pull request");
        }
        let text = body.get("body").and_then(Value::as_str).unwrap_or("");
        if let Err(err) = check_event(pull, user, state, text) {
            return unprocessable(&err);
        }
        let commit_id = match body.get("commit_id") {
            None | Some(Value::Null) => pull.head().sha.clone(),
            Some(Value::String(sha)) if pull.lines(sha).is_some() => sha.clone(),
            Some(_) => return unprocessable("commit_id is not part of the pull request"),
        };
        let specs = match body.get("comments") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => {
                match items
                    .iter()
                    .map(CommentSpec::from_rest)
                    .collect::<Result<Vec<_>, _>>()
                {
                    Ok(specs) => specs,
                    Err(err) => return unprocessable(&err),
                }
            }
            Some(_) => return unprocessable("comments is not an array"),
        };
        for spec in &specs {
            if let Err(err) = pull.validate(&commit_id, spec) {
                return unprocessable(&err);
            }
        }
        let review = self.insert_review(number, &user.login, text, state, commit_id, Origin::Api);
        for spec in &specs {
            self.insert_comment(number, review.id, &user.login, spec, None, Origin::Api);
        }
        self.with_pull(number, |pull| ok(review_json(pull, &review)))
    }

    fn submit_review(&mut self, request: &Incoming, user: &User, number: u64, id: u64) -> Response {
        let body = match request.json() {
            Ok(Value::Object(body)) => body,
            Ok(Value::Null) => Map::new(),
            _ => return message(400, "Problems parsing JSON"),
        };
        let Some(pull) = self.pulls.get(&number) else {
            return not_found();
        };
        let review = match pull.review(id) {
            Some(review) if pull.review_visible(review, &user.login) => review.clone(),
            _ => return not_found(),
        };
        if body.contains_key("comments") {
            return unprocessable("forge-fake: Submit does not accept comments[]");
        }
        if review.user != user.login {
            return message(403, "Must be the review author");
        }
        if review.state != ReviewState::Pending {
            return unprocessable("Can not submit a non-pending review");
        }
        let state = match body.get("event").and_then(Value::as_str) {
            Some(event) => match ReviewState::from_event(event) {
                Some(state) => state,
                None => return unprocessable("event is not included in the list"),
            },
            None => return unprocessable("event can't be blank"),
        };
        let text = body
            .get("body")
            .and_then(Value::as_str)
            .map_or(review.body.clone(), str::to_owned);
        if let Err(err) = check_event(pull, user, Some(state), &text) {
            return unprocessable(&err);
        }
        let submitted_at = self.now();
        let Some(pull) = self.pulls.get_mut(&number) else {
            return not_found();
        };
        let Some(stored) = pull.reviews.iter_mut().find(|r| r.id == id) else {
            return not_found();
        };
        stored.state = state;
        stored.body = text;
        stored.submitted_at = Some(submitted_at);
        let review = stored.clone();
        ok(review_json(pull, &review))
    }

    fn update_review(&mut self, request: &Incoming, user: &User, number: u64, id: u64) -> Response {
        let text = match request.json() {
            Ok(body) => body.get("body").and_then(Value::as_str).map(str::to_owned),
            Err(_) => return message(400, "Problems parsing JSON"),
        };
        let Some(pull) = self.pulls.get_mut(&number) else {
            return not_found();
        };
        let Some(index) = pull.reviews.iter().position(|review| review.id == id) else {
            return not_found();
        };
        let review = &pull.reviews[index];
        if !pull.review_visible(review, &user.login) {
            return not_found();
        }
        if review.user != user.login {
            return message(403, "Must be the review author");
        }
        let Some(text) = text.filter(|text| !text.trim().is_empty()) else {
            return unprocessable("body can't be blank");
        };
        pull.reviews[index].body = text;
        let review = pull.reviews[index].clone();
        ok(review_json(pull, &review))
    }

    fn delete_review(&mut self, user: &User, number: u64, id: u64) -> Response {
        let Some(pull) = self.pulls.get_mut(&number) else {
            return not_found();
        };
        let Some(review) = pull.review(id).cloned() else {
            return not_found();
        };
        if !pull.review_visible(&review, &user.login) {
            return not_found();
        }
        if review.user != user.login {
            return message(403, "Must be the review author");
        }
        if review.state != ReviewState::Pending {
            return unprocessable("Can not delete a non-pending pull request review");
        }
        let threads: BTreeSet<u64> = pull
            .comments
            .iter()
            .filter(|comment| comment.review_id == id)
            .map(|comment| comment.thread_id)
            .collect();
        pull.comments.retain(|comment| comment.review_id != id);
        pull.threads.retain(|thread| !threads.contains(&thread.id));
        pull.reviews.retain(|review| review.id != id);
        ok(review_json(pull, &review))
    }

    fn reply(&mut self, request: &Incoming, user: &User, number: u64, parent: u64) -> Response {
        let text = match request.json() {
            Ok(body) => body
                .get("body")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            Err(_) => return message(400, "Problems parsing JSON"),
        };
        let Some(pull) = self.pulls.get(&number) else {
            return not_found();
        };
        let parent = match pull.comments.iter().find(|comment| comment.id == parent) {
            Some(comment) if pull.comment_visible(comment, &user.login) => comment.clone(),
            _ => return not_found(),
        };
        if !pull.comment_published(&parent) {
            return unprocessable("forge-fake: replies to pending comments are not modeled");
        }
        if parent.in_reply_to.is_some() {
            return unprocessable("Replies to replies are not supported");
        }
        if text.trim().is_empty() {
            return unprocessable("body can't be blank");
        }
        let spec = CommentSpec {
            path: parent.path.clone(),
            body: text,
            line: parent.line,
            side: parent.side,
            start_line: parent.start_line,
            start_side: parent.start_side,
        };
        let commit_id = parent.commit_id.clone();
        let review = self.insert_review(
            number,
            &user.login,
            "",
            Some(ReviewState::Commented),
            commit_id,
            Origin::Api,
        );
        let id = self.insert_comment(
            number,
            review.id,
            &user.login,
            &spec,
            Some(&parent),
            Origin::Api,
        );
        self.with_pull(number, |pull| {
            let comment = pull.comments.iter().find(|comment| comment.id == id);
            comment.map_or_else(not_found, |comment| created(comment_json(pull, comment)))
        })
    }

    fn graphql(&mut self, request: &Incoming, user: &User) -> Response {
        let body = match request.json() {
            Ok(body) => body,
            Err(_) => return message(400, "Problems parsing JSON"),
        };
        let query = body.get("query").and_then(Value::as_str).unwrap_or("");
        let variables = body.get("variables").cloned().unwrap_or(Value::Null);
        let Some((kind, field)) = graphql_root(query) else {
            return gql_response(None, Some(gql_error("PARSE", "Parse error on query")));
        };
        let removed = kind == GraphqlKind::Mutation && self.removed.contains(&field);
        let result = match (kind, field.as_str()) {
            (GraphqlKind::Query, "__type") => Ok(self.introspect(&variables)),
            (GraphqlKind::Mutation, _) if removed => {
                return gql_response(
                    None,
                    Some(GqlError {
                        kind: "undefinedField",
                        message: format!("Field '{field}' doesn't exist on type 'Mutation'"),
                    }),
                );
            }
            (GraphqlKind::Mutation, "markFileAsViewed") => self.gql_viewed(&variables, user, true),
            (GraphqlKind::Mutation, "unmarkFileAsViewed") => {
                self.gql_viewed(&variables, user, false)
            }
            (GraphqlKind::Mutation, "addPullRequestReviewThread") => {
                self.gql_add_thread(&variables, user)
            }
            (GraphqlKind::Mutation, "updatePullRequestReviewComment") => {
                self.gql_update_comment(&variables, user)
            }
            (GraphqlKind::Mutation, "resolveReviewThread") => {
                self.gql_resolve(&variables, user, true)
            }
            (GraphqlKind::Mutation, "unresolveReviewThread") => {
                self.gql_resolve(&variables, user, false)
            }
            (GraphqlKind::Query, "node") => self.gql_node(&variables, user),
            (kind, other) => {
                let type_name = match kind {
                    GraphqlKind::Query => "Query",
                    GraphqlKind::Mutation => "Mutation",
                };
                return gql_response(
                    None,
                    Some(GqlError {
                        kind: "undefinedField",
                        message: format!("Field '{other}' doesn't exist on type '{type_name}'"),
                    }),
                );
            }
        };
        match result {
            Ok(value) => gql_response(Some(json!({ field.clone(): value })), None),
            Err(err) => gql_response(Some(json!({ field: Value::Null })), Some(err)),
        }
    }

    /// `__type(name: $name)`: имя типа только из `variables`; известен лишь `Mutation`.
    fn introspect(&self, variables: &Value) -> Value {
        if variables.get("name").and_then(Value::as_str) != Some("Mutation") {
            return Value::Null;
        }
        let fields: Vec<Value> = MUTATIONS
            .iter()
            .filter(|name| !self.removed.contains(**name))
            .map(|name| json!({"name": name}))
            .collect();
        json!({"name": "Mutation", "fields": fields})
    }

    fn gql_viewed(
        &mut self,
        variables: &Value,
        user: &User,
        viewed: bool,
    ) -> Result<Value, GqlError> {
        let input = gql_input(variables)?;
        let node = gql_str(input, "pullRequestId")?;
        let path = gql_str(input, "path")?;
        let pull = self
            .pulls
            .values_mut()
            .find(|pull| pull.node_id == node)
            .ok_or_else(|| not_resolved(&node))?;
        if !pull.head().files.iter().any(|file| file.path == path) {
            return Err(gql_error(
                "NOT_FOUND",
                format!("forge-fake: no file at path '{path}'"),
            ));
        }
        let files = pull.viewed.entry(user.login.clone()).or_default();
        if viewed {
            files.insert(path, Viewed::Viewed);
        } else {
            files.remove(&path);
        }
        Ok(json!({"clientMutationId": null, "pullRequest": {"id": pull.node_id}}))
    }

    fn gql_add_thread(&mut self, variables: &Value, user: &User) -> Result<Value, GqlError> {
        let input = gql_input(variables)?;
        let Some(review_node) = input.get("pullRequestReviewId").and_then(Value::as_str) else {
            return Err(gql_error(
                "UNPROCESSABLE",
                "forge-fake: only pullRequestReviewId is modeled",
            ));
        };
        let (number, review) = self
            .pulls
            .values()
            .find_map(|pull| {
                pull.reviews
                    .iter()
                    .find(|review| review.node_id == review_node)
                    .filter(|review| pull.review_visible(review, &user.login))
                    .map(|review| (pull.number, review.clone()))
            })
            .ok_or_else(|| not_resolved(review_node))?;
        if review.user != user.login {
            return Err(gql_error("FORBIDDEN", "Must be the review author"));
        }
        if review.state != ReviewState::Pending {
            return Err(gql_error("UNPROCESSABLE", "Review is not pending"));
        }
        let spec =
            CommentSpec::from_graphql(input).map_err(|err| gql_error("UNPROCESSABLE", err))?;
        if let Some(pull) = self.pulls.get(&number) {
            pull.validate(&review.commit_id, &spec)
                .map_err(|err| gql_error("UNPROCESSABLE", err))?;
        }
        let id = self.insert_comment(number, review.id, &user.login, &spec, None, Origin::Api);
        let pull = self
            .pulls
            .get(&number)
            .ok_or_else(|| not_resolved(review_node))?;
        let thread = pull
            .comments
            .iter()
            .find(|comment| comment.id == id)
            .and_then(|comment| pull.threads.iter().find(|t| t.id == comment.thread_id))
            .ok_or_else(|| not_resolved(review_node))?;
        Ok(json!({"clientMutationId": null, "thread": thread_gql(pull, thread, &user.login)}))
    }

    fn gql_update_comment(&mut self, variables: &Value, user: &User) -> Result<Value, GqlError> {
        let input = gql_input(variables)?;
        let node = gql_str(input, "pullRequestReviewCommentId")?;
        let body = gql_str(input, "body")?;
        if body.trim().is_empty() {
            return Err(gql_error("UNPROCESSABLE", "body can't be blank"));
        }
        let pull = self
            .pulls
            .values_mut()
            .find(|pull| pull.comments.iter().any(|comment| comment.node_id == node))
            .ok_or_else(|| not_resolved(&node))?;
        let index = pull
            .comments
            .iter()
            .position(|comment| comment.node_id == node)
            .ok_or_else(|| not_resolved(&node))?;
        if !pull.comment_visible(&pull.comments[index], &user.login) {
            return Err(not_resolved(&node));
        }
        if pull.comments[index].user != user.login {
            return Err(gql_error("FORBIDDEN", "Must be the comment author"));
        }
        pull.comments[index].body = body;
        let comment = pull.comments[index].clone();
        Ok(
            json!({"clientMutationId": null, "pullRequestReviewComment": comment_gql(pull, &comment)}),
        )
    }

    fn gql_resolve(
        &mut self,
        variables: &Value,
        user: &User,
        resolved: bool,
    ) -> Result<Value, GqlError> {
        let input = gql_input(variables)?;
        let node = gql_str(input, "threadId")?;
        let pull = self
            .pulls
            .values_mut()
            .find(|pull| pull.threads.iter().any(|thread| thread.node_id == node))
            .ok_or_else(|| not_resolved(&node))?;
        let index = pull
            .threads
            .iter()
            .position(|thread| thread.node_id == node)
            .ok_or_else(|| not_resolved(&node))?;
        let thread_id = pull.threads[index].id;
        let root_published = pull
            .comments
            .iter()
            .find(|comment| comment.thread_id == thread_id && comment.in_reply_to.is_none())
            .is_some_and(|comment| pull.comment_published(comment));
        if !root_published {
            return Err(not_resolved(&node));
        }
        pull.threads[index].resolved = resolved;
        let thread = pull.threads[index].clone();
        Ok(json!({"clientMutationId": null, "thread": thread_gql(pull, &thread, &user.login)}))
    }

    fn gql_node(&self, variables: &Value, user: &User) -> Result<Value, GqlError> {
        let id = variables
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| gql_error("VARIABLE", "forge-fake: variables.id is required"))?;
        for pull in self.pulls.values() {
            if pull.node_id == id {
                let viewed = pull.viewed.get(&user.login);
                let files: Vec<Value> = pull
                    .head()
                    .files
                    .iter()
                    .map(|file| {
                        let state = viewed
                            .and_then(|files| files.get(&file.path))
                            .map_or("UNVIEWED", |state| viewed_name(*state));
                        json!({"path": file.path, "viewerViewedState": state})
                    })
                    .collect();
                let threads: Vec<Value> = pull
                    .threads
                    .iter()
                    .filter(|thread| thread_visible(pull, thread, &user.login))
                    .map(|thread| thread_gql(pull, thread, &user.login))
                    .collect();
                return Ok(json!({
                    "__typename": "PullRequest",
                    "id": pull.node_id,
                    "number": pull.number,
                    "headRefOid": pull.head().sha,
                    "files": connection(files),
                    "reviewThreads": connection(threads),
                }));
            }
            if let Some(review) = pull.reviews.iter().find(|review| review.node_id == id) {
                if !pull.review_visible(review, &user.login) {
                    break;
                }
                let comments: Vec<Value> = pull
                    .comments
                    .iter()
                    .filter(|comment| comment.review_id == review.id)
                    .map(|comment| comment_gql(pull, comment))
                    .collect();
                return Ok(json!({
                    "__typename": "PullRequestReview",
                    "id": review.node_id,
                    "databaseId": review.id,
                    "state": review.state.name(),
                    "body": review.body,
                    "author": {"login": review.user},
                    "commit": {"oid": review.commit_id},
                    "comments": connection(comments),
                }));
            }
            if let Some(thread) = pull.threads.iter().find(|thread| thread.node_id == id) {
                if !thread_visible(pull, thread, &user.login) {
                    break;
                }
                let mut value = thread_gql(pull, thread, &user.login);
                value["__typename"] = json!("PullRequestReviewThread");
                return Ok(value);
            }
        }
        Err(not_resolved(id))
    }
}

impl Forge for GithubFake {
    fn key(&self, request: &Incoming) -> String {
        match self.route(&request.path) {
            Some(Route::Graphql) => {
                let field = request
                    .json()
                    .ok()
                    .and_then(|body| body.get("query").and_then(Value::as_str).map(str::to_owned))
                    .and_then(|query| graphql_root(&query))
                    .map_or_else(|| "?".to_owned(), |(_, field)| field);
                format!("graphql {field}")
            }
            Some(route) => format!("{} {}", request.method, route.template()),
            None => format!("unrouted {}", request.method),
        }
    }

    fn user(&self, request: &Incoming) -> Option<String> {
        request
            .bearer()
            .and_then(|token| self.user_by_token(token))
            .map(|user| user.login.clone())
    }

    fn handle(&mut self, request: &Incoming) -> Response {
        let resource = if self.route(&request.path) == Some(Route::Graphql) {
            "graphql"
        } else {
            "core"
        };
        let response = self.dispatch(request);
        self.rate_headers(response, resource)
    }

    fn rate_limited(&self, _request: &Incoming) -> Response {
        Response::json(
            403,
            &json!({
                "message": "API rate limit exceeded for user ID 1.",
                "documentation_url": DOCS,
            }),
        )
        .with_header("x-ratelimit-limit", RATE_LIMIT.to_string())
        .with_header("x-ratelimit-remaining", "0")
        .with_header("x-ratelimit-used", RATE_LIMIT.to_string())
        .with_header("x-ratelimit-reset", "1767229200")
        .with_header("x-ratelimit-resource", "core")
    }
}

fn lines_of(commit: &FakeCommit) -> BTreeMap<String, FileLines> {
    commit
        .files
        .iter()
        .map(|file| (file.path.clone(), FileLines::scan(&file.patch)))
        .collect()
}

fn check_event(
    pull: &Pull,
    user: &User,
    state: Option<ReviewState>,
    body: &str,
) -> Result<(), String> {
    match state {
        Some(ReviewState::Approved) if pull.author == user.login => {
            Err("Can not approve your own pull request".to_owned())
        }
        Some(ReviewState::ChangesRequested) if pull.author == user.login => {
            Err("Can not request changes on your own pull request".to_owned())
        }
        Some(ReviewState::Commented | ReviewState::ChangesRequested) if body.trim().is_empty() => {
            Err("Body is required for COMMENT and REQUEST_CHANGES".to_owned())
        }
        _ => Ok(()),
    }
}

fn viewed_name(state: Viewed) -> &'static str {
    match state {
        Viewed::Viewed => "VIEWED",
        Viewed::Dismissed => "DISMISSED",
    }
}

fn thread_visible(pull: &Pull, thread: &Thread, login: &str) -> bool {
    pull.comments
        .iter()
        .find(|comment| comment.thread_id == thread.id && comment.in_reply_to.is_none())
        .is_some_and(|comment| pull.comment_visible(comment, login))
}

fn ok(value: Value) -> Response {
    Response::json(200, &value)
}

fn created(value: Value) -> Response {
    Response::json(201, &value)
}

fn message(status: u16, text: &str) -> Response {
    Response::json(status, &json!({"message": text, "documentation_url": DOCS}))
}

fn not_found() -> Response {
    message(404, "Not Found")
}

fn unprocessable(error: &str) -> Response {
    Response::json(
        422,
        &json!({"message": "Unprocessable Entity", "errors": [error], "documentation_url": DOCS}),
    )
}

/// Страница списка: `per_page` по умолчанию 30, не больше 100, `page` с 1, ссылки `Link`.
fn paged(request: &Incoming, items: &[Value]) -> Response {
    let per_page = request
        .query_param("per_page")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(PER_PAGE_DEFAULT)
        .clamp(1, PER_PAGE_MAX);
    let page = request
        .query_param("page")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(1)
        .max(1);
    let last = items.len().div_ceil(per_page).max(1);
    let chunk: Vec<Value> = items
        .iter()
        .skip((page - 1) * per_page)
        .take(per_page)
        .cloned()
        .collect();
    let link = |target: usize, rel: &str| {
        format!(
            "<{}{}?per_page={per_page}&page={target}>; rel=\"{rel}\"",
            request.base, request.path
        )
    };
    let mut links = Vec::new();
    if page < last {
        links.push(link(page + 1, "next"));
        links.push(link(last, "last"));
    }
    if page > 1 {
        links.push(link(1, "first"));
        links.push(link(page - 1, "prev"));
    }
    let response = Response::json(200, &Value::Array(chunk));
    if links.is_empty() {
        response
    } else {
        response.with_header("link", links.join(", "))
    }
}

fn pull_json(pull: &Pull) -> Value {
    json!({
        "number": pull.number,
        "node_id": pull.node_id,
        "state": "open",
        "draft": false,
        "user": {"login": pull.author},
        "head": {"sha": pull.head().sha, "ref": format!("probe/{}", pull.number)},
        "base": {"sha": pull.base_sha, "ref": "main"},
    })
}

fn file_json(file: &FakeFile) -> Value {
    let mut value = json!({
        "filename": file.path,
        "status": file.status,
        "patch": file.patch,
    });
    if let Some(previous) = &file.previous_path {
        value["previous_filename"] = json!(previous);
    }
    value
}

fn review_json(pull: &Pull, review: &Review) -> Value {
    let mut value = json!({
        "id": review.id,
        "node_id": review.node_id,
        "user": {"login": review.user},
        "body": review.body,
        "state": review.state.name(),
        "commit_id": review.commit_id,
        "pull_request_url": format!("/pulls/{}", pull.number),
    });
    if let Some(submitted_at) = &review.submitted_at {
        value["submitted_at"] = json!(submitted_at);
    }
    value
}

fn comment_json(_pull: &Pull, comment: &Comment) -> Value {
    let mut value = json!({
        "id": comment.id,
        "node_id": comment.node_id,
        "pull_request_review_id": comment.review_id,
        "user": {"login": comment.user},
        "body": comment.body,
        "path": comment.path,
        "line": comment.line,
        "side": comment.side.name(),
        "start_line": comment.start_line,
        "start_side": comment.start_side.map(DiffSide::name),
        "original_line": comment.line,
        "commit_id": comment.commit_id,
        "original_commit_id": comment.commit_id,
        "subject_type": "line",
    });
    if let Some(parent) = comment.in_reply_to {
        value["in_reply_to_id"] = json!(parent);
    }
    value
}

fn comment_gql(pull: &Pull, comment: &Comment) -> Value {
    let review = pull.review(comment.review_id);
    let reply_to = comment.in_reply_to.and_then(|parent| {
        pull.comments
            .iter()
            .find(|c| c.id == parent)
            .map(|c| json!({"id": c.node_id, "fullDatabaseId": c.id.to_string()}))
    });
    json!({
        "id": comment.node_id,
        "fullDatabaseId": comment.id.to_string(),
        "body": comment.body,
        "path": comment.path,
        "line": comment.line,
        "startLine": comment.start_line,
        "diffSide": comment.side.name(),
        "startDiffSide": comment.start_side.map(DiffSide::name),
        "author": {"login": comment.user},
        "state": review.map_or("SUBMITTED", |r| if r.state == ReviewState::Pending { "PENDING" } else { "SUBMITTED" }),
        "pullRequestReview": review.map(|r| json!({"id": r.node_id, "databaseId": r.id})),
        "replyTo": reply_to,
    })
}

fn thread_gql(pull: &Pull, thread: &Thread, login: &str) -> Value {
    let comments: Vec<&Comment> = pull
        .comments
        .iter()
        .filter(|comment| comment.thread_id == thread.id && pull.comment_visible(comment, login))
        .collect();
    let root = comments
        .iter()
        .find(|comment| comment.in_reply_to.is_none());
    json!({
        "id": thread.node_id,
        "isResolved": thread.resolved,
        "isOutdated": false,
        "path": root.map(|c| c.path.clone()),
        "line": root.map(|c| c.line),
        "startLine": root.and_then(|c| c.start_line),
        "diffSide": root.map(|c| c.side.name()),
        "startDiffSide": root.and_then(|c| c.start_side.map(DiffSide::name)),
        "comments": connection(comments.iter().map(|c| comment_gql(pull, c)).collect()),
    })
}

/// Соединение GraphQL целиком на одной странице: пагинация по курсорам не моделируется.
fn connection(nodes: Vec<Value>) -> Value {
    let total = nodes.len();
    json!({
        "nodes": nodes,
        "totalCount": total,
        "pageInfo": {"hasNextPage": false, "endCursor": null},
    })
}

fn gql_input(variables: &Value) -> Result<&Value, GqlError> {
    variables
        .get("input")
        .filter(|input| input.is_object())
        .ok_or_else(|| gql_error("VARIABLE", "forge-fake: variables.input is required"))
}

fn gql_str(input: &Value, name: &str) -> Result<String, GqlError> {
    input
        .get(name)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| {
            gql_error(
                "VARIABLE",
                format!("Variable $input of type input was provided invalid value for {name}"),
            )
        })
}

fn not_resolved(id: &str) -> GqlError {
    gql_error(
        "NOT_FOUND",
        format!("Could not resolve to a node with the global id of '{id}'"),
    )
}

fn gql_response(data: Option<Value>, error: Option<GqlError>) -> Response {
    let mut body = Map::new();
    if let Some(data) = data {
        body.insert("data".to_owned(), data);
    }
    if let Some(error) = error {
        body.insert(
            "errors".to_owned(),
            json!([{"type": error.kind, "message": error.message}]),
        );
    }
    Response::json(200, &Value::Object(body))
}

#[cfg(test)]
mod tests;
