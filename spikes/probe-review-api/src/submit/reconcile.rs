//! Сверка элемента в `unknown` с провайдером по таблице отчёта «Отправка и сверка».
//!
//! Сверка читает сервер только методами адаптеров. Сбой чтения — [`Verdict::Unresolved`], а не
//! «не найдено». Итог записывает [`apply`].

use crate::github::{self, GitHub, ReviewEvent, ReviewState};
use crate::gitlab::{self, GitLab, RemotePosition, ReviewerState};
use crate::journal::{
    Confirmation, ItemId, JournalError, Operation, Scope, Submission, SubmitItem,
};
use crate::model::{DraftComment, Placement, ProviderId, ReviewSession};
use crate::transport::Transport;

/// Итог сверки одного элемента.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Связь с операцией и ожидаемый итог известного объекта.
    Confirmed { remote: Option<ProviderId> },
    /// Известный объект прочитан по ID в исходном состоянии: запись не применена.
    Refuted { detail: String },
    /// Один объект совпал по содержимому без связи с операцией.
    Candidate { remote: ProviderId, detail: String },
    /// Совпало несколько объектов.
    Ambiguous { candidates: Vec<ProviderId> },
    /// Ничего связанного не наблюдалось или чтение не удалось.
    Unresolved { detail: String },
}

/// Записывает итог сверки в журнал: подтверждение, опровержение или описание без перехода.
pub fn apply(scope: &Scope<'_>, item: ItemId, verdict: &Verdict) -> Result<(), JournalError> {
    match verdict {
        Verdict::Confirmed { remote } => scope.confirm_with(
            item,
            &Confirmation {
                remote: remote.clone(),
                ..Confirmation::default()
            },
        ),
        Verdict::Refuted { detail } => scope.refute(item, &format!("сверка: {detail}")),
        Verdict::Candidate { remote, detail } => {
            scope.note_reconcile(item, Some(remote), &format!("сверка: кандидат: {detail}"))
        }
        Verdict::Ambiguous { candidates } => {
            let ids: Vec<&str> = candidates.iter().map(ProviderId::as_str).collect();
            scope.note_reconcile(
                item,
                None,
                &format!(
                    "сверка: неоднозначно, совпадений {}: {}",
                    candidates.len(),
                    ids.join(", ")
                ),
            )
        }
        Verdict::Unresolved { detail } => {
            scope.note_reconcile(item, None, &format!("сверка: не выяснено: {detail}"))
        }
    }
}

/// Тело после нормализации GitLab по документации Notes API: без `\r` и хвостовых пробелов и
/// переводов строки в конце.
pub fn normalize_body(body: &str) -> String {
    body.replace('\r', "").trim_end().to_owned()
}

fn same_body(a: &str, b: &str) -> bool {
    normalize_body(a) == normalize_body(b)
}

fn verdict_of(candidates: Vec<ProviderId>, what: &str) -> Verdict {
    let mut candidates = candidates;
    match candidates.len() {
        0 => Verdict::Unresolved {
            detail: format!("{what}: совпадений нет"),
        },
        1 => Verdict::Candidate {
            remote: candidates.remove(0),
            detail: format!("{what} совпал по содержимому без связи с операцией"),
        },
        _ => Verdict::Ambiguous { candidates },
    }
}

fn unread(what: &str, err: impl std::fmt::Display) -> Verdict {
    Verdict::Unresolved {
        detail: format!("чтение {what} не удалось: {err}"),
    }
}

/// Что сверка GitHub знает из журнала.
pub struct GitHubLedger<'a> {
    pub session: &'a ReviewSession,
    pub submission: &'a Submission,
    pub item: &'a SubmitItem,
    /// Черновики элемента.
    pub drafts: &'a [DraftComment],
    /// Node ID своих комментариев `PENDING` из журнала (кроме элемента сверки).
    pub own_comments: &'a [ProviderId],
}

fn event_of(submission: &Submission) -> Option<ReviewEvent> {
    match submission.spec.event.as_deref() {
        Some("COMMENT") => Some(ReviewEvent::Comment),
        Some("APPROVE") => Some(ReviewEvent::Approve),
        Some("REQUEST_CHANGES") => Some(ReviewEvent::RequestChanges),
        _ => None,
    }
}

/// Сессия на ревизии черновика: черновик, который держит элемент, при смене head остаётся на
/// прежней ревизии, и якорь отправленного запроса строится из неё.
fn at_draft(session: &ReviewSession, draft: &DraftComment) -> ReviewSession {
    ReviewSession::new(
        session.account(),
        session.target().clone(),
        draft.revision().clone(),
    )
}

/// Комментарий стоит на якоре черновика с его текстом.
fn github_matches(
    session: &ReviewSession,
    draft: &DraftComment,
    comment: &github::RemoteComment,
) -> bool {
    let Ok(anchor) = at_draft(session, draft).github_anchor(draft) else {
        return false;
    };
    comment.path == anchor.path()
        && comment.line == Some(anchor.line())
        && comment.side.as_deref() == Some(anchor.side())
        && comment.start_line == anchor.start_line()
        && comment.start_side.as_deref() == anchor.start_side()
        && same_body(&comment.body, &draft.body)
}

pub fn reconcile_github<T: Transport>(github: &GitHub<T>, ledger: &GitHubLedger<'_>) -> Verdict {
    let account = ledger.session.account();
    let pending = ledger.submission.spec.pending_review.as_ref();
    match ledger.item.operation {
        Operation::GitHubCreateReview => {
            let Some(event) = event_of(ledger.submission) else {
                return Verdict::Unresolved {
                    detail: "у отправки нет итога review".to_owned(),
                };
            };
            let reviews = match github.reviews() {
                Ok(reviews) => reviews,
                Err(err) => return unread("reviews", err),
            };
            let head = ledger
                .drafts
                .first()
                .map_or(ledger.session.revision(), DraftComment::revision)
                .head_sha()
                .as_str();
            let mut candidates = Vec::new();
            for review in reviews.iter().filter(|review| {
                review.user.eq_ignore_ascii_case(account)
                    && review.state == event.state()
                    && review.commit_id.eq_ignore_ascii_case(head)
                    && same_body(&review.body, &ledger.submission.spec.summary)
            }) {
                let comments = match github.review_comments(&review.id) {
                    Ok(comments) => comments,
                    Err(err) => return unread("комментариев review", err),
                };
                let all = comments.len() == ledger.drafts.len()
                    && ledger.drafts.iter().all(|draft| {
                        comments
                            .iter()
                            .any(|comment| github_matches(ledger.session, draft, comment))
                    });
                if all {
                    candidates.push(review.id.clone());
                }
            }
            verdict_of(candidates, "review")
        }
        Operation::GitHubCreatePending => match github.find_pending(account) {
            Ok(Some(existing)) => Verdict::Candidate {
                remote: existing.id().clone(),
                detail: "PENDING аккаунта есть, его мог создать другой клиент".to_owned(),
            },
            Ok(None) => Verdict::Unresolved {
                detail: "PENDING аккаунта нет".to_owned(),
            },
            Err(err) => unread("PENDING", err),
        },
        Operation::GitHubAddPendingComment | Operation::GitHubUpdatePendingComment => {
            let (Some(review), [draft]) = (pending, ledger.drafts) else {
                return Verdict::Unresolved {
                    detail: "нет PENDING или черновика элемента".to_owned(),
                };
            };
            let comments = match github.review_comments(review) {
                Ok(comments) => comments,
                Err(err) => return unread("комментариев PENDING", err),
            };
            if ledger.item.operation == Operation::GitHubAddPendingComment {
                let candidates = comments
                    .iter()
                    .filter(|comment| !ledger.own_comments.contains(&comment.node_id))
                    .filter(|comment| github_matches(ledger.session, draft, comment))
                    .map(|comment| comment.node_id.clone())
                    .collect();
                return verdict_of(candidates, "комментарий");
            }
            let Some(known) = draft.remote_id.as_ref() else {
                return Verdict::Unresolved {
                    detail: "у черновика нет ID комментария".to_owned(),
                };
            };
            match comments.iter().find(|comment| &comment.node_id == known) {
                Some(comment) if same_body(&comment.body, &draft.body) => Verdict::Confirmed {
                    remote: Some(known.clone()),
                },
                Some(_) => Verdict::Refuted {
                    detail: "у известного комментария прежний текст".to_owned(),
                },
                None => Verdict::Unresolved {
                    detail: "известного комментария нет в PENDING".to_owned(),
                },
            }
        }
        Operation::GitHubSubmitReview => {
            let (Some(review), Some(event)) = (pending, event_of(ledger.submission)) else {
                return Verdict::Unresolved {
                    detail: "нет PENDING или итога review".to_owned(),
                };
            };
            match github.review(review) {
                Ok(found) if found.state == event.state() => Verdict::Confirmed {
                    remote: Some(found.id),
                },
                Ok(found) if found.state == ReviewState::Pending => Verdict::Refuted {
                    detail: "известный review всё ещё PENDING".to_owned(),
                },
                Ok(found) => Verdict::Unresolved {
                    detail: format!(
                        "известный review в состоянии {}, ожидалось {}",
                        found.state.name(),
                        event.state().name()
                    ),
                },
                Err(err) => unread("review", err),
            }
        }
        other => Verdict::Unresolved {
            detail: format!("операция {} не GitHub", other.name()),
        },
    }
}

/// Что сверка GitLab знает из журнала.
pub struct GitLabLedger<'a> {
    pub session: &'a ReviewSession,
    pub submission: &'a Submission,
    pub item: &'a SubmitItem,
    pub drafts: &'a [DraftComment],
    /// ID черновиков GitLab из журнала (кроме элемента сверки).
    pub own_drafts: &'a [ProviderId],
    /// ID заметок из журнала (сводки прежних элементов).
    pub own_notes: &'a [ProviderId],
}

/// Позиция сервера совпадает с позицией, которую строит сессия для черновика.
fn gitlab_position_matches(
    session: &ReviewSession,
    draft: &DraftComment,
    remote: Option<&RemotePosition>,
) -> bool {
    let session = at_draft(session, draft);
    let (Ok(position), Some(remote)) = (session.gitlab_position(draft), remote) else {
        return false;
    };
    let Ok(sent) = serde_json::to_value(&position) else {
        return false;
    };
    let line = |name: &str| sent.get(name).and_then(serde_json::Value::as_u64);
    remote
        .revision()
        .is_ok_and(|triple| session.verify_outgoing(&triple).is_ok())
        && remote.old_path == sent["old_path"].as_str().unwrap_or_default()
        && remote.new_path == sent["new_path"].as_str().unwrap_or_default()
        && remote.old_line.map(u64::from) == line("old_line")
        && remote.new_line.map(u64::from) == line("new_line")
}

pub fn reconcile_gitlab<T: Transport>(gitlab: &GitLab<T>, ledger: &GitLabLedger<'_>) -> Verdict {
    let account = ledger.session.account();
    match (ledger.item.operation, ledger.drafts) {
        (Operation::GitLabCreateDraftNote, [draft]) => {
            let drafts = match gitlab.drafts() {
                Ok(drafts) => drafts,
                Err(err) => return unread("черновиков", err),
            };
            let candidates = drafts
                .iter()
                .filter(|remote| !ledger.own_drafts.contains(&remote.id))
                .filter(|remote| {
                    let placed = match draft.placement() {
                        Placement::Line(_) => {
                            gitlab_position_matches(ledger.session, draft, remote.position.as_ref())
                        }
                        Placement::Reply { thread } => {
                            remote.discussion_id.as_ref() == Some(thread)
                        }
                    };
                    placed && same_body(&remote.note, &draft.body)
                })
                .map(|remote| remote.id.clone())
                .collect();
            verdict_of(candidates, "черновик")
        }
        (Operation::GitLabUpdateDraftNote | Operation::GitLabPublishDraftNote, [draft]) => {
            let Some(known) = draft.remote_id.as_ref() else {
                return Verdict::Unresolved {
                    detail: "у черновика нет ID GitLab".to_owned(),
                };
            };
            let read = gitlab.draft(known);
            if ledger.item.operation == Operation::GitLabUpdateDraftNote {
                if let Placement::Line(_) = draft.placement()
                    && at_draft(ledger.session, draft)
                        .gitlab_position(draft)
                        .is_err()
                {
                    return Verdict::Unresolved {
                        detail: "позиция черновика не строится".to_owned(),
                    };
                }
                return match read {
                    Ok(remote) => {
                        let placed = match draft.placement() {
                            Placement::Line(_) => gitlab_position_matches(
                                ledger.session,
                                draft,
                                remote.position.as_ref(),
                            ),
                            Placement::Reply { thread } => {
                                remote.discussion_id.as_ref() == Some(thread)
                            }
                        };
                        if placed && same_body(&remote.note, &draft.body) {
                            Verdict::Confirmed {
                                remote: Some(known.clone()),
                            }
                        } else {
                            Verdict::Refuted {
                                detail: "у известного черновика прежний текст или позиция"
                                    .to_owned(),
                            }
                        }
                    }
                    Err(err) => unread("черновика", err),
                };
            }
            match read {
                Ok(_) => Verdict::Refuted {
                    detail: "известный черновик всё ещё черновик".to_owned(),
                },
                Err(gitlab::ApiError::Status { status: 404, .. }) => {
                    let discussions = match gitlab.discussions() {
                        Ok(discussions) => discussions,
                        Err(err) => return unread("тредов", err),
                    };
                    let candidates = discussions
                        .iter()
                        .filter(|discussion| match draft.placement() {
                            Placement::Reply { thread } => &discussion.id == thread,
                            Placement::Line(_) => true,
                        })
                        .flat_map(|discussion| discussion.notes.iter())
                        .filter(|note| {
                            !note.system
                                && note.author.eq_ignore_ascii_case(account)
                                && !ledger.own_notes.contains(&note.id)
                                && same_body(&note.body, &draft.body)
                                && match draft.placement() {
                                    Placement::Line(_) => gitlab_position_matches(
                                        ledger.session,
                                        draft,
                                        note.position.as_ref(),
                                    ),
                                    Placement::Reply { .. } => true,
                                }
                        })
                        .map(|note| note.id.clone())
                        .collect();
                    verdict_of(candidates, "заметка")
                }
                Err(err) => unread("черновика", err),
            }
        }
        (Operation::GitLabSummaryNote, []) => match gitlab.notes() {
            Ok(notes) => verdict_of(
                notes
                    .iter()
                    .filter(|note| {
                        !note.system
                            && note.position.is_none()
                            && note.author.eq_ignore_ascii_case(account)
                            && !ledger.own_notes.contains(&note.id)
                            && same_body(&note.body, &ledger.submission.spec.summary)
                    })
                    .map(|note| note.id.clone())
                    .collect(),
                "заметка",
            ),
            Err(err) => unread("заметок", err),
        },
        (Operation::GitLabApprove, []) => match gitlab.approvals() {
            Ok(approval) => {
                let own = approval.user_has_approved.unwrap_or_else(|| {
                    approval
                        .approved_by
                        .iter()
                        .any(|name| name.eq_ignore_ascii_case(account))
                });
                if own {
                    Verdict::Confirmed { remote: None }
                } else {
                    Verdict::Refuted {
                        detail: "одобрения токена нет".to_owned(),
                    }
                }
            }
            Err(err) => unread("одобрений", err),
        },
        (Operation::GitLabRequestChanges, []) => match gitlab.reviewer_state(account) {
            Ok(Some(ReviewerState::RequestedChanges)) => Verdict::Confirmed { remote: None },
            Ok(Some(other)) => Verdict::Refuted {
                detail: format!("состояние ревьюера {}", other.graphql()),
            },
            // Ревьюера нет в списке или у него нет состояния: отсутствие не опровергает запись.
            Ok(None) => Verdict::Unresolved {
                detail: "токен не ревьюер MR или без состояния".to_owned(),
            },
            Err(err) => unread("ревьюеров", err),
        },
        (operation, drafts) => Verdict::Unresolved {
            detail: format!(
                "операция {} с {} черновиками не сверяется сверкой GitLab",
                operation.name(),
                drafts.len()
            ),
        },
    }
}
