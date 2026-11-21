//! Сценарии столбца `fake`: строка матрицы, стоимость (`volume`) и устойчивость (`rate-limit`)
//! против `forge-fake` в процессе сценария.
//!
//! Ожидание проверяется независимо от клиента: по журналу вызовов сервера (ключ, тело, код) и по
//! его состоянию, включая видимые только оракулу поля, против литералов сценария. Журнал пробы
//! лежит в `SHELLIO_STATE_DIR` процесса и проверяется после переоткрытия. Ветви одной строки идут
//! каждая со своим сервером и своим файлом журнала. Ожидания записаны в отчёте (раздел
//! «Ожидания») до первого прогона.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt::Debug;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use super::Scenario;
use crate::fake::github::{self as ghf, CommentSpec, DiffSide, FakeCommit, FakeFile, GithubFake};
use crate::fake::gitlab::{self as glf, Access, FakeDiff, GitlabFake, RequestChangesMode};
use crate::fake::{FakeServer, Fault, Forge, GraphqlKind, Logged, Reply, graphql_root};
use crate::github::{self, GitHub, ReviewEvent};
use crate::gitlab::{self, GitLab, KnownDraft};
use crate::journal::{ItemState, Journal, Operation, ProjectId, Scope, SessionId, SubmissionId};
use crate::model::anchor::{Anchor, LinePoint, Side};
use crate::model::diff::FileDiff;
use crate::model::revision::{
    AnchorFate, HeadCheck, HeadPath, Revision, Sha, StaleReason, TripleOrigin, triple_origin,
};
use crate::model::{DraftComment, DraftId, ModelError, Placement, Provider, ReviewSession};
use crate::model::{ProviderId, ReviewTarget};
use crate::results::{Execution, Host, ResultFile, Status};
use crate::scrub::{LABEL_AUTHOR, LABEL_REVIEWER, Scrubber};
use crate::submit::reconcile::Verdict;
use crate::submit::{
    Delivery, GitLabPlan, SubmitError, plan_github_local, plan_github_pending, plan_gitlab,
    reconcile_github, reconcile_gitlab, report, run_github, run_gitlab,
};
use crate::transport::http::{Credential, HttpConfig, HttpTransport, Secret};
use crate::transport::{Failure, Request, Response, Trace, Traced, Transport};

type Outcome = Result<(), Box<dyn Error>>;
type Client = Traced<HttpTransport>;

const PROJECT: &str = "fake-project";
const REVIEWER: &str = "reviewer-bob";
const REVIEWER2: &str = "reviewer-carol";
const REPORTER: &str = "reporter-dan";
const AUTHOR: &str = "author-alice";
const TOKEN_REVIEWER: &str = "fake-token-reviewer";
const TOKEN_REVIEWER2: &str = "fake-token-carol";
const TOKEN_REPORTER: &str = "fake-token-reporter";
const TOKEN_AUTHOR: &str = "fake-token-author";
const ACTOR_REVIEWER: &str = "reviewer";
const ACTOR_REVIEWER2: &str = "reviewer2";
const ACTOR_REPORTER: &str = "reporter";
const ACTOR_AUTHOR: &str = "author";
/// Второй клиент того же пользователя (браузер или другой API-клиент) с тем же токеном.
const ACTOR_SECOND: &str = "second-client";

const GH_OWNER: &str = "probe-owner";
const GH_REPO: &str = "sandbox-repo";
const GH_NUMBER: u64 = 7;
const GL_PROJECT: &str = "probe-group/sandbox-project";
const GL_IID: u64 = 3;

const BASE: &str = "1111111111111111111111111111111111111111";
const HEAD: &str = "2222222222222222222222222222222222222222";
const HEAD2: &str = "3333333333333333333333333333333333333333";
const BASE2: &str = "4444444444444444444444444444444444444444";
const PATH: &str = "src/review.rs";
/// Старая сторона: 1 `ctx1`, 2 `old2` (удалена), 3 `ctx3`, 4 `ctx4`. Новая: 1 `ctx1`, 2 `new2`
/// (добавлена), 3 `ctx3`, 4 `add4` (добавлена), 5 `ctx4`.
const DIFF: &str = "@@ -1,4 +1,5 @@\n ctx1\n-old2\n+new2\n ctx3\n+add4\n ctx4\n";
/// Diff v2 для `head-change`: `ctx1` на месте, тексты строк 2 и 4 новой стороны другие.
const DIFF2: &str = "@@ -1,4 +1,5 @@\n ctx1\n-old2\n+NEW2\n ctx3\n+ADD4\n ctx4\n";
const SUMMARY: &str = "Итог ревью";
const RECEIPT: Duration = Duration::from_secs(10);

const GH_CREATE: &str = "POST /repos/{owner}/{repo}/pulls/{number}/reviews";
const GH_SUBMIT: &str = "POST /repos/{owner}/{repo}/pulls/{number}/reviews/{review_id}/events";
const GH_REPLIES: &str = "POST /repos/{owner}/{repo}/pulls/{number}/comments/{comment_id}/replies";
const GH_ADD_THREAD: &str = "graphql addPullRequestReviewThread";
const GH_UPDATE_COMMENT: &str = "graphql updatePullRequestReviewComment";
const GH_RESOLVE: &str = "graphql resolveReviewThread";
const GH_MARK: &str = "graphql markFileAsViewed";
const GH_UNMARK: &str = "graphql unmarkFileAsViewed";
const GL_DRAFT_DELETE: &str =
    "DELETE /projects/{id}/merge_requests/{merge_request_iid}/draft_notes/{draft_note_id}";

/// Проверки сценария: описание, итог и наблюдённое значение.
#[derive(Debug, Default)]
pub struct Checks {
    items: Vec<(String, bool, String)>,
}

impl Checks {
    pub fn check(&mut self, what: &str, ok: bool, observed: impl Into<String>) {
        self.items.push((what.to_owned(), ok, observed.into()));
    }

    pub fn eq<T: PartialEq + Debug>(&mut self, what: &str, actual: T, expected: T) {
        let ok = actual == expected;
        let observed = if ok {
            format!("{actual:?}")
        } else {
            format!("{actual:?}, ожидалось {expected:?}")
        };
        self.check(what, ok, observed);
    }

    /// Записи по ключам — ровно `expected`, других записей нет.
    pub fn writes(&mut self, what: &str, log: &[Logged], expected: &[(&str, usize)]) {
        let expected: BTreeMap<String, usize> = expected
            .iter()
            .filter(|(_, count)| *count > 0)
            .map(|(key, count)| ((*key).to_owned(), *count))
            .collect();
        self.eq(what, writes(log), expected);
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Итог: `pass`, если проверки есть и все прошли; иначе `fail` со списком непрошедших.
    pub fn verdict(&self) -> (Status, String) {
        if self.items.is_empty() {
            return (
                Status::Fail,
                "сценарий не выполнил ни одной проверки".to_owned(),
            );
        }
        let failed: Vec<String> = self
            .items
            .iter()
            .filter(|(_, ok, _)| !ok)
            .map(|(what, _, observed)| format!("{what}: {observed}"))
            .collect();
        if failed.is_empty() {
            (
                Status::Pass,
                format!("все {} проверок ожидания прошли", self.items.len()),
            )
        } else {
            (
                Status::Fail,
                format!(
                    "не прошли {} из {} проверок: {}",
                    failed.len(),
                    self.items.len(),
                    failed.join("; ")
                ),
            )
        }
    }

    pub fn to_json(&self) -> Vec<String> {
        self.items
            .iter()
            .map(|(what, ok, observed)| {
                format!("{} {what}: {observed}", if *ok { "ok" } else { "НЕТ" })
            })
            .collect()
    }
}

/// Записи в журнале сервера по ключу: REST-методы записи и мутации GraphQL.
pub fn writes(log: &[Logged]) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for call in log {
        let write = if call.path.ends_with("/graphql") {
            call.body
                .get("query")
                .and_then(Value::as_str)
                .and_then(graphql_root)
                .is_some_and(|(kind, _)| kind == GraphqlKind::Mutation)
        } else {
            call.method.is_write()
        };
        if write {
            *counts.entry(call.key.clone()).or_insert(0) += 1;
        }
    }
    counts
}

/// Отметка квитанции: длина следа клиентов и журнала сервера ветви перед ожидаемыми вызовами.
#[derive(Clone, Copy, Debug)]
struct Mark {
    trace: usize,
    logged: usize,
}

/// Стенд процесса: след всех ролей, очиститель, журналы ветвей, снимки серверов, проверки.
struct Stand {
    trace: Trace,
    scrubber: Scrubber,
    dir: PathBuf,
    checks: Checks,
    before: Map<String, Value>,
    after: Map<String, Value>,
    journals: Map<String, Value>,
    notes: Vec<String>,
    host: Host,
}

impl Stand {
    fn new(dir: &Path) -> Stand {
        let mut scrubber = Scrubber::new();
        for token in [
            TOKEN_REVIEWER,
            TOKEN_REVIEWER2,
            TOKEN_REPORTER,
            TOKEN_AUTHOR,
        ] {
            scrubber.add_token(token);
        }
        scrubber
            .add_login(REVIEWER, LABEL_REVIEWER)
            .add_login(REVIEWER2, "<reviewer2>")
            .add_login(REPORTER, "<reporter>")
            .add_login(AUTHOR, LABEL_AUTHOR)
            .add_repo(&format!("{GH_OWNER}/{GH_REPO}"))
            .add_repo(GL_PROJECT);
        Stand {
            trace: Trace::new(),
            scrubber,
            dir: dir.to_path_buf(),
            checks: Checks::default(),
            before: Map::new(),
            after: Map::new(),
            journals: Map::new(),
            notes: Vec::new(),
            host: Host::default(),
        }
    }

    fn journal_path(&self, branch: &str) -> PathBuf {
        self.dir.join(format!("{branch}.db"))
    }

    fn client(&self, token: &str, actor: &'static str, private: bool) -> Client {
        let credential = if private {
            Credential::private_token(Secret::new(token))
        } else {
            Credential::bearer(Secret::new(token))
        };
        Traced::new(
            HttpTransport::new(HttpConfig::default(), Some(credential)),
            self.trace.clone(),
            actor,
        )
    }

    fn gh(
        &self,
        server: &FakeServer<GithubFake>,
        token: &str,
        actor: &'static str,
    ) -> Result<GitHub<Client>, github::ApiError> {
        GitHub::new(
            self.client(token, actor, false),
            github::Endpoints::single(server.base()),
            &gh_target(),
        )
    }

    fn gl(
        &self,
        server: &FakeServer<GitlabFake>,
        token: &str,
        actor: &'static str,
    ) -> Result<GitLab<Client>, gitlab::ApiError> {
        GitLab::new(
            self.client(token, actor, true),
            gitlab::Endpoints::instance(server.base()),
            &gl_target(),
        )
    }

    fn before(&mut self, branch: &str, snapshot: Value) {
        self.before.insert(branch.to_owned(), snapshot);
    }

    fn after(&mut self, branch: &str, snapshot: Value) {
        self.after.insert(branch.to_owned(), snapshot);
    }

    /// Дамп журнала ветви, уже переоткрытого сценарием.
    fn journal(&mut self, branch: &str, journal: &Journal) -> Result<(), SubmitError> {
        self.journals.insert(branch.to_owned(), dump(journal)?);
        Ok(())
    }

    fn mark<F: Forge>(&self, server: &FakeServer<F>) -> Mark {
        Mark {
            trace: self.trace.len(),
            logged: server.logged(),
        }
    }

    /// Квитанция: журнал сервера ветви дорос до своей длины на отметке плюс вызовы, отправленные
    /// клиентами с неё.
    fn receipt<F: Forge>(&self, server: &FakeServer<F>, mark: Mark) -> Result<(), String> {
        self.receipt_within(server, mark, RECEIPT)
    }

    fn receipt_within<F: Forge>(
        &self,
        server: &FakeServer<F>,
        mark: Mark,
        timeout: Duration,
    ) -> Result<(), String> {
        let sent = self.trace.len().saturating_sub(mark.trace);
        server.wait_calls(mark.logged + sent, timeout)
    }

    fn check(&mut self, what: &str, ok: bool, observed: impl Into<String>) {
        self.checks.check(what, ok, observed);
    }

    fn eq<T: PartialEq + Debug>(&mut self, what: &str, actual: T, expected: T) {
        self.checks.eq(what, actual, expected);
    }

    fn writes(&mut self, what: &str, log: &[Logged], expected: &[(&str, usize)]) {
        self.checks.writes(what, log, expected);
    }

    fn finish(self, outcome: Outcome, result: &mut ResultFile) {
        let entries = self.trace.entries(&self.scrubber);
        result.trace = entries
            .iter()
            .filter_map(|entry| serde_json::to_value(entry).ok())
            .collect();
        result.calls = self
            .trace
            .counts(&self.scrubber)
            .into_iter()
            .map(|(key, count)| (key, count as u64))
            .collect();
        let scrub = |map: Map<String, Value>| self.scrubber.scrub_json(&Value::Object(map));
        let missing = [
            (self.before.is_empty(), "снимок сервера до"),
            (self.after.is_empty(), "снимок сервера после"),
            (self.journals.is_empty(), "журнал после переоткрытия"),
            (entries.is_empty(), "след вызовов"),
        ]
        .into_iter()
        .filter(|(empty, _)| *empty)
        .map(|(_, what)| what)
        .collect::<Vec<_>>();
        result.server_before = Some(scrub(self.before));
        result.server_after = Some(scrub(self.after));
        result.journal = Some(scrub(self.journals));
        result.host = self.host;
        result.notes = self
            .notes
            .iter()
            .map(|note| self.scrubber.scrub_text(note))
            .chain(
                self.checks
                    .to_json()
                    .iter()
                    .map(|line| self.scrubber.scrub_text(line)),
            )
            .collect();
        if let Err(err) = outcome {
            result.execution = Execution::error(format!(
                "стенд не дошёл до конца: {}",
                self.scrubber.scrub_text(&err.to_string())
            ));
            return;
        }
        if !missing.is_empty() {
            result.execution = Execution::error(format!(
                "нет обязательных артефактов: {}",
                missing.join(", ")
            ));
            return;
        }
        let (status, reason) = self.checks.verdict();
        result.status = status;
        result.reason = self.scrubber.scrub_text(&reason);
    }
}

/// Сценарий строки матрицы в столбце `fake`.
pub fn run(scenario: Scenario, state_dir: &Path, result: &mut ResultFile) {
    let mut stand = Stand::new(state_dir);
    let outcome = match scenario {
        Scenario::Smoke => Err("smoke — самопроверка стенда, а не строка матрицы".into()),
        Scenario::GhLocalCreate => gh_local_create(&mut stand),
        Scenario::GhPendingCycle => gh_pending_cycle(&mut stand),
        Scenario::GhPendingForeign => gh_pending_foreign(&mut stand),
        Scenario::GhAnchors => gh_anchors(&mut stand),
        Scenario::GhViewed => gh_viewed(&mut stand),
        Scenario::GlDraftPublish => gl_draft_publish(&mut stand),
        Scenario::GlForeignDraft => gl_foreign_draft(&mut stand),
        Scenario::GlApproveSha => gl_approve_sha(&mut stand),
        Scenario::GlRequestChanges => gl_request_changes(&mut stand),
        Scenario::GlAnchors => gl_anchors(&mut stand),
        Scenario::HeadChange => head_change(&mut stand),
        Scenario::JournalCrash => journal_crash(&mut stand),
        Scenario::ReconcileAmbiguous => reconcile_ambiguous(&mut stand),
        Scenario::ReconcileForeign => reconcile_foreign(&mut stand),
        Scenario::ReconcileDeleted => reconcile_deleted(&mut stand),
        Scenario::ReconcilePaged => reconcile_paged(&mut stand),
        Scenario::Partial => partial(&mut stand),
        Scenario::Capabilities => capabilities(&mut stand),
        Scenario::Volume => volume(&mut stand),
        Scenario::RateLimit => rate_limit(&mut stand),
    };
    stand.finish(outcome, result);
}

fn project() -> ProjectId {
    ProjectId::new(PROJECT)
}

fn gh_target() -> ReviewTarget {
    ReviewTarget {
        provider: Provider::GitHub,
        host: "github.com".to_owned(),
        repo: format!("{GH_OWNER}/{GH_REPO}"),
        number: GH_NUMBER,
    }
}

fn gl_target() -> ReviewTarget {
    ReviewTarget {
        provider: Provider::GitLab,
        host: "gitlab.fake".to_owned(),
        repo: GL_PROJECT.to_owned(),
        number: GL_IID,
    }
}

fn github_fake() -> GithubFake {
    github_fake_with(DIFF)
}

fn github_fake_with(diff: &str) -> GithubFake {
    let mut fake = GithubFake::new(GH_OWNER, GH_REPO);
    fake.add_user(REVIEWER, TOKEN_REVIEWER, ghf::Permission::Write);
    fake.add_user(AUTHOR, TOKEN_AUTHOR, ghf::Permission::Write);
    fake.add_pull(
        GH_NUMBER,
        AUTHOR,
        BASE,
        FakeCommit {
            sha: HEAD.to_owned(),
            files: vec![FakeFile::modified(PATH, diff)],
        },
    );
    fake
}

fn gitlab_fake() -> Result<GitlabFake, String> {
    gitlab_fake_with(DIFF)
}

fn gitlab_fake_with(diff: &str) -> Result<GitlabFake, String> {
    let mut fake = GitlabFake::new(GL_PROJECT);
    fake.add_user(REVIEWER, TOKEN_REVIEWER, Some(Access::Developer));
    fake.add_user(AUTHOR, TOKEN_AUTHOR, Some(Access::Developer));
    fake.add_mr(
        GL_IID,
        AUTHOR,
        BASE,
        HEAD,
        vec![FakeDiff::modified(PATH, diff)],
    );
    fake.add_reviewer(GL_IID, REVIEWER)?;
    Ok(fake)
}

/// Место черновика в diff: строка или диапазон.
#[derive(Clone, Copy, Debug)]
pub(super) enum Spot {
    Line(Side, u32),
    Range(Side, u32, Side, u32),
}

impl Spot {
    pub(super) fn placement(self, file: &FileDiff) -> Result<Placement, ModelError> {
        let anchor = match self {
            Spot::Line(side, line) => Anchor::line(file, LinePoint::at(file, side, line)?)?,
            Spot::Range(start_side, start, end_side, end) => Anchor::range(
                file,
                LinePoint::at(file, start_side, start)?,
                LinePoint::at(file, end_side, end)?,
            )?,
        };
        Ok(Placement::Line(anchor))
    }
}

fn first_file(files: &[FileDiff]) -> Result<&FileDiff, String> {
    files.first().ok_or_else(|| "в diff нет файлов".to_owned())
}

pub(super) fn add_drafts(
    scope: &Scope<'_>,
    session: SessionId,
    files: &[FileDiff],
    drafts: &[(Spot, &str)],
) -> Result<Vec<DraftId>, Box<dyn Error>> {
    let file = first_file(files)?;
    let mut ids = Vec::new();
    for (spot, body) in drafts {
        ids.push(scope.add_draft(session, spot.placement(file)?, body)?.id);
    }
    Ok(ids)
}

fn gh_session<T: Transport>(
    scope: &Scope<'_>,
    github: &GitHub<T>,
    drafts: &[(Spot, &str)],
) -> Result<(SessionId, Vec<DraftId>), Box<dyn Error>> {
    let snapshot = github.pull()?;
    let session = ReviewSession::new(REVIEWER, gh_target(), snapshot.revision.clone());
    let id = scope.create_session(&session)?;
    let drafts = add_drafts(scope, id, &snapshot.files, drafts)?;
    Ok((id, drafts))
}

fn gl_session<T: Transport>(
    scope: &Scope<'_>,
    gitlab: &GitLab<T>,
    account: &str,
    drafts: &[(Spot, &str)],
) -> Result<(SessionId, gitlab::MergeSnapshot, Vec<DraftId>), Box<dyn Error>> {
    let snapshot = gitlab.merge_request()?;
    let session = ReviewSession::new(account, gl_target(), snapshot.revision.clone());
    let id = scope.create_session(&session)?;
    let drafts = add_drafts(scope, id, &snapshot.files, drafts)?;
    Ok((id, snapshot, drafts))
}

fn delivery_name(delivery: Delivery) -> &'static str {
    match delivery {
        Delivery::Full => "full",
        Delivery::Partial => "partial",
        Delivery::NotSent => "not_sent",
    }
}

fn id_text(id: Option<&ProviderId>) -> Value {
    id.map_or(Value::Null, |id| Value::String(id.as_str().to_owned()))
}

/// Журнал после переоткрытия: сессии, черновики, отправки с элементами и итогом шага 5.
fn dump(journal: &Journal) -> Result<Value, SubmitError> {
    dump_in(journal, &project())
}

/// [`dump`] для проекта журнала живой клетки.
pub(super) fn dump_in(journal: &Journal, project: &ProjectId) -> Result<Value, SubmitError> {
    let scope = journal.scope(project);
    let mut sessions = Vec::new();
    for (id, session) in scope.sessions()? {
        let drafts: Vec<Value> = scope
            .drafts(id)?
            .iter()
            .map(|draft| {
                json!({
                    "id": draft.id.0,
                    "body": draft.body,
                    "remote_id": id_text(draft.remote_id.as_ref()),
                    "head_sha": draft.revision().head_sha().as_str(),
                    "current": draft.is_current(),
                })
            })
            .collect();
        let mut submissions = Vec::new();
        for submission in scope.submissions(id)? {
            let delivered = report(&scope, submission.id)?;
            let items: Vec<Value> = submission
                .items
                .iter()
                .map(|item| {
                    json!({
                        "seq": item.seq,
                        "operation": item.operation.name(),
                        "state": item.state.name(),
                        "drafts": item.drafts.iter().map(|d| d.0).collect::<Vec<_>>(),
                        "remote_id": id_text(item.remote_id.as_ref()),
                        "candidate": id_text(item.candidate.as_ref()),
                        "detail": item.detail,
                    })
                })
                .collect();
            submissions.push(json!({
                "id": submission.id.0,
                "closed": submission.closed,
                "delivery": delivery_name(delivered.delivery),
                "event": submission.spec.event,
                "summary": submission.spec.summary,
                "pending_review": id_text(submission.spec.pending_review.as_ref()),
                "pending_foreign": submission.spec.pending_foreign.as_ref().map(Vec::len),
                "items": items,
            }));
        }
        sessions.push(json!({
            "id": id.0,
            "account": session.account(),
            "head_sha": session.revision().head_sha().as_str(),
            "drafts": drafts,
            "submissions": submissions,
        }));
    }
    Ok(json!({
        "recovered": journal.recovered().iter().map(|item| item.0).collect::<Vec<_>>(),
        "sessions": sessions,
    }))
}

/// Состояния элементов отправки по порядку.
pub(super) fn states(
    scope: &Scope<'_>,
    id: SubmissionId,
) -> Result<Vec<(Operation, ItemState)>, SubmitError> {
    Ok(scope
        .submission(id)?
        .items
        .iter()
        .map(|item| (item.operation, item.state))
        .collect())
}

fn draft_id_in(path: &str) -> Option<u64> {
    path.split("/draft_notes/")
        .nth(1)?
        .split(['/', '?'])
        .next()?
        .parse()
        .ok()
}

fn bodies<'a>(log: &'a [Logged], key: &str) -> Vec<&'a Value> {
    log.iter()
        .filter(|call| call.key == key)
        .map(|call| &call.body)
        .collect()
}

fn sha(value: &str) -> Result<Sha, ModelError> {
    Sha::parse(value)
}

fn revision(base: &str, head: &str) -> Result<Revision, ModelError> {
    Ok(Revision::new(sha(base)?, sha(base)?, sha(head)?))
}

// ---------------------------------------------------------------------------------------------
// GitHub

fn gh_local_create(stand: &mut Stand) -> Outcome {
    for event in [
        ReviewEvent::Comment,
        ReviewEvent::Approve,
        ReviewEvent::RequestChanges,
    ] {
        let branch = event.name().to_ascii_lowercase();
        let server = FakeServer::start(github_fake())?;
        stand.before(&branch, server.with(|f| f.snapshot()));
        let github = stand.gh(&server, TOKEN_REVIEWER, ACTOR_REVIEWER)?;
        let caps = github.capabilities()?;
        let path = stand.journal_path(&branch);
        let submission = {
            let journal = Journal::open(&path)?;
            let scope = journal.scope(&project());
            let (session, _) = gh_session(
                &scope,
                &github,
                &[
                    (Spot::Line(Side::New, 2), "первый"),
                    (Spot::Line(Side::New, 4), "второй"),
                ],
            )?;
            let submission = plan_github_local(&scope, session, event, SUMMARY)?;
            run_github(&scope, submission.id, &github, &caps)?;
            submission.id
        };
        stand.after(&branch, server.with(|f| f.snapshot()));
        let log = server.log();
        stand.writes(&format!("{branch}: записи"), &log, &[(GH_CREATE, 1)]);
        if let [body] = bodies(&log, GH_CREATE)[..] {
            stand.eq(
                &format!("{branch}: event Create review"),
                body["event"].as_str(),
                Some(event.name()),
            );
            stand.check(
                &format!("{branch}: body Create review непустой"),
                body["body"].as_str().is_some_and(|b| !b.trim().is_empty()),
                body["body"].to_string(),
            );
            stand.eq(
                &format!("{branch}: commit_id Create review"),
                body["commit_id"].as_str(),
                Some(HEAD),
            );
            let comments: Vec<(Option<&str>, Option<u64>, Option<&str>)> = body["comments"]
                .as_array()
                .map(|list| {
                    list.iter()
                        .map(|c| (c["path"].as_str(), c["line"].as_u64(), c["side"].as_str()))
                        .collect()
                })
                .unwrap_or_default();
            stand.eq(
                &format!("{branch}: comments[] Create review"),
                comments,
                vec![
                    (Some(PATH), Some(2), Some("RIGHT")),
                    (Some(PATH), Some(4), Some("RIGHT")),
                ],
            );
        }
        let expected_state = match event {
            ReviewEvent::Comment => ghf::ReviewState::Commented,
            ReviewEvent::Approve => ghf::ReviewState::Approved,
            ReviewEvent::RequestChanges => ghf::ReviewState::ChangesRequested,
        };
        let reviews = server.with(|f| f.reviews(GH_NUMBER));
        let summary: Vec<(String, ghf::ReviewState, String, ghf::Origin)> = reviews
            .iter()
            .map(|r| (r.user.clone(), r.state, r.commit_id.clone(), r.origin))
            .collect();
        stand.eq(
            &format!("{branch}: review на сервере"),
            summary,
            vec![(
                REVIEWER.to_owned(),
                expected_state,
                HEAD.to_owned(),
                ghf::Origin::Api,
            )],
        );
        let review_id = reviews.first().map(|r| r.id);
        let mut comments: Vec<(u32, DiffSide, String)> = server
            .with(|f| f.comments(GH_NUMBER))
            .iter()
            .filter(|c| Some(c.review_id) == review_id)
            .map(|c| (c.line, c.side, c.body.clone()))
            .collect();
        comments.sort_by_key(|c| c.0);
        stand.eq(
            &format!("{branch}: комментарии review"),
            comments,
            vec![
                (2, DiffSide::Right, "первый".to_owned()),
                (4, DiffSide::Right, "второй".to_owned()),
            ],
        );
        let journal = Journal::open(&path)?;
        let scope = journal.scope(&project());
        let done = report(&scope, submission)?;
        stand.eq(
            &format!("{branch}: итог в журнале"),
            done.delivery,
            Delivery::Full,
        );
        let items: Vec<(Operation, ItemState, Option<String>)> = done
            .items
            .iter()
            .map(|i| {
                (
                    i.operation,
                    i.state,
                    i.remote_id.as_ref().map(|r| r.as_str().to_owned()),
                )
            })
            .collect();
        stand.eq(
            &format!("{branch}: элементы в журнале"),
            items,
            vec![(
                Operation::GitHubCreateReview,
                ItemState::Confirmed,
                review_id.map(|id| id.to_string()),
            )],
        );
        stand.eq(
            &format!("{branch}: восстановленные элементы"),
            journal.recovered().len(),
            0,
        );
        stand.journal(&branch, &journal)?;
    }
    Ok(())
}

fn gh_pending_cycle(stand: &mut Stand) -> Outcome {
    let server = FakeServer::start(github_fake())?;
    server.inject(Fault::new(GH_SUBMIT, Reply::Status(422)));
    stand.before("main", server.with(|f| f.snapshot()));
    let github = stand.gh(&server, TOKEN_REVIEWER, ACTOR_REVIEWER)?;
    let caps = github.capabilities()?;
    let path = stand.journal_path("main");
    let (first, second, before_update) = {
        let journal = Journal::open(&path)?;
        let scope = journal.scope(&project());
        let (session, ids) = gh_session(
            &scope,
            &github,
            &[
                (Spot::Line(Side::New, 2), "первый"),
                (Spot::Line(Side::New, 4), "второй"),
            ],
        )?;
        let first = plan_github_pending(&scope, session, ReviewEvent::Comment, SUMMARY, None, &[])?;
        run_github(&scope, first.id, &github, &caps)?;
        let before_update = server.with(|f| f.comments(GH_NUMBER));
        let first_draft = *ids.first().ok_or("нет черновика")?;
        scope.update_draft_body(first_draft, "первый, исправлен")?;
        let second = plan_github_pending(
            &scope,
            session,
            ReviewEvent::Comment,
            SUMMARY,
            None,
            &[first_draft],
        )?;
        run_github(&scope, second.id, &github, &caps)?;
        (first.id, second.id, before_update)
    };
    stand.after("main", server.with(|f| f.snapshot()));
    server.finish()?;
    let log = server.log();
    stand.writes(
        "записи",
        &log,
        &[
            (GH_CREATE, 1),
            (GH_ADD_THREAD, 2),
            (GH_UPDATE_COMMENT, 1),
            (GH_SUBMIT, 2),
        ],
    );
    let reviews = server.with(|f| f.reviews(GH_NUMBER));
    let review = reviews.first().cloned();
    stand.eq(
        "review на сервере: число, состояние",
        reviews.iter().map(|r| r.state).collect::<Vec<_>>(),
        vec![ghf::ReviewState::Commented],
    );
    if let [create] = bodies(&log, GH_CREATE)[..] {
        stand.check(
            "создание PENDING без event и comments",
            create.get("event").is_none_or(Value::is_null)
                && create.get("comments").is_none_or(Value::is_null),
            create.to_string(),
        );
    }
    let node = review
        .as_ref()
        .map(|r| r.node_id.clone())
        .unwrap_or_default();
    for body in bodies(&log, GH_ADD_THREAD) {
        stand.eq(
            "addPullRequestReviewThread в созданный review",
            body.pointer("/variables/input/pullRequestReviewId")
                .and_then(Value::as_str),
            Some(node.as_str()),
        );
    }
    let mut first_before = before_update.iter().filter(|c| c.line == 2);
    let first_before = first_before.next().cloned();
    if let (Some(before), [update]) = (&first_before, &bodies(&log, GH_UPDATE_COMMENT)[..]) {
        stand.check(
            "updatePullRequestReviewComment по node ID первого комментария",
            update.to_string().contains(&before.node_id),
            update.to_string(),
        );
    }
    for body in bodies(&log, GH_SUBMIT) {
        stand.check(
            "Submit с event и body, без comments[]",
            body["event"].as_str() == Some("COMMENT")
                && body["body"].as_str().is_some_and(|b| !b.trim().is_empty())
                && body.get("comments").is_none(),
            body.to_string(),
        );
    }
    let comments = server.with(|f| f.comments(GH_NUMBER));
    stand.eq(
        "комментарии review",
        comments
            .iter()
            .filter(|c| Some(c.review_id) == review.as_ref().map(|r| r.id))
            .count(),
        2,
    );
    let first_after = comments.iter().find(|c| c.line == 2);
    stand.eq(
        "первый комментарий: прежние ID и node ID, новый текст",
        first_after.map(|c| (c.id, c.node_id.clone(), c.body.clone())),
        first_before.map(|c| (c.id, c.node_id, "первый, исправлен".to_owned())),
    );
    let journal = Journal::open(&path)?;
    let scope = journal.scope(&project());
    let first_report = report(&scope, first)?;
    stand.eq(
        "первая отправка: закрыта, итог",
        (first_report.closed, first_report.delivery),
        (true, Delivery::NotSent),
    );
    stand.eq(
        "первая отправка: Submit",
        states(&scope, first)?.last().copied(),
        Some((Operation::GitHubSubmitReview, ItemState::Failed)),
    );
    stand.eq(
        "вторая отправка",
        (states(&scope, second)?, report(&scope, second)?.delivery),
        (
            vec![
                (Operation::GitHubUpdatePendingComment, ItemState::Confirmed),
                (Operation::GitHubSubmitReview, ItemState::Confirmed),
            ],
            Delivery::Full,
        ),
    );
    stand.journal("main", &journal)?;
    Ok(())
}

fn gh_pending_foreign(stand: &mut Stand) -> Outcome {
    let server = FakeServer::start(github_fake())?;
    let specs: Vec<CommentSpec> = (0..101)
        .map(|index| {
            CommentSpec::line(
                PATH,
                DiffSide::Right,
                [1, 2, 3, 4, 5][index % 5],
                &format!("браузер {index}"),
            )
        })
        .collect();
    let (planted_review, planted) =
        server.with(|f| f.plant_pending(GH_NUMBER, REVIEWER, "черновик браузера", &specs))?;
    stand.before("main", server.with(|f| f.snapshot()));
    let github = stand.gh(&server, TOKEN_REVIEWER, ACTOR_REVIEWER)?;
    let caps = github.capabilities()?;
    let path = stand.journal_path("main");
    let planted_ids: BTreeSet<String> = planted.iter().map(u64::to_string).collect();
    let (first, second) = {
        let journal = Journal::open(&path)?;
        let scope = journal.scope(&project());
        let (session, _) = gh_session(&scope, &github, &[(Spot::Line(Side::New, 2), "наш")])?;
        let first = plan_github_pending(&scope, session, ReviewEvent::Comment, SUMMARY, None, &[])?;
        match run_github(&scope, first.id, &github, &caps) {
            Err(SubmitError::GitHub(github::ApiError::PendingExists(existing))) => {
                let read: BTreeSet<String> = existing
                    .comments
                    .iter()
                    .map(|c| c.id.as_str().to_owned())
                    .collect();
                stand.eq(
                    "найденный PENDING прочитан целиком",
                    (
                        existing.id().as_str().to_owned(),
                        read.len(),
                        read == planted_ids,
                    ),
                    (planted_review.to_string(), 101, true),
                );
            }
            other => stand.check(
                "отправка без принятия: «PENDING существует»",
                false,
                format!("{other:?}"),
            ),
        }
        stand.writes("записи до принятия", &server.log(), &[]);
        scope.close_submission(first.id)?;
        let existing = github
            .find_pending(REVIEWER)?
            .ok_or("PENDING не найден при принятии")?;
        let pending = existing.adopt();
        let second = plan_github_pending(
            &scope,
            session,
            ReviewEvent::Comment,
            SUMMARY,
            Some(&pending),
            &[],
        )?;
        run_github(&scope, second.id, &github, &caps)?;
        (first.id, second.id)
    };
    stand.after("main", server.with(|f| f.snapshot()));
    let log = server.log();
    stand.writes("записи", &log, &[(GH_ADD_THREAD, 1), (GH_SUBMIT, 1)]);
    let reviews = server.with(|f| f.reviews(GH_NUMBER));
    let node = reviews
        .first()
        .map(|r| r.node_id.clone())
        .unwrap_or_default();
    for body in bodies(&log, GH_ADD_THREAD) {
        stand.eq(
            "addPullRequestReviewThread в принятый PENDING",
            body.pointer("/variables/input/pullRequestReviewId")
                .and_then(Value::as_str),
            Some(node.as_str()),
        );
    }
    stand.eq(
        "review на сервере",
        reviews
            .iter()
            .map(|r| (r.id, r.state, r.origin))
            .collect::<Vec<_>>(),
        vec![(
            planted_review,
            ghf::ReviewState::Commented,
            ghf::Origin::Planted,
        )],
    );
    let comments = server.with(|f| f.comments(GH_NUMBER));
    let kept = planted.iter().enumerate().all(|(index, id)| {
        comments
            .iter()
            .any(|c| c.id == *id && c.body == format!("браузер {index}"))
    });
    stand.eq(
        "чужие комментарии на месте, наш добавлен",
        (
            comments.len(),
            kept,
            comments
                .iter()
                .filter(|c| c.body == "наш" && c.review_id == planted_review)
                .count(),
        ),
        (102, true, 1),
    );
    let journal = Journal::open(&path)?;
    let scope = journal.scope(&project());
    let first_states = states(&scope, first)?;
    stand.check(
        "первая отправка закрыта без in_flight, unknown, confirmed",
        scope.submission(first)?.closed
            && first_states
                .iter()
                .all(|(_, state)| *state == ItemState::Planned),
        format!("{first_states:?}"),
    );
    let second_sub = scope.submission(second)?;
    stand.eq(
        "вторая отправка: итог и чужие комментарии",
        (
            report(&scope, second)?.delivery,
            second_sub.spec.pending_foreign.as_ref().map(Vec::len),
        ),
        (Delivery::Full, Some(101)),
    );
    stand.journal("main", &journal)?;
    Ok(())
}

fn gh_anchors(stand: &mut Stand) -> Outcome {
    let server = FakeServer::start(github_fake())?;
    stand.before("main", server.with(|f| f.snapshot()));
    let github = stand.gh(&server, TOKEN_REVIEWER, ACTOR_REVIEWER)?;
    let caps = github.capabilities()?;
    let path = stand.journal_path("main");
    let submission = {
        let journal = Journal::open(&path)?;
        let scope = journal.scope(&project());
        let (session, _) = gh_session(
            &scope,
            &github,
            &[
                (Spot::Line(Side::Old, 2), "старая сторона"),
                (Spot::Line(Side::New, 4), "новая сторона"),
                (Spot::Range(Side::New, 1, Side::New, 2), "диапазон"),
            ],
        )?;
        let submission = plan_github_local(&scope, session, ReviewEvent::Comment, SUMMARY)?;
        run_github(&scope, submission.id, &github, &caps)?;
        let review = scope
            .item(submission.items.first().ok_or("нет элемента")?.id)?
            .remote_id
            .ok_or("у review нет ID")?;
        // Как живая клетка: корень и тред читает автор, тред соединяется с корнем по node ID.
        let author = stand.gh(&server, TOKEN_AUTHOR, ACTOR_AUTHOR)?;
        let pull = author.pull()?.node_id;
        let root = author
            .review_comments(&review)?
            .into_iter()
            .find(|c| c.body == "старая сторона" && c.in_reply_to.is_none())
            .ok_or("нет корня треда в чтении автора")?;
        github.reply(&root.id, "ответ в треде")?;
        let thread = author
            .review_threads(&pull)?
            .into_iter()
            .find(|t| t.root.as_ref() == Some(&root.node_id))
            .ok_or("тред корня не найден чтением тредов автором")?;
        stand.notes.push(format!(
            "node ID треда для resolve — из чтения тредов PR автором (review_threads) по node ID \
             корня, comments.totalCount {}",
            thread.comments
        ));
        github.set_thread_resolved(&thread.node_id, true)?;
        submission.id
    };
    stand.after("main", server.with(|f| f.snapshot()));
    let log = server.log();
    stand.writes(
        "записи",
        &log,
        &[(GH_CREATE, 1), (GH_REPLIES, 1), (GH_RESOLVE, 1)],
    );
    type Sent<'a> = (Option<u64>, Option<&'a str>, Option<u64>, Option<&'a str>);
    if let [body] = bodies(&log, GH_CREATE)[..] {
        let sent: Vec<Sent<'_>> = body["comments"]
            .as_array()
            .map(|list| {
                list.iter()
                    .map(|c| {
                        (
                            c["line"].as_u64(),
                            c["side"].as_str(),
                            c["start_line"].as_u64(),
                            c["start_side"].as_str(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        stand.eq(
            "якоря в comments[]",
            sent,
            vec![
                (Some(2), Some("LEFT"), None, None),
                (Some(4), Some("RIGHT"), None, None),
                (Some(2), Some("RIGHT"), Some(1), Some("RIGHT")),
            ],
        );
    }
    let comments = server.with(|f| f.comments(GH_NUMBER));
    type Stored = (String, u32, DiffSide, Option<u32>, Option<DiffSide>, String);
    let mut stored: Vec<Stored> = comments
        .iter()
        .filter(|c| c.in_reply_to.is_none())
        .map(|c| {
            (
                c.body.clone(),
                c.line,
                c.side,
                c.start_line,
                c.start_side,
                c.commit_id.clone(),
            )
        })
        .collect();
    stored.sort_by(|a, b| a.0.cmp(&b.0));
    stand.eq(
        "комментарии review на сервере",
        stored,
        vec![
            (
                "диапазон".to_owned(),
                2,
                DiffSide::Right,
                Some(1),
                Some(DiffSide::Right),
                HEAD.to_owned(),
            ),
            (
                "новая сторона".to_owned(),
                4,
                DiffSide::Right,
                None,
                None,
                HEAD.to_owned(),
            ),
            (
                "старая сторона".to_owned(),
                2,
                DiffSide::Left,
                None,
                None,
                HEAD.to_owned(),
            ),
        ],
    );
    let root = comments.iter().find(|c| c.body == "старая сторона");
    let reply = comments.iter().find(|c| c.in_reply_to.is_some());
    stand.eq(
        "ответ в том же треде",
        reply.map(|r| (r.in_reply_to, r.thread_id, r.body.clone())),
        root.map(|r| (Some(r.id), r.thread_id, "ответ в треде".to_owned())),
    );
    let resolved = root.and_then(|r| {
        server.with(|f| {
            f.threads(GH_NUMBER)
                .into_iter()
                .find(|t| t.id == r.thread_id)
                .map(|t| t.resolved)
        })
    });
    stand.eq("тред разрешён", resolved, Some(true));
    let journal = Journal::open(&path)?;
    let scope = journal.scope(&project());
    stand.eq(
        "журнал",
        (
            states(&scope, submission)?,
            report(&scope, submission)?.delivery,
        ),
        (
            vec![(Operation::GitHubCreateReview, ItemState::Confirmed)],
            Delivery::Full,
        ),
    );
    stand.journal("main", &journal)?;
    Ok(())
}

fn gh_viewed(stand: &mut Stand) -> Outcome {
    let server = FakeServer::start(github_fake())?;
    stand.before("main", server.with(|f| f.snapshot()));
    let github = stand.gh(&server, TOKEN_REVIEWER, ACTOR_REVIEWER)?;
    let caps = github.capabilities()?;
    let path = stand.journal_path("main");
    let pull = github.pull()?;
    {
        let journal = Journal::open(&path)?;
        let scope = journal.scope(&project());
        gh_session(&scope, &github, &[])?;
    }
    let state_of = |files: Vec<(String, github::ViewedState)>| {
        files
            .into_iter()
            .find(|(file, _)| file == PATH)
            .map(|(_, state)| state)
    };
    github.set_viewed(&caps, &pull.node_id, PATH, true)?;
    let read = state_of(github.viewed_files(&pull.node_id)?);
    let stored = server.with(|f| f.viewed(GH_NUMBER, REVIEWER).get(PATH).copied());
    stand.eq(
        "после markFileAsViewed",
        (read, stored),
        (Some(github::ViewedState::Viewed), Some(ghf::Viewed::Viewed)),
    );
    github.set_viewed(&caps, &pull.node_id, PATH, false)?;
    let read = state_of(github.viewed_files(&pull.node_id)?);
    let stored = server.with(|f| f.viewed(GH_NUMBER, REVIEWER).get(PATH).copied());
    stand.eq(
        "после unmarkFileAsViewed",
        (read, stored),
        (Some(github::ViewedState::Unviewed), None),
    );
    stand.after("main", server.with(|f| f.snapshot()));
    let log = server.log();
    stand.writes("записи", &log, &[(GH_MARK, 1), (GH_UNMARK, 1)]);
    for key in [GH_MARK, GH_UNMARK] {
        for body in bodies(&log, key) {
            stand.eq(
                &format!("{key}: вход"),
                (
                    body.pointer("/variables/input/pullRequestId")
                        .and_then(Value::as_str),
                    body.pointer("/variables/input/path")
                        .and_then(Value::as_str),
                ),
                (Some(pull.node_id.as_str()), Some(PATH)),
            );
        }
    }
    let journal = Journal::open(&path)?;
    let scope = journal.scope(&project());
    let sessions = scope.sessions()?;
    let submissions: usize = sessions
        .iter()
        .map(|(id, _)| scope.submissions(*id).map(|s| s.len()))
        .sum::<Result<usize, _>>()?;
    stand.eq(
        "журнал: сессия без отправок",
        (sessions.len(), submissions),
        (1, 0),
    );
    stand.journal("main", &journal)?;
    Ok(())
}

fn reconcile_foreign(stand: &mut Stand) -> Outcome {
    let server = FakeServer::start(github_fake())?;
    server.inject(Fault::new(GH_CREATE, Reply::Status(502)));
    stand.before("main", server.with(|f| f.snapshot()));
    let github = stand.gh(&server, TOKEN_REVIEWER, ACTOR_REVIEWER)?;
    let second = stand.gh(&server, TOKEN_REVIEWER, ACTOR_SECOND)?;
    let caps = github.capabilities()?;
    let path = stand.journal_path("main");
    let mark = stand.mark(&server);
    let (submission, foreign) = {
        let journal = Journal::open(&path)?;
        let scope = journal.scope(&project());
        let (session, ids) = gh_session(
            &scope,
            &github,
            &[(Spot::Line(Side::New, 2), "тот же текст")],
        )?;
        let submission = plan_github_local(&scope, session, ReviewEvent::Comment, SUMMARY)?;
        let stopped = run_github(&scope, submission.id, &github, &caps)?;
        stand.eq("Create review: неизвестный итог", stopped.unresolved, 1);
        let at = scope.session(session)?;
        let draft = scope.draft(*ids.first().ok_or("нет черновика")?)?;
        let caps2 = second.capabilities()?;
        let clear = second.check_no_pending(&caps2)?;
        let foreign = second.create_review(&at, &clear, &[draft], ReviewEvent::Comment, SUMMARY)?;
        stand.receipt(&server, mark)?;
        let verdicts = reconcile_github(&scope, submission.id, &github)?;
        stand.check(
            "сверка: кандидат — review второго клиента",
            matches!(&verdicts[..], [Verdict::Candidate { remote, .. }] if *remote == foreign.id),
            format!("{verdicts:?}"),
        );
        (submission.id, foreign.id)
    };
    stand.after("main", server.with(|f| f.snapshot()));
    server.finish()?;
    let log = server.log();
    stand.writes("записи", &log, &[(GH_CREATE, 2)]);
    let ours = stand
        .trace
        .calls()
        .iter()
        .filter(|c| {
            c.actor == ACTOR_REVIEWER
                && c.request.method == crate::transport::Method::Post
                && c.request.path().ends_with("/reviews")
        })
        .count();
    stand.eq("записей пробы (без повтора)", ours, 1);
    let journal = Journal::open(&path)?;
    let scope = journal.scope(&project());
    let item = scope
        .submission(submission)?
        .items
        .into_iter()
        .next()
        .ok_or("нет элемента")?;
    stand.eq(
        "журнал: unknown с кандидатом, итог",
        (
            item.state,
            item.candidate,
            report(&scope, submission)?.delivery,
        ),
        (ItemState::Unknown, Some(foreign), Delivery::Partial),
    );
    stand.journal("main", &journal)?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// GitLab

/// Позиция черновика в форме API для подложенных объектов второго клиента.
fn position_of(
    session: &ReviewSession,
    file: &FileDiff,
    spot: Spot,
) -> Result<Value, Box<dyn Error>> {
    let draft = DraftComment::new(DraftId(0), session, spot.placement(file)?, "");
    Ok(serde_json::to_value(session.gitlab_position(&draft)?)?)
}

fn gl_draft_publish(stand: &mut Stand) -> Outcome {
    let server = FakeServer::start(gitlab_fake()?)?;
    stand.before("main", server.with(|f| f.snapshot()));
    let gitlab = stand.gl(&server, TOKEN_REVIEWER, ACTOR_REVIEWER)?;
    let caps = gitlab.capabilities()?;
    let path = stand.journal_path("main");
    let submission = {
        let journal = Journal::open(&path)?;
        let scope = journal.scope(&project());
        let (session, _, _) = gl_session(
            &scope,
            &gitlab,
            REVIEWER,
            &[
                (Spot::Line(Side::New, 2), "первый"),
                (Spot::Line(Side::New, 4), "второй"),
            ],
        )?;
        let plan = GitLabPlan {
            summary: SUMMARY.to_owned(),
            ..GitLabPlan::default()
        };
        let submission = plan_gitlab(&scope, session, &plan)?;
        run_gitlab(&scope, submission.id, &gitlab, &caps)?;
        submission.id
    };
    stand.after("main", server.with(|f| f.snapshot()));
    let log = server.log();
    stand.writes(
        "записи",
        &log,
        &[(glf::DRAFTS, 2), (glf::PUBLISH, 2), (glf::NOTES, 1)],
    );
    let published: BTreeSet<u64> = log
        .iter()
        .filter(|c| c.key == glf::PUBLISH)
        .filter_map(|c| draft_id_in(&c.path))
        .collect();
    let last_publish = log
        .iter()
        .filter(|c| c.key == glf::PUBLISH)
        .map(|c| c.seq)
        .max();
    let note_seq = log.iter().find(|c| c.key == glf::NOTES).map(|c| c.seq);
    stand.check(
        "сводка после последней публикации",
        matches!((last_publish, note_seq), (Some(p), Some(n)) if n > p),
        format!("публикация {last_publish:?}, сводка {note_seq:?}"),
    );
    let notes = server.with(|f| f.notes(GL_IID));
    let from: BTreeSet<u64> = notes.iter().filter_map(|n| n.from_draft).collect();
    stand.eq(
        "заметки из черновиков опубликованы по ID созданных",
        (from.len(), from == published),
        (2, true),
    );
    stand.eq(
        "сводка — заметка без позиции",
        notes
            .iter()
            .filter(|n| n.position.is_none())
            .map(|n| (n.body.clone(), n.from_draft, n.origin))
            .collect::<Vec<_>>(),
        vec![(SUMMARY.to_owned(), None, glf::Origin::Api)],
    );
    stand.eq(
        "черновиков на сервере",
        server.with(|f| f.drafts(GL_IID)).len(),
        0,
    );
    let journal = Journal::open(&path)?;
    let scope = journal.scope(&project());
    let done = report(&scope, submission)?;
    stand.check(
        "журнал: все элементы confirmed, итог «полностью»",
        done.delivery == Delivery::Full
            && done.items.iter().all(|i| i.state == ItemState::Confirmed),
        format!("{:?}", states(&scope, submission)?),
    );
    let session = scope.submission(submission)?.session;
    stand.check(
        "журнал: у черновиков ID GitLab",
        scope.drafts(session)?.iter().all(|d| d.remote_id.is_some()),
        "",
    );
    stand.eq("восстановленные элементы", journal.recovered().len(), 0);
    stand.journal("main", &journal)?;
    Ok(())
}

fn gl_foreign_draft(stand: &mut Stand) -> Outcome {
    let server = FakeServer::start(gitlab_fake()?)?;
    let gitlab = stand.gl(&server, TOKEN_REVIEWER, ACTOR_REVIEWER)?;
    let caps = gitlab.capabilities()?;
    let path = stand.journal_path("main");
    let (submission, planted) = {
        let journal = Journal::open(&path)?;
        let scope = journal.scope(&project());
        let (session, snapshot, _) = gl_session(
            &scope,
            &gitlab,
            REVIEWER,
            &[(Spot::Line(Side::New, 2), "свой")],
        )?;
        let at = scope.session(session)?;
        let position = position_of(&at, first_file(&snapshot.files)?, Spot::Line(Side::New, 4))?;
        let planted = server
            .with(|f| f.plant_draft(GL_IID, REVIEWER, "черновик браузера", Some(&position)))?;
        stand.before("main", server.with(|f| f.snapshot()));
        let submission = plan_gitlab(&scope, session, &GitLabPlan::default())?;
        run_gitlab(&scope, submission.id, &gitlab, &caps)?;
        (submission.id, planted)
    };
    stand.after("main", server.with(|f| f.snapshot()));
    let log = server.log();
    stand.writes("записи", &log, &[(glf::DRAFTS, 1), (glf::PUBLISH, 1)]);
    let published: Vec<u64> = log
        .iter()
        .filter(|c| c.key == glf::PUBLISH)
        .filter_map(|c| draft_id_in(&c.path))
        .collect();
    let notes = server.with(|f| f.notes(GL_IID));
    stand.eq(
        "опубликован только свой черновик",
        (
            published.clone(),
            notes.iter().map(|n| n.from_draft).collect::<Vec<_>>(),
        ),
        (
            published
                .iter()
                .copied()
                .filter(|id| *id != planted)
                .collect(),
            published.iter().map(|id| Some(*id)).collect(),
        ),
    );
    stand.eq(
        "подложенный черновик остался черновиком",
        server
            .with(|f| f.drafts(GL_IID))
            .iter()
            .map(|d| (d.id, d.note.clone(), d.origin))
            .collect::<Vec<_>>(),
        vec![(
            planted,
            "черновик браузера".to_owned(),
            glf::Origin::Planted,
        )],
    );
    let journal = Journal::open(&path)?;
    let scope = journal.scope(&project());
    let session = scope.submission(submission)?.session;
    let planted_text = planted.to_string();
    stand.eq(
        "журнал: итог, ID подложенного нет",
        (
            report(&scope, submission)?.delivery,
            scope
                .drafts(session)?
                .iter()
                .any(|d| d.remote_id.as_ref().map(ProviderId::as_str) == Some(&planted_text)),
        ),
        (Delivery::Full, false),
    );
    stand.journal("main", &journal)?;
    Ok(())
}

fn gl_approve_sha(stand: &mut Stand) -> Outcome {
    let mut fake = gitlab_fake()?;
    fake.add_user(REVIEWER2, TOKEN_REVIEWER2, Some(Access::Developer));
    fake.add_reviewer(GL_IID, REVIEWER2)?;
    let server = FakeServer::start(fake)?;
    stand.before("main", server.with(|f| f.snapshot()));
    let bob = stand.gl(&server, TOKEN_REVIEWER, ACTOR_REVIEWER)?;
    let carol = stand.gl(&server, TOKEN_REVIEWER2, ACTOR_REVIEWER2)?;
    let bob_caps = bob.capabilities()?;
    let carol_caps = carol.capabilities()?;
    let path = stand.journal_path("main");
    let approve = GitLabPlan {
        approve: true,
        ..GitLabPlan::default()
    };
    let (first, stale) = {
        let journal = Journal::open(&path)?;
        let scope = journal.scope(&project());
        let (bob_session, _, _) = gl_session(&scope, &bob, REVIEWER, &[])?;
        let (carol_session, _, _) = gl_session(&scope, &carol, REVIEWER2, &[])?;
        let first = plan_gitlab(&scope, bob_session, &approve)?;
        run_gitlab(&scope, first.id, &bob, &bob_caps)?;
        let approved = server.with(|f| f.approved_by(GL_IID));
        stand.eq("после первого approve", approved, vec![REVIEWER.to_owned()]);
        server.with(|f| f.push_head(GL_IID, HEAD2, vec![FakeDiff::modified(PATH, DIFF)]))?;
        let stale = plan_gitlab(&scope, carol_session, &approve)?;
        let stopped = run_gitlab(&scope, stale.id, &carol, &carol_caps);
        stand.check(
            "исполнитель с устаревшей ревизией останавливается на шаге 1",
            matches!(stopped, Err(SubmitError::HeadChanged { .. })),
            format!("{stopped:?}"),
        );
        let direct = carol.approve(&scope.session(carol_session)?);
        stand.check(
            "approve с устаревшим sha — 409",
            matches!(direct, Err(gitlab::ApiError::ShaRejected(_))),
            format!("{direct:?}"),
        );
        (first.id, stale.id)
    };
    stand.after("main", server.with(|f| f.snapshot()));
    let log = server.log();
    stand.writes("записи", &log, &[(glf::APPROVE, 2)]);
    stand.eq(
        "approve: sha и коды",
        log.iter()
            .filter(|c| c.key == glf::APPROVE)
            .map(|c| (c.body["sha"].as_str().map(str::to_owned), c.status))
            .collect::<Vec<_>>(),
        vec![
            (Some(HEAD.to_owned()), Some(201)),
            (Some(HEAD.to_owned()), Some(409)),
        ],
    );
    stand.check(
        "второй ревьюер не одобрил",
        !server
            .with(|f| f.approved_by(GL_IID))
            .contains(&REVIEWER2.to_owned()),
        format!("{:?}", server.with(|f| f.approved_by(GL_IID))),
    );
    let journal = Journal::open(&path)?;
    let scope = journal.scope(&project());
    stand.eq(
        "журнал: approve первого ревьюера",
        (states(&scope, first)?, report(&scope, first)?.delivery),
        (
            vec![(Operation::GitLabApprove, ItemState::Confirmed)],
            Delivery::Full,
        ),
    );
    let stale_items = scope.submission(stale)?.items;
    stand.check(
        "журнал: элемент второго ревьюера в planned, в in_flight не входил",
        stale_items
            .iter()
            .all(|i| i.state == ItemState::Planned && i.in_flight_at.is_none()),
        format!("{stale_items:?}"),
    );
    stand.journal("main", &journal)?;
    Ok(())
}

/// Класс отказа `request_changes` для сравнения ветвей.
fn refusal_class(result: &Result<gitlab::ReviewerState, gitlab::ApiError>) -> &'static str {
    match result {
        Ok(_) => "успех",
        Err(gitlab::ApiError::Absent(_)) => "нет возможности",
        Err(gitlab::ApiError::NotPermitted { .. }) => "нет прав",
        Err(gitlab::ApiError::Unavailable(_)) => "нет объекта или нет прав",
        Err(gitlab::ApiError::Graphql(_)) => "ошибка GraphQL",
        Err(gitlab::ApiError::Payload(_)) => "payload.errors",
        Err(_) => "другое",
    }
}

fn gl_request_changes(stand: &mut Stand) -> Outcome {
    // Ветвь с правами и назначением ревьюером — исполнителем с журналом.
    let server = FakeServer::start(gitlab_fake()?)?;
    stand.before("available", server.with(|f| f.snapshot()));
    let gitlab = stand.gl(&server, TOKEN_REVIEWER, ACTOR_REVIEWER)?;
    let caps = gitlab.capabilities()?;
    let path = stand.journal_path("available");
    let submission = {
        let journal = Journal::open(&path)?;
        let scope = journal.scope(&project());
        let (session, _, _) = gl_session(&scope, &gitlab, REVIEWER, &[])?;
        let plan = GitLabPlan {
            request_changes: true,
            ..GitLabPlan::default()
        };
        let submission = plan_gitlab(&scope, session, &plan)?;
        run_gitlab(&scope, submission.id, &gitlab, &caps)?;
        submission.id
    };
    stand.after("available", server.with(|f| f.snapshot()));
    let log = server.log();
    stand.writes("available: записи", &log, &[(glf::REQUEST_CHANGES, 1)]);
    stand.eq(
        "available: состояние ревьюера",
        server.with(|f| f.reviewer_state(GL_IID, REVIEWER)),
        Some(glf::ReviewerState::RequestedChanges),
    );
    let journal = Journal::open(&path)?;
    let scope = journal.scope(&project());
    stand.eq(
        "available: журнал",
        (
            states(&scope, submission)?,
            report(&scope, submission)?.delivery,
        ),
        (
            vec![(Operation::GitLabRequestChanges, ItemState::Confirmed)],
            Delivery::Full,
        ),
    );
    stand.journal("available", &journal)?;

    // Ветви отказа — прямым вызовом адаптера: класс итога и отсутствие подмены заметкой.
    let branches: [(&str, RequestChangesMode, bool, &str, usize); 6] = [
        (
            "absent",
            RequestChangesMode::Absent,
            true,
            "нет возможности",
            0,
        ),
        (
            "reporter",
            RequestChangesMode::Available,
            false,
            "нет прав",
            0,
        ),
        (
            "forbidden",
            RequestChangesMode::Forbidden,
            true,
            "нет объекта или нет прав",
            1,
        ),
        (
            "execution-error",
            RequestChangesMode::ExecutionError,
            true,
            "ошибка GraphQL",
            1,
        ),
        (
            "payload-error",
            RequestChangesMode::PayloadError,
            true,
            "payload.errors",
            1,
        ),
        (
            "not-reviewer",
            RequestChangesMode::Available,
            true,
            "payload.errors",
            1,
        ),
    ];
    for (branch, mode, developer, expected, calls) in branches {
        let mut fake = gitlab_fake()?;
        fake.set_request_changes(mode);
        fake.add_user(REPORTER, TOKEN_REPORTER, Some(Access::Reporter));
        fake.add_user(REVIEWER2, TOKEN_REVIEWER2, Some(Access::Developer));
        let server = FakeServer::start(fake)?;
        stand.before(branch, server.with(|f| f.snapshot()));
        let (token, actor, user) = match (developer, branch) {
            (false, _) => (TOKEN_REPORTER, ACTOR_REPORTER, REPORTER),
            (true, "not-reviewer") => (TOKEN_REVIEWER2, ACTOR_REVIEWER2, REVIEWER2),
            (true, _) => (TOKEN_REVIEWER, ACTOR_REVIEWER, REVIEWER),
        };
        let gitlab = stand.gl(&server, token, actor)?;
        let caps = gitlab.capabilities()?;
        let result = gitlab.request_changes(&caps);
        stand.after(branch, server.with(|f| f.snapshot()));
        stand.eq(&format!("{branch}: итог"), refusal_class(&result), expected);
        let log = server.log();
        stand.writes(
            &format!("{branch}: записи"),
            &log,
            &[(glf::REQUEST_CHANGES, calls)],
        );
        stand.check(
            &format!("{branch}: состояние ревьюера не REQUESTED_CHANGES"),
            server.with(|f| f.reviewer_state(GL_IID, user))
                != Some(glf::ReviewerState::RequestedChanges),
            format!("{:?}", server.with(|f| f.reviewer_state(GL_IID, user))),
        );
    }
    Ok(())
}

fn gl_anchors(stand: &mut Stand) -> Outcome {
    let server = FakeServer::start(gitlab_fake()?)?;
    stand.before("main", server.with(|f| f.snapshot()));
    let gitlab = stand.gl(&server, TOKEN_REVIEWER, ACTOR_REVIEWER)?;
    let caps = gitlab.capabilities()?;
    let path = stand.journal_path("main");
    let submission = {
        let journal = Journal::open(&path)?;
        let scope = journal.scope(&project());
        let (session, _, _) = gl_session(
            &scope,
            &gitlab,
            REVIEWER,
            &[
                (Spot::Line(Side::Old, 2), "удалённая"),
                (Spot::Line(Side::New, 4), "добавленная"),
                (Spot::Line(Side::New, 3), "неизменённая"),
                (Spot::Range(Side::New, 1, Side::New, 2), "диапазон"),
            ],
        )?;
        let submission = plan_gitlab(&scope, session, &GitLabPlan::default())?;
        run_gitlab(&scope, submission.id, &gitlab, &caps)?;
        submission.id
    };
    stand.after("main", server.with(|f| f.snapshot()));
    let log = server.log();
    stand.writes("записи", &log, &[(glf::DRAFTS, 4), (glf::PUBLISH, 4)]);
    for body in bodies(&log, glf::DRAFTS) {
        let position = &body["position"];
        stand.eq(
            "позиция черновика: тип и тройка diff_refs",
            (
                position["position_type"].as_str(),
                position["base_sha"].as_str(),
                position["start_sha"].as_str(),
                position["head_sha"].as_str(),
            ),
            (Some("text"), Some(BASE), Some(BASE), Some(HEAD)),
        );
    }
    let path_sha = sha1_smol::Sha1::from(PATH).digest().to_string();
    let notes = server.with(|f| f.notes(GL_IID));
    let mut stored: Vec<(String, Option<u32>, Option<u32>, String)> = notes
        .iter()
        .filter_map(|n| {
            n.position.as_ref().map(|p| {
                (
                    n.body.clone(),
                    p.old_line,
                    p.new_line,
                    format!("{}/{}/{}", p.base_sha, p.start_sha, p.head_sha),
                )
            })
        })
        .collect();
    stored.sort();
    let triple = format!("{BASE}/{BASE}/{HEAD}");
    stand.eq(
        "заметки с позициями",
        stored,
        vec![
            ("диапазон".to_owned(), None, Some(2), triple.clone()),
            ("добавленная".to_owned(), None, Some(4), triple.clone()),
            ("неизменённая".to_owned(), Some(3), Some(3), triple.clone()),
            ("удалённая".to_owned(), Some(2), None, triple.clone()),
        ],
    );
    let range = notes
        .iter()
        .find(|n| n.body == "диапазон")
        .and_then(|n| n.position.as_ref())
        .and_then(|p| p.line_range.clone())
        .unwrap_or(Value::Null);
    stand.eq(
        "line_range диапазона",
        (
            range.pointer("/start/line_code").and_then(Value::as_str),
            range.pointer("/end/line_code").and_then(Value::as_str),
            range.pointer("/end/type").and_then(Value::as_str),
            // Начало — неизменённая строка: Discussions API велит не передавать её `type`
            // («Omit for an unchanged line»), так же строят модель и оракул.
            range.pointer("/start/type").and_then(Value::as_str),
        ),
        (
            Some(format!("{path_sha}_1_1").as_str()),
            Some(format!("{path_sha}_3_2").as_str()),
            Some("new"),
            None,
        ),
    );
    let journal = Journal::open(&path)?;
    let scope = journal.scope(&project());
    stand.eq(
        "журнал: итог",
        report(&scope, submission)?.delivery,
        Delivery::Full,
    );
    stand.journal("main", &journal)?;
    Ok(())
}

fn head_change(stand: &mut Stand) -> Outcome {
    let server = FakeServer::start(gitlab_fake()?)?;
    stand.before("main", server.with(|f| f.snapshot()));
    let gitlab = stand.gl(&server, TOKEN_REVIEWER, ACTOR_REVIEWER)?;
    let caps = gitlab.capabilities()?;
    let path = stand.journal_path("main");
    let held = revision(BASE, HEAD)?;
    let fresh_revision = revision(BASE2, HEAD2)?;
    let (s_id, t_id, ids) = {
        let journal = Journal::open(&path)?;
        let scope = journal.scope(&project());
        let (s_id, snap1, ids) = gl_session(
            &scope,
            &gitlab,
            REVIEWER,
            &[
                (Spot::Line(Side::New, 1), "сохранится"),
                (Spot::Line(Side::New, 2), "перепривязать"),
                (Spot::Line(Side::New, 4), "устареет"),
            ],
        )?;
        let (t_id, _, t_ids) = gl_session(
            &scope,
            &gitlab,
            REVIEWER,
            &[(Spot::Line(Side::New, 3), "остаться")],
        )?;
        server.with(|f| {
            f.push_version(
                GL_IID,
                BASE2,
                BASE2,
                HEAD2,
                vec![FakeDiff::modified(PATH, DIFF2)],
            )
        })?;
        let blocked = plan_gitlab(&scope, s_id, &GitLabPlan::default())?;
        let stopped = run_gitlab(&scope, blocked.id, &gitlab, &caps);
        stand.check(
            "исполнитель предупреждает о смене head до записи",
            matches!(&stopped, Err(SubmitError::HeadChanged { fresh }) if **fresh == fresh_revision),
            format!("{stopped:?}"),
        );
        stand.writes("записи до выбора пути", &server.log(), &[]);
        scope.close_submission(blocked.id)?;

        let fresh = gitlab.merge_request()?;
        let mut session = scope.session(s_id)?;
        let mut drafts = scope.drafts(s_id)?;
        let HeadCheck::Changed(change) =
            session.check_head(&fresh.revision, &snap1.files, &fresh.files, &drafts)
        else {
            return Err("смена head не замечена".into());
        };
        let fates: Vec<(DraftId, AnchorFate)> = change.fates().to_vec();
        stand.eq(
            "классификация якорей",
            fates,
            vec![
                (ids[0], AnchorFate::Kept),
                (ids[1], AnchorFate::Stale(StaleReason::LinesChanged)),
                (ids[2], AnchorFate::Stale(StaleReason::LinesChanged)),
            ],
        );
        let file = first_file(&fresh.files)?;
        let rebound = Anchor::line(file, LinePoint::at(file, Side::New, 2)?)?;
        session.apply_head(
            &change,
            HeadPath::Rebind(vec![(ids[1], rebound)]),
            &mut drafts,
            &fresh.files,
        )?;
        scope.save_head(s_id, &session, &drafts)?;

        let t_session = scope.session(t_id)?;
        let t_draft = scope.draft(*t_ids.first().ok_or("нет черновика T")?)?;
        let (known, _) = gitlab.create_draft(&t_session, &t_draft, false)?;
        gitlab.publish_draft(&known)?;
        stand.notes.push(
            "путь «остаться»: черновик T создан и опубликован адаптером на v1 — исполнитель \
             останавливается на шаге 1 при любой смене head"
                .to_owned(),
        );

        let moved = plan_gitlab(&scope, s_id, &GitLabPlan::default())?;
        let planned: Vec<(Operation, Vec<DraftId>)> = moved
            .items
            .iter()
            .map(|i| (i.operation, i.drafts.clone()))
            .collect();
        stand.eq(
            "после перепривязки в отправку входят два черновика",
            planned,
            vec![
                (Operation::GitLabCreateDraftNote, vec![ids[0]]),
                (Operation::GitLabCreateDraftNote, vec![ids[1]]),
                (Operation::GitLabPublishDraftNote, vec![ids[0]]),
                (Operation::GitLabPublishDraftNote, vec![ids[1]]),
            ],
        );
        let done = run_gitlab(&scope, moved.id, &gitlab, &caps)?;
        stand.eq("отправка S", done.delivery, Delivery::Full);
        (s_id, t_id, ids)
    };
    stand.after("main", server.with(|f| f.snapshot()));
    let log = server.log();
    stand.writes("записи", &log, &[(glf::DRAFTS, 3), (glf::PUBLISH, 3)]);
    stand.check(
        "ответов 400 нет",
        log.iter().all(|c| c.status != Some(400)),
        "",
    );
    let mut origins: BTreeMap<&'static str, usize> = BTreeMap::new();
    for body in bodies(&log, glf::DRAFTS) {
        let position = &body["position"];
        let triple = (|| -> Option<Revision> {
            Some(Revision::new(
                sha(position["base_sha"].as_str()?).ok()?,
                sha(position["start_sha"].as_str()?).ok()?,
                sha(position["head_sha"].as_str()?).ok()?,
            ))
        })();
        let origin = match triple.map(|t| triple_origin(&held, &fresh_revision, &t)) {
            Some(TripleOrigin::Held) => "held",
            Some(TripleOrigin::Fresh) => "fresh",
            Some(TripleOrigin::Mixed { .. }) => "mixed",
            Some(TripleOrigin::Foreign) | None => "foreign",
        };
        *origins.entry(origin).or_insert(0) += 1;
    }
    stand.eq(
        "тройки исходящих позиций",
        origins,
        BTreeMap::from([("fresh", 2), ("held", 1)]),
    );
    let journal = Journal::open(&path)?;
    let scope = journal.scope(&project());
    let stale = scope.draft(ids[2])?;
    stand.eq(
        "журнал: ревизии сессий и неперепривязанного черновика",
        (
            scope
                .session(s_id)?
                .revision()
                .head_sha()
                .as_str()
                .to_owned(),
            stale.revision().head_sha().as_str().to_owned(),
            stale.is_current(),
            scope
                .session(t_id)?
                .revision()
                .head_sha()
                .as_str()
                .to_owned(),
        ),
        (HEAD2.to_owned(), HEAD.to_owned(), false, HEAD.to_owned()),
    );
    stand.journal("main", &journal)?;
    Ok(())
}

/// Транспорт, который паникует после доставки записи с окончанием пути `path`: процесс умер
/// между принятой записью и записью ответа в журнал.
struct PanicAfter<T> {
    inner: T,
    path: &'static str,
}

impl<T: Transport> Transport for PanicAfter<T> {
    fn send(&self, request: &Request) -> Result<Response, Failure> {
        let response = self.inner.send(request);
        if request.method.is_write() && request.path().ends_with(self.path) {
            panic!("сбой после доставки записи");
        }
        response
    }
}

fn journal_crash(stand: &mut Stand) -> Outcome {
    // Сбой до запроса: in_flight записан, запроса нет.
    {
        let server = FakeServer::start(gitlab_fake()?)?;
        stand.before("before-request", server.with(|f| f.snapshot()));
        let gitlab = stand.gl(&server, TOKEN_REVIEWER, ACTOR_REVIEWER)?;
        let path = stand.journal_path("before-request");
        let (submission, first) = {
            let journal = Journal::open(&path)?;
            let scope = journal.scope(&project());
            let (session, _, _) = gl_session(
                &scope,
                &gitlab,
                REVIEWER,
                &[(Spot::Line(Side::New, 2), "первый")],
            )?;
            let submission = plan_gitlab(&scope, session, &GitLabPlan::default())?;
            let first = submission.items.first().ok_or("нет элемента")?.id;
            scope.begin(first)?;
            (submission.id, first)
        };
        let journal = Journal::open(&path)?;
        stand.eq(
            "до запроса: восстановлен",
            journal.recovered().to_vec(),
            vec![first],
        );
        let scope = journal.scope(&project());
        let verdicts = reconcile_gitlab(&scope, submission, &gitlab)?;
        stand.check(
            "до запроса: сверка «не выяснено»",
            matches!(verdicts[..], [Verdict::Unresolved { .. }]),
            format!("{verdicts:?}"),
        );
        stand.eq(
            "до запроса: элемент",
            scope.item(first)?.state,
            ItemState::Unknown,
        );
        stand.after("before-request", server.with(|f| f.snapshot()));
        stand.writes("до запроса: записи", &server.log(), &[]);
        stand.journal("before-request", &journal)?;
    }
    // Сбой после принятой записи до ответа: поток исполнителя падает после доставки.
    let server = FakeServer::start(gitlab_fake()?)?;
    stand.before("after-delivery", server.with(|f| f.snapshot()));
    let path = stand.journal_path("after-delivery");
    let submission = {
        let gitlab = stand.gl(&server, TOKEN_REVIEWER, ACTOR_REVIEWER)?;
        let journal = Journal::open(&path)?;
        let scope = journal.scope(&project());
        let (session, _, _) = gl_session(
            &scope,
            &gitlab,
            REVIEWER,
            &[(Spot::Line(Side::New, 2), "первый")],
        )?;
        plan_gitlab(&scope, session, &GitLabPlan::default())?
    };
    let mark = stand.mark(&server);
    let transport = PanicAfter {
        inner: stand.client(TOKEN_REVIEWER, ACTOR_REVIEWER, true),
        path: "/draft_notes",
    };
    let base = server.base().to_owned();
    let thread_path = path.clone();
    let id = submission.id;
    let crashed = std::thread::spawn(move || -> Result<(), String> {
        let gitlab = GitLab::new(transport, gitlab::Endpoints::instance(&base), &gl_target())
            .map_err(|e| e.to_string())?;
        let caps = gitlab.capabilities().map_err(|e| e.to_string())?;
        let journal = Journal::open(&thread_path).map_err(|e| e.to_string())?;
        let scope = journal.scope(&project());
        run_gitlab(&scope, id, &gitlab, &caps).map_err(|e| e.to_string())?;
        Ok(())
    })
    .join();
    stand.check(
        "после доставки: поток исполнителя упал",
        crashed.is_err(),
        format!("{crashed:?}"),
    );
    stand.receipt(&server, mark)?;
    let drafts = server.with(|f| f.drafts(GL_IID));
    stand.eq("после доставки: сервер записал черновик", drafts.len(), 1);
    let journal = Journal::open(&path)?;
    let first = submission.items.first().ok_or("нет элемента")?.id;
    stand.eq(
        "после доставки: восстановлен",
        journal.recovered().to_vec(),
        vec![first],
    );
    let scope = journal.scope(&project());
    let gitlab = stand.gl(&server, TOKEN_REVIEWER, ACTOR_REVIEWER)?;
    let verdicts = reconcile_gitlab(&scope, submission.id, &gitlab)?;
    let expected = drafts.first().map(|d| ProviderId::new(d.id.to_string()));
    stand.check(
        "после доставки: сверка даёт кандидата — записанный черновик",
        matches!(&verdicts[..], [Verdict::Candidate { remote, .. }] if Some(remote) == expected.as_ref()),
        format!("{verdicts:?}"),
    );
    let item = scope.item(first)?;
    stand.eq(
        "после доставки: элемент",
        (item.state, item.candidate),
        (ItemState::Unknown, expected),
    );
    stand.after("after-delivery", server.with(|f| f.snapshot()));
    stand.writes("после доставки: записи", &server.log(), &[(glf::DRAFTS, 1)]);
    stand.journal("after-delivery", &journal)?;
    Ok(())
}

fn reconcile_ambiguous(stand: &mut Stand) -> Outcome {
    let server = FakeServer::start(gitlab_fake()?)?;
    server.inject(
        Fault::new(glf::PUBLISH, Reply::Status(502))
            .after_apply()
            .skip(1),
    );
    stand.before("main", server.with(|f| f.snapshot()));
    let gitlab = stand.gl(&server, TOKEN_REVIEWER, ACTOR_REVIEWER)?;
    let caps = gitlab.capabilities()?;
    let path = stand.journal_path("main");
    let mark = stand.mark(&server);
    let (submission, lost) = {
        let journal = Journal::open(&path)?;
        let scope = journal.scope(&project());
        let (session, _, _) = gl_session(
            &scope,
            &gitlab,
            REVIEWER,
            &[
                (Spot::Line(Side::New, 2), "одинаковый"),
                (Spot::Line(Side::New, 2), "одинаковый"),
            ],
        )?;
        let submission = plan_gitlab(&scope, session, &GitLabPlan::default())?;
        let stopped = run_gitlab(&scope, submission.id, &gitlab, &caps)?;
        stand.receipt(&server, mark)?;
        let lost = stopped.items.last().ok_or("нет элемента")?.id;
        let verdicts = reconcile_gitlab(&scope, submission.id, &gitlab)?;
        stand.check(
            "сверка: два кандидата, неоднозначно",
            matches!(&verdicts[..], [Verdict::Ambiguous { candidates }] if candidates.len() == 2),
            format!("{verdicts:?}"),
        );
        (submission.id, lost)
    };
    stand.after("main", server.with(|f| f.snapshot()));
    server.finish()?;
    let log = server.log();
    stand.writes("записи", &log, &[(glf::DRAFTS, 2), (glf::PUBLISH, 2)]);
    let notes: Vec<(String, Option<u32>)> = server
        .with(|f| f.notes(GL_IID))
        .iter()
        .map(|n| (n.body.clone(), n.position.as_ref().and_then(|p| p.new_line)))
        .collect();
    stand.eq(
        "две одинаковые заметки",
        notes,
        vec![
            ("одинаковый".to_owned(), Some(2)),
            ("одинаковый".to_owned(), Some(2)),
        ],
    );
    let journal = Journal::open(&path)?;
    let scope = journal.scope(&project());
    stand.eq(
        "журнал: элемент и итог",
        (
            scope.item(lost)?.state,
            report(&scope, submission)?.delivery,
        ),
        (ItemState::Unknown, Delivery::Partial),
    );
    stand.journal("main", &journal)?;
    Ok(())
}

fn reconcile_deleted(stand: &mut Stand) -> Outcome {
    let server = FakeServer::start(gitlab_fake()?)?;
    server.inject(Fault::new(glf::PUBLISH, Reply::Status(502)));
    stand.before("main", server.with(|f| f.snapshot()));
    let gitlab = stand.gl(&server, TOKEN_REVIEWER, ACTOR_REVIEWER)?;
    let second = stand.gl(&server, TOKEN_REVIEWER, ACTOR_SECOND)?;
    let caps = gitlab.capabilities()?;
    let path = stand.journal_path("main");
    let mark = stand.mark(&server);
    let (submission, lost) = {
        let journal = Journal::open(&path)?;
        let scope = journal.scope(&project());
        let (session, _, ids) = gl_session(
            &scope,
            &gitlab,
            REVIEWER,
            &[(Spot::Line(Side::New, 2), "удалят")],
        )?;
        let submission = plan_gitlab(&scope, session, &GitLabPlan::default())?;
        let stopped = run_gitlab(&scope, submission.id, &gitlab, &caps)?;
        let lost = stopped.items.last().ok_or("нет элемента")?.id;
        let remote = scope
            .draft(*ids.first().ok_or("нет черновика")?)?
            .remote_id
            .ok_or("у черновика нет ID")?;
        second.delete_draft(&KnownDraft::recorded(remote))?;
        stand.receipt(&server, mark)?;
        let verdicts = reconcile_gitlab(&scope, submission.id, &gitlab)?;
        stand.check(
            "сверка: «не выяснено» без кандидата",
            matches!(verdicts[..], [Verdict::Unresolved { .. }]),
            format!("{verdicts:?}"),
        );
        (submission.id, lost)
    };
    stand.after("main", server.with(|f| f.snapshot()));
    server.finish()?;
    let log = server.log();
    stand.writes(
        "записи",
        &log,
        &[(glf::DRAFTS, 1), (glf::PUBLISH, 1), (GL_DRAFT_DELETE, 1)],
    );
    stand.eq(
        "сервер: ни черновика, ни заметки",
        (
            server.with(|f| f.drafts(GL_IID)).len(),
            server.with(|f| f.notes(GL_IID)).len(),
        ),
        (0, 0),
    );
    let journal = Journal::open(&path)?;
    let scope = journal.scope(&project());
    let item = scope.item(lost)?;
    stand.eq(
        "журнал: публикация и итог",
        (
            item.operation,
            item.state,
            item.candidate,
            report(&scope, submission)?.delivery,
        ),
        (
            Operation::GitLabPublishDraftNote,
            ItemState::Unknown,
            None,
            Delivery::Partial,
        ),
    );
    stand.journal("main", &journal)?;
    Ok(())
}

fn reconcile_paged(stand: &mut Stand) -> Outcome {
    let server = FakeServer::start(gitlab_fake()?)?;
    server.with(|f| -> Result<(), String> {
        for index in 0..100 {
            f.plant_discussion(GL_IID, REVIEWER, &format!("другая заметка {index}"), None)?;
        }
        Ok(())
    })?;
    server.inject(Fault::new(glf::PUBLISH, Reply::Status(502)).after_apply());
    stand.before("main", server.with(|f| f.snapshot()));
    let gitlab = stand.gl(&server, TOKEN_REVIEWER, ACTOR_REVIEWER)?;
    let caps = gitlab.capabilities()?;
    let path = stand.journal_path("main");
    let mark = stand.mark(&server);
    let (submission, lost, reads_from) = {
        let journal = Journal::open(&path)?;
        let scope = journal.scope(&project());
        let (session, _, _) = gl_session(
            &scope,
            &gitlab,
            REVIEWER,
            &[(Spot::Line(Side::New, 2), "текст ревью  \r\n")],
        )?;
        let submission = plan_gitlab(&scope, session, &GitLabPlan::default())?;
        let stopped = run_gitlab(&scope, submission.id, &gitlab, &caps)?;
        stand.receipt(&server, mark)?;
        let lost = stopped.items.last().ok_or("нет элемента")?.id;
        let reads_from = stand.trace.len();
        let verdicts = reconcile_gitlab(&scope, submission.id, &gitlab)?;
        let last = server
            .with(|f| f.notes(GL_IID))
            .last()
            .map(|n| n.id.to_string());
        stand.check(
            "сверка: кандидат — заметка на второй странице",
            matches!(&verdicts[..], [Verdict::Candidate { remote, .. }] if Some(remote.as_str()) == last.as_deref()),
            format!("{verdicts:?}"),
        );
        (submission.id, lost, reads_from)
    };
    stand.after("main", server.with(|f| f.snapshot()));
    server.finish()?;
    let log = server.log();
    stand.writes("записи", &log, &[(glf::DRAFTS, 1), (glf::PUBLISH, 1)]);
    let second_page = stand.trace.calls()[reads_from..].iter().any(|c| {
        let path = c.request.path();
        path.contains("/discussions?") && path.contains("&page=2")
    });
    stand.check(
        "сверка читает вторую страницу заметок (треды MR с заметками)",
        second_page,
        "",
    );
    let notes = server.with(|f| f.notes(GL_IID));
    stand.eq(
        "сервер: 101 заметка, последняя с нормализованным текстом",
        (
            notes.len(),
            notes
                .last()
                .map(|n| (n.body.clone(), n.from_draft.is_some())),
        ),
        (101, Some(("текст ревью".to_owned(), true))),
    );
    let journal = Journal::open(&path)?;
    let scope = journal.scope(&project());
    let item = scope.item(lost)?;
    stand.eq(
        "журнал: unknown с кандидатом, итог",
        (
            item.state,
            item.candidate.map(|c| c.as_str().to_owned()),
            report(&scope, submission)?.delivery,
        ),
        (
            ItemState::Unknown,
            notes.last().map(|n| n.id.to_string()),
            Delivery::Partial,
        ),
    );
    stand.journal("main", &journal)?;
    Ok(())
}

fn partial(stand: &mut Stand) -> Outcome {
    let server = FakeServer::start(gitlab_fake()?)?;
    server.inject(Fault::new(glf::PUBLISH, Reply::Status(403)).skip(1));
    stand.before("main", server.with(|f| f.snapshot()));
    let gitlab = stand.gl(&server, TOKEN_REVIEWER, ACTOR_REVIEWER)?;
    let caps = gitlab.capabilities()?;
    let path = stand.journal_path("main");
    let (submission, ids) = {
        let journal = Journal::open(&path)?;
        let scope = journal.scope(&project());
        let (session, _, ids) = gl_session(
            &scope,
            &gitlab,
            REVIEWER,
            &[
                (Spot::Line(Side::New, 1), "первый"),
                (Spot::Line(Side::New, 2), "второй"),
                (Spot::Line(Side::New, 4), "третий"),
            ],
        )?;
        let submission = plan_gitlab(&scope, session, &GitLabPlan::default())?;
        run_gitlab(&scope, submission.id, &gitlab, &caps)?;
        (submission.id, ids)
    };
    stand.after("main", server.with(|f| f.snapshot()));
    server.finish()?;
    let log = server.log();
    stand.writes("записи", &log, &[(glf::DRAFTS, 3), (glf::PUBLISH, 2)]);
    let journal = Journal::open(&path)?;
    let scope = journal.scope(&project());
    let remote = |id: DraftId| -> Result<Option<u64>, SubmitError> {
        Ok(scope
            .draft(id)?
            .remote_id
            .and_then(|r| r.as_str().parse().ok()))
    };
    let first = remote(ids[0])?;
    let rest: Vec<Option<u64>> = vec![remote(ids[1])?, remote(ids[2])?];
    stand.eq(
        "сервер: одна заметка из первого черновика",
        server
            .with(|f| f.notes(GL_IID))
            .iter()
            .map(|n| n.from_draft)
            .collect::<Vec<_>>(),
        vec![first],
    );
    let mut left: Vec<Option<u64>> = server
        .with(|f| f.drafts(GL_IID))
        .iter()
        .map(|d| Some(d.id))
        .collect();
    left.sort();
    stand.eq("сервер: второй и третий остались черновиками", left, rest);
    let done = report(&scope, submission)?;
    stand.eq(
        "журнал: состояния, закрыта, итог",
        (states(&scope, submission)?, done.closed, done.delivery),
        (
            vec![
                (Operation::GitLabCreateDraftNote, ItemState::Confirmed),
                (Operation::GitLabCreateDraftNote, ItemState::Confirmed),
                (Operation::GitLabCreateDraftNote, ItemState::Confirmed),
                (Operation::GitLabPublishDraftNote, ItemState::Confirmed),
                (Operation::GitLabPublishDraftNote, ItemState::Failed),
                (Operation::GitLabPublishDraftNote, ItemState::Planned),
            ],
            true,
            Delivery::Partial,
        ),
    );
    stand.journal("main", &journal)?;
    Ok(())
}

fn capabilities(stand: &mut Stand) -> Outcome {
    let gh_server = FakeServer::start(github_fake())?;
    stand.before("github", gh_server.with(|f| f.snapshot()));
    let github = stand.gh(&gh_server, TOKEN_REVIEWER, ACTOR_REVIEWER)?;
    let caps = github.capabilities()?;
    stand.eq(
        "GitHub: возможности",
        (
            caps.server_pending(),
            caps.viewed(),
            caps.resolve(),
            caps.repo_private,
            caps.api.requested.clone(),
            caps.api
                .supported
                .as_ref()
                .is_some_and(|list| list.contains(&caps.api.requested)),
        ),
        (true, true, true, true, github::API_VERSION.to_owned(), true),
    );
    gh_server.with(|f| f.remove_mutation("addPullRequestReviewThread"));
    let reduced = github.capabilities()?;
    stand.eq(
        "GitHub: без мутации в схеме серверного PENDING нет",
        reduced.server_pending(),
        false,
    );
    stand.after("github", gh_server.with(|f| f.snapshot()));
    stand.writes("GitHub: записи", &gh_server.log(), &[]);

    let mut fake = gitlab_fake()?;
    fake.set_version("17.6.0", false);
    fake.add_user(REPORTER, TOKEN_REPORTER, Some(Access::Reporter));
    let gl_server = FakeServer::start(fake)?;
    stand.before("gitlab", gl_server.with(|f| f.snapshot()));
    let gitlab = stand.gl(&gl_server, TOKEN_REVIEWER, ACTOR_REVIEWER)?;
    let gl_caps = gitlab.capabilities()?;
    stand.eq(
        "GitLab: версия, редакция, мутация, роль",
        (
            gl_caps.host.version.clone(),
            gl_caps.host.enterprise,
            gl_caps.request_changes(),
            gl_caps.access_level,
        ),
        (
            "17.6.0".to_owned(),
            Some(false),
            true,
            Some(gitlab::DEVELOPER),
        ),
    );
    gl_server.with(|f| f.set_request_changes(RequestChangesMode::Absent));
    stand.eq(
        "GitLab: без мутации в схеме",
        gitlab.capabilities()?.request_changes(),
        false,
    );
    let reporter = stand.gl(&gl_server, TOKEN_REPORTER, ACTOR_REPORTER)?;
    stand.eq(
        "GitLab: Reporter без права ревью",
        reporter.capabilities()?.can_review(),
        false,
    );
    stand.after("gitlab", gl_server.with(|f| f.snapshot()));
    stand.writes("GitLab: записи", &gl_server.log(), &[]);
    stand.host = Host {
        version: Some(format!(
            "GitHub API {}; GitLab {} ({})",
            caps.api.requested,
            gl_caps.host.version,
            if gl_caps.host.enterprise == Some(true) {
                "EE"
            } else {
                "CE"
            }
        )),
        tier: caps.plan.clone().or_else(|| gl_caps.plan.clone()),
    };
    let path = stand.journal_path("main");
    drop(Journal::open(&path)?);
    let journal = Journal::open(&path)?;
    stand.eq(
        "журнал не используется: сессий нет",
        journal.scope(&project()).sessions()?.len(),
        0,
    );
    stand.journal("main", &journal)?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Стоимость и устойчивость (в матрицу не входят)

/// Черновиков в одном ревью сценария `volume`.
const VOLUME: u32 = 200;
const GL_DRAFT_GET: &str =
    "GET /projects/{id}/merge_requests/{merge_request_iid}/draft_notes/{draft_note_id}";
const GH_REVIEW_COMMENTS: &str =
    "GET /repos/{owner}/{repo}/pulls/{number}/reviews/{review_id}/comments";

/// Diff `volume`: строка контекста и [`VOLUME`] добавленных строк (новая сторона, строки 2–201).
fn volume_diff() -> String {
    let mut diff = format!("@@ -1,1 +1,{} @@\n ctx1\n", VOLUME + 1);
    for index in 1..=VOLUME {
        diff.push_str(&format!("+volume {index}\n"));
    }
    diff
}

/// Тексты черновиков `volume`, разные.
fn volume_texts() -> Vec<String> {
    (1..=VOLUME)
        .map(|index| format!("черновик {index}"))
        .collect()
}

/// Черновик на каждой добавленной строке diff `volume`.
fn volume_spots(texts: &[String]) -> Vec<(Spot, &str)> {
    texts
        .iter()
        .zip(2..)
        .map(|(text, line)| (Spot::Line(Side::New, line), text.as_str()))
        .collect()
}

/// Замер отправки ветви: время планирования и отправки, вызовы сервера ветви по ключам.
fn measure_note(branch: &str, planned: Duration, sent: Duration, log: &[Logged]) -> String {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for call in log {
        *counts.entry(call.key.as_str()).or_insert(0) += 1;
    }
    let written: usize = writes(log).values().sum();
    let list = counts
        .iter()
        .map(|(key, count)| format!("{key}: {count}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "замер {branch}: планирование {} мс, отправка {} мс, вызовов {} (записей {written}, \
         чтений {}): {list}",
        planned.as_millis(),
        sent.as_millis(),
        log.len(),
        log.len().saturating_sub(written)
    )
}

/// Состав строк diff в запросе: число, первая и последняя строка, разных строк, все ли `RIGHT`.
fn sent_span(comments: &[Value]) -> (usize, Option<u64>, Option<u64>, usize, bool) {
    let lines: Vec<u64> = comments.iter().filter_map(|c| c["line"].as_u64()).collect();
    let distinct: BTreeSet<u64> = lines.iter().copied().collect();
    (
        comments.len(),
        lines.iter().min().copied(),
        lines.iter().max().copied(),
        distinct.len(),
        comments.iter().all(|c| c["side"].as_str() == Some("RIGHT")),
    )
}

fn volume(stand: &mut Stand) -> Outcome {
    let diff = volume_diff();
    let texts = volume_texts();
    let spots = volume_spots(&texts);
    let expected: BTreeSet<String> = texts.iter().cloned().collect();
    volume_github(stand, &diff, &spots, &expected, false)?;
    volume_github(stand, &diff, &spots, &expected, true)?;
    volume_gitlab(stand, &diff, &spots, &expected)
}

fn volume_github(
    stand: &mut Stand,
    diff: &str,
    spots: &[(Spot, &str)],
    expected: &BTreeSet<String>,
    pending: bool,
) -> Outcome {
    let branch = if pending {
        "github-pending"
    } else {
        "github-local"
    };
    let server = FakeServer::start(github_fake_with(diff))?;
    stand.before(branch, server.with(|f| f.snapshot()));
    let github = stand.gh(&server, TOKEN_REVIEWER, ACTOR_REVIEWER)?;
    let caps = github.capabilities()?;
    let path = stand.journal_path(branch);
    let (submission, planned, sent, from) = {
        let journal = Journal::open(&path)?;
        let scope = journal.scope(&project());
        let (session, drafts) = gh_session(&scope, &github, spots)?;
        stand.eq(
            &format!("{branch}: черновиков"),
            drafts.len(),
            VOLUME as usize,
        );
        let started = Instant::now();
        let submission = if pending {
            plan_github_pending(&scope, session, ReviewEvent::Comment, SUMMARY, None, &[])?
        } else {
            plan_github_local(&scope, session, ReviewEvent::Comment, SUMMARY)?
        };
        let planned = started.elapsed();
        let from = server.logged();
        let started = Instant::now();
        run_github(&scope, submission.id, &github, &caps)?;
        (submission.id, planned, started.elapsed(), from)
    };
    stand.after(branch, server.with(|f| f.snapshot()));
    let log = server.log();
    stand.notes.push(measure_note(
        branch,
        planned,
        sent,
        &log[from.min(log.len())..],
    ));
    if pending {
        stand.writes(
            &format!("{branch}: записи"),
            &log,
            &[
                (GH_CREATE, 1),
                (GH_ADD_THREAD, VOLUME as usize),
                (GH_SUBMIT, 1),
            ],
        );
    } else {
        stand.writes(&format!("{branch}: записи"), &log, &[(GH_CREATE, 1)]);
        if let [body] = bodies(&log, GH_CREATE)[..] {
            let comments = body["comments"].as_array().cloned().unwrap_or_default();
            stand.eq(
                &format!("{branch}: comments[] Create review (число, строки, разных, RIGHT)"),
                sent_span(&comments),
                (
                    VOLUME as usize,
                    Some(2),
                    Some(u64::from(VOLUME) + 1),
                    VOLUME as usize,
                    true,
                ),
            );
        }
    }
    stand.eq(
        &format!("{branch}: review на сервере"),
        server
            .with(|f| f.reviews(GH_NUMBER))
            .iter()
            .map(|r| (r.user.clone(), r.state))
            .collect::<Vec<_>>(),
        vec![(REVIEWER.to_owned(), ghf::ReviewState::Commented)],
    );
    let stored: Vec<String> = server
        .with(|f| f.comments(GH_NUMBER))
        .into_iter()
        .filter(|c| c.in_reply_to.is_none())
        .map(|c| c.body)
        .collect();
    stand.eq(
        &format!("{branch}: комментарии review (число, тексты черновиков)"),
        (
            stored.len(),
            stored.into_iter().collect::<BTreeSet<_>>() == *expected,
        ),
        (VOLUME as usize, true),
    );
    let journal = Journal::open(&path)?;
    let scope = journal.scope(&project());
    let done = report(&scope, submission)?;
    let items = if pending { VOLUME as usize + 2 } else { 1 };
    stand.eq(
        &format!("{branch}: журнал (элементов, все confirmed, итог, восстановлено)"),
        (
            done.items.len(),
            done.items.iter().all(|i| i.state == ItemState::Confirmed),
            done.delivery,
            journal.recovered().len(),
        ),
        (items, true, Delivery::Full, 0),
    );
    stand.journal(branch, &journal)?;
    Ok(())
}

fn volume_gitlab(
    stand: &mut Stand,
    diff: &str,
    spots: &[(Spot, &str)],
    expected: &BTreeSet<String>,
) -> Outcome {
    let branch = "gitlab";
    let server = FakeServer::start(gitlab_fake_with(diff)?)?;
    stand.before(branch, server.with(|f| f.snapshot()));
    let gitlab = stand.gl(&server, TOKEN_REVIEWER, ACTOR_REVIEWER)?;
    let caps = gitlab.capabilities()?;
    let path = stand.journal_path(branch);
    let (submission, planned, sent, from) = {
        let journal = Journal::open(&path)?;
        let scope = journal.scope(&project());
        let (session, _, drafts) = gl_session(&scope, &gitlab, REVIEWER, spots)?;
        stand.eq(
            &format!("{branch}: черновиков"),
            drafts.len(),
            VOLUME as usize,
        );
        let plan = GitLabPlan {
            summary: SUMMARY.to_owned(),
            ..GitLabPlan::default()
        };
        let started = Instant::now();
        let submission = plan_gitlab(&scope, session, &plan)?;
        let planned = started.elapsed();
        let from = server.logged();
        let started = Instant::now();
        run_gitlab(&scope, submission.id, &gitlab, &caps)?;
        (submission.id, planned, started.elapsed(), from)
    };
    stand.after(branch, server.with(|f| f.snapshot()));
    let log = server.log();
    stand.notes.push(measure_note(
        branch,
        planned,
        sent,
        &log[from.min(log.len())..],
    ));
    stand.writes(
        &format!("{branch}: записи"),
        &log,
        &[
            (glf::DRAFTS, VOLUME as usize),
            (glf::PUBLISH, VOLUME as usize),
            (glf::NOTES, 1),
        ],
    );
    let published: BTreeSet<u64> = log
        .iter()
        .filter(|c| c.key == glf::PUBLISH)
        .filter_map(|c| draft_id_in(&c.path))
        .collect();
    let last_publish = log
        .iter()
        .filter(|c| c.key == glf::PUBLISH)
        .map(|c| c.seq)
        .max();
    let note_seq = log.iter().find(|c| c.key == glf::NOTES).map(|c| c.seq);
    stand.check(
        &format!("{branch}: сводка после последней публикации"),
        matches!((last_publish, note_seq), (Some(p), Some(n)) if n > p),
        format!("публикация {last_publish:?}, сводка {note_seq:?}"),
    );
    let notes = server.with(|f| f.notes(GL_IID));
    let from_drafts: Vec<&glf::Note> = notes.iter().filter(|n| n.from_draft.is_some()).collect();
    let sources: BTreeSet<u64> = from_drafts.iter().filter_map(|n| n.from_draft).collect();
    let bodies: BTreeSet<String> = from_drafts.iter().map(|n| n.body.clone()).collect();
    stand.eq(
        &format!("{branch}: заметки из черновиков (число, по ID опубликованных, тексты)"),
        (
            from_drafts.len(),
            sources == published && sources.len() == VOLUME as usize,
            bodies == *expected,
        ),
        (VOLUME as usize, true, true),
    );
    stand.eq(
        &format!("{branch}: сводка — заметка без черновика"),
        notes
            .iter()
            .filter(|n| n.from_draft.is_none())
            .map(|n| (n.body.clone(), n.position.is_none()))
            .collect::<Vec<_>>(),
        vec![(SUMMARY.to_owned(), true)],
    );
    stand.eq(
        &format!("{branch}: черновиков на сервере"),
        server.with(|f| f.drafts(GL_IID)).len(),
        0,
    );
    let journal = Journal::open(&path)?;
    let scope = journal.scope(&project());
    let done = report(&scope, submission)?;
    stand.eq(
        &format!("{branch}: журнал (элементов, все confirmed, итог, восстановлено)"),
        (
            done.items.len(),
            done.items.iter().all(|i| i.state == ItemState::Confirmed),
            done.delivery,
            journal.recovered().len(),
        ),
        (2 * VOLUME as usize + 1, true, Delivery::Full, 0),
    );
    stand.journal(branch, &journal)?;
    Ok(())
}

/// Ветвь `rate-limit`: явный отказ по лимиту на записи или потерянный ответ и лимит на сверке.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Limit {
    Refused,
    Lost,
}

impl Limit {
    fn name(self) -> &'static str {
        match self {
            Limit::Refused => "refused",
            Limit::Lost => "lost",
        }
    }
}

/// Заголовки лимита в записанном следе ветви (с вызова `from`) у вызовов с кодом `status`.
fn limit_headers(stand: &Stand, from: usize, status: u16) -> Vec<BTreeMap<String, String>> {
    stand
        .trace
        .entries(&stand.scrubber)
        .into_iter()
        .skip(from)
        .filter(|entry| entry.status == Some(status))
        .map(|entry| entry.rate_limit)
        .collect()
}

fn rate_limit(stand: &mut Stand) -> Outcome {
    for limit in [Limit::Refused, Limit::Lost] {
        rate_limit_gitlab(stand, limit)?;
    }
    for limit in [Limit::Refused, Limit::Lost] {
        rate_limit_github(stand, limit)?;
    }
    Ok(())
}

fn rate_limit_gitlab(stand: &mut Stand, limit: Limit) -> Outcome {
    let branch = format!("gitlab-{}", limit.name());
    let server = FakeServer::start(gitlab_fake()?)?;
    server.inject(match limit {
        Limit::Refused => Fault::new(glf::PUBLISH, Reply::RateLimited).skip(1),
        Limit::Lost => Fault::new(glf::PUBLISH, Reply::Status(502))
            .after_apply()
            .skip(1),
    });
    stand.before(&branch, server.with(|f| f.snapshot()));
    let gitlab = stand.gl(&server, TOKEN_REVIEWER, ACTOR_REVIEWER)?;
    let caps = gitlab.capabilities()?;
    let path = stand.journal_path(&branch);
    let mark = stand.mark(&server);
    let (submission, ids, stopped) = {
        let journal = Journal::open(&path)?;
        let scope = journal.scope(&project());
        let (session, _, ids) = gl_session(
            &scope,
            &gitlab,
            REVIEWER,
            &[
                (Spot::Line(Side::New, 1), "первый"),
                (Spot::Line(Side::New, 2), "второй"),
                (Spot::Line(Side::New, 4), "третий"),
            ],
        )?;
        let submission = plan_gitlab(&scope, session, &GitLabPlan::default())?;
        run_gitlab(&scope, submission.id, &gitlab, &caps)?;
        stand.receipt(&server, mark)?;
        let stopped = scope
            .submission(submission.id)?
            .items
            .iter()
            .find(|item| item.state != ItemState::Confirmed)
            .map(|item| item.id)
            .ok_or("нет неподтверждённого элемента")?;
        if limit == Limit::Lost {
            server.inject(Fault::new(GL_DRAFT_GET, Reply::RateLimited));
            let verdicts = reconcile_gitlab(&scope, submission.id, &gitlab)?;
            stand.check(
                &format!("{branch}: сверка «не выяснено» без кандидата из-за лимита"),
                matches!(&verdicts[..], [Verdict::Unresolved { detail }] if detail.contains("лимит")),
                format!("{verdicts:?}"),
            );
        }
        let again = server.logged();
        let rerun = run_gitlab(&scope, submission.id, &gitlab, &caps);
        let expected_refusal = match (&rerun, limit) {
            (Err(SubmitError::Closed(_)), Limit::Refused) => true,
            (Err(SubmitError::Unresolved(item)), Limit::Lost) => *item == stopped,
            _ => false,
        };
        stand.check(
            &format!("{branch}: повторный запуск отказывает без записей"),
            expected_refusal && writes(&server.log()[again..]).is_empty(),
            format!("{rerun:?}"),
        );
        (submission.id, ids, stopped)
    };
    stand.after(&branch, server.with(|f| f.snapshot()));
    server.finish()?;
    let log = server.log();
    stand.writes(
        &format!("{branch}: записи"),
        &log,
        &[(glf::DRAFTS, 3), (glf::PUBLISH, 2)],
    );
    if limit == Limit::Refused {
        let headers: Vec<(Option<String>, bool)> = limit_headers(stand, mark.trace, 429)
            .iter()
            .map(|h| {
                (
                    h.get("ratelimit-remaining").cloned(),
                    h.contains_key("retry-after"),
                )
            })
            .collect();
        stand.eq(
            &format!("{branch}: заголовки лимита у отказа 429 в следе"),
            headers,
            vec![(Some("0".to_owned()), true)],
        );
    }
    let journal = Journal::open(&path)?;
    let scope = journal.scope(&project());
    let remote = |id: DraftId| -> Result<Option<u64>, SubmitError> {
        Ok(scope
            .draft(id)?
            .remote_id
            .and_then(|r| r.as_str().parse().ok()))
    };
    let created: Vec<Option<u64>> = ids.iter().map(|id| remote(*id)).collect::<Result<_, _>>()?;
    let published = match limit {
        Limit::Refused => 1,
        Limit::Lost => 2,
    };
    let mut notes: Vec<Option<u64>> = server
        .with(|f| f.notes(GL_IID))
        .iter()
        .map(|n| n.from_draft)
        .collect();
    notes.sort();
    stand.eq(
        &format!("{branch}: сервер: заметки из черновиков"),
        notes,
        created[..published].to_vec(),
    );
    let mut left: Vec<Option<u64>> = server
        .with(|f| f.drafts(GL_IID))
        .iter()
        .map(|d| Some(d.id))
        .collect();
    left.sort();
    stand.eq(
        &format!("{branch}: сервер: остались черновиками"),
        left,
        created[published..].to_vec(),
    );
    let (second, closed) = match limit {
        Limit::Refused => (ItemState::Failed, true),
        Limit::Lost => (ItemState::Unknown, false),
    };
    let done = report(&scope, submission)?;
    let item = scope.item(stopped)?;
    stand.eq(
        &format!("{branch}: журнал: состояния, закрыта, итог, кандидат"),
        (
            states(&scope, submission)?,
            done.closed,
            done.delivery,
            item.candidate,
        ),
        (
            vec![
                (Operation::GitLabCreateDraftNote, ItemState::Confirmed),
                (Operation::GitLabCreateDraftNote, ItemState::Confirmed),
                (Operation::GitLabCreateDraftNote, ItemState::Confirmed),
                (Operation::GitLabPublishDraftNote, ItemState::Confirmed),
                (Operation::GitLabPublishDraftNote, second),
                (Operation::GitLabPublishDraftNote, ItemState::Planned),
            ],
            closed,
            Delivery::Partial,
            None,
        ),
    );
    stand.journal(&branch, &journal)?;
    Ok(())
}

fn rate_limit_github(stand: &mut Stand, limit: Limit) -> Outcome {
    let branch = format!("github-{}", limit.name());
    let server = FakeServer::start(github_fake())?;
    server.inject(match limit {
        Limit::Refused => Fault::new(GH_ADD_THREAD, Reply::RateLimited).skip(1),
        Limit::Lost => Fault::new(GH_ADD_THREAD, Reply::Status(502))
            .after_apply()
            .skip(1),
    });
    stand.before(&branch, server.with(|f| f.snapshot()));
    let github = stand.gh(&server, TOKEN_REVIEWER, ACTOR_REVIEWER)?;
    let caps = github.capabilities()?;
    let path = stand.journal_path(&branch);
    let mark = stand.mark(&server);
    let (submission, stopped) = {
        let journal = Journal::open(&path)?;
        let scope = journal.scope(&project());
        let (session, _) = gh_session(
            &scope,
            &github,
            &[
                (Spot::Line(Side::New, 1), "первый"),
                (Spot::Line(Side::New, 2), "второй"),
                (Spot::Line(Side::New, 4), "третий"),
            ],
        )?;
        let submission =
            plan_github_pending(&scope, session, ReviewEvent::Comment, SUMMARY, None, &[])?;
        run_github(&scope, submission.id, &github, &caps)?;
        stand.receipt(&server, mark)?;
        let stopped = scope
            .submission(submission.id)?
            .items
            .iter()
            .find(|item| item.state != ItemState::Confirmed)
            .map(|item| item.id)
            .ok_or("нет неподтверждённого элемента")?;
        if limit == Limit::Lost {
            server.inject(Fault::new(GH_REVIEW_COMMENTS, Reply::RateLimited));
            let verdicts = reconcile_github(&scope, submission.id, &github)?;
            stand.check(
                &format!("{branch}: сверка «не выяснено» без кандидата из-за лимита"),
                matches!(&verdicts[..], [Verdict::Unresolved { detail }] if detail.contains("лимит")),
                format!("{verdicts:?}"),
            );
        }
        let again = server.logged();
        let rerun = run_github(&scope, submission.id, &github, &caps);
        let expected_refusal = match (&rerun, limit) {
            (Err(SubmitError::Closed(_)), Limit::Refused) => true,
            (Err(SubmitError::Unresolved(item)), Limit::Lost) => *item == stopped,
            _ => false,
        };
        stand.check(
            &format!("{branch}: повторный запуск отказывает без записей"),
            expected_refusal && writes(&server.log()[again..]).is_empty(),
            format!("{rerun:?}"),
        );
        (submission.id, stopped)
    };
    stand.after(&branch, server.with(|f| f.snapshot()));
    server.finish()?;
    let log = server.log();
    stand.writes(
        &format!("{branch}: записи"),
        &log,
        &[(GH_CREATE, 1), (GH_ADD_THREAD, 2)],
    );
    if limit == Limit::Refused {
        let headers: Vec<Option<String>> = limit_headers(stand, mark.trace, 403)
            .iter()
            .map(|h| h.get("x-ratelimit-remaining").cloned())
            .collect();
        stand.eq(
            &format!("{branch}: заголовок лимита у отказа 403 в следе"),
            headers,
            vec![Some("0".to_owned())],
        );
    }
    let kept = match limit {
        Limit::Refused => vec!["первый".to_owned()],
        Limit::Lost => vec!["первый".to_owned(), "второй".to_owned()],
    };
    stand.eq(
        &format!("{branch}: сервер: PENDING ревьюера и его комментарии"),
        (
            server
                .with(|f| f.reviews(GH_NUMBER))
                .iter()
                .map(|r| (r.user.clone(), r.state))
                .collect::<Vec<_>>(),
            server
                .with(|f| f.comments(GH_NUMBER))
                .into_iter()
                .map(|c| c.body)
                .collect::<Vec<_>>(),
        ),
        (vec![(REVIEWER.to_owned(), ghf::ReviewState::Pending)], kept),
    );
    let (second, closed, delivery) = match limit {
        Limit::Refused => (ItemState::Failed, true, Delivery::NotSent),
        Limit::Lost => (ItemState::Unknown, false, Delivery::Partial),
    };
    let journal = Journal::open(&path)?;
    let scope = journal.scope(&project());
    let done = report(&scope, submission)?;
    let item = scope.item(stopped)?;
    stand.eq(
        &format!("{branch}: журнал: состояния, закрыта, итог, кандидат"),
        (
            states(&scope, submission)?,
            done.closed,
            done.delivery,
            item.candidate,
        ),
        (
            vec![
                (Operation::GitHubCreatePending, ItemState::Confirmed),
                (Operation::GitHubAddPendingComment, ItemState::Confirmed),
                (Operation::GitHubAddPendingComment, second),
                (Operation::GitHubAddPendingComment, ItemState::Planned),
                (Operation::GitHubSubmitReview, ItemState::Planned),
            ],
            closed,
            delivery,
            None,
        ),
    );
    stand.journal(&branch, &journal)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenario::Group;
    use crate::transport::Method;

    fn logged(key: &str, method: Method, path: &str, body: Value) -> Logged {
        Logged {
            seq: 1,
            key: key.to_owned(),
            user: None,
            method,
            path: path.to_owned(),
            body,
            status: Some(200),
            fault: None,
            applied: true,
        }
    }

    #[test]
    fn writes_count_rest_writes_and_graphql_mutations_only() {
        let log = vec![
            logged("GET /user", Method::Get, "/user", Value::Null),
            logged(
                GH_CREATE,
                Method::Post,
                "/repos/o/r/pulls/7/reviews",
                json!({}),
            ),
            logged(
                GH_ADD_THREAD,
                Method::Post,
                "/graphql",
                json!({"query": "mutation($input: X!) { addPullRequestReviewThread(input: $input) { thread { id } } }"}),
            ),
            logged(
                "graphql node",
                Method::Post,
                "/graphql",
                json!({"query": "query($id: ID!) { node(id: $id) { id } }"}),
            ),
            logged(
                glf::PUBLISH,
                Method::Put,
                "/api/v4/x/draft_notes/5/publish",
                Value::Null,
            ),
        ];
        assert_eq!(
            writes(&log),
            BTreeMap::from([
                (GH_ADD_THREAD.to_owned(), 1),
                (GH_CREATE.to_owned(), 1),
                (glf::PUBLISH.to_owned(), 1),
            ])
        );
    }

    #[test]
    fn writes_check_is_exact_and_ignores_zero_expectations() {
        let log = vec![logged(GH_CREATE, Method::Post, "/x", json!({}))];
        let mut checks = Checks::default();
        checks.writes("ровно", &log, &[(GH_CREATE, 1), (GH_SUBMIT, 0)]);
        checks.writes("лишняя запись", &log, &[]);
        checks.writes("не хватает", &log, &[(GH_CREATE, 1), (GH_SUBMIT, 1)]);
        let (status, reason) = checks.verdict();
        assert_eq!(status, Status::Fail);
        assert!(!reason.contains("ровно"), "{reason}");
        assert!(reason.contains("лишняя запись"), "{reason}");
        assert!(reason.contains("не хватает"), "{reason}");
        assert!(reason.contains("не прошли 2 из 3"), "{reason}");
    }

    #[test]
    fn verdict_needs_checks_and_all_of_them() {
        let mut checks = Checks::default();
        assert_eq!(checks.verdict().0, Status::Fail, "без проверок — не pass");
        checks.eq("одно", 1, 1);
        checks.check("другое", true, "");
        assert_eq!(checks.verdict().0, Status::Pass);
        assert_eq!(checks.len(), 2);
        checks.eq("третье", 1, 2);
        let (status, reason) = checks.verdict();
        assert_eq!(status, Status::Fail);
        assert!(reason.contains("третье: 1, ожидалось 2"), "{reason}");
    }

    #[test]
    fn draft_id_is_read_from_publish_path() {
        assert_eq!(
            draft_id_in("/api/v4/projects/1/merge_requests/3/draft_notes/42/publish"),
            Some(42)
        );
        assert_eq!(
            draft_id_in("/api/v4/projects/1/merge_requests/3/notes"),
            None
        );
    }

    #[test]
    fn stand_without_artifacts_is_execution_error() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let mut result = ResultFile::new("s", "b", "partial", crate::results::Column::Fake);
        let mut stand = Stand::new(dir.path());
        stand.eq("проверка", 1, 1);
        stand.finish(Ok(()), &mut result);
        assert!(!result.execution.is_ok(), "{:?}", result.execution);
        Ok(())
    }

    #[test]
    fn stand_error_is_execution_error_without_token() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let mut result = ResultFile::new("s", "b", "partial", crate::results::Column::Fake);
        let stand = Stand::new(dir.path());
        stand.finish(Err(format!("отказ с {TOKEN_REVIEWER}").into()), &mut result);
        match &result.execution {
            Execution::Error { reason } => {
                assert!(!reason.contains(TOKEN_REVIEWER), "{reason}");
            }
            Execution::Ok => return Err("ожидалась ошибка исполнения".to_owned()),
        }
        Ok(())
    }

    /// Порог квитанции включает журнал сервера до отметки: вызов, отправленный клиентом с отметки,
    /// но не записанный сервером ветви, квитанцию не выполняет, даже если до отметки сервер записал
    /// больше вызовов, чем отправлено с неё.
    #[test]
    fn receipt_threshold_adds_server_log_at_the_mark() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let stand = Stand::new(dir.path());
        let server = FakeServer::start(github_fake())?;
        let other = FakeServer::start(github_fake())?;
        let client = stand.client(TOKEN_REVIEWER, ACTOR_REVIEWER, false);
        let get_user = |base: &str| -> Result<(), String> {
            client
                .send(&Request::get(format!("{base}/user")))
                .map(drop)
                .map_err(|e| e.to_string())
        };
        get_user(server.base())?;
        get_user(server.base())?;
        let mark = stand.mark(&server);
        get_user(other.base())?;
        assert!(
            stand
                .receipt_within(&server, mark, Duration::from_millis(200))
                .is_err(),
            "вызов к другому серверу засчитан серверу ветви"
        );
        let mark = stand.mark(&server);
        get_user(server.base())?;
        stand.receipt_within(&server, mark, RECEIPT)?;
        assert_eq!(server.logged(), 3);
        Ok(())
    }

    /// Каждая строка матрицы прогоняется в столбце `fake` до `pass`.
    #[test]
    fn every_matrix_row_passes_against_forge_fake() -> Result<(), String> {
        let mut failures = Vec::new();
        for scenario in Scenario::ALL
            .into_iter()
            .filter(|s| s.group() == Group::Row)
        {
            let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
            let mut result =
                ResultFile::new("s", "b", scenario.name(), crate::results::Column::Fake);
            run(scenario, dir.path(), &mut result);
            if !result.execution.is_ok() || result.status != Status::Pass {
                failures.push(format!(
                    "{}: {:?} {:?} {}",
                    scenario.name(),
                    result.execution,
                    result.status,
                    result.reason
                ));
            }
            if result.journal.is_none() || result.trace.is_empty() {
                failures.push(format!("{}: нет журнала или следа", scenario.name()));
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("\n"))
        }
    }

    /// Diff `volume`: каждая из 200 добавленных строк держит свой черновик.
    #[test]
    fn volume_spots_cover_every_added_line() -> Result<(), String> {
        let file = FileDiff::parse(PATH, PATH, &volume_diff()).map_err(|e| e.to_string())?;
        let texts = volume_texts();
        let spots = volume_spots(&texts);
        assert_eq!(spots.len(), VOLUME as usize);
        assert_eq!(
            texts.iter().collect::<BTreeSet<_>>().len(),
            VOLUME as usize,
            "тексты разные"
        );
        for (spot, _) in &spots {
            spot.placement(&file)
                .map_err(|e| format!("{spot:?}: {e}"))?;
        }
        assert!(matches!(spots.first(), Some((Spot::Line(Side::New, 2), _))));
        assert!(matches!(
            spots.last(),
            Some((Spot::Line(Side::New, 201), _))
        ));
        assert!(Spot::Line(Side::New, VOLUME + 2).placement(&file).is_err());
        Ok(())
    }

    #[test]
    fn measure_note_splits_reads_and_writes() {
        let log = vec![
            logged("GET /user", Method::Get, "/user", Value::Null),
            logged(GH_CREATE, Method::Post, "/x", json!({})),
            logged(GH_CREATE, Method::Post, "/x", json!({})),
        ];
        let note = measure_note(
            "ветвь",
            Duration::from_millis(3),
            Duration::from_millis(42),
            &log,
        );
        assert!(
            note.starts_with(
                "замер ветвь: планирование 3 мс, отправка 42 мс, вызовов 3 (записей 2, чтений 1)"
            ),
            "{note}"
        );
        assert!(note.contains(&format!("{GH_CREATE}: 2")), "{note}");
        assert!(note.contains("GET /user: 1"), "{note}");
    }

    /// `volume` и `rate-limit` проходят против `forge-fake`, у каждой ветви свой журнал, у ветвей
    /// `volume` — замер в заметках.
    #[test]
    fn cost_scenarios_pass_against_forge_fake() -> Result<(), String> {
        let branches = [
            (
                Scenario::Volume,
                vec!["github-local", "github-pending", "gitlab"],
            ),
            (
                Scenario::RateLimit,
                vec![
                    "gitlab-refused",
                    "gitlab-lost",
                    "github-refused",
                    "github-lost",
                ],
            ),
        ];
        let mut failures = Vec::new();
        for (scenario, names) in branches {
            assert_eq!(scenario.group(), Group::Cost);
            let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
            let mut result =
                ResultFile::new("s", "b", scenario.name(), crate::results::Column::Fake);
            run(scenario, dir.path(), &mut result);
            if !result.execution.is_ok() || result.status != Status::Pass {
                failures.push(format!(
                    "{}: {:?} {:?} {}",
                    scenario.name(),
                    result.execution,
                    result.status,
                    result.reason
                ));
                continue;
            }
            let journals: Vec<String> = result
                .journal
                .as_ref()
                .and_then(Value::as_object)
                .map(|map| map.keys().cloned().collect())
                .unwrap_or_default();
            let mut expected: Vec<String> = names.iter().map(|n| (*n).to_owned()).collect();
            expected.sort();
            if journals != expected {
                failures.push(format!("{}: журналы ветвей {journals:?}", scenario.name()));
            }
            if scenario == Scenario::Volume {
                for name in &names {
                    let prefix = format!("замер {name}: ");
                    if !result.notes.iter().any(|n| n.starts_with(&prefix)) {
                        failures.push(format!("volume: нет замера ветви {name}"));
                    }
                }
            }
            if result.problems().iter().any(|p| !p.is_empty()) {
                failures.push(format!("{}: {:?}", scenario.name(), result.problems()));
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("\n"))
        }
    }
}
