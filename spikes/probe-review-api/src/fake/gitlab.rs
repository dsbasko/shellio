//! GitLab для `forge-fake`: подмножество REST v4 и GraphQL по документации
//! (<https://docs.gitlab.com/api/draft_notes/>, <https://docs.gitlab.com/api/discussions/>,
//! <https://docs.gitlab.com/api/notes/>, <https://docs.gitlab.com/api/merge_request_approvals/>,
//! <https://docs.gitlab.com/api/merge_requests/>, <https://docs.gitlab.com/api/graphql/reference/>).
//!
//! Строже живого хоста: позиция принимается, только если её тройка SHA равна тройке одной из
//! версий MR, а строки и `line_range` проверяются по diff этой версии собственным разбором; тело —
//! только JSON. Публикация черновика отвечает 204 без тела: связь заметки с черновиком видят только
//! оракулы. `bulk_publish` реализован и публикует все черновики пользователя, в том числе
//! подложенные.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use super::{Forge, GraphqlKind, Incoming, graphql_root};
use crate::transport::{Method, Response};

const PER_PAGE_DEFAULT: usize = 20;
const PER_PAGE_MAX: usize = 100;
const RATE_LIMIT: u64 = 2000;
const NOT_AVAILABLE: &str = "The resource that you are attempting to access does not exist or \
                             you don't have permission to perform this action";
/// Команды, которые нормализация снимает с тела (подмножество quick actions).
const QUICK_ACTIONS: [&str; 14] = [
    "approve",
    "unapprove",
    "assign",
    "unassign",
    "assign_reviewer",
    "request_review",
    "unassign_reviewer",
    "label",
    "unlabel",
    "close",
    "reopen",
    "draft",
    "ready",
    "submit_review",
];

pub const DRAFTS: &str = "POST /projects/{id}/merge_requests/{merge_request_iid}/draft_notes";
pub const DRAFT_UPDATE: &str =
    "PUT /projects/{id}/merge_requests/{merge_request_iid}/draft_notes/{draft_note_id}";
pub const PUBLISH: &str =
    "PUT /projects/{id}/merge_requests/{merge_request_iid}/draft_notes/{draft_note_id}/publish";
pub const BULK_PUBLISH: &str =
    "POST /projects/{id}/merge_requests/{merge_request_iid}/draft_notes/bulk_publish";
pub const NOTES: &str = "POST /projects/{id}/merge_requests/{merge_request_iid}/notes";
pub const APPROVE: &str = "POST /projects/{id}/merge_requests/{merge_request_iid}/approve";
pub const UNAPPROVE: &str = "POST /projects/{id}/merge_requests/{merge_request_iid}/unapprove";
pub const REQUEST_CHANGES: &str = "graphql mergeRequestRequestChanges";

/// Уровень доступа участника проекта (`access_level`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Access {
    Guest = 10,
    Reporter = 20,
    Developer = 30,
    Maintainer = 40,
    Owner = 50,
}

#[derive(Clone, Debug)]
struct User {
    id: u64,
    username: String,
    name: String,
    token: String,
    access: Option<Access>,
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

/// `MergeRequestReviewState`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewerState {
    Unreviewed,
    ReviewStarted,
    Reviewed,
    RequestedChanges,
    Approved,
    Unapproved,
}

impl ReviewerState {
    /// Значение REST (`state` в списке ревьюеров).
    pub fn rest(self) -> &'static str {
        match self {
            ReviewerState::Unreviewed => "unreviewed",
            ReviewerState::ReviewStarted => "review_started",
            ReviewerState::Reviewed => "reviewed",
            ReviewerState::RequestedChanges => "requested_changes",
            ReviewerState::Approved => "approved",
            ReviewerState::Unapproved => "unapproved",
        }
    }

    /// Значение перечисления GraphQL.
    pub fn graphql(self) -> &'static str {
        match self {
            ReviewerState::Unreviewed => "UNREVIEWED",
            ReviewerState::ReviewStarted => "REVIEW_STARTED",
            ReviewerState::Reviewed => "REVIEWED",
            ReviewerState::RequestedChanges => "REQUESTED_CHANGES",
            ReviewerState::Approved => "APPROVED",
            ReviewerState::Unapproved => "UNAPPROVED",
        }
    }
}

/// Режим `mergeRequestRequestChanges` — состояние сервера, а не правило отказа.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestChangesMode {
    /// Мутация есть; итог определяют права и назначение ревьюером.
    Available,
    /// Мутации нет в схеме: introspection её не показывает, вызов — `undefinedField`.
    Absent,
    /// Ответ «не существует или нет прав» при любых правах.
    Forbidden,
    /// Ошибка исполнения верхнего уровня при HTTP 200, `data.<поле>` — `null`.
    ExecutionError,
    /// `payload.errors` не пуст при HTTP 200.
    PayloadError,
}

/// Файл версии MR: `diff` — unified diff без заголовков файла.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FakeDiff {
    pub old_path: String,
    pub new_path: String,
    pub diff: String,
    pub new_file: bool,
    pub deleted_file: bool,
}

impl FakeDiff {
    pub fn modified(path: &str, diff: &str) -> FakeDiff {
        FakeDiff {
            old_path: path.to_owned(),
            new_path: path.to_owned(),
            diff: diff.to_owned(),
            new_file: false,
            deleted_file: false,
        }
    }

    pub fn renamed(old_path: &str, new_path: &str, diff: &str) -> FakeDiff {
        FakeDiff {
            old_path: old_path.to_owned(),
            ..FakeDiff::modified(new_path, diff)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Added,
    Removed,
    Context,
}

/// Строка diff по разбору GitLab: оба счётчика у каждой строки. Разбор свой, независимый от модели.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ScanLine {
    kind: Kind,
    old: u32,
    new: u32,
    hunk: usize,
    index: usize,
}

impl ScanLine {
    fn old_line(self) -> Option<u32> {
        (self.kind != Kind::Added).then_some(self.old)
    }

    fn new_line(self) -> Option<u32> {
        (self.kind != Kind::Removed).then_some(self.new)
    }

    fn type_name(self) -> Option<&'static str> {
        match self.kind {
            Kind::Added => Some("new"),
            Kind::Removed => Some("old"),
            Kind::Context => None,
        }
    }
}

fn scan(diff: &str) -> Vec<ScanLine> {
    let mut lines = Vec::new();
    let (mut old, mut new, mut hunk) = (0u32, 0u32, 0usize);
    for text in diff.lines() {
        if let Some(header) = text.strip_prefix("@@ ") {
            hunk += 1;
            let mut parts = header.split_whitespace();
            old = hunk_start(parts.next(), '-');
            new = hunk_start(parts.next(), '+');
            continue;
        }
        if hunk == 0 {
            continue;
        }
        let kind = match text.chars().next() {
            Some('+') => Kind::Added,
            Some('-') => Kind::Removed,
            Some('\\') => continue,
            _ => Kind::Context,
        };
        lines.push(ScanLine {
            kind,
            old,
            new,
            hunk,
            index: lines.len(),
        });
        match kind {
            Kind::Added => new += 1,
            Kind::Removed => old += 1,
            Kind::Context => {
                old += 1;
                new += 1;
            }
        }
    }
    lines
}

fn hunk_start(part: Option<&str>, sign: char) -> u32 {
    part.and_then(|part| part.strip_prefix(sign))
        .and_then(|range| range.split(',').next())
        .and_then(|start| start.parse().ok())
        .unwrap_or(0)
}

/// `line_code`: SHA-1 нового пути и оба счётчика строки.
fn line_code(path: &str, line: ScanLine) -> String {
    format!(
        "{}_{}_{}",
        sha1_smol::Sha1::from(path).digest(),
        line.old,
        line.new
    )
}

#[derive(Clone, Debug)]
struct Version {
    id: u64,
    head_sha: String,
    base_sha: String,
    start_sha: String,
    diffs: Vec<FakeDiff>,
    lines: Vec<Vec<ScanLine>>,
    created_at: String,
}

impl Version {
    fn same_triple(&self, base: &str, start: &str, head: &str) -> bool {
        self.base_sha == base && self.start_sha == start && self.head_sha == head
    }
}

/// Проверенная позиция `text`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Position {
    pub base_sha: String,
    pub start_sha: String,
    pub head_sha: String,
    pub old_path: String,
    pub new_path: String,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    pub line_range: Option<Value>,
    pub line_code: String,
}

impl Position {
    fn json(&self) -> Value {
        json!({
            "base_sha": self.base_sha,
            "start_sha": self.start_sha,
            "head_sha": self.head_sha,
            "old_path": self.old_path,
            "new_path": self.new_path,
            "position_type": "text",
            "old_line": self.old_line,
            "new_line": self.new_line,
            "line_range": self.line_range,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DraftNote {
    pub id: u64,
    pub author: String,
    pub note: String,
    pub position: Option<Position>,
    pub discussion_id: Option<String>,
    pub resolve_discussion: bool,
    pub origin: Origin,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    pub id: u64,
    pub discussion_id: String,
    pub author: String,
    pub body: String,
    pub position: Option<Position>,
    pub created_at: String,
    pub origin: Origin,
    /// Черновик, из которого опубликована заметка. Ответы API эту связь не показывают.
    pub from_draft: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Discussion {
    pub id: String,
    pub individual_note: bool,
    pub resolvable: bool,
    pub resolved: bool,
}

#[derive(Clone, Debug)]
struct Mr {
    iid: u64,
    id: u64,
    author: String,
    versions: Vec<Version>,
    reviewers: Vec<(String, ReviewerState)>,
    approved_by: Vec<(String, String)>,
    drafts: Vec<DraftNote>,
    notes: Vec<Note>,
    discussions: Vec<Discussion>,
    /// Целевая ветка сменила diff без нового head: `approve` с `sha` даёт 409 до следующего
    /// коммита исходной ветки (документация Approvals).
    target_moved: bool,
}

impl Mr {
    fn latest(&self) -> &Version {
        &self.versions[self.versions.len() - 1]
    }

    fn reviewer(&self, username: &str) -> Option<ReviewerState> {
        self.reviewers
            .iter()
            .find(|(name, _)| name == username)
            .map(|(_, state)| *state)
    }

    fn set_reviewer(&mut self, username: &str, state: ReviewerState) -> bool {
        match self.reviewers.iter_mut().find(|(name, _)| name == username) {
            Some(entry) => {
                entry.1 = state;
                true
            }
            None => false,
        }
    }

    fn discussion(&self, id: &str) -> Option<&Discussion> {
        self.discussions.iter().find(|d| d.id == id)
    }

    /// Проверка позиции по версии с той же тройкой SHA.
    fn position(&self, value: &Value) -> Result<Position, String> {
        let object = value.as_object().ok_or("position is invalid")?;
        let text = |name: &str| object.get(name).and_then(Value::as_str).unwrap_or("");
        if text("position_type") != "text" {
            return Err("forge-fake: only text positions are modeled".to_owned());
        }
        let (base, start, head) = (text("base_sha"), text("start_sha"), text("head_sha"));
        if base.is_empty() || start.is_empty() || head.is_empty() {
            return Err("position is incomplete".to_owned());
        }
        let version = self
            .versions
            .iter()
            .find(|version| version.same_triple(base, start, head))
            .ok_or("forge-fake: position SHAs do not match a merge request version")?;
        let (old_path, new_path) = (text("old_path"), text("new_path"));
        let file = version
            .diffs
            .iter()
            .position(|diff| diff.old_path == old_path && diff.new_path == new_path)
            .ok_or("forge-fake: position paths are not in the version diff")?;
        let lines = &version.lines[file];
        let number = |name: &str, from: &Map<String, Value>| -> Result<Option<u32>, String> {
            match from.get(name) {
                None | Some(Value::Null) => Ok(None),
                Some(value) => value
                    .as_u64()
                    .and_then(|line| u32::try_from(line).ok())
                    .filter(|line| *line > 0)
                    .map(Some)
                    .ok_or_else(|| format!("{name} is not a positive integer")),
            }
        };
        let old_line = number("old_line", object)?;
        let new_line = number("new_line", object)?;
        let end = find_line(lines, old_line, new_line)?;
        let line_range = match object.get("line_range") {
            None | Some(Value::Null) => None,
            Some(range) => {
                let range_object = range.as_object().ok_or("line_range is invalid")?;
                let point = |name: &str| -> Result<ScanLine, String> {
                    let point = range_object
                        .get(name)
                        .and_then(Value::as_object)
                        .ok_or_else(|| format!("line_range[{name}] is missing"))?;
                    let line = find_line(
                        lines,
                        number("old_line", point)?,
                        number("new_line", point)?,
                    )?;
                    if point.get("line_code").and_then(Value::as_str)
                        != Some(line_code(new_path, line).as_str())
                    {
                        return Err(format!("line_range[{name}][line_code] is invalid"));
                    }
                    let kind = point.get("type").and_then(Value::as_str);
                    if kind != line.type_name() {
                        return Err(format!("line_range[{name}][type] is invalid"));
                    }
                    Ok(line)
                };
                let (first, last) = (point("start")?, point("end")?);
                if last != end {
                    return Err("forge-fake: line_range end is not the position line".to_owned());
                }
                if first.hunk != last.hunk || first.index > last.index {
                    return Err(
                        "forge-fake: line_range start must precede end in one hunk".to_owned()
                    );
                }
                Some(range.clone())
            }
        };
        Ok(Position {
            base_sha: base.to_owned(),
            start_sha: start.to_owned(),
            head_sha: head.to_owned(),
            old_path: old_path.to_owned(),
            new_path: new_path.to_owned(),
            old_line,
            new_line,
            line_range,
            line_code: line_code(new_path, end),
        })
    }
}

/// Строка по паре номеров: добавленная — только `new_line`, удалённая — только `old_line`,
/// неизменённая — оба.
fn find_line(
    lines: &[ScanLine],
    old_line: Option<u32>,
    new_line: Option<u32>,
) -> Result<ScanLine, String> {
    if old_line.is_none() && new_line.is_none() {
        return Err("position is incomplete: old_line or new_line is required".to_owned());
    }
    lines
        .iter()
        .find(|line| line.old_line() == old_line && line.new_line() == new_line)
        .copied()
        .ok_or_else(|| "forge-fake: line is not in the diff with these numbers".to_owned())
}

/// Состояние GitLab одного проекта.
#[derive(Clone, Debug)]
pub struct GitlabFake {
    project_id: u64,
    path: String,
    visibility: String,
    default_branch: String,
    version: String,
    enterprise: bool,
    users: Vec<User>,
    mrs: BTreeMap<u64, Mr>,
    request_changes: RequestChangesMode,
    next_id: u64,
    clock: u64,
    remaining: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
    User,
    Version,
    Metadata,
    Graphql,
    Project,
    Mr(u64),
    Versions(u64),
    VersionOne(u64, u64),
    Diffs(u64),
    Reviewers(u64),
    Drafts(u64),
    Draft(u64, u64),
    Publish(u64, u64),
    BulkPublish(u64),
    Notes(u64),
    Discussions(u64),
    Discussion(u64),
    DiscussionNotes(u64),
    Approve(u64),
    Unapprove(u64),
    Approvals(u64),
}

impl Route {
    fn template(self) -> &'static str {
        match self {
            Route::User => "/user",
            Route::Version => "/version",
            Route::Metadata => "/metadata",
            Route::Graphql => "/graphql",
            Route::Project => "/projects/{id}",
            Route::Mr(_) => "/projects/{id}/merge_requests/{merge_request_iid}",
            Route::Versions(_) => "/projects/{id}/merge_requests/{merge_request_iid}/versions",
            Route::VersionOne(..) => {
                "/projects/{id}/merge_requests/{merge_request_iid}/versions/{version_id}"
            }
            Route::Diffs(_) => "/projects/{id}/merge_requests/{merge_request_iid}/diffs",
            Route::Reviewers(_) => "/projects/{id}/merge_requests/{merge_request_iid}/reviewers",
            Route::Drafts(_) => "/projects/{id}/merge_requests/{merge_request_iid}/draft_notes",
            Route::Draft(..) => {
                "/projects/{id}/merge_requests/{merge_request_iid}/draft_notes/{draft_note_id}"
            }
            Route::Publish(..) => {
                "/projects/{id}/merge_requests/{merge_request_iid}/draft_notes/{draft_note_id}/publish"
            }
            Route::BulkPublish(_) => {
                "/projects/{id}/merge_requests/{merge_request_iid}/draft_notes/bulk_publish"
            }
            Route::Notes(_) => "/projects/{id}/merge_requests/{merge_request_iid}/notes",
            Route::Discussions(_) => {
                "/projects/{id}/merge_requests/{merge_request_iid}/discussions"
            }
            Route::Discussion(_) => {
                "/projects/{id}/merge_requests/{merge_request_iid}/discussions/{discussion_id}"
            }
            Route::DiscussionNotes(_) => {
                "/projects/{id}/merge_requests/{merge_request_iid}/discussions/{discussion_id}/notes"
            }
            Route::Approve(_) => "/projects/{id}/merge_requests/{merge_request_iid}/approve",
            Route::Unapprove(_) => "/projects/{id}/merge_requests/{merge_request_iid}/unapprove",
            Route::Approvals(_) => "/projects/{id}/merge_requests/{merge_request_iid}/approvals",
        }
    }
}

/// Сегмент пути с `%XX`; `+` остаётся собой.
fn decode_segment(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let hex = (bytes[index] == b'%')
            .then(|| bytes.get(index + 1..index + 3))
            .flatten()
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match hex {
            Some(byte) => {
                out.push(byte);
                index += 3;
            }
            None => {
                out.push(bytes[index]);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Нормализация тела заметки по документации Notes API: без `\r` и хвостовых пробелов.
fn normalize(body: &str) -> String {
    let lines: Vec<&str> = body.split('\n').collect();
    let cleaned: Vec<String> = lines.iter().map(|line| line.replace('\r', "")).collect();
    cleaned.join("\n").trim_end().to_owned()
}

/// Публикация снимает строки quick actions (исполнение не моделируется) и нормализует тело.
fn strip_quick_actions(body: &str) -> String {
    let kept: Vec<&str> = body
        .split('\n')
        .filter(|line| {
            let Some(command) = line.trim_start().strip_prefix('/') else {
                return true;
            };
            let name = command.split_whitespace().next().unwrap_or("");
            !QUICK_ACTIONS.contains(&name)
        })
        .collect();
    normalize(&kept.join("\n"))
}

impl GitlabFake {
    /// Приватный проект `path` (`группа/проект`) с веткой по умолчанию `main`.
    pub fn new(path: &str) -> GitlabFake {
        GitlabFake {
            project_id: 42,
            path: path.to_owned(),
            visibility: "private".to_owned(),
            default_branch: "main".to_owned(),
            version: "18.5.1".to_owned(),
            enterprise: false,
            users: Vec::new(),
            mrs: BTreeMap::new(),
            request_changes: RequestChangesMode::Available,
            next_id: 2000,
            clock: 0,
            remaining: RATE_LIMIT,
        }
    }

    pub fn project_id(&self) -> u64 {
        self.project_id
    }

    pub fn set_visibility(&mut self, visibility: &str) {
        self.visibility = visibility.to_owned();
    }

    pub fn set_version(&mut self, version: &str, enterprise: bool) {
        self.version = version.to_owned();
        self.enterprise = enterprise;
    }

    pub fn set_request_changes(&mut self, mode: RequestChangesMode) {
        self.request_changes = mode;
    }

    /// Пользователь; `access: None` — не участник проекта.
    pub fn add_user(&mut self, username: &str, token: &str, access: Option<Access>) {
        let id = self.id();
        self.users.push(User {
            id,
            username: username.to_owned(),
            name: format!("{username} name"),
            token: token.to_owned(),
            access,
        });
    }

    /// MR с первой версией.
    pub fn add_mr(
        &mut self,
        iid: u64,
        author: &str,
        base_sha: &str,
        head_sha: &str,
        diffs: Vec<FakeDiff>,
    ) {
        let id = self.id();
        let version = self.version_of(base_sha, base_sha, head_sha, diffs);
        self.mrs.insert(
            iid,
            Mr {
                iid,
                id,
                author: author.to_owned(),
                versions: vec![version],
                reviewers: Vec::new(),
                approved_by: Vec::new(),
                drafts: Vec::new(),
                notes: Vec::new(),
                discussions: Vec::new(),
                target_moved: false,
            },
        );
    }

    pub fn add_reviewer(&mut self, iid: u64, username: &str) -> Result<(), String> {
        let mr = self.mrs.get_mut(&iid).ok_or("нет MR")?;
        if mr.reviewer(username).is_none() {
            mr.reviewers
                .push((username.to_owned(), ReviewerState::Unreviewed));
        }
        Ok(())
    }

    /// Новый коммит исходной ветки: новая версия с тем же `base_sha` и `start_sha`.
    pub fn push_head(
        &mut self,
        iid: u64,
        head_sha: &str,
        diffs: Vec<FakeDiff>,
    ) -> Result<(), String> {
        let latest = self.mrs.get(&iid).ok_or("нет MR")?.latest();
        let (base, start) = (latest.base_sha.clone(), latest.start_sha.clone());
        self.push_version(iid, &base, &start, head_sha, diffs)
    }

    /// Новый head вместе с новой базой (rebase исходной ветки).
    pub fn push_version(
        &mut self,
        iid: u64,
        base_sha: &str,
        start_sha: &str,
        head_sha: &str,
        diffs: Vec<FakeDiff>,
    ) -> Result<(), String> {
        let mr = self.mrs.get(&iid).ok_or("нет MR")?;
        if mr
            .versions
            .iter()
            .any(|version| version.head_sha == head_sha)
        {
            return Err(format!("коммит {head_sha} уже есть в MR"));
        }
        let version = self.version_of(base_sha, start_sha, head_sha, diffs);
        let mr = self.mrs.get_mut(&iid).ok_or("нет MR")?;
        mr.versions.push(version);
        mr.target_moved = false;
        Ok(())
    }

    /// Целевая ветка ушла вперёд: новая версия с тем же head и новыми `start_sha`, `base_sha`.
    pub fn move_target(
        &mut self,
        iid: u64,
        base_sha: &str,
        start_sha: &str,
        diffs: Vec<FakeDiff>,
    ) -> Result<(), String> {
        let head = self
            .mrs
            .get(&iid)
            .ok_or("нет MR")?
            .latest()
            .head_sha
            .clone();
        let version = self.version_of(base_sha, start_sha, &head, diffs);
        let mr = self.mrs.get_mut(&iid).ok_or("нет MR")?;
        mr.versions.push(version);
        mr.target_moved = true;
        Ok(())
    }

    /// Черновик другого клиента того же пользователя (браузер) в обход HTTP; счётчики вызовов не
    /// меняются. `position` — в форме API.
    pub fn plant_draft(
        &mut self,
        iid: u64,
        username: &str,
        note: &str,
        position: Option<&Value>,
    ) -> Result<u64, String> {
        let mr = self.mrs.get(&iid).ok_or("нет MR")?;
        let position = position.map(|value| mr.position(value)).transpose()?;
        let id = self.id();
        let draft = DraftNote {
            id,
            author: username.to_owned(),
            note: normalize(note),
            position,
            discussion_id: None,
            resolve_discussion: false,
            origin: Origin::Planted,
        };
        self.mrs.get_mut(&iid).ok_or("нет MR")?.drafts.push(draft);
        Ok(id)
    }

    /// Другой клиент удалил черновик по ID.
    pub fn remove_draft(&mut self, iid: u64, id: u64) -> Result<(), String> {
        let mr = self.mrs.get_mut(&iid).ok_or("нет MR")?;
        let before = mr.drafts.len();
        mr.drafts.retain(|draft| draft.id != id);
        if mr.drafts.len() == before {
            return Err(format!("нет черновика {id}"));
        }
        Ok(())
    }

    /// Другой клиент того же пользователя опубликовал заметку (тред с позицией или без).
    /// Возвращает ID треда и заметки.
    pub fn plant_discussion(
        &mut self,
        iid: u64,
        username: &str,
        body: &str,
        position: Option<&Value>,
    ) -> Result<(String, u64), String> {
        let mr = self.mrs.get(&iid).ok_or("нет MR")?;
        let position = position.map(|value| mr.position(value)).transpose()?;
        let individual = position.is_none();
        let discussion = self.new_discussion(iid, individual)?;
        let id = self.insert_note(
            iid,
            &discussion,
            username,
            &strip_quick_actions(body),
            position,
            Origin::Planted,
            None,
        )?;
        Ok((discussion, id))
    }

    pub fn diff_refs(&self, iid: u64) -> Option<(String, String, String)> {
        self.mrs.get(&iid).map(|mr| {
            let latest = mr.latest();
            (
                latest.base_sha.clone(),
                latest.start_sha.clone(),
                latest.head_sha.clone(),
            )
        })
    }

    pub fn drafts(&self, iid: u64) -> Vec<DraftNote> {
        self.mrs
            .get(&iid)
            .map(|mr| mr.drafts.clone())
            .unwrap_or_default()
    }

    pub fn notes(&self, iid: u64) -> Vec<Note> {
        self.mrs
            .get(&iid)
            .map(|mr| mr.notes.clone())
            .unwrap_or_default()
    }

    pub fn discussions(&self, iid: u64) -> Vec<Discussion> {
        self.mrs
            .get(&iid)
            .map(|mr| mr.discussions.clone())
            .unwrap_or_default()
    }

    pub fn reviewer_state(&self, iid: u64, username: &str) -> Option<ReviewerState> {
        self.mrs.get(&iid).and_then(|mr| mr.reviewer(username))
    }

    pub fn approved_by(&self, iid: u64) -> Vec<String> {
        self.mrs
            .get(&iid)
            .map(|mr| {
                mr.approved_by
                    .iter()
                    .map(|(name, _)| name.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Серверное состояние для `result.json` (до записи на диск — через очиститель).
    pub fn snapshot(&self) -> Value {
        let mrs: Vec<Value> = self
            .mrs
            .values()
            .map(|mr| {
                json!({
                    "iid": mr.iid,
                    "author": mr.author,
                    "versions": mr.versions.iter().map(|v| json!({
                        "id": v.id,
                        "base_sha": v.base_sha,
                        "start_sha": v.start_sha,
                        "head_sha": v.head_sha,
                    })).collect::<Vec<_>>(),
                    "reviewers": mr.reviewers.iter().map(|(name, state)| json!({
                        "username": name,
                        "state": state.rest(),
                    })).collect::<Vec<_>>(),
                    "approved_by": mr.approved_by.iter().map(|(name, _)| name.clone()).collect::<Vec<_>>(),
                    "drafts": mr.drafts.iter().map(|d| json!({
                        "id": d.id,
                        "author": d.author,
                        "note": d.note,
                        "position": d.position.as_ref().map(Position::json),
                        "discussion_id": d.discussion_id,
                        "origin": d.origin.name(),
                    })).collect::<Vec<_>>(),
                    "discussions": mr.discussions.iter().map(|d| json!({
                        "id": d.id,
                        "individual_note": d.individual_note,
                        "resolved": d.resolved,
                    })).collect::<Vec<_>>(),
                    "notes": mr.notes.iter().map(|n| json!({
                        "id": n.id,
                        "discussion_id": n.discussion_id,
                        "author": n.author,
                        "body": n.body,
                        "position": n.position.as_ref().map(Position::json),
                        "origin": n.origin.name(),
                        "from_draft": n.from_draft,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();
        json!({ "merge_requests": mrs, "request_changes": format!("{:?}", self.request_changes) })
    }

    fn id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn now(&mut self) -> String {
        self.clock += 1;
        let seconds = self.clock;
        format!(
            "2026-01-01T{:02}:{:02}:{:02}.000Z",
            (seconds / 3600) % 24,
            (seconds / 60) % 60,
            seconds % 60
        )
    }

    fn version_of(&mut self, base: &str, start: &str, head: &str, diffs: Vec<FakeDiff>) -> Version {
        let id = self.id();
        let created_at = self.now();
        Version {
            id,
            head_sha: head.to_owned(),
            base_sha: base.to_owned(),
            start_sha: start.to_owned(),
            lines: diffs.iter().map(|diff| scan(&diff.diff)).collect(),
            diffs,
            created_at,
        }
    }

    fn new_discussion(&mut self, iid: u64, individual: bool) -> Result<String, String> {
        let n = self.id();
        let id = sha1_smol::Sha1::from(format!("forge-fake discussion {n}"))
            .digest()
            .to_string();
        let mr = self.mrs.get_mut(&iid).ok_or("нет MR")?;
        mr.discussions.push(Discussion {
            id: id.clone(),
            individual_note: individual,
            resolvable: !individual,
            resolved: false,
        });
        Ok(id)
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_note(
        &mut self,
        iid: u64,
        discussion: &str,
        username: &str,
        body: &str,
        position: Option<Position>,
        origin: Origin,
        from_draft: Option<u64>,
    ) -> Result<u64, String> {
        let id = self.id();
        let created_at = self.now();
        let mr = self.mrs.get_mut(&iid).ok_or("нет MR")?;
        mr.notes.push(Note {
            id,
            discussion_id: discussion.to_owned(),
            author: username.to_owned(),
            body: body.to_owned(),
            position,
            created_at,
            origin,
            from_draft,
        });
        Ok(id)
    }

    /// Публикация черновика: новая заметка в новом или указанном треде, черновик удаляется.
    fn publish_draft(&mut self, iid: u64, draft: DraftNote) -> Result<u64, String> {
        let discussion = match &draft.discussion_id {
            Some(id) => id.clone(),
            None => self.new_discussion(iid, draft.position.is_none())?,
        };
        let id = self.insert_note(
            iid,
            &discussion,
            &draft.author,
            &strip_quick_actions(&draft.note),
            draft.position.clone(),
            draft.origin,
            Some(draft.id),
        )?;
        let mr = self.mrs.get_mut(&iid).ok_or("нет MR")?;
        mr.drafts.retain(|d| d.id != draft.id);
        if draft.resolve_discussion
            && let Some(stored) = mr.discussions.iter_mut().find(|d| d.id == discussion)
        {
            stored.resolved = true;
        }
        Ok(id)
    }

    fn user_by_token(&self, token: &str) -> Option<&User> {
        self.users.iter().find(|user| user.token == token)
    }

    fn caller(&self, request: &Incoming) -> Option<&User> {
        let token = request
            .header("private-token")
            .or_else(|| request.bearer())?;
        self.user_by_token(token)
    }

    fn route(&self, path: &str) -> Option<(Route, Option<String>)> {
        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let number = |s: &str| s.parse::<u64>().ok();
        match segments[..] {
            ["api", "graphql"] => Some((Route::Graphql, None)),
            ["api", "v4", "user"] => Some((Route::User, None)),
            ["api", "v4", "version"] => Some((Route::Version, None)),
            ["api", "v4", "metadata"] => Some((Route::Metadata, None)),
            ["api", "v4", "projects", project, ref rest @ ..] => {
                let project = decode_segment(project);
                if project != self.project_id.to_string()
                    && !project.eq_ignore_ascii_case(&self.path)
                {
                    return None;
                }
                let route = match *rest {
                    [] => Route::Project,
                    ["merge_requests", n] => Route::Mr(number(n)?),
                    ["merge_requests", n, "versions"] => Route::Versions(number(n)?),
                    ["merge_requests", n, "versions", v] => {
                        Route::VersionOne(number(n)?, number(v)?)
                    }
                    ["merge_requests", n, "diffs"] => Route::Diffs(number(n)?),
                    ["merge_requests", n, "reviewers"] => Route::Reviewers(number(n)?),
                    ["merge_requests", n, "draft_notes"] => Route::Drafts(number(n)?),
                    ["merge_requests", n, "draft_notes", "bulk_publish"] => {
                        Route::BulkPublish(number(n)?)
                    }
                    ["merge_requests", n, "draft_notes", d] => Route::Draft(number(n)?, number(d)?),
                    ["merge_requests", n, "draft_notes", d, "publish"] => {
                        Route::Publish(number(n)?, number(d)?)
                    }
                    ["merge_requests", n, "notes"] => Route::Notes(number(n)?),
                    ["merge_requests", n, "discussions"] => Route::Discussions(number(n)?),
                    ["merge_requests", n, "discussions", d] => {
                        return Some((Route::Discussion(number(n)?), Some(decode_segment(d))));
                    }
                    ["merge_requests", n, "discussions", d, "notes"] => {
                        return Some((Route::DiscussionNotes(number(n)?), Some(decode_segment(d))));
                    }
                    ["merge_requests", n, "approve"] => Route::Approve(number(n)?),
                    ["merge_requests", n, "unapprove"] => Route::Unapprove(number(n)?),
                    ["merge_requests", n, "approvals"] => Route::Approvals(number(n)?),
                    _ => return None,
                };
                Some((route, None))
            }
            _ => None,
        }
    }

    fn rate_headers(&mut self, response: Response) -> Response {
        self.remaining = self.remaining.saturating_sub(1);
        response
            .with_header("ratelimit-limit", RATE_LIMIT.to_string())
            .with_header(
                "ratelimit-observed",
                (RATE_LIMIT - self.remaining).to_string(),
            )
            .with_header("ratelimit-remaining", self.remaining.to_string())
            .with_header("ratelimit-reset", "1767229200")
            .with_header("ratelimit-name", "throttle_authenticated_api")
    }

    fn dispatch(&mut self, request: &Incoming) -> Response {
        let Some((route, discussion)) = self.route(&request.path) else {
            return message(404, "404 Not Found");
        };
        let Some(user) = self.caller(request).cloned() else {
            return message(401, "401 Unauthorized");
        };
        if route == Route::Graphql {
            return match request.method {
                Method::Post => self.graphql(request, &user),
                _ => message(404, "404 Not Found"),
            };
        }
        let body = match request.json() {
            Ok(Value::Object(body)) => body,
            Ok(Value::Null) => Map::new(),
            _ => return message(400, "400 Bad request - body is not a JSON object"),
        };
        match (route, request.method) {
            (Route::User, Method::Get) => return ok(user_json(&user)),
            (Route::Version, Method::Get) => {
                return ok(json!({"version": self.version, "revision": "fake0001"}));
            }
            (Route::Metadata, Method::Get) => {
                return ok(json!({
                    "version": self.version,
                    "revision": "fake0001",
                    "kas": {"enabled": false, "externalUrl": null, "version": null},
                    "enterprise": self.enterprise,
                }));
            }
            _ => {}
        }
        if user.access.is_none() {
            return message(404, "404 Project Not Found");
        }
        if route == Route::Project {
            return match request.method {
                Method::Get => ok(self.project_json(&user)),
                _ => message(405, "405 Method Not Allowed"),
            };
        }
        let iid = match route {
            Route::Mr(n)
            | Route::Versions(n)
            | Route::VersionOne(n, _)
            | Route::Diffs(n)
            | Route::Reviewers(n)
            | Route::Drafts(n)
            | Route::Draft(n, _)
            | Route::Publish(n, _)
            | Route::BulkPublish(n)
            | Route::Notes(n)
            | Route::Discussions(n)
            | Route::Discussion(n)
            | Route::DiscussionNotes(n)
            | Route::Approve(n)
            | Route::Unapprove(n)
            | Route::Approvals(n) => n,
            Route::User | Route::Version | Route::Metadata | Route::Graphql | Route::Project => {
                return message(404, "404 Not Found");
            }
        };
        if !self.mrs.contains_key(&iid) {
            return message(404, "404 Not found");
        }
        let discussion = discussion.unwrap_or_default();
        match (route, request.method) {
            (Route::Mr(_), Method::Get) => self.read(iid, |fake, mr| ok(fake.mr_json(mr))),
            (Route::Versions(_), Method::Get) => self.read(iid, |_, mr| {
                let versions: Vec<Value> = mr.versions.iter().rev().map(version_json).collect();
                paged(request, &versions)
            }),
            (Route::VersionOne(_, v), Method::Get) => self.read(iid, |_, mr| {
                match mr.versions.iter().find(|version| version.id == v) {
                    Some(version) => {
                        let mut value = version_json(version);
                        value["diffs"] =
                            json!(version.diffs.iter().map(diff_json).collect::<Vec<_>>());
                        ok(value)
                    }
                    None => message(404, "404 Not found"),
                }
            }),
            (Route::Diffs(_), Method::Get) => self.read(iid, |_, mr| {
                let diffs: Vec<Value> = mr.latest().diffs.iter().map(diff_json).collect();
                paged(request, &diffs)
            }),
            (Route::Reviewers(_), Method::Get) => self.read(iid, |fake, mr| {
                let reviewers: Vec<Value> = mr
                    .reviewers
                    .iter()
                    .map(|(name, state)| {
                        json!({
                            "user": fake.user_ref(name),
                            "state": state.rest(),
                            "created_at": "2026-01-01T00:00:00.000Z",
                        })
                    })
                    .collect();
                ok(Value::Array(reviewers))
            }),
            (Route::Drafts(_), Method::Get) => self.read(iid, |fake, mr| {
                let drafts: Vec<Value> = mr
                    .drafts
                    .iter()
                    .filter(|draft| draft.author == user.username)
                    .map(|draft| fake.draft_json(mr, draft))
                    .collect();
                paged(request, &drafts)
            }),
            (Route::Drafts(_), Method::Post) => self.create_draft(&body, &user, iid),
            (Route::Draft(_, d), Method::Get) => {
                self.read(iid, |fake, mr| match own_draft(mr, d, &user) {
                    Some(draft) => ok(fake.draft_json(mr, draft)),
                    None => message(404, "404 Not found"),
                })
            }
            (Route::Draft(_, d), Method::Put) => self.update_draft(&body, &user, iid, d),
            (Route::Draft(_, d), Method::Delete) => {
                let Some(mr) = self.mrs.get_mut(&iid) else {
                    return message(404, "404 Not found");
                };
                if own_draft(mr, d, &user).is_none() {
                    return message(404, "404 Not found");
                }
                mr.drafts.retain(|draft| draft.id != d);
                Response::new(204)
            }
            (Route::Publish(_, d), Method::Put) => {
                let draft = self
                    .mrs
                    .get(&iid)
                    .and_then(|mr| own_draft(mr, d, &user).cloned());
                let Some(draft) = draft else {
                    return message(404, "404 Not found");
                };
                match self.publish_draft(iid, draft) {
                    Ok(_) => Response::new(204),
                    Err(err) => message(500, &err),
                }
            }
            (Route::BulkPublish(_), Method::Post) => self.bulk_publish(&body, &user, iid),
            (Route::Notes(_), Method::Get) => self.read(iid, |fake, mr| {
                let mut notes: Vec<Value> = mr
                    .notes
                    .iter()
                    .map(|note| fake.note_json(mr, note))
                    .collect();
                if request.query_param("sort") != Some("asc") {
                    notes.reverse();
                }
                paged(request, &notes)
            }),
            (Route::Notes(_), Method::Post) => self.create_thread(&body, &user, iid, false),
            (Route::Discussions(_), Method::Get) => self.read(iid, |fake, mr| {
                let discussions: Vec<Value> = mr
                    .discussions
                    .iter()
                    .map(|discussion| fake.discussion_json(mr, discussion))
                    .collect();
                paged(request, &discussions)
            }),
            (Route::Discussions(_), Method::Post) => self.create_thread(&body, &user, iid, true),
            (Route::Discussion(_), Method::Get) => {
                self.read(iid, |fake, mr| match mr.discussion(&discussion) {
                    Some(found) => ok(fake.discussion_json(mr, found)),
                    None => message(404, "404 Not found"),
                })
            }
            (Route::Discussion(_), Method::Put) => self.resolve(&body, &user, iid, &discussion),
            (Route::DiscussionNotes(_), Method::Post) => self.reply(&body, &user, iid, &discussion),
            (Route::Approve(_), Method::Post) => self.approve(&body, request, &user, iid),
            (Route::Unapprove(_), Method::Post) => self.unapprove(&user, iid),
            (Route::Approvals(_), Method::Get) => {
                self.read(iid, |fake, mr| ok(fake.approval_json(mr, &user)))
            }
            _ => message(405, "405 Method Not Allowed"),
        }
    }

    fn read(&self, iid: u64, f: impl FnOnce(&GitlabFake, &Mr) -> Response) -> Response {
        match self.mrs.get(&iid) {
            Some(mr) => f(self, mr),
            None => message(404, "404 Not found"),
        }
    }

    fn create_draft(&mut self, body: &Map<String, Value>, user: &User, iid: u64) -> Response {
        let Some(note) = body.get("note").and_then(Value::as_str) else {
            return error(400, "note is missing");
        };
        let note = normalize(note);
        if note.is_empty() {
            return message(400, "400 Bad request - Note can't be blank");
        }
        let Some(mr) = self.mrs.get(&iid) else {
            return message(404, "404 Not found");
        };
        let position = match body.get("position") {
            None | Some(Value::Null) => None,
            Some(value) => match mr.position(value) {
                Ok(position) => Some(position),
                Err(err) => return message(400, &format!("400 Bad request - {err}")),
            },
        };
        let discussion_id = match body.get("in_reply_to_discussion_id") {
            None | Some(Value::Null) => None,
            Some(Value::String(id)) if mr.discussion(id).is_some() => Some(id.clone()),
            Some(_) => return message(404, "404 Discussion Not Found"),
        };
        if discussion_id.is_some() && position.is_some() {
            return message(
                400,
                "400 Bad request - forge-fake: reply with position is not modeled",
            );
        }
        let resolve_discussion = body
            .get("resolve_discussion")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let id = self.id();
        let draft = DraftNote {
            id,
            author: user.username.clone(),
            note,
            position,
            discussion_id,
            resolve_discussion,
            origin: Origin::Api,
        };
        let Some(mr) = self.mrs.get_mut(&iid) else {
            return message(404, "404 Not found");
        };
        mr.drafts.push(draft.clone());
        self.read(iid, |fake, mr| created(fake.draft_json(mr, &draft)))
    }

    fn update_draft(
        &mut self,
        body: &Map<String, Value>,
        user: &User,
        iid: u64,
        id: u64,
    ) -> Response {
        let Some(mr) = self.mrs.get(&iid) else {
            return message(404, "404 Not found");
        };
        if own_draft(mr, id, user).is_none() {
            return message(404, "404 Not found");
        }
        let position = match body.get("position") {
            None | Some(Value::Null) => None,
            Some(value) => match mr.position(value) {
                Ok(position) => Some(position),
                Err(err) => return message(400, &format!("400 Bad request - {err}")),
            },
        };
        let note = match body.get("note") {
            None | Some(Value::Null) => None,
            Some(Value::String(note)) => match normalize(note) {
                note if note.is_empty() => {
                    return message(400, "400 Bad request - Note can't be blank");
                }
                note => Some(note),
            },
            Some(_) => return error(400, "note is invalid"),
        };
        let Some(mr) = self.mrs.get_mut(&iid) else {
            return message(404, "404 Not found");
        };
        let Some(draft) = mr.drafts.iter_mut().find(|draft| draft.id == id) else {
            return message(404, "404 Not found");
        };
        if let Some(note) = note {
            draft.note = note;
        }
        if position.is_some() {
            draft.position = position;
        }
        let draft = draft.clone();
        self.read(iid, |fake, mr| ok(fake.draft_json(mr, &draft)))
    }

    fn bulk_publish(&mut self, body: &Map<String, Value>, user: &User, iid: u64) -> Response {
        let reviewer_state = match body.get("reviewer_state").and_then(Value::as_str) {
            None => None,
            Some("requested_changes") => Some(ReviewerState::RequestedChanges),
            Some("reviewed") => Some(ReviewerState::Reviewed),
            Some(_) => return error(400, "reviewer_state does not have a valid value"),
        };
        let drafts: Vec<DraftNote> = self
            .mrs
            .get(&iid)
            .map(|mr| {
                mr.drafts
                    .iter()
                    .filter(|draft| draft.author == user.username)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        for draft in drafts {
            if let Err(err) = self.publish_draft(iid, draft) {
                return message(500, &err);
            }
        }
        if let Some(note) = body.get("note").and_then(Value::as_str) {
            let note = strip_quick_actions(note);
            if !note.is_empty() {
                let created = self.new_discussion(iid, true).and_then(|discussion| {
                    self.insert_note(
                        iid,
                        &discussion,
                        &user.username,
                        &note,
                        None,
                        Origin::Api,
                        None,
                    )
                });
                if let Err(err) = created {
                    return message(500, &err);
                }
            }
        }
        if let (Some(state), Some(mr)) = (reviewer_state, self.mrs.get_mut(&iid)) {
            mr.set_reviewer(&user.username, state);
        }
        Response::new(204)
    }

    /// `POST notes` (`thread == false`, отдельная заметка) или `POST discussions` (тред, позиция
    /// допустима).
    fn create_thread(
        &mut self,
        body: &Map<String, Value>,
        user: &User,
        iid: u64,
        thread: bool,
    ) -> Response {
        let Some(text) = body.get("body").and_then(Value::as_str) else {
            return error(400, "body is missing");
        };
        let text = strip_quick_actions(text);
        if text.is_empty() {
            return message(400, "400 Bad request - Note can't be blank");
        }
        let Some(mr) = self.mrs.get(&iid) else {
            return message(404, "404 Not found");
        };
        let position = match body.get("position") {
            Some(value) if thread && !value.is_null() => match mr.position(value) {
                Ok(position) => Some(position),
                Err(err) => return message(400, &format!("400 Bad request - {err}")),
            },
            _ => None,
        };
        let individual = !thread;
        let result = self.new_discussion(iid, individual).and_then(|discussion| {
            self.insert_note(
                iid,
                &discussion,
                &user.username,
                &text,
                position,
                Origin::Api,
                None,
            )
            .map(|id| (discussion, id))
        });
        let (discussion, id) = match result {
            Ok(created) => created,
            Err(err) => return message(500, &err),
        };
        self.read(iid, |fake, mr| {
            if thread {
                mr.discussion(&discussion).map_or_else(
                    || message(500, "lost"),
                    |d| created(fake.discussion_json(mr, d)),
                )
            } else {
                mr.notes.iter().find(|note| note.id == id).map_or_else(
                    || message(500, "lost"),
                    |note| created(fake.note_json(mr, note)),
                )
            }
        })
    }

    fn reply(&mut self, body: &Map<String, Value>, user: &User, iid: u64, id: &str) -> Response {
        let Some(text) = body.get("body").and_then(Value::as_str) else {
            return error(400, "body is missing");
        };
        let text = strip_quick_actions(text);
        if text.is_empty() {
            return message(400, "400 Bad request - Note can't be blank");
        }
        match self.mrs.get(&iid).and_then(|mr| mr.discussion(id)) {
            Some(discussion) if !discussion.individual_note => {}
            Some(_) => {
                return message(
                    400,
                    "400 Bad request - forge-fake: replies to individual notes are not modeled",
                );
            }
            None => return message(404, "404 Discussion Not Found"),
        }
        let note = match self.insert_note(iid, id, &user.username, &text, None, Origin::Api, None) {
            Ok(note) => note,
            Err(err) => return message(500, &err),
        };
        self.read(iid, |fake, mr| {
            mr.notes
                .iter()
                .find(|n| n.id == note)
                .map_or_else(|| message(500, "lost"), |n| created(fake.note_json(mr, n)))
        })
    }

    fn resolve(&mut self, body: &Map<String, Value>, user: &User, iid: u64, id: &str) -> Response {
        let Some(resolved) = body.get("resolved").and_then(Value::as_bool) else {
            return error(400, "resolved is missing");
        };
        let Some(mr) = self.mrs.get_mut(&iid) else {
            return message(404, "404 Not found");
        };
        let author = mr
            .notes
            .iter()
            .find(|note| note.discussion_id == id)
            .map(|note| note.author.clone());
        let allowed =
            user.access >= Some(Access::Developer) || author.as_deref() == Some(&user.username);
        let Some(discussion) = mr.discussions.iter_mut().find(|d| d.id == id) else {
            return message(404, "404 Discussion Not Found");
        };
        if !discussion.resolvable {
            return message(400, "400 Bad request - Discussion is not resolvable");
        }
        if !allowed {
            return message(403, "403 Forbidden");
        }
        discussion.resolved = resolved;
        let discussion = discussion.clone();
        self.read(iid, |fake, mr| ok(fake.discussion_json(mr, &discussion)))
    }

    fn approve(
        &mut self,
        body: &Map<String, Value>,
        request: &Incoming,
        user: &User,
        iid: u64,
    ) -> Response {
        let Some(mr) = self.mrs.get(&iid) else {
            return message(404, "404 Not found");
        };
        let sha = body
            .get("sha")
            .and_then(Value::as_str)
            .or_else(|| request.query_param("sha"));
        let head = mr.latest().head_sha.clone();
        if let Some(sha) = sha
            && (sha != head || mr.target_moved)
        {
            return message(
                409,
                &format!("SHA does not match HEAD of source branch: {head}"),
            );
        }
        let eligible = user.access >= Some(Access::Developer)
            && mr.author != user.username
            && !mr
                .approved_by
                .iter()
                .any(|(name, _)| name == &user.username);
        if !eligible {
            return message(401, "401 Unauthorized");
        }
        let at = self.now();
        let Some(mr) = self.mrs.get_mut(&iid) else {
            return message(404, "404 Not found");
        };
        mr.approved_by.push((user.username.clone(), at));
        mr.set_reviewer(&user.username, ReviewerState::Approved);
        self.read(iid, |fake, mr| created(fake.approval_json(mr, user)))
    }

    fn unapprove(&mut self, user: &User, iid: u64) -> Response {
        let Some(mr) = self.mrs.get_mut(&iid) else {
            return message(404, "404 Not found");
        };
        if !mr
            .approved_by
            .iter()
            .any(|(name, _)| name == &user.username)
        {
            return message(404, "404 Not found");
        }
        mr.approved_by.retain(|(name, _)| name != &user.username);
        mr.set_reviewer(&user.username, ReviewerState::Unapproved);
        self.read(iid, |fake, mr| created(fake.approval_json(mr, user)))
    }

    fn graphql(&mut self, request: &Incoming, user: &User) -> Response {
        let body = match request.json() {
            Ok(body) => body,
            Err(_) => return message(400, "400 Bad request"),
        };
        let query = body.get("query").and_then(Value::as_str).unwrap_or("");
        let variables = body.get("variables").cloned().unwrap_or(Value::Null);
        let Some((kind, field)) = graphql_root(query) else {
            return gql_errors(None, json!([{"message": "Unexpected end of document"}]));
        };
        match (kind, field.as_str()) {
            (GraphqlKind::Query, "__type") => {
                let name = variables
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| inline_name(query));
                gql_data(json!({"__type": self.introspect(name.as_deref())}))
            }
            (GraphqlKind::Query, "currentUser") => gql_data(
                json!({"currentUser": {"id": format!("gid://gitlab/User/{}", user.id), "username": user.username}}),
            ),
            (GraphqlKind::Query, "project") => {
                let path = variables
                    .get("fullPath")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let iid = variables.get("iid").and_then(Value::as_str).unwrap_or("");
                gql_data(json!({"project": self.project_gql(user, path, iid)}))
            }
            (GraphqlKind::Mutation, "mergeRequestRequestChanges")
                if self.request_changes != RequestChangesMode::Absent =>
            {
                self.request_changes_gql(&variables, user)
            }
            (kind, other) => {
                let type_name = match kind {
                    GraphqlKind::Query => "Query",
                    GraphqlKind::Mutation => "Mutation",
                };
                gql_errors(
                    None,
                    json!([{
                        "message": format!("Field '{other}' doesn't exist on type '{type_name}'"),
                        "locations": [{"line": 1, "column": 1}],
                        "path": [kind_word(kind), other],
                        "extensions": {"code": "undefinedField", "typeName": type_name, "fieldName": other},
                    }]),
                )
            }
        }
    }

    fn introspect(&self, name: Option<&str>) -> Value {
        if name != Some("Mutation") {
            return Value::Null;
        }
        let mut fields = vec!["createNote", "mergeRequestAccept", "mergeRequestSetDraft"];
        if self.request_changes != RequestChangesMode::Absent {
            fields.push("mergeRequestRequestChanges");
        }
        fields.sort_unstable();
        json!({
            "name": "Mutation",
            "fields": fields.iter().map(|name| json!({"name": name})).collect::<Vec<_>>(),
        })
    }

    fn project_gql(&self, user: &User, path: &str, iid: &str) -> Value {
        if user.access.is_none() || !path.eq_ignore_ascii_case(&self.path) {
            return Value::Null;
        }
        let mr = iid
            .parse::<u64>()
            .ok()
            .and_then(|iid| self.mrs.get(&iid))
            .map(|mr| self.mr_gql(mr));
        json!({"fullPath": self.path, "mergeRequest": mr})
    }

    fn mr_gql(&self, mr: &Mr) -> Value {
        let reviewers: Vec<Value> = mr
            .reviewers
            .iter()
            .map(|(name, state)| {
                json!({
                    "username": name,
                    "mergeRequestInteraction": {"reviewState": state.graphql()},
                })
            })
            .collect();
        json!({
            "iid": mr.iid.to_string(),
            "diffHeadSha": mr.latest().head_sha,
            "approved": !mr.approved_by.is_empty(),
            "approvedBy": {"nodes": mr.approved_by.iter().map(|(name, _)| json!({"username": name})).collect::<Vec<_>>()},
            "reviewers": {"nodes": reviewers},
        })
    }

    fn request_changes_gql(&mut self, variables: &Value, user: &User) -> Response {
        let input = variables.get("input").unwrap_or(&Value::Null);
        let path = input
            .get("projectPath")
            .and_then(Value::as_str)
            .unwrap_or("");
        let iid = input
            .get("iid")
            .and_then(Value::as_str)
            .and_then(|iid| iid.parse::<u64>().ok());
        let not_available = || {
            gql_errors(
                Some(json!({"mergeRequestRequestChanges": null})),
                json!([{
                    "message": NOT_AVAILABLE,
                    "locations": [{"line": 1, "column": 1}],
                    "path": ["mergeRequestRequestChanges"],
                }]),
            )
        };
        let found =
            iid.filter(|iid| path.eq_ignore_ascii_case(&self.path) && self.mrs.contains_key(iid));
        let Some(iid) = found else {
            return not_available();
        };
        if self.request_changes == RequestChangesMode::Forbidden
            || user.access < Some(Access::Developer)
        {
            return not_available();
        }
        if self.request_changes == RequestChangesMode::ExecutionError {
            return gql_errors(
                Some(json!({"mergeRequestRequestChanges": null})),
                json!([{
                    "message": "forge-fake: execution error",
                    "locations": [{"line": 1, "column": 1}],
                    "path": ["mergeRequestRequestChanges"],
                }]),
            );
        }
        let payload_error = |message: &str| {
            gql_data(json!({"mergeRequestRequestChanges": {
                "clientMutationId": null,
                "errors": [message],
                "mergeRequest": null,
            }}))
        };
        if self.request_changes == RequestChangesMode::PayloadError {
            return payload_error("forge-fake: payload error");
        }
        let Some(mr) = self.mrs.get_mut(&iid) else {
            return not_available();
        };
        if !mr.set_reviewer(&user.username, ReviewerState::RequestedChanges) {
            return payload_error("Reviewer not found");
        }
        mr.approved_by.retain(|(name, _)| name != &user.username);
        let value = self.mrs.get(&iid).map(|mr| self.mr_gql(mr));
        gql_data(json!({"mergeRequestRequestChanges": {
            "clientMutationId": null,
            "errors": [],
            "mergeRequest": value,
        }}))
    }

    fn user_ref(&self, username: &str) -> Value {
        match self.users.iter().find(|user| user.username == username) {
            Some(user) => user_json(user),
            None => json!({"id": 0, "username": username, "name": username, "state": "active"}),
        }
    }

    fn user_id(&self, username: &str) -> u64 {
        self.users
            .iter()
            .find(|user| user.username == username)
            .map_or(0, |user| user.id)
    }

    fn project_json(&self, user: &User) -> Value {
        let name = self.path.rsplit('/').next().unwrap_or(&self.path);
        json!({
            "id": self.project_id,
            "name": name,
            "path": name,
            "path_with_namespace": self.path,
            "visibility": self.visibility,
            "default_branch": self.default_branch,
            "permissions": {
                "project_access": user.access.map(|access| json!({"access_level": access as u8})),
                "group_access": null,
            },
        })
    }

    fn mr_json(&self, mr: &Mr) -> Value {
        let latest = mr.latest();
        json!({
            "id": mr.id,
            "iid": mr.iid,
            "project_id": self.project_id,
            "state": "opened",
            "draft": false,
            "author": self.user_ref(&mr.author),
            "reviewers": mr.reviewers.iter().map(|(name, _)| self.user_ref(name)).collect::<Vec<_>>(),
            "source_branch": format!("probe/{}", mr.iid),
            "target_branch": self.default_branch,
            "sha": latest.head_sha,
            "diff_refs": {
                "base_sha": latest.base_sha,
                "head_sha": latest.head_sha,
                "start_sha": latest.start_sha,
            },
        })
    }

    fn draft_json(&self, mr: &Mr, draft: &DraftNote) -> Value {
        let position = draft.position.as_ref().map_or_else(
            || {
                json!({
                    "base_sha": null, "start_sha": null, "head_sha": null,
                    "old_path": null, "new_path": null, "position_type": "text",
                    "old_line": null, "new_line": null, "line_range": null,
                })
            },
            Position::json,
        );
        json!({
            "id": draft.id,
            "author_id": self.user_id(&draft.author),
            "merge_request_id": mr.id,
            "resolve_discussion": draft.resolve_discussion,
            "discussion_id": draft.discussion_id,
            "note": draft.note,
            "commit_id": null,
            "line_code": draft.position.as_ref().map(|p| p.line_code.clone()),
            "position": position,
        })
    }

    fn note_json(&self, mr: &Mr, note: &Note) -> Value {
        let discussion = mr.discussion(&note.discussion_id);
        let individual = discussion.is_none_or(|d| d.individual_note);
        let kind = match (&note.position, individual) {
            (Some(_), _) => json!("DiffNote"),
            (None, false) => json!("DiscussionNote"),
            (None, true) => Value::Null,
        };
        let mut value = json!({
            "id": note.id,
            "type": kind,
            "body": note.body,
            "author": self.user_ref(&note.author),
            "created_at": note.created_at,
            "updated_at": note.created_at,
            "system": false,
            "noteable_id": mr.id,
            "noteable_type": "MergeRequest",
            "noteable_iid": mr.iid,
            "project_id": self.project_id,
            "commit_id": null,
            "resolvable": discussion.is_some_and(|d| d.resolvable),
        });
        if let Some(discussion) = discussion.filter(|d| d.resolvable) {
            value["resolved"] = json!(discussion.resolved);
            value["resolved_by"] = Value::Null;
        }
        if let Some(position) = &note.position {
            value["position"] = position.json();
        }
        value
    }

    fn discussion_json(&self, mr: &Mr, discussion: &Discussion) -> Value {
        let notes: Vec<Value> = mr
            .notes
            .iter()
            .filter(|note| note.discussion_id == discussion.id)
            .map(|note| self.note_json(mr, note))
            .collect();
        json!({
            "id": discussion.id,
            "individual_note": discussion.individual_note,
            "notes": notes,
        })
    }

    fn approval_json(&self, mr: &Mr, user: &User) -> Value {
        let approved_by: Vec<Value> = mr
            .approved_by
            .iter()
            .map(|(name, at)| json!({"user": self.user_ref(name), "approved_at": at}))
            .collect();
        let has_approved = mr
            .approved_by
            .iter()
            .any(|(name, _)| name == &user.username);
        json!({
            "id": mr.id,
            "iid": mr.iid,
            "project_id": self.project_id,
            "state": "opened",
            "approved": !mr.approved_by.is_empty(),
            "approvals_required": 0,
            "approvals_left": 0,
            "approved_by": approved_by,
            "user_has_approved": has_approved,
            "user_can_approve": !has_approved && user.access >= Some(Access::Developer) && mr.author != user.username,
        })
    }
}

impl Forge for GitlabFake {
    fn key(&self, request: &Incoming) -> String {
        match self.route(&request.path) {
            Some((Route::Graphql, _)) => {
                let field = request
                    .json()
                    .ok()
                    .and_then(|body| body.get("query").and_then(Value::as_str).map(str::to_owned))
                    .and_then(|query| graphql_root(&query))
                    .map_or_else(|| "?".to_owned(), |(_, field)| field);
                format!("graphql {field}")
            }
            Some((route, _)) => format!("{} {}", request.method, route.template()),
            None => format!("unrouted {}", request.method),
        }
    }

    fn user(&self, request: &Incoming) -> Option<String> {
        self.caller(request).map(|user| user.username.clone())
    }

    fn handle(&mut self, request: &Incoming) -> Response {
        let response = self.dispatch(request);
        self.rate_headers(response)
    }

    fn rate_limited(&self, _request: &Incoming) -> Response {
        Response::new(429)
            .with_header("content-type", "text/plain")
            .with_header("ratelimit-limit", RATE_LIMIT.to_string())
            .with_header("ratelimit-observed", RATE_LIMIT.to_string())
            .with_header("ratelimit-remaining", "0")
            .with_header("ratelimit-reset", "1767229200")
            .with_header("ratelimit-name", "throttle_authenticated_api")
            .with_header("retry-after", "60")
            .with_body("Retry later\n")
    }
}

fn own_draft<'a>(mr: &'a Mr, id: u64, user: &User) -> Option<&'a DraftNote> {
    mr.drafts
        .iter()
        .find(|draft| draft.id == id && draft.author == user.username)
}

/// Имя типа из `__type(name: "…")` в тексте запроса.
fn inline_name(query: &str) -> Option<String> {
    let after = &query[query.find("__type")?..];
    let after = &after[after.find("name")? + 4..];
    let after = after
        .trim_start()
        .strip_prefix(':')?
        .trim_start()
        .strip_prefix('"')?;
    Some(after[..after.find('"')?].to_owned())
}

fn kind_word(kind: GraphqlKind) -> &'static str {
    match kind {
        GraphqlKind::Query => "query",
        GraphqlKind::Mutation => "mutation",
    }
}

fn user_json(user: &User) -> Value {
    json!({"id": user.id, "username": user.username, "name": user.name, "state": "active"})
}

fn version_json(version: &Version) -> Value {
    json!({
        "id": version.id,
        "head_commit_sha": version.head_sha,
        "base_commit_sha": version.base_sha,
        "start_commit_sha": version.start_sha,
        "created_at": version.created_at,
        "state": "collected",
        "real_size": version.diffs.len().to_string(),
    })
}

fn diff_json(diff: &FakeDiff) -> Value {
    json!({
        "old_path": diff.old_path,
        "new_path": diff.new_path,
        "a_mode": "100644",
        "b_mode": "100644",
        "diff": diff.diff,
        "new_file": diff.new_file,
        "renamed_file": diff.old_path != diff.new_path,
        "deleted_file": diff.deleted_file,
        "too_large": false,
        "collapsed": false,
    })
}

fn ok(value: Value) -> Response {
    Response::json(200, &value)
}

fn created(value: Value) -> Response {
    Response::json(201, &value)
}

fn message(status: u16, text: &str) -> Response {
    Response::json(status, &json!({"message": text}))
}

/// Ошибка проверки параметров (Grape): `{"error": "…"}`.
fn error(status: u16, text: &str) -> Response {
    Response::json(status, &json!({"error": text}))
}

fn gql_data(data: Value) -> Response {
    Response::json(200, &json!({"data": data}))
}

fn gql_errors(data: Option<Value>, errors: Value) -> Response {
    let mut body = Map::new();
    if let Some(data) = data {
        body.insert("data".to_owned(), data);
    }
    body.insert("errors".to_owned(), errors);
    Response::json(200, &Value::Object(body))
}

/// Страница списка: `per_page` по умолчанию 20, не больше 100, заголовки `x-*` и `link`.
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
    let total_pages = items.len().div_ceil(per_page).max(1);
    let chunk: Vec<Value> = items
        .iter()
        .skip((page - 1) * per_page)
        .take(per_page)
        .cloned()
        .collect();
    let keep: Vec<String> = request
        .query
        .iter()
        .filter(|(key, _)| key != "page" && key != "per_page")
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    let link = |target: usize, rel: &str| {
        let mut query = keep.clone();
        query.push(format!("page={target}"));
        query.push(format!("per_page={per_page}"));
        format!(
            "<{}{}?{}>; rel=\"{rel}\"",
            request.base,
            request.path,
            query.join("&")
        )
    };
    let mut links = Vec::new();
    let next = (page < total_pages).then(|| page + 1);
    let prev = (page > 1).then(|| page - 1);
    if let Some(prev) = prev {
        links.push(link(prev, "prev"));
    }
    if let Some(next) = next {
        links.push(link(next, "next"));
    }
    links.push(link(1, "first"));
    links.push(link(total_pages, "last"));
    Response::json(200, &Value::Array(chunk))
        .with_header("x-page", page.to_string())
        .with_header("x-per-page", per_page.to_string())
        .with_header("x-total", items.len().to_string())
        .with_header("x-total-pages", total_pages.to_string())
        .with_header(
            "x-next-page",
            next.map_or_else(String::new, |n| n.to_string()),
        )
        .with_header(
            "x-prev-page",
            prev.map_or_else(String::new, |n| n.to_string()),
        )
        .with_header("link", links.join(", "))
}

#[cfg(test)]
mod tests;
