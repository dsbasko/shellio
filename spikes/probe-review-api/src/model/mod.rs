//! UI-free модель ревью ADR-0015: цель, ревизия, треды, черновики, сессия.
//!
//! Инвариант: SHA якоря черновика и запроса отправки берутся только из ревизии сессии. Черновик
//! запоминает ревизию, на которой поставлен якорь; якорь провайдера и позицию GitLab строит только
//! сессия, копируя все три SHA из своей ревизии, и отказывает черновику с другой ревизией или с
//! устаревшим якорем. Ревизия меняется только целиком, через выбранный путь смены head.

pub mod anchor;
pub mod diff;
pub mod revision;

use std::fmt;

use anchor::{Anchor, GitHubAnchor, GitLabPosition, Side};
use diff::{FileDiff, LineKind};
use revision::{HeadChange, HeadCheck, HeadPath, Revision, RevisionField, StaleReason};

/// ID объекта у провайдера (REST ID, node ID GraphQL, ID заметки) — непрозрачная строка.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProviderId(String);

impl ProviderId {
    pub fn new(value: impl Into<String>) -> ProviderId {
        ProviderId(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Provider {
    GitHub,
    GitLab,
}

impl Provider {
    pub fn name(self) -> &'static str {
        match self {
            Provider::GitHub => "GitHub",
            Provider::GitLab => "GitLab",
        }
    }
}

/// PR GitHub или MR GitLab: хост, путь репозитория (проекта), номер (`number` или `iid`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ReviewTarget {
    pub provider: Provider,
    pub host: String,
    pub repo: String,
    pub number: u64,
}

/// Тред на сервере: ID, якорь (если тред к строке), состояние resolve, ID комментариев.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Thread {
    pub id: ProviderId,
    pub anchor: Option<Anchor>,
    pub resolved: bool,
    pub comment_ids: Vec<ProviderId>,
}

/// Локальный ID черновика в журнале.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DraftId(pub u64);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Placement {
    /// Новый тред к строке или диапазону diff.
    Line(Anchor),
    /// Ответ в существующем треде: якоря нет, смена head его не задевает.
    Reply { thread: ProviderId },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnchorState {
    Current,
    Stale(StaleReason),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DraftComment {
    pub id: DraftId,
    pub body: String,
    pub remote_id: Option<ProviderId>,
    placement: Placement,
    revision: Revision,
    state: AnchorState,
}

impl DraftComment {
    /// Черновик на ревизии сессии: другую ревизию черновику передать нельзя.
    pub fn new(
        id: DraftId,
        session: &ReviewSession,
        placement: Placement,
        body: impl Into<String>,
    ) -> DraftComment {
        DraftComment {
            id,
            body: body.into(),
            remote_id: None,
            placement,
            revision: session.revision.clone(),
            state: AnchorState::Current,
        }
    }

    /// Черновик из журнала с записанными ревизией и состоянием якоря. Отправку по-прежнему сверяет
    /// сессия: черновик другой ревизии или с устаревшим якорем она отклонит.
    pub(crate) fn restore(
        id: DraftId,
        body: String,
        remote_id: Option<ProviderId>,
        placement: Placement,
        revision: Revision,
        state: AnchorState,
    ) -> DraftComment {
        DraftComment {
            id,
            body,
            remote_id,
            placement,
            revision,
            state,
        }
    }

    pub fn placement(&self) -> &Placement {
        &self.placement
    }

    /// Ревизия, на которой поставлен якорь.
    pub fn revision(&self) -> &Revision {
        &self.revision
    }

    pub fn state(&self) -> AnchorState {
        self.state
    }

    pub fn is_current(&self) -> bool {
        self.state == AnchorState::Current
    }

    fn rebase(&mut self, revision: Revision) {
        self.revision = revision;
        self.state = AnchorState::Current;
    }

    fn mark_stale(&mut self, reason: StaleReason) {
        self.state = AnchorState::Stale(reason);
    }
}

/// Сессия ревью: аккаунт, цель (хост и репозиторий) и закреплённая ревизия.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewSession {
    account: String,
    target: ReviewTarget,
    revision: Revision,
}

impl ReviewSession {
    pub fn new(account: impl Into<String>, target: ReviewTarget, revision: Revision) -> Self {
        ReviewSession {
            account: account.into(),
            target,
            revision,
        }
    }

    pub fn account(&self) -> &str {
        &self.account
    }

    pub fn target(&self) -> &ReviewTarget {
        &self.target
    }

    pub fn revision(&self) -> &Revision {
        &self.revision
    }

    fn anchored<'a>(
        &self,
        draft: &'a DraftComment,
        provider: Provider,
    ) -> Result<&'a Anchor, ModelError> {
        if self.target.provider != provider {
            return Err(ModelError::Provider {
                expected: provider,
                found: self.target.provider,
            });
        }
        if let AnchorState::Stale(reason) = draft.state {
            return Err(ModelError::Stale { reason });
        }
        if draft.revision != self.revision {
            return Err(ModelError::RevisionMismatch {
                fields: self.revision.differing_fields(&draft.revision),
            });
        }
        match &draft.placement {
            Placement::Line(anchor) => Ok(anchor),
            Placement::Reply { .. } => Err(ModelError::NotAnchored),
        }
    }

    /// Якорь GitHub: `commit_id` — `head_sha` ревизии сессии.
    pub fn github_anchor(&self, draft: &DraftComment) -> Result<GitHubAnchor, ModelError> {
        let anchor = self.anchored(draft, Provider::GitHub)?;
        Ok(anchor.github(self.revision.head_sha()))
    }

    /// Позиция GitLab: тройка SHA — ревизия сессии целиком.
    pub fn gitlab_position(&self, draft: &DraftComment) -> Result<GitLabPosition, ModelError> {
        let anchor = self.anchored(draft, Provider::GitLab)?;
        Ok(anchor.gitlab(&self.revision))
    }

    /// Сверка тройки SHA, попавшей в исходящий запрос, с ревизией сессии.
    pub fn verify_outgoing(&self, triple: &Revision) -> Result<(), ModelError> {
        let fields = self.revision.differing_fields(triple);
        if fields.is_empty() {
            Ok(())
        } else {
            Err(ModelError::RevisionMismatch { fields })
        }
    }

    /// Шаг 1 отправки: свежая ревизия против ревизии сессии и судьба якорей черновиков.
    pub fn check_head(
        &self,
        fresh: &Revision,
        held_diff: &[FileDiff],
        fresh_diff: &[FileDiff],
        drafts: &[DraftComment],
    ) -> HeadCheck {
        revision::check_head(&self.revision, fresh, held_diff, fresh_diff, drafts)
    }

    pub fn apply_head(
        &mut self,
        change: &HeadChange,
        path: HeadPath,
        drafts: &mut [DraftComment],
        fresh_diff: &[FileDiff],
    ) -> Result<(), ModelError> {
        revision::apply_head(&mut self.revision, change, path, drafts, fresh_diff)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelError {
    Sha { value: String },
    Patch { line: usize, reason: String },
    Side { side: Side, kind: LineKind },
    AnchorMissing { path: String, side: Side, line: u32 },
    RangeOrder,
    RangeHunk,
    Provider { expected: Provider, found: Provider },
    RevisionMismatch { fields: Vec<RevisionField> },
    Stale { reason: StaleReason },
    NotAnchored,
    RebindNotStale { draft: DraftId },
}

impl fmt::Display for ModelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ModelError::Sha { value } => write!(f, "неверный SHA «{value}»"),
            ModelError::Patch { line, reason } => write!(f, "diff, строка {line}: {reason}"),
            ModelError::Side { side, kind } => {
                write!(f, "строка {kind:?} не лежит на стороне {side:?}")
            }
            ModelError::AnchorMissing { path, side, line } => {
                write!(f, "в diff {path} нет строки {line} стороны {side:?}")
            }
            ModelError::RangeOrder => f.write_str("начало диапазона не раньше конца"),
            ModelError::RangeHunk => f.write_str("начало и конец диапазона в разных блоках"),
            ModelError::Provider { expected, found } => {
                write!(f, "якорь {} для сессии {}", expected.name(), found.name())
            }
            ModelError::RevisionMismatch { fields } => {
                let names: Vec<&str> = fields.iter().map(|field| field.name()).collect();
                write!(
                    f,
                    "ревизия не совпадает с ревизией сессии: {}",
                    names.join(", ")
                )
            }
            ModelError::Stale { reason } => {
                write!(f, "якорь устарел: {}", reason.describe())
            }
            ModelError::NotAnchored => f.write_str("у черновика нет якоря"),
            ModelError::RebindNotStale { draft } => {
                write!(f, "черновик {} не устарел, перепривязка не нужна", draft.0)
            }
        }
    }
}

impl std::error::Error for ModelError {}

#[cfg(test)]
mod tests;
