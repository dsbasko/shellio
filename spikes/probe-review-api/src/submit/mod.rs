//! Отправка ревью по пяти шагам ADR-0015 поверх журнала и адаптеров и сверка `unknown`.
//!
//! 1. Свежая ревизия против ревизии сессии: сменилась — [`SubmitError::HeadChanged`] до первой
//!    записи, путь выбирает вызывающий (`ReviewSession::apply_head`).
//! 2. Планировщик строит конкретный список запросов по строкам журнала ([`plan_github_local`],
//!    [`plan_github_pending`], [`plan_gitlab`]).
//! 3. Исполнитель ([`run_github`], [`run_gitlab`]) идёт по элементам: чтения → `begin` → одна
//!    запись → итог. Остановка на первом не подтверждённом элементе, отказ закрывает отправку.
//! 4. Удалённые ID пишутся одной фиксацией с подтверждением (`Scope::confirm_with`).
//! 5. Итог ([`report`]) — из строк журнала: полностью, частично, не отправлено.
//!
//! Правило сверки — таблица раздела «Отправка и сверка» отчёта пробы: `confirmed` только по связи
//! с нашей операцией, совпадение по содержимому — кандидат, `failed` по сверке — только у операции
//! над известным ID, прочитанным в исходном состоянии.

pub mod reconcile;

use std::collections::BTreeSet;
use std::fmt;

use crate::github::{self, GitHub, NoPending, PendingComment, PendingReview, ReviewEvent};
use crate::gitlab::{self, GitLab, KnownDraft};
use crate::journal::{
    Confirmation, ItemState, JournalError, Operation, PlannedItem, Scope, SessionId, Submission,
    SubmissionId, SubmissionSpec, SubmitItem,
};
use crate::model::revision::Revision;
use crate::model::{DraftComment, DraftId, Placement, ProviderId, ReviewSession};
use crate::transport::{Transport, WriteFate};

use reconcile::{GitHubLedger, GitLabLedger, Verdict};

#[derive(Debug)]
pub enum SubmitError {
    Journal(JournalError),
    GitHub(github::ApiError),
    GitLab(gitlab::ApiError),
    /// Шаг 1: ревизия на сервере не равна ревизии сессии; записей не было.
    HeadChanged {
        fresh: Box<Revision>,
    },
    /// Отправка закрыта: её элементы в `planned` не отправляются.
    Closed(SubmissionId),
    /// Элемент в `unknown` или `in_flight`: до сверки отправка не продолжается.
    Unresolved(crate::journal::ItemId),
    /// Отказ до записи: отправка закрыта, запросов записи не было.
    Refused(String),
    /// Планировать нечего.
    Empty,
}

impl fmt::Display for SubmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SubmitError::Journal(err) => write!(f, "журнал: {err}"),
            SubmitError::GitHub(err) => write!(f, "GitHub: {err}"),
            SubmitError::GitLab(err) => write!(f, "GitLab: {err}"),
            SubmitError::HeadChanged { fresh } => write!(
                f,
                "ревизия на сервере сменилась (head {}), нужен выбор пути",
                fresh.head_sha()
            ),
            SubmitError::Closed(id) => write!(f, "отправка {} закрыта", id.0),
            SubmitError::Unresolved(id) => {
                write!(f, "элемент {} не сверен, отправка не продолжается", id.0)
            }
            SubmitError::Refused(reason) => write!(f, "отправка остановлена до записи: {reason}"),
            SubmitError::Empty => f.write_str("нечего отправлять"),
        }
    }
}

impl std::error::Error for SubmitError {}

impl From<JournalError> for SubmitError {
    fn from(err: JournalError) -> SubmitError {
        SubmitError::Journal(err)
    }
}

impl From<github::ApiError> for SubmitError {
    fn from(err: github::ApiError) -> SubmitError {
        SubmitError::GitHub(err)
    }
}

impl From<gitlab::ApiError> for SubmitError {
    fn from(err: gitlab::ApiError) -> SubmitError {
        SubmitError::GitLab(err)
    }
}

/// Итог одной записи для журнала.
enum Written {
    Confirmed(Confirmation),
    Failed(String),
    Unknown(String),
}

fn written<E: fmt::Display>(fate: WriteFate, err: &E) -> Written {
    match fate {
        WriteFate::NotAccepted => Written::Failed(err.to_string()),
        WriteFate::Unknown => Written::Unknown(err.to_string()),
    }
}

/// Чтения до записи и одна запись элемента — то, чем провайдеры отличаются.
trait Writer {
    type Ready;
    /// Чтения перед `begin`. Ошибка оставляет элемент в `planned`.
    fn prepare(
        &mut self,
        item: &SubmitItem,
        drafts: &[DraftComment],
    ) -> Result<Self::Ready, SubmitError>;
    /// Ровно один запрос записи.
    fn write(&mut self, item: &SubmitItem, ready: Self::Ready) -> Written;
}

/// Шаги 3 и 4: элементы по порядку, продолжение незакрытой отправки с первого `planned`.
fn drive<W: Writer>(
    scope: &Scope<'_>,
    id: SubmissionId,
    writer: &mut W,
) -> Result<Report, SubmitError> {
    let submission = scope.submission(id)?;
    if submission.closed {
        return Err(SubmitError::Closed(id));
    }
    for item in &submission.items {
        match item.state {
            ItemState::Confirmed => continue,
            ItemState::Failed => {
                scope.close_submission(id)?;
                return report(scope, id);
            }
            ItemState::Unknown | ItemState::InFlight => {
                return Err(SubmitError::Unresolved(item.id));
            }
            ItemState::Planned => {}
        }
        let drafts = item
            .drafts
            .iter()
            .map(|draft| scope.draft(*draft))
            .collect::<Result<Vec<_>, _>>()?;
        let ready = match writer.prepare(item, &drafts) {
            Ok(ready) => ready,
            Err(SubmitError::Refused(reason)) => {
                scope.close_submission(id)?;
                return Err(SubmitError::Refused(reason));
            }
            Err(err) => return Err(err),
        };
        scope.begin(item.id)?;
        match writer.write(item, ready) {
            Written::Confirmed(confirmation) => scope.confirm_with(item.id, &confirmation)?,
            Written::Failed(detail) => {
                scope.fail(item.id, &detail)?;
                scope.close_submission(id)?;
                break;
            }
            Written::Unknown(detail) => {
                scope.mark_unknown(item.id, &detail)?;
                break;
            }
        }
    }
    report(scope, id)
}

/// Шаг 5.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// Все элементы подтверждены.
    Full,
    /// Видимое подтверждено частично или итог части элементов неизвестен.
    Partial,
    /// Ничего видимого не подтверждено и неизвестных элементов нет.
    NotSent,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub submission: SubmissionId,
    pub delivery: Delivery,
    pub closed: bool,
    /// Элементы в `unknown` и `in_flight`.
    pub unresolved: usize,
    pub items: Vec<SubmitItem>,
}

/// Операция, после подтверждения которой ревью видно другим.
fn visible(operation: Operation) -> bool {
    match operation {
        Operation::GitHubCreateReview
        | Operation::GitHubSubmitReview
        | Operation::GitLabPublishDraftNote
        | Operation::GitLabSummaryNote
        | Operation::GitLabApprove
        | Operation::GitLabRequestChanges => true,
        Operation::GitHubCreatePending
        | Operation::GitHubAddPendingComment
        | Operation::GitHubUpdatePendingComment
        | Operation::GitLabCreateDraftNote
        | Operation::GitLabUpdateDraftNote => false,
    }
}

pub fn report(scope: &Scope<'_>, id: SubmissionId) -> Result<Report, SubmitError> {
    let submission = scope.submission(id)?;
    let items = submission.items;
    let unresolved = items
        .iter()
        .filter(|item| matches!(item.state, ItemState::Unknown | ItemState::InFlight))
        .count();
    let shown = items
        .iter()
        .any(|item| visible(item.operation) && item.state == ItemState::Confirmed);
    let delivery = if items.iter().all(|item| item.state == ItemState::Confirmed) {
        Delivery::Full
    } else if !shown && unresolved == 0 {
        Delivery::NotSent
    } else {
        Delivery::Partial
    };
    Ok(Report {
        submission: id,
        delivery,
        closed: submission.closed,
        unresolved,
        items,
    })
}

/// История черновиков сессии по всем отправкам.
struct History {
    submissions: Vec<Submission>,
}

impl History {
    fn load(scope: &Scope<'_>, session: SessionId) -> Result<History, JournalError> {
        Ok(History {
            submissions: scope.submissions(session)?,
        })
    }

    fn confirmed(&self) -> impl Iterator<Item = (&Submission, &SubmitItem)> {
        self.submissions.iter().flat_map(|submission| {
            submission
                .items
                .iter()
                .filter(|item| item.state == ItemState::Confirmed)
                .map(move |item| (submission, item))
        })
    }

    /// `PENDING` отправлен: Submit подтверждён в какой-нибудь отправке.
    fn pending_submitted(&self, review: &ProviderId) -> bool {
        self.confirmed().any(|(submission, item)| {
            item.operation == Operation::GitHubSubmitReview
                && submission.spec.pending_review.as_ref() == Some(review)
        })
    }

    /// Черновик опубликован подтверждённой операцией: Create review, публикация `draft_note` или
    /// комментарий `PENDING`, Submit которого подтверждён.
    fn published(&self) -> BTreeSet<DraftId> {
        let mut published = BTreeSet::new();
        for (submission, item) in self.confirmed() {
            let shown = match item.operation {
                Operation::GitHubCreateReview | Operation::GitLabPublishDraftNote => true,
                Operation::GitHubAddPendingComment => submission
                    .spec
                    .pending_review
                    .as_ref()
                    .is_some_and(|review| self.pending_submitted(review)),
                _ => false,
            };
            if shown {
                published.extend(item.drafts.iter().copied());
            }
        }
        published
    }

    /// Свои комментарии `PENDING` (node ID) по всем отправкам с этим `PENDING`.
    fn own_comments(&self, review: &ProviderId) -> Vec<ProviderId> {
        self.confirmed()
            .filter(|(submission, item)| {
                item.operation == Operation::GitHubAddPendingComment
                    && submission.spec.pending_review.as_ref() == Some(review)
            })
            .filter_map(|(_, item)| item.remote_id.clone())
            .collect()
    }

    /// Незавершённый `PENDING` сессии: создан или принят, а Submit не подтверждён.
    fn open_pending(&self) -> Option<&SubmissionSpec> {
        self.submissions
            .iter()
            .rev()
            .map(|submission| &submission.spec)
            .find(|spec| {
                spec.pending_review
                    .as_ref()
                    .is_some_and(|review| !self.pending_submitted(review))
            })
    }

    fn remote_of(&self, operation: Operation) -> Vec<ProviderId> {
        self.confirmed()
            .filter(|(_, item)| item.operation == operation)
            .filter_map(|(_, item)| item.remote_id.clone())
            .collect()
    }
}

/// Черновики, которые можно отправить: на ревизии сессии, с действующим якорем, не
/// опубликованные и не удерживаемые элементом.
fn plannable(
    scope: &Scope<'_>,
    session_id: SessionId,
    history: &History,
) -> Result<Vec<DraftComment>, SubmitError> {
    let session = scope.session(session_id)?;
    let published = history.published();
    let mut drafts = Vec::new();
    for draft in scope.drafts(session_id)? {
        if draft.revision() != session.revision()
            || !draft.is_current()
            || published.contains(&draft.id)
            || scope.draft_hold(draft.id)?.is_some()
        {
            continue;
        }
        drafts.push(draft);
    }
    Ok(drafts)
}

fn item(operation: Operation, drafts: &[DraftId]) -> PlannedItem {
    PlannedItem {
        operation,
        drafts: drafts.to_vec(),
    }
}

/// Ветвь «только локальные черновики»: один Create review со всеми черновиками на строке, у
/// которых нет удалённого ID.
pub fn plan_github_local(
    scope: &Scope<'_>,
    session: SessionId,
    event: ReviewEvent,
    summary: &str,
) -> Result<Submission, SubmitError> {
    let history = History::load(scope, session)?;
    if history.open_pending().is_some() {
        return Err(SubmitError::Refused(
            "у сессии есть неотправленный PENDING, ветвь локальных черновиков его не трогает"
                .to_owned(),
        ));
    }
    let drafts: Vec<DraftId> = plannable(scope, session, &history)?
        .into_iter()
        .filter(|draft| {
            matches!(draft.placement(), Placement::Line(_)) && draft.remote_id.is_none()
        })
        .map(|draft| draft.id)
        .collect();
    let spec = SubmissionSpec {
        event: Some(event.name().to_owned()),
        summary: summary.to_owned(),
        ..SubmissionSpec::default()
    };
    Ok(scope.plan(
        session,
        &spec,
        &[item(Operation::GitHubCreateReview, &drafts)],
    )?)
}

/// Ветвь «серверный `PENDING`». Незавершённый `PENDING` сессии (после опровергнутого Submit или
/// прерванной отправки) продолжается: та же `pending_review` и те же чужие комментарии, без
/// создания. Иначе — принятый `adopt` или новый. `update` — черновики с ID комментария, чей
/// текст нужно обновить перед Submit.
pub fn plan_github_pending(
    scope: &Scope<'_>,
    session: SessionId,
    event: ReviewEvent,
    summary: &str,
    adopt: Option<&PendingReview>,
    update: &[DraftId],
) -> Result<Submission, SubmitError> {
    let history = History::load(scope, session)?;
    let mut spec = SubmissionSpec {
        event: Some(event.name().to_owned()),
        summary: summary.to_owned(),
        ..SubmissionSpec::default()
    };
    let mut items = Vec::new();
    match (history.open_pending(), adopt) {
        (Some(open), None) => {
            spec.pending_review = open.pending_review.clone();
            spec.pending_foreign = open.pending_foreign.clone();
        }
        (Some(_), Some(_)) => {
            return Err(SubmitError::Refused(
                "у сессии уже есть свой неотправленный PENDING".to_owned(),
            ));
        }
        (None, Some(pending)) => {
            spec.pending_review = Some(pending.id().clone());
            spec.pending_foreign = Some(pending.foreign().to_vec());
        }
        (None, None) => items.push(item(Operation::GitHubCreatePending, &[])),
    }
    for draft in plannable(scope, session, &history)? {
        if !matches!(draft.placement(), Placement::Line(_)) {
            continue;
        }
        match &draft.remote_id {
            None => items.push(item(Operation::GitHubAddPendingComment, &[draft.id])),
            Some(_) if update.contains(&draft.id) => {
                items.push(item(Operation::GitHubUpdatePendingComment, &[draft.id]));
            }
            Some(_) => {}
        }
    }
    items.push(item(Operation::GitHubSubmitReview, &[]));
    Ok(scope.plan(session, &spec, &items)?)
}

/// Что отправить в GitLab сверх черновиков.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitLabPlan {
    pub summary: String,
    pub approve: bool,
    pub request_changes: bool,
    /// Черновики с ID GitLab, текст или позицию которых нужно обновить перед публикацией.
    pub update: Vec<DraftId>,
}

/// GitLab: черновику без ID — создание, затем публикация каждого по ID; сводка, `approve`, запрос
/// изменений — отдельными элементами. Черновику с ID второе создание не планируется.
pub fn plan_gitlab(
    scope: &Scope<'_>,
    session: SessionId,
    plan: &GitLabPlan,
) -> Result<Submission, SubmitError> {
    let history = History::load(scope, session)?;
    let drafts = plannable(scope, session, &history)?;
    let mut items = Vec::new();
    for draft in &drafts {
        match &draft.remote_id {
            None => items.push(item(Operation::GitLabCreateDraftNote, &[draft.id])),
            Some(_) if plan.update.contains(&draft.id) => {
                items.push(item(Operation::GitLabUpdateDraftNote, &[draft.id]));
            }
            Some(_) => {}
        }
    }
    for draft in &drafts {
        items.push(item(Operation::GitLabPublishDraftNote, &[draft.id]));
    }
    if !plan.summary.trim().is_empty() {
        items.push(item(Operation::GitLabSummaryNote, &[]));
    }
    if plan.approve {
        items.push(item(Operation::GitLabApprove, &[]));
    }
    if plan.request_changes {
        items.push(item(Operation::GitLabRequestChanges, &[]));
    }
    if items.is_empty() {
        return Err(SubmitError::Empty);
    }
    let spec = SubmissionSpec {
        summary: plan.summary.clone(),
        ..SubmissionSpec::default()
    };
    Ok(scope.plan(session, &spec, &items)?)
}

fn event_of(spec: &SubmissionSpec) -> Result<ReviewEvent, SubmitError> {
    match spec.event.as_deref() {
        Some("COMMENT") => Ok(ReviewEvent::Comment),
        Some("APPROVE") => Ok(ReviewEvent::Approve),
        Some("REQUEST_CHANGES") => Ok(ReviewEvent::RequestChanges),
        other => Err(SubmitError::Refused(format!("итог review {other:?}"))),
    }
}

fn one(drafts: &[DraftComment]) -> Result<&DraftComment, SubmitError> {
    match drafts {
        [draft] => Ok(draft),
        _ => Err(SubmitError::Refused(format!(
            "элемент несёт {} черновиков вместо одного",
            drafts.len()
        ))),
    }
}

enum GitHubReady {
    CreateReview {
        clear: NoPending,
        drafts: Vec<DraftComment>,
        event: ReviewEvent,
    },
    CreatePending {
        clear: NoPending,
    },
    AddComment {
        draft: DraftComment,
    },
    UpdateComment {
        comment: PendingComment,
        body: String,
    },
    Submit {
        event: ReviewEvent,
    },
}

struct GitHubWriter<'a, T: Transport> {
    github: &'a GitHub<T>,
    session: &'a ReviewSession,
    caps: &'a github::Capabilities,
    history: History,
    spec: SubmissionSpec,
    pending: Option<PendingReview>,
    own: Vec<PendingComment>,
}

impl<T: Transport> GitHubWriter<'_, T> {
    /// Свой `PENDING` отправки: созданный в этом запуске или восстановленный по журналу.
    fn pending(&mut self) -> Result<PendingReview, SubmitError> {
        if let Some(pending) = &self.pending {
            return Ok(pending.clone());
        }
        let Some(review) = self.spec.pending_review.clone() else {
            return Err(SubmitError::Refused("у отправки нет PENDING".to_owned()));
        };
        let own = self.history.own_comments(&review);
        let (pending, comments) = self.github.resume_pending(
            self.session,
            &review,
            &own,
            self.spec.pending_foreign.is_some(),
        )?;
        self.own = comments;
        self.pending = Some(pending.clone());
        Ok(pending)
    }
}

impl<T: Transport> Writer for GitHubWriter<'_, T> {
    type Ready = GitHubReady;

    fn prepare(
        &mut self,
        item: &SubmitItem,
        drafts: &[DraftComment],
    ) -> Result<GitHubReady, SubmitError> {
        match item.operation {
            Operation::GitHubCreateReview => Ok(GitHubReady::CreateReview {
                clear: self.github.check_no_pending(self.caps)?,
                drafts: drafts.to_vec(),
                event: event_of(&self.spec)?,
            }),
            Operation::GitHubCreatePending => Ok(GitHubReady::CreatePending {
                clear: self.github.check_no_pending(self.caps)?,
            }),
            Operation::GitHubAddPendingComment => {
                self.pending()?;
                Ok(GitHubReady::AddComment {
                    draft: one(drafts)?.clone(),
                })
            }
            Operation::GitHubUpdatePendingComment => {
                self.pending()?;
                let draft = one(drafts)?;
                let comment = self
                    .own
                    .iter()
                    .find(|comment| Some(&comment.id) == draft.remote_id.as_ref())
                    .cloned()
                    .ok_or_else(|| {
                        SubmitError::Refused(format!(
                            "комментарий черновика {} не из своего PENDING",
                            draft.id.0
                        ))
                    })?;
                Ok(GitHubReady::UpdateComment {
                    comment,
                    body: draft.body.clone(),
                })
            }
            Operation::GitHubSubmitReview => {
                let pending = self.pending()?;
                // Submit отправляет весь review: состав перечитывается, чужой комментарий, не
                // принятый явно, останавливает отправку без записи.
                let own: Vec<&ProviderId> = self.own.iter().map(|comment| &comment.id).collect();
                let accepted = self.spec.pending_foreign.clone().unwrap_or_default();
                let unknown: Vec<String> = self
                    .github
                    .review_comments(pending.id())?
                    .into_iter()
                    .filter(|comment| {
                        !own.contains(&&comment.node_id) && !accepted.contains(&comment.id)
                    })
                    .map(|comment| comment.id.as_str().to_owned())
                    .collect();
                if !unknown.is_empty() {
                    return Err(SubmitError::Refused(format!(
                        "в PENDING чужие комментарии, не принятые явно: {}",
                        unknown.join(", ")
                    )));
                }
                Ok(GitHubReady::Submit {
                    event: event_of(&self.spec)?,
                })
            }
            other => Err(SubmitError::Refused(format!(
                "операция {} не GitHub",
                other.name()
            ))),
        }
    }

    fn write(&mut self, item: &SubmitItem, ready: GitHubReady) -> Written {
        let summary = self.spec.summary.clone();
        let result = match ready {
            GitHubReady::CreateReview {
                clear,
                drafts,
                event,
            } => self
                .github
                .create_review(self.session, &clear, &drafts, event, &summary)
                .map(|review| Confirmation {
                    remote: Some(review.id),
                    ..Confirmation::default()
                }),
            GitHubReady::CreatePending { clear } => self
                .github
                .start_pending(self.session, self.caps, &clear)
                .map(|pending| {
                    let id = pending.id().clone();
                    self.pending = Some(pending);
                    self.own.clear();
                    Confirmation {
                        remote: Some(id.clone()),
                        drafts: Vec::new(),
                        pending_review: Some(id),
                    }
                }),
            GitHubReady::AddComment { draft } => match &self.pending {
                Some(pending) => self
                    .github
                    .add_pending_comment(self.session, pending, &draft)
                    .map(|comment| {
                        let id = comment.id.clone();
                        self.own.push(comment);
                        Confirmation {
                            remote: Some(id.clone()),
                            drafts: vec![(draft.id, id)],
                            pending_review: None,
                        }
                    }),
                None => Err(github::ApiError::Invalid("нет PENDING".to_owned())),
            },
            GitHubReady::UpdateComment { comment, body } => match &self.pending {
                Some(pending) => self
                    .github
                    .update_pending_comment(pending, &comment, &body)
                    .map(|updated| Confirmation {
                        remote: Some(updated.id),
                        ..Confirmation::default()
                    }),
                None => Err(github::ApiError::Invalid("нет PENDING".to_owned())),
            },
            GitHubReady::Submit { event } => match &self.pending {
                Some(pending) => {
                    self.github
                        .submit_pending(pending, event, &summary)
                        .map(|review| Confirmation {
                            remote: Some(review.id),
                            ..Confirmation::default()
                        })
                }
                None => Err(github::ApiError::Invalid("нет PENDING".to_owned())),
            },
        };
        let _ = item;
        match result {
            Ok(confirmation) => Written::Confirmed(confirmation),
            Err(err) => written(err.write_fate(), &err),
        }
    }
}

/// Шаг 1 и шаги 3–5 для GitHub.
pub fn run_github<T: Transport>(
    scope: &Scope<'_>,
    id: SubmissionId,
    github: &GitHub<T>,
    caps: &github::Capabilities,
) -> Result<Report, SubmitError> {
    let submission = scope.submission(id)?;
    let session = scope.session(submission.session)?;
    let fresh = github.pull()?.revision;
    if &fresh != session.revision() {
        return Err(SubmitError::HeadChanged {
            fresh: Box::new(fresh),
        });
    }
    let mut writer = GitHubWriter {
        github,
        session: &session,
        caps,
        history: History::load(scope, submission.session)?,
        spec: submission.spec.clone(),
        pending: None,
        own: Vec::new(),
    };
    drive(scope, id, &mut writer)
}

enum GitLabReady {
    Create {
        draft: DraftComment,
    },
    Update {
        known: KnownDraft,
        draft: DraftComment,
    },
    Publish {
        known: KnownDraft,
    },
    Summary,
    Approve,
    RequestChanges,
}

struct GitLabWriter<'a, T: Transport> {
    gitlab: &'a GitLab<T>,
    session: &'a ReviewSession,
    caps: &'a gitlab::Capabilities,
    summary: String,
}

fn known(draft: &DraftComment) -> Result<KnownDraft, SubmitError> {
    draft
        .remote_id
        .clone()
        .map(KnownDraft::recorded)
        .ok_or_else(|| SubmitError::Refused(format!("у черновика {} нет ID GitLab", draft.id.0)))
}

impl<T: Transport> Writer for GitLabWriter<'_, T> {
    type Ready = GitLabReady;

    fn prepare(
        &mut self,
        item: &SubmitItem,
        drafts: &[DraftComment],
    ) -> Result<GitLabReady, SubmitError> {
        Ok(match item.operation {
            Operation::GitLabCreateDraftNote => GitLabReady::Create {
                draft: one(drafts)?.clone(),
            },
            Operation::GitLabUpdateDraftNote => {
                let draft = one(drafts)?;
                GitLabReady::Update {
                    known: known(draft)?,
                    draft: draft.clone(),
                }
            }
            Operation::GitLabPublishDraftNote => GitLabReady::Publish {
                known: known(one(drafts)?)?,
            },
            Operation::GitLabSummaryNote => GitLabReady::Summary,
            Operation::GitLabApprove => GitLabReady::Approve,
            Operation::GitLabRequestChanges => GitLabReady::RequestChanges,
            other => {
                return Err(SubmitError::Refused(format!(
                    "операция {} не GitLab",
                    other.name()
                )));
            }
        })
    }

    fn write(&mut self, _item: &SubmitItem, ready: GitLabReady) -> Written {
        let result = match ready {
            GitLabReady::Create { draft } => self
                .gitlab
                .create_draft(self.session, &draft, false)
                .map(|(known, _)| Confirmation {
                    remote: Some(known.id().clone()),
                    drafts: vec![(draft.id, known.id().clone())],
                    pending_review: None,
                }),
            GitLabReady::Update { known, draft } => self
                .gitlab
                .update_draft(self.session, &known, &draft)
                .map(|remote| Confirmation {
                    remote: Some(remote.id),
                    ..Confirmation::default()
                }),
            GitLabReady::Publish { known } => {
                self.gitlab.publish_draft(&known).map(|()| Confirmation {
                    remote: Some(known.id().clone()),
                    ..Confirmation::default()
                })
            }
            GitLabReady::Summary => {
                self.gitlab
                    .summary_note(&self.summary)
                    .map(|note| Confirmation {
                        remote: Some(note.id),
                        ..Confirmation::default()
                    })
            }
            GitLabReady::Approve => self
                .gitlab
                .approve(self.session)
                .map(|_| Confirmation::default()),
            GitLabReady::RequestChanges => self
                .gitlab
                .request_changes(self.caps)
                .map(|_| Confirmation::default()),
        };
        match result {
            Ok(confirmation) => Written::Confirmed(confirmation),
            Err(err) => written(err.write_fate(), &err),
        }
    }
}

/// Шаг 1 и шаги 3–5 для GitLab.
pub fn run_gitlab<T: Transport>(
    scope: &Scope<'_>,
    id: SubmissionId,
    gitlab: &GitLab<T>,
    caps: &gitlab::Capabilities,
) -> Result<Report, SubmitError> {
    let submission = scope.submission(id)?;
    let session = scope.session(submission.session)?;
    let fresh = gitlab.merge_request()?.revision;
    if &fresh != session.revision() {
        return Err(SubmitError::HeadChanged {
            fresh: Box::new(fresh),
        });
    }
    let mut writer = GitLabWriter {
        gitlab,
        session: &session,
        caps,
        summary: submission.spec.summary.clone(),
    };
    drive(scope, id, &mut writer)
}

/// Сверка всех элементов отправки в `unknown`: свои объекты из журнала исключаются из
/// кандидатов, итог записывается. Повторов записи нет.
pub fn reconcile_github<T: Transport>(
    scope: &Scope<'_>,
    id: SubmissionId,
    github: &GitHub<T>,
) -> Result<Vec<Verdict>, SubmitError> {
    let submission = scope.submission(id)?;
    let session = scope.session(submission.session)?;
    let history = History::load(scope, submission.session)?;
    let mut verdicts = Vec::new();
    for item in submission
        .items
        .iter()
        .filter(|item| item.state == ItemState::Unknown)
    {
        let drafts = item
            .drafts
            .iter()
            .map(|draft| scope.draft(*draft))
            .collect::<Result<Vec<_>, _>>()?;
        let own = submission
            .spec
            .pending_review
            .as_ref()
            .map(|review| history.own_comments(review))
            .unwrap_or_default();
        let current = scope.submission(id)?;
        let verdict = reconcile::reconcile_github(
            github,
            &GitHubLedger {
                session: &session,
                submission: &current,
                item,
                drafts: &drafts,
                own_comments: &own,
            },
        );
        reconcile::apply(scope, item.id, &verdict)?;
        verdicts.push(verdict);
    }
    Ok(verdicts)
}

pub fn reconcile_gitlab<T: Transport>(
    scope: &Scope<'_>,
    id: SubmissionId,
    gitlab: &GitLab<T>,
) -> Result<Vec<Verdict>, SubmitError> {
    let submission = scope.submission(id)?;
    let session = scope.session(submission.session)?;
    let history = History::load(scope, submission.session)?;
    let mut own_drafts: Vec<ProviderId> = scope
        .drafts(submission.session)?
        .into_iter()
        .filter_map(|draft| draft.remote_id)
        .collect();
    own_drafts.extend(history.remote_of(Operation::GitLabCreateDraftNote));
    let own_notes = history.remote_of(Operation::GitLabSummaryNote);
    let mut verdicts = Vec::new();
    for item in submission
        .items
        .iter()
        .filter(|item| item.state == ItemState::Unknown)
    {
        let drafts = item
            .drafts
            .iter()
            .map(|draft| scope.draft(*draft))
            .collect::<Result<Vec<_>, _>>()?;
        let verdict = reconcile::reconcile_gitlab(
            gitlab,
            &GitLabLedger {
                session: &session,
                submission: &submission,
                item,
                drafts: &drafts,
                own_drafts: &own_drafts,
                own_notes: &own_notes,
            },
        );
        reconcile::apply(scope, item.id, &verdict)?;
        verdicts.push(verdict);
    }
    Ok(verdicts)
}

#[cfg(test)]
mod tests;
