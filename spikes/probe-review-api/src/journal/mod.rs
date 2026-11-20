//! Журнал черновиков ADR-0015 в `state.db` (SQLite в режиме WAL, ADR-0018): сессии, черновики и
//! элементы отправки.
//!
//! Черновик записывается сразу, каждый переход элемента отправки — отдельной фиксацией до
//! следующего шага. Отметка `in_flight` ложится на диск до запроса, поэтому после сбоя журнал
//! знает, что запрос мог уйти. Переход — сравнение с заменой одним `UPDATE`: элемент меняет
//! состояние, только если сейчас он в допустимом предшественнике.
//!
//! Открытие журнала — начало процесса: `in_flight` прежнего открытия ответа уже не получит и
//! становится `unknown` с сохранённой отметкой входа. Из `unknown` элемент выходит только в
//! `confirmed` по сверке с провайдером; обратно в `in_flight` (слепой повтор) — нет.
//!
//! Черновики принадлежат проекту (ADR-0025): чтение и запись идут через [`Scope`] проекта, и
//! объект другого проекта для него не существует, даже если цель на сервере та же.

use std::fmt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{
    Connection, OptionalExtension, Row, Transaction, TransactionBehavior, named_params, params,
};

use crate::model::anchor::{Anchor, LinePoint, Side};
use crate::model::diff::{DiffLine, LineKind};
use crate::model::revision::{Revision, Sha, StaleReason};
use crate::model::{
    AnchorState, DraftComment, DraftId, Placement, Provider, ProviderId, ReviewSession,
    ReviewTarget,
};

const SCHEMA: &str = include_str!("schema.sql");
const SCHEMA_VERSION: i64 = 2;
pub const RECOVERED_DETAIL: &str = "переоткрытие журнала: ответ на запрос не получен";

/// Проект каталога (ADR-0025). Ключ корня проекта — непрозрачная строка.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ProjectId(String);

impl ProjectId {
    pub fn new(value: impl Into<String>) -> ProjectId {
        ProjectId(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SubmissionId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ItemId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ItemState {
    Planned,
    InFlight,
    Confirmed,
    Unknown,
    Failed,
}

impl ItemState {
    pub const ALL: [ItemState; 5] = [
        ItemState::Planned,
        ItemState::InFlight,
        ItemState::Confirmed,
        ItemState::Unknown,
        ItemState::Failed,
    ];

    pub fn name(self) -> &'static str {
        match self {
            ItemState::Planned => "planned",
            ItemState::InFlight => "in_flight",
            ItemState::Confirmed => "confirmed",
            ItemState::Unknown => "unknown",
            ItemState::Failed => "failed",
        }
    }

    pub fn from_name(name: &str) -> Option<ItemState> {
        ItemState::ALL.into_iter().find(|s| s.name() == name)
    }

    /// Допустимые предшественники. `failed` — запись явно не принята (отказ, лимит);
    /// `unknown` — запрос мог быть принят, выход только в `confirmed` по сверке.
    pub fn predecessors(self) -> &'static [ItemState] {
        match self {
            ItemState::Planned => &[],
            ItemState::InFlight => &[ItemState::Planned],
            ItemState::Confirmed => &[ItemState::InFlight, ItemState::Unknown],
            ItemState::Unknown => &[ItemState::InFlight],
            ItemState::Failed => &[ItemState::InFlight],
        }
    }

    pub fn can_become(self, next: ItemState) -> bool {
        next.predecessors().contains(&self)
    }

    /// Итог известен: запись подтверждена или явно не принята.
    pub fn is_finished(self) -> bool {
        match self {
            ItemState::Confirmed | ItemState::Failed => true,
            ItemState::Planned | ItemState::InFlight | ItemState::Unknown => false,
        }
    }
}

/// Сколько черновиков несёт запрос.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DraftArity {
    None,
    One,
    Any,
}

/// Запрос записи к провайдеру по ADR-0015.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Operation {
    /// Только локальные черновики: один Create review с `comments[]` и `event`.
    GitHubCreateReview,
    /// Серверный `PENDING`: Create review без `event`.
    GitHubCreatePending,
    GitHubAddPendingComment,
    GitHubUpdatePendingComment,
    /// Submit известного `PENDING` без `comments[]`.
    GitHubSubmitReview,
    GitLabCreateDraftNote,
    GitLabUpdateDraftNote,
    /// Публикация одного известного `draft_note` по ID.
    GitLabPublishDraftNote,
    /// Сводка отдельной заметкой.
    GitLabSummaryNote,
    GitLabApprove,
    GitLabRequestChanges,
}

impl Operation {
    pub const ALL: [Operation; 11] = [
        Operation::GitHubCreateReview,
        Operation::GitHubCreatePending,
        Operation::GitHubAddPendingComment,
        Operation::GitHubUpdatePendingComment,
        Operation::GitHubSubmitReview,
        Operation::GitLabCreateDraftNote,
        Operation::GitLabUpdateDraftNote,
        Operation::GitLabPublishDraftNote,
        Operation::GitLabSummaryNote,
        Operation::GitLabApprove,
        Operation::GitLabRequestChanges,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Operation::GitHubCreateReview => "github.create_review",
            Operation::GitHubCreatePending => "github.create_pending",
            Operation::GitHubAddPendingComment => "github.add_pending_comment",
            Operation::GitHubUpdatePendingComment => "github.update_pending_comment",
            Operation::GitHubSubmitReview => "github.submit_review",
            Operation::GitLabCreateDraftNote => "gitlab.create_draft_note",
            Operation::GitLabUpdateDraftNote => "gitlab.update_draft_note",
            Operation::GitLabPublishDraftNote => "gitlab.publish_draft_note",
            Operation::GitLabSummaryNote => "gitlab.summary_note",
            Operation::GitLabApprove => "gitlab.approve",
            Operation::GitLabRequestChanges => "gitlab.request_changes",
        }
    }

    pub fn from_name(name: &str) -> Option<Operation> {
        Operation::ALL.into_iter().find(|o| o.name() == name)
    }

    pub fn provider(self) -> Provider {
        match self {
            Operation::GitHubCreateReview
            | Operation::GitHubCreatePending
            | Operation::GitHubAddPendingComment
            | Operation::GitHubUpdatePendingComment
            | Operation::GitHubSubmitReview => Provider::GitHub,
            Operation::GitLabCreateDraftNote
            | Operation::GitLabUpdateDraftNote
            | Operation::GitLabPublishDraftNote
            | Operation::GitLabSummaryNote
            | Operation::GitLabApprove
            | Operation::GitLabRequestChanges => Provider::GitLab,
        }
    }

    /// Операция над известным объектом по его ID: повтор не создаёт второго объекта (второй
    /// Submit и вторая публикация отвергаются, изменение и одобрение идемпотентны). Только такой
    /// элемент сверка может перевести из `unknown` в `failed` по чтению объекта в исходном
    /// состоянии ([`Scope::refute`]).
    pub fn targets_known_object(self) -> bool {
        match self {
            Operation::GitHubUpdatePendingComment
            | Operation::GitHubSubmitReview
            | Operation::GitLabUpdateDraftNote
            | Operation::GitLabPublishDraftNote
            | Operation::GitLabApprove
            | Operation::GitLabRequestChanges => true,
            Operation::GitHubCreateReview
            | Operation::GitHubCreatePending
            | Operation::GitHubAddPendingComment
            | Operation::GitLabCreateDraftNote
            | Operation::GitLabSummaryNote => false,
        }
    }

    pub fn drafts(self) -> DraftArity {
        match self {
            Operation::GitHubCreateReview => DraftArity::Any,
            Operation::GitHubAddPendingComment
            | Operation::GitHubUpdatePendingComment
            | Operation::GitLabCreateDraftNote
            | Operation::GitLabUpdateDraftNote
            | Operation::GitLabPublishDraftNote => DraftArity::One,
            Operation::GitHubCreatePending
            | Operation::GitHubSubmitReview
            | Operation::GitLabSummaryNote
            | Operation::GitLabApprove
            | Operation::GitLabRequestChanges => DraftArity::None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedItem {
    pub operation: Operation,
    pub drafts: Vec<DraftId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubmitItem {
    pub id: ItemId,
    pub submission: SubmissionId,
    pub seq: u32,
    pub operation: Operation,
    pub drafts: Vec<DraftId>,
    pub state: ItemState,
    pub remote_id: Option<ProviderId>,
    /// Объект, совпавший по содержимому без связи с операцией: кандидат, а не подтверждение.
    pub candidate: Option<ProviderId>,
    pub detail: Option<String>,
    /// Вход в `in_flight`, мс Unix.
    pub in_flight_at: Option<i64>,
}

/// Параметры отправки, нужные сверке после переоткрытия журнала.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SubmissionSpec {
    /// Итог review GitHub (`COMMENT`, `APPROVE`, `REQUEST_CHANGES`).
    pub event: Option<String>,
    /// Текст итога review GitHub или сводки GitLab.
    pub summary: String,
    /// `PENDING`, с которым работает отправка: принятый или созданный ею (числовой ID).
    pub pending_review: Option<ProviderId>,
    /// Чужие комментарии принятого `PENDING`, показанные при принятии; `None` — `PENDING` свой.
    pub pending_foreign: Option<Vec<ProviderId>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Submission {
    pub id: SubmissionId,
    pub session: SessionId,
    pub spec: SubmissionSpec,
    /// Отправка остановлена: элементы в `planned` не отправляются, их черновики свободны.
    pub closed: bool,
    pub items: Vec<SubmitItem>,
}

/// Подтверждение элемента вместе с удалёнными ID шага 4 ADR-0015 — одной фиксацией: сбой между
/// ними оставил бы подтверждённый элемент и черновик без ID, то есть дубль при продолжении.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Confirmation {
    pub remote: Option<ProviderId>,
    pub drafts: Vec<(DraftId, ProviderId)>,
    /// ID `PENDING`, созданного элементом: отправка дальше работает с ним.
    pub pending_review: Option<ProviderId>,
}

#[derive(Debug)]
pub enum JournalError {
    Sqlite(rusqlite::Error),
    /// Режим WAL не включился.
    JournalMode {
        found: String,
    },
    Version {
        found: i64,
    },
    NotFound {
        what: &'static str,
        id: u64,
    },
    Transition {
        item: ItemId,
        from: ItemState,
        to: ItemState,
    },
    /// Кандидат записывается только элементу в `unknown`.
    Candidate {
        item: ItemId,
        state: ItemState,
    },
    OperationProvider {
        operation: Operation,
        session: Provider,
    },
    Drafts {
        operation: Operation,
        found: usize,
    },
    DuplicateDraft {
        draft: DraftId,
    },
    /// Сохранить можно только ревизию той же сессии: аккаунт и цель не меняются.
    SessionIdentity {
        session: SessionId,
    },
    /// Черновик держит элемент: в `in_flight` или `unknown` (до сверки) или в `planned`
    /// незакрытой отправки. Он не входит в новую отправку, его текст и якорь не меняются.
    DraftHeld {
        draft: DraftId,
        item: ItemId,
        state: ItemState,
    },
    /// Сверка опровергает только операцию над известным объектом (`Operation::targets_known_object`).
    Refute {
        item: ItemId,
        operation: Operation,
    },
    /// Закрытая отправка не продолжается.
    Closed {
        submission: SubmissionId,
    },
    Corrupt {
        reason: String,
    },
    Clock,
}

impl fmt::Display for JournalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JournalError::Sqlite(err) => write!(f, "SQLite: {err}"),
            JournalError::JournalMode { found } => {
                write!(f, "режим журнала SQLite «{found}» вместо WAL")
            }
            JournalError::Version { found } => write!(
                f,
                "версия схемы журнала {found}, проба знает {SCHEMA_VERSION}"
            ),
            JournalError::NotFound { what, id } => write!(f, "{what} {id} не найден в проекте"),
            JournalError::Transition { item, from, to } => write!(
                f,
                "элемент {}: переход {} → {} недопустим",
                item.0,
                from.name(),
                to.name()
            ),
            JournalError::Candidate { item, state } => write!(
                f,
                "элемент {} в состоянии {}: кандидат записывается только в unknown",
                item.0,
                state.name()
            ),
            JournalError::OperationProvider { operation, session } => write!(
                f,
                "операция {} в сессии {}",
                operation.name(),
                session.name()
            ),
            JournalError::Drafts { operation, found } => write!(
                f,
                "операция {} не несёт {found} черновиков",
                operation.name()
            ),
            JournalError::DuplicateDraft { draft } => {
                write!(f, "черновик {} дважды в одном элементе", draft.0)
            }
            JournalError::SessionIdentity { session } => write!(
                f,
                "сессия {}: аккаунт и цель не меняются, меняется только ревизия",
                session.0
            ),
            JournalError::DraftHeld { draft, item, state } => write!(
                f,
                "черновик {} держит элемент {} в состоянии {}",
                draft.0,
                item.0,
                state.name()
            ),
            JournalError::Refute { item, operation } => write!(
                f,
                "элемент {}: операция {} создаёт объект, сверка её не опровергает",
                item.0,
                operation.name()
            ),
            JournalError::Closed { submission } => {
                write!(f, "отправка {} закрыта", submission.0)
            }
            JournalError::Corrupt { reason } => write!(f, "повреждённая запись журнала: {reason}"),
            JournalError::Clock => f.write_str("системное время раньше эпохи Unix"),
        }
    }
}

impl std::error::Error for JournalError {}

impl From<rusqlite::Error> for JournalError {
    fn from(err: rusqlite::Error) -> JournalError {
        JournalError::Sqlite(err)
    }
}

fn corrupt(reason: impl Into<String>) -> JournalError {
    JournalError::Corrupt {
        reason: reason.into(),
    }
}

pub struct Journal {
    conn: Connection,
    recovered: Vec<ItemId>,
}

impl Journal {
    /// Открывает или создаёт журнал и восстанавливает незавершённую отправку прежнего открытия.
    pub fn open(path: &Path) -> Result<Journal, JournalError> {
        let conn = Connection::open(path)?;
        let mode: String =
            conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(JournalError::JournalMode { found: mode });
        }
        // Каждая фиксация синхронизируется до возврата; без fullfsync macOS кэш накопителя не сбрасывает.
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        migrate(&conn)?;
        let recovered = recover(&conn)?;
        Ok(Journal { conn, recovered })
    }

    /// Элементы всех проектов, переведённые при открытии из `in_flight` в `unknown`.
    pub fn recovered(&self) -> &[ItemId] {
        &self.recovered
    }

    pub fn scope(&self, project: &ProjectId) -> Scope<'_> {
        Scope {
            conn: &self.conn,
            project: project.clone(),
        }
    }
}

fn migrate(conn: &Connection) -> Result<(), JournalError> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let version: i64 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    match version {
        0 => {
            tx.execute_batch(SCHEMA)?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        SCHEMA_VERSION => {}
        found => return Err(JournalError::Version { found }),
    }
    tx.commit()?;
    Ok(())
}

fn recover(conn: &Connection) -> Result<Vec<ItemId>, JournalError> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let ids = {
        let mut stmt = tx.prepare("SELECT id FROM submit_items WHERE state = ?1 ORDER BY id")?;
        stmt.query_map([ItemState::InFlight.name()], |row| row.get(0))?
            .map(|id| id.map(ItemId))
            .collect::<Result<Vec<_>, _>>()?
    };
    tx.execute(
        "UPDATE submit_items SET state = ?1, detail = ?2 WHERE state = ?3",
        params![
            ItemState::Unknown.name(),
            RECOVERED_DETAIL,
            ItemState::InFlight.name()
        ],
    )?;
    tx.commit()?;
    Ok(ids)
}

fn now_ms() -> Result<i64, JournalError> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| JournalError::Clock)?;
    i64::try_from(elapsed.as_millis()).map_err(|_| JournalError::Clock)
}

/// Журнал, видимый из одного проекта.
pub struct Scope<'j> {
    conn: &'j Connection,
    project: ProjectId,
}

const SESSION_COLUMNS: &str =
    "id, account, provider, host, repo, number, base_sha, start_sha, head_sha";

const DRAFT_COLUMNS: &str = "d.id, d.body, d.remote_id, d.base_sha, d.start_sha, d.head_sha, \
     d.anchor_state, d.placement, d.reply_thread, d.old_path, d.new_path, d.start_side, \
     d.start_kind, d.start_old, d.start_new, d.end_side, d.end_kind, d.end_old, d.end_new";

const ITEM_COLUMNS: &str = "i.id, i.submission_id, i.seq, i.operation, i.state, i.remote_id, \
     i.candidate, i.detail, i.in_flight_at";

/// Элементы отправок проекта: подзапрос для `submission_id IN (…)`.
const PROJECT_SUBMISSIONS: &str = "SELECT s.id FROM submissions s \
     JOIN sessions p ON p.id = s.session_id WHERE p.project = :project";

impl Scope<'_> {
    pub fn project(&self) -> &ProjectId {
        &self.project
    }

    pub fn create_session(&self, session: &ReviewSession) -> Result<SessionId, JournalError> {
        let target = session.target();
        let revision = session.revision();
        self.conn.execute(
            "INSERT INTO sessions (project, account, provider, host, repo, number, base_sha, \
             start_sha, head_sha) VALUES (:project, :account, :provider, :host, :repo, :number, \
             :base, :start, :head)",
            named_params! {
                ":project": self.project.as_str(),
                ":account": session.account(),
                ":provider": provider_name(target.provider),
                ":host": target.host,
                ":repo": target.repo,
                ":number": target.number,
                ":base": revision.base_sha().as_str(),
                ":start": revision.start_sha().as_str(),
                ":head": revision.head_sha().as_str(),
            },
        )?;
        Ok(SessionId(row_id(self.conn.last_insert_rowid())?))
    }

    pub fn session(&self, id: SessionId) -> Result<ReviewSession, JournalError> {
        load_session(self.conn, &self.project, id)
    }

    pub fn sessions(&self) -> Result<Vec<(SessionId, ReviewSession)>, JournalError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {SESSION_COLUMNS} FROM sessions WHERE project = ?1 ORDER BY id"
        ))?;
        let rows = stmt
            .query_map([self.project.as_str()], SessionRow::read)?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|row| Ok((SessionId(row.id), row.into_session()?)))
            .collect()
    }

    /// Сохраняет итог выбранного пути смены head (`ReviewSession::apply_head`): ревизию сессии и
    /// ревизию, состояние и якорь переданных черновиков — одной фиксацией.
    pub fn save_head(
        &self,
        id: SessionId,
        session: &ReviewSession,
        drafts: &[DraftComment],
    ) -> Result<(), JournalError> {
        let tx = Transaction::new_unchecked(self.conn, TransactionBehavior::Immediate)?;
        let stored = load_session(&tx, &self.project, id)?;
        if stored.account() != session.account() || stored.target() != session.target() {
            return Err(JournalError::SessionIdentity { session: id });
        }
        let revision = session.revision();
        tx.execute(
            "UPDATE sessions SET base_sha = ?1, start_sha = ?2, head_sha = ?3 WHERE id = ?4",
            params![
                revision.base_sha().as_str(),
                revision.start_sha().as_str(),
                revision.head_sha().as_str(),
                id.0
            ],
        )?;
        for draft in drafts {
            if let Some((item, state)) = held_by(&tx, draft.id, false)? {
                return Err(JournalError::DraftHeld {
                    draft: draft.id,
                    item,
                    state,
                });
            }
            let columns = PlacementColumns::of(draft.placement());
            let draft_revision = draft.revision();
            let changed = tx.execute(
                "UPDATE drafts SET base_sha = :base, start_sha = :start, head_sha = :head, \
                 anchor_state = :state, placement = :placement, reply_thread = :reply, \
                 old_path = :old_path, new_path = :new_path, start_side = :start_side, \
                 start_kind = :start_kind, start_old = :start_old, start_new = :start_new, \
                 end_side = :end_side, end_kind = :end_kind, end_old = :end_old, \
                 end_new = :end_new WHERE id = :id AND session_id = :session",
                named_params! {
                    ":base": draft_revision.base_sha().as_str(),
                    ":start": draft_revision.start_sha().as_str(),
                    ":head": draft_revision.head_sha().as_str(),
                    ":state": anchor_state_name(draft.state()),
                    ":placement": columns.placement,
                    ":reply": columns.reply_thread,
                    ":old_path": columns.old_path,
                    ":new_path": columns.new_path,
                    ":start_side": columns.start_side,
                    ":start_kind": columns.start_kind,
                    ":start_old": columns.start_old,
                    ":start_new": columns.start_new,
                    ":end_side": columns.end_side,
                    ":end_kind": columns.end_kind,
                    ":end_old": columns.end_old,
                    ":end_new": columns.end_new,
                    ":id": draft.id.0,
                    ":session": id.0,
                },
            )?;
            if changed != 1 {
                return Err(JournalError::NotFound {
                    what: "черновик",
                    id: draft.id.0,
                });
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Новый черновик на ревизии сессии, записанный до возврата.
    pub fn add_draft(
        &self,
        session: SessionId,
        placement: Placement,
        body: &str,
    ) -> Result<DraftComment, JournalError> {
        let tx = Transaction::new_unchecked(self.conn, TransactionBehavior::Immediate)?;
        let review = load_session(&tx, &self.project, session)?;
        let columns = PlacementColumns::of(&placement);
        let revision = review.revision();
        tx.execute(
            "INSERT INTO drafts (session_id, body, base_sha, start_sha, head_sha, anchor_state, \
             placement, reply_thread, old_path, new_path, start_side, start_kind, start_old, \
             start_new, end_side, end_kind, end_old, end_new) VALUES (:session, :body, :base, \
             :start, :head, :state, :placement, :reply, :old_path, :new_path, :start_side, \
             :start_kind, :start_old, :start_new, :end_side, :end_kind, :end_old, :end_new)",
            named_params! {
                ":session": session.0,
                ":body": body,
                ":base": revision.base_sha().as_str(),
                ":start": revision.start_sha().as_str(),
                ":head": revision.head_sha().as_str(),
                ":state": anchor_state_name(AnchorState::Current),
                ":placement": columns.placement,
                ":reply": columns.reply_thread,
                ":old_path": columns.old_path,
                ":new_path": columns.new_path,
                ":start_side": columns.start_side,
                ":start_kind": columns.start_kind,
                ":start_old": columns.start_old,
                ":start_new": columns.start_new,
                ":end_side": columns.end_side,
                ":end_kind": columns.end_kind,
                ":end_old": columns.end_old,
                ":end_new": columns.end_new,
            },
        )?;
        let id = DraftId(row_id(tx.last_insert_rowid())?);
        tx.commit()?;
        Ok(DraftComment::new(id, &review, placement, body))
    }

    /// Новый текст черновика. Черновик с элементом в `in_flight` или `unknown` не меняется до
    /// сверки: иначе сверка искала бы по содержимому другой текст.
    pub fn update_draft_body(&self, draft: DraftId, body: &str) -> Result<(), JournalError> {
        self.draft(draft)?;
        if let Some((item, state)) = held_by(self.conn, draft, false)? {
            return Err(JournalError::DraftHeld { draft, item, state });
        }
        let changed = self.conn.execute(
            "UPDATE drafts SET body = :body WHERE id = :id AND session_id IN \
             (SELECT id FROM sessions WHERE project = :project)",
            named_params! {
                ":body": body,
                ":id": draft.0,
                ":project": self.project.as_str(),
            },
        )?;
        expect_one(changed, "черновик", draft.0)
    }

    pub fn set_draft_remote_id(
        &self,
        draft: DraftId,
        remote: &ProviderId,
    ) -> Result<(), JournalError> {
        let changed = self.conn.execute(
            "UPDATE drafts SET remote_id = :remote WHERE id = :id AND session_id IN \
             (SELECT id FROM sessions WHERE project = :project)",
            named_params! {
                ":remote": remote.as_str(),
                ":id": draft.0,
                ":project": self.project.as_str(),
            },
        )?;
        expect_one(changed, "черновик", draft.0)
    }

    pub fn draft(&self, id: DraftId) -> Result<DraftComment, JournalError> {
        let row = self
            .conn
            .query_row(
                &format!(
                    "SELECT {DRAFT_COLUMNS} FROM drafts d JOIN sessions s ON s.id = d.session_id \
                     WHERE d.id = ?1 AND s.project = ?2"
                ),
                params![id.0, self.project.as_str()],
                DraftRow::read,
            )
            .optional()?
            .ok_or(JournalError::NotFound {
                what: "черновик",
                id: id.0,
            })?;
        row.into_draft()
    }

    pub fn drafts(&self, session: SessionId) -> Result<Vec<DraftComment>, JournalError> {
        load_session(self.conn, &self.project, session)?;
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {DRAFT_COLUMNS} FROM drafts d WHERE d.session_id = ?1 ORDER BY d.id"
        ))?;
        let rows = stmt
            .query_map([session.0], DraftRow::read)?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter().map(DraftRow::into_draft).collect()
    }

    /// Шаг 2 отправки без параметров (отправка GitLab без сводки, тесты журнала).
    pub fn plan_submission(
        &self,
        session: SessionId,
        items: &[PlannedItem],
    ) -> Result<Submission, JournalError> {
        self.plan(session, &SubmissionSpec::default(), items)
    }

    /// Шаг 2 отправки: конкретный список запросов, все в `planned`, с параметрами одной
    /// фиксацией. Черновик, который держит элемент другой отправки, не входит.
    pub fn plan(
        &self,
        session: SessionId,
        spec: &SubmissionSpec,
        items: &[PlannedItem],
    ) -> Result<Submission, JournalError> {
        let tx = Transaction::new_unchecked(self.conn, TransactionBehavior::Immediate)?;
        let review = load_session(&tx, &self.project, session)?;
        let provider = review.target().provider;
        for item in items {
            if item.operation.provider() != provider {
                return Err(JournalError::OperationProvider {
                    operation: item.operation,
                    session: provider,
                });
            }
            let fits = match item.operation.drafts() {
                DraftArity::None => item.drafts.is_empty(),
                DraftArity::One => item.drafts.len() == 1,
                DraftArity::Any => true,
            };
            if !fits {
                return Err(JournalError::Drafts {
                    operation: item.operation,
                    found: item.drafts.len(),
                });
            }
            for (index, draft) in item.drafts.iter().enumerate() {
                if item.drafts[..index].contains(draft) {
                    return Err(JournalError::DuplicateDraft { draft: *draft });
                }
                let known = tx
                    .query_row(
                        "SELECT 1 FROM drafts WHERE id = ?1 AND session_id = ?2",
                        params![draft.0, session.0],
                        |_| Ok(()),
                    )
                    .optional()?;
                if known.is_none() {
                    return Err(JournalError::NotFound {
                        what: "черновик",
                        id: draft.0,
                    });
                }
                if let Some((item, state)) = held_by(&tx, *draft, true)? {
                    return Err(JournalError::DraftHeld {
                        draft: *draft,
                        item,
                        state,
                    });
                }
            }
        }
        let foreign = spec
            .pending_foreign
            .as_ref()
            .map(|ids| {
                serde_json::to_string(&ids.iter().map(ProviderId::as_str).collect::<Vec<_>>())
            })
            .transpose()
            .map_err(|err| corrupt(err.to_string()))?;
        tx.execute(
            "INSERT INTO submissions (session_id, event, summary, pending_review, pending_foreign) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                session.0,
                spec.event,
                spec.summary,
                spec.pending_review.as_ref().map(ProviderId::as_str),
                foreign
            ],
        )?;
        let submission = SubmissionId(row_id(tx.last_insert_rowid())?);
        for (seq, item) in items.iter().enumerate() {
            tx.execute(
                "INSERT INTO submit_items (submission_id, seq, operation, state) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    submission.0,
                    seq,
                    item.operation.name(),
                    ItemState::Planned.name()
                ],
            )?;
            let item_id = tx.last_insert_rowid();
            for draft in &item.drafts {
                tx.execute(
                    "INSERT INTO submit_item_drafts (item_id, draft_id) VALUES (?1, ?2)",
                    params![item_id, draft.0],
                )?;
            }
        }
        tx.commit()?;
        self.submission(submission)
    }

    pub fn submission(&self, id: SubmissionId) -> Result<Submission, JournalError> {
        type Row = (
            u64,
            Option<String>,
            String,
            Option<String>,
            Option<String>,
            bool,
        );
        let (session, event, summary, pending_review, foreign, closed): Row = self
            .conn
            .query_row(
                "SELECT s.session_id, s.event, s.summary, s.pending_review, s.pending_foreign, \
                 s.closed FROM submissions s JOIN sessions p ON p.id = s.session_id \
                 WHERE s.id = ?1 AND p.project = ?2",
                params![id.0, self.project.as_str()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .optional()?
            .ok_or(JournalError::NotFound {
                what: "отправка",
                id: id.0,
            })?;
        let pending_foreign = foreign
            .map(|text| serde_json::from_str::<Vec<String>>(&text))
            .transpose()
            .map_err(|err| corrupt(format!("pending_foreign: {err}")))?
            .map(|ids| ids.into_iter().map(ProviderId::new).collect());
        let items = self.items("i.submission_id = :key", id.0)?;
        Ok(Submission {
            id,
            session: SessionId(session),
            spec: SubmissionSpec {
                event,
                summary,
                pending_review: pending_review.map(ProviderId::new),
                pending_foreign,
            },
            closed,
            items,
        })
    }

    /// Отправки сессии по порядку.
    pub fn submissions(&self, session: SessionId) -> Result<Vec<Submission>, JournalError> {
        load_session(self.conn, &self.project, session)?;
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM submissions WHERE session_id = ?1 ORDER BY id")?;
        let ids = stmt
            .query_map([session.0], |row| row.get(0))?
            .collect::<Result<Vec<u64>, _>>()?;
        ids.into_iter()
            .map(|id| self.submission(SubmissionId(id)))
            .collect()
    }

    /// Остановить отправку: её элементы в `planned` больше не отправляются, черновики этих
    /// элементов свободны для новой отправки. Элементы в `unknown` остаются до сверки.
    pub fn close_submission(&self, id: SubmissionId) -> Result<(), JournalError> {
        let changed = self.conn.execute(
            "UPDATE submissions SET closed = 1 WHERE id = ?1 AND session_id IN \
             (SELECT id FROM sessions WHERE project = ?2)",
            params![id.0, self.project.as_str()],
        )?;
        expect_one(changed, "отправка", id.0)
    }

    /// Элемент, который держит черновик: `in_flight` или `unknown` любой отправки, `planned`
    /// незакрытой.
    pub fn draft_hold(&self, draft: DraftId) -> Result<Option<(ItemId, ItemState)>, JournalError> {
        self.draft(draft)?;
        held_by(self.conn, draft, true)
    }

    pub fn item(&self, id: ItemId) -> Result<SubmitItem, JournalError> {
        self.items("i.id = :key", id.0)?
            .pop()
            .ok_or(JournalError::NotFound {
                what: "элемент отправки",
                id: id.0,
            })
    }

    /// Незавершённые элементы проекта: `planned`, `in_flight`, `unknown`.
    pub fn unfinished(&self) -> Result<Vec<SubmitItem>, JournalError> {
        let mut items = Vec::new();
        for state in [ItemState::Planned, ItemState::InFlight, ItemState::Unknown] {
            let mut stmt = self.conn.prepare(&format!(
                "SELECT {ITEM_COLUMNS} FROM submit_items i WHERE i.state = :state \
                 AND i.submission_id IN ({PROJECT_SUBMISSIONS})"
            ))?;
            let rows = stmt
                .query_map(
                    named_params! {
                        ":state": state.name(),
                        ":project": self.project.as_str(),
                    },
                    ItemRow::read,
                )?
                .collect::<Result<Vec<_>, _>>()?;
            for row in rows {
                items.push(self.finish_item(row)?);
            }
        }
        items.sort_by_key(|item| item.id);
        Ok(items)
    }

    /// `planned → in_flight` с отметкой времени, зафиксированной до запроса.
    pub fn begin(&self, item: ItemId) -> Result<(), JournalError> {
        self.transition(item, ItemState::InFlight, None, None)
    }

    /// Ответ на операцию или сверка, связавшая объект с операцией.
    pub fn confirm(&self, item: ItemId, remote: Option<&ProviderId>) -> Result<(), JournalError> {
        self.transition(item, ItemState::Confirmed, remote, None)
    }

    /// Подтверждение с удалёнными ID черновиков элемента и созданного `PENDING` одной фиксацией.
    pub fn confirm_with(
        &self,
        item: ItemId,
        confirmation: &Confirmation,
    ) -> Result<(), JournalError> {
        let tx = Transaction::new_unchecked(self.conn, TransactionBehavior::Immediate)?;
        self.transition(
            item,
            ItemState::Confirmed,
            confirmation.remote.as_ref(),
            None,
        )?;
        let stored = self.item(item)?;
        for (draft, remote) in &confirmation.drafts {
            if !stored.drafts.contains(draft) {
                return Err(JournalError::NotFound {
                    what: "черновик элемента",
                    id: draft.0,
                });
            }
            tx.execute(
                "UPDATE drafts SET remote_id = ?1 WHERE id = ?2",
                params![remote.as_str(), draft.0],
            )?;
        }
        if let Some(review) = &confirmation.pending_review {
            tx.execute(
                "UPDATE submissions SET pending_review = ?1 WHERE id = ?2",
                params![review.as_str(), stored.submission.0],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Сверка прочитала известный объект по ID в исходном состоянии: запись не применена.
    /// `unknown → failed` только у операции над известным объектом, повтор которой дубля не
    /// создаёт; отсутствие объекта в списке таким доказательством не считается. Отправка
    /// элемента закрывается.
    pub fn refute(&self, item: ItemId, detail: &str) -> Result<(), JournalError> {
        let stored = self.item(item)?;
        if !stored.operation.targets_known_object() {
            return Err(JournalError::Refute {
                item,
                operation: stored.operation,
            });
        }
        let tx = Transaction::new_unchecked(self.conn, TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE submit_items SET state = ?1, detail = ?2 WHERE id = ?3 AND state = ?4",
            params![
                ItemState::Failed.name(),
                detail,
                item.0,
                ItemState::Unknown.name()
            ],
        )?;
        if changed != 1 {
            return Err(JournalError::Transition {
                item,
                from: stored.state,
                to: ItemState::Failed,
            });
        }
        tx.execute(
            "UPDATE submissions SET closed = 1 WHERE id = ?1",
            [stored.submission.0],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Итог сверки, не давший подтверждения: кандидат (если он один) и описание. Только `unknown`.
    pub fn note_reconcile(
        &self,
        item: ItemId,
        candidate: Option<&ProviderId>,
        detail: &str,
    ) -> Result<(), JournalError> {
        let changed = self.conn.execute(
            &format!(
                "UPDATE submit_items SET candidate = COALESCE(:candidate, candidate), \
                 detail = :detail WHERE id = :id AND state = :state \
                 AND submission_id IN ({PROJECT_SUBMISSIONS})"
            ),
            named_params! {
                ":candidate": candidate.map(ProviderId::as_str),
                ":detail": detail,
                ":id": item.0,
                ":state": ItemState::Unknown.name(),
                ":project": self.project.as_str(),
            },
        )?;
        if changed == 1 {
            return Ok(());
        }
        let state = self.item(item)?.state;
        Err(JournalError::Candidate { item, state })
    }

    /// Ответа нет, а запрос мог быть принят: таймаут, обрыв после отправки.
    pub fn mark_unknown(&self, item: ItemId, detail: &str) -> Result<(), JournalError> {
        self.transition(item, ItemState::Unknown, None, Some(detail))
    }

    /// Запись явно не принята: отказ провайдера, исчерпанный лимит.
    pub fn fail(&self, item: ItemId, detail: &str) -> Result<(), JournalError> {
        self.transition(item, ItemState::Failed, None, Some(detail))
    }

    pub fn record_candidate(
        &self,
        item: ItemId,
        candidate: &ProviderId,
    ) -> Result<(), JournalError> {
        let changed = self.conn.execute(
            &format!(
                "UPDATE submit_items SET candidate = :candidate WHERE id = :id \
                 AND state = :state AND submission_id IN ({PROJECT_SUBMISSIONS})"
            ),
            named_params! {
                ":candidate": candidate.as_str(),
                ":id": item.0,
                ":state": ItemState::Unknown.name(),
                ":project": self.project.as_str(),
            },
        )?;
        if changed == 1 {
            return Ok(());
        }
        let state = self.item(item)?.state;
        Err(JournalError::Candidate { item, state })
    }

    fn transition(
        &self,
        item: ItemId,
        to: ItemState,
        remote: Option<&ProviderId>,
        detail: Option<&str>,
    ) -> Result<(), JournalError> {
        let (first, second) = match to.predecessors() {
            [only] => (*only, *only),
            [first, second] => (*first, *second),
            _ => {
                let from = self.item(item)?.state;
                return Err(JournalError::Transition { item, from, to });
            }
        };
        let entered = match to {
            ItemState::InFlight => Some(now_ms()?),
            _ => None,
        };
        let changed = self.conn.execute(
            &format!(
                "UPDATE submit_items SET state = :to, remote_id = COALESCE(:remote, remote_id), \
                 detail = COALESCE(:detail, detail), \
                 in_flight_at = COALESCE(:entered, in_flight_at) \
                 WHERE id = :id AND state IN (:first, :second) \
                 AND submission_id IN ({PROJECT_SUBMISSIONS})"
            ),
            named_params! {
                ":to": to.name(),
                ":remote": remote.map(ProviderId::as_str),
                ":detail": detail,
                ":entered": entered,
                ":id": item.0,
                ":first": first.name(),
                ":second": second.name(),
                ":project": self.project.as_str(),
            },
        )?;
        if changed == 1 {
            return Ok(());
        }
        let from = self.item(item)?.state;
        Err(JournalError::Transition { item, from, to })
    }

    fn items(&self, filter: &str, key: u64) -> Result<Vec<SubmitItem>, JournalError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {ITEM_COLUMNS} FROM submit_items i WHERE {filter} \
             AND i.submission_id IN ({PROJECT_SUBMISSIONS}) ORDER BY i.seq, i.id"
        ))?;
        let rows = stmt
            .query_map(
                named_params! {
                    ":key": key,
                    ":project": self.project.as_str(),
                },
                ItemRow::read,
            )?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter().map(|row| self.finish_item(row)).collect()
    }

    fn finish_item(&self, row: ItemRow) -> Result<SubmitItem, JournalError> {
        let mut stmt = self
            .conn
            .prepare("SELECT draft_id FROM submit_item_drafts WHERE item_id = ?1 ORDER BY rowid")?;
        let drafts = stmt
            .query_map([row.id], |r| r.get(0))?
            .map(|id| id.map(DraftId))
            .collect::<Result<Vec<_>, _>>()?;
        let operation = Operation::from_name(&row.operation)
            .ok_or_else(|| corrupt(format!("операция «{}»", row.operation)))?;
        let state = ItemState::from_name(&row.state)
            .ok_or_else(|| corrupt(format!("состояние «{}»", row.state)))?;
        Ok(SubmitItem {
            id: ItemId(row.id),
            submission: SubmissionId(row.submission),
            seq: row.seq,
            operation,
            drafts,
            state,
            remote_id: row.remote_id.map(ProviderId::new),
            candidate: row.candidate.map(ProviderId::new),
            detail: row.detail,
            in_flight_at: row.in_flight_at,
        })
    }
}

/// Элемент, который держит черновик: `in_flight` или `unknown`, а с `planned` — ещё и `planned`
/// незакрытой отправки.
fn held_by(
    conn: &Connection,
    draft: DraftId,
    planned: bool,
) -> Result<Option<(ItemId, ItemState)>, JournalError> {
    let row: Option<(u64, String)> = conn
        .query_row(
            "SELECT i.id, i.state FROM submit_items i \
             JOIN submit_item_drafts d ON d.item_id = i.id \
             JOIN submissions s ON s.id = i.submission_id \
             WHERE d.draft_id = ?1 AND (i.state IN (?2, ?3) OR (?4 AND i.state = ?5 AND s.closed = 0)) \
             ORDER BY i.id LIMIT 1",
            params![
                draft.0,
                ItemState::InFlight.name(),
                ItemState::Unknown.name(),
                planned,
                ItemState::Planned.name()
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    row.map(|(id, state)| {
        ItemState::from_name(&state)
            .map(|state| (ItemId(id), state))
            .ok_or_else(|| corrupt(format!("состояние «{state}»")))
    })
    .transpose()
}

fn row_id(value: i64) -> Result<u64, JournalError> {
    u64::try_from(value).map_err(|_| corrupt(format!("отрицательный rowid {value}")))
}

fn expect_one(changed: usize, what: &'static str, id: u64) -> Result<(), JournalError> {
    if changed == 1 {
        Ok(())
    } else {
        Err(JournalError::NotFound { what, id })
    }
}

fn load_session(
    conn: &Connection,
    project: &ProjectId,
    id: SessionId,
) -> Result<ReviewSession, JournalError> {
    conn.query_row(
        &format!("SELECT {SESSION_COLUMNS} FROM sessions WHERE id = ?1 AND project = ?2"),
        params![id.0, project.as_str()],
        SessionRow::read,
    )
    .optional()?
    .ok_or(JournalError::NotFound {
        what: "сессия",
        id: id.0,
    })?
    .into_session()
}

struct SessionRow {
    id: u64,
    account: String,
    provider: String,
    host: String,
    repo: String,
    number: u64,
    shas: [String; 3],
}

impl SessionRow {
    fn read(row: &Row<'_>) -> rusqlite::Result<SessionRow> {
        Ok(SessionRow {
            id: row.get(0)?,
            account: row.get(1)?,
            provider: row.get(2)?,
            host: row.get(3)?,
            repo: row.get(4)?,
            number: row.get(5)?,
            shas: [row.get(6)?, row.get(7)?, row.get(8)?],
        })
    }

    fn into_session(self) -> Result<ReviewSession, JournalError> {
        let provider = parse_provider(&self.provider)?;
        let target = ReviewTarget {
            provider,
            host: self.host,
            repo: self.repo,
            number: self.number,
        };
        Ok(ReviewSession::new(
            self.account,
            target,
            parse_revision(&self.shas)?,
        ))
    }
}

struct DraftRow {
    id: u64,
    body: String,
    remote_id: Option<String>,
    shas: [String; 3],
    anchor_state: String,
    placement: String,
    reply_thread: Option<String>,
    old_path: Option<String>,
    new_path: Option<String>,
    start: PointColumns,
    end: PointColumns,
}

struct PointColumns {
    side: Option<String>,
    kind: Option<String>,
    old: Option<u32>,
    new: Option<u32>,
}

impl PointColumns {
    fn read(row: &Row<'_>, first: usize) -> rusqlite::Result<PointColumns> {
        Ok(PointColumns {
            side: row.get(first)?,
            kind: row.get(first + 1)?,
            old: row.get(first + 2)?,
            new: row.get(first + 3)?,
        })
    }

    fn of(point: Option<LinePoint>) -> PointColumns {
        PointColumns {
            side: point.map(|p| side_name(p.side()).to_owned()),
            kind: point.map(|p| kind_name(p.line().kind).to_owned()),
            old: point.map(|p| p.line().old),
            new: point.map(|p| p.line().new),
        }
    }

    fn into_point(self) -> Result<Option<LinePoint>, JournalError> {
        match (self.side, self.kind, self.old, self.new) {
            (None, None, None, None) => Ok(None),
            (Some(side), Some(kind), Some(old), Some(new)) => {
                let line = DiffLine {
                    kind: parse_kind(&kind)?,
                    old,
                    new,
                };
                LinePoint::new(parse_side(&side)?, line)
                    .map(Some)
                    .map_err(|err| corrupt(err.to_string()))
            }
            _ => Err(corrupt("строка якоря записана не полностью")),
        }
    }
}

impl DraftRow {
    fn read(row: &Row<'_>) -> rusqlite::Result<DraftRow> {
        Ok(DraftRow {
            id: row.get(0)?,
            body: row.get(1)?,
            remote_id: row.get(2)?,
            shas: [row.get(3)?, row.get(4)?, row.get(5)?],
            anchor_state: row.get(6)?,
            placement: row.get(7)?,
            reply_thread: row.get(8)?,
            old_path: row.get(9)?,
            new_path: row.get(10)?,
            start: PointColumns::read(row, 11)?,
            end: PointColumns::read(row, 15)?,
        })
    }

    fn into_draft(self) -> Result<DraftComment, JournalError> {
        let placement = match (self.placement.as_str(), self.reply_thread) {
            ("reply", Some(thread)) => Placement::Reply {
                thread: ProviderId::new(thread),
            },
            ("line", None) => {
                let (Some(old_path), Some(new_path)) = (self.old_path, self.new_path) else {
                    return Err(corrupt("у якоря нет путей"));
                };
                let end = self
                    .end
                    .into_point()?
                    .ok_or_else(|| corrupt("у якоря нет конца"))?;
                let anchor = Anchor::restore(old_path, new_path, self.start.into_point()?, end)
                    .map_err(|err| corrupt(err.to_string()))?;
                Placement::Line(anchor)
            }
            (other, _) => return Err(corrupt(format!("размещение «{other}»"))),
        };
        Ok(DraftComment::restore(
            DraftId(self.id),
            self.body,
            self.remote_id.map(ProviderId::new),
            placement,
            parse_revision(&self.shas)?,
            parse_anchor_state(&self.anchor_state)?,
        ))
    }
}

struct PlacementColumns {
    placement: &'static str,
    reply_thread: Option<String>,
    old_path: Option<String>,
    new_path: Option<String>,
    start_side: Option<String>,
    start_kind: Option<String>,
    start_old: Option<u32>,
    start_new: Option<u32>,
    end_side: Option<String>,
    end_kind: Option<String>,
    end_old: Option<u32>,
    end_new: Option<u32>,
}

impl PlacementColumns {
    fn of(placement: &Placement) -> PlacementColumns {
        let (kind, reply_thread, anchor) = match placement {
            Placement::Reply { thread } => ("reply", Some(thread.as_str().to_owned()), None),
            Placement::Line(anchor) => ("line", None, Some(anchor)),
        };
        let start = PointColumns::of(anchor.and_then(Anchor::start));
        let end = PointColumns::of(anchor.map(Anchor::end));
        PlacementColumns {
            placement: kind,
            reply_thread,
            old_path: anchor.map(|a| a.old_path().to_owned()),
            new_path: anchor.map(|a| a.new_path().to_owned()),
            start_side: start.side,
            start_kind: start.kind,
            start_old: start.old,
            start_new: start.new,
            end_side: end.side,
            end_kind: end.kind,
            end_old: end.old,
            end_new: end.new,
        }
    }
}

struct ItemRow {
    id: u64,
    submission: u64,
    seq: u32,
    operation: String,
    state: String,
    remote_id: Option<String>,
    candidate: Option<String>,
    detail: Option<String>,
    in_flight_at: Option<i64>,
}

impl ItemRow {
    fn read(row: &Row<'_>) -> rusqlite::Result<ItemRow> {
        Ok(ItemRow {
            id: row.get(0)?,
            submission: row.get(1)?,
            seq: row.get(2)?,
            operation: row.get(3)?,
            state: row.get(4)?,
            remote_id: row.get(5)?,
            candidate: row.get(6)?,
            detail: row.get(7)?,
            in_flight_at: row.get(8)?,
        })
    }
}

fn parse_revision(shas: &[String; 3]) -> Result<Revision, JournalError> {
    let parse = |value: &str| Sha::parse(value).map_err(|err| corrupt(err.to_string()));
    Ok(Revision::new(
        parse(&shas[0])?,
        parse(&shas[1])?,
        parse(&shas[2])?,
    ))
}

fn provider_name(provider: Provider) -> &'static str {
    match provider {
        Provider::GitHub => "github",
        Provider::GitLab => "gitlab",
    }
}

fn parse_provider(name: &str) -> Result<Provider, JournalError> {
    match name {
        "github" => Ok(Provider::GitHub),
        "gitlab" => Ok(Provider::GitLab),
        other => Err(corrupt(format!("провайдер «{other}»"))),
    }
}

fn side_name(side: Side) -> &'static str {
    match side {
        Side::Old => "old",
        Side::New => "new",
    }
}

fn parse_side(name: &str) -> Result<Side, JournalError> {
    match name {
        "old" => Ok(Side::Old),
        "new" => Ok(Side::New),
        other => Err(corrupt(format!("сторона «{other}»"))),
    }
}

fn kind_name(kind: LineKind) -> &'static str {
    match kind {
        LineKind::Added => "added",
        LineKind::Removed => "removed",
        LineKind::Context => "context",
    }
}

fn parse_kind(name: &str) -> Result<LineKind, JournalError> {
    match name {
        "added" => Ok(LineKind::Added),
        "removed" => Ok(LineKind::Removed),
        "context" => Ok(LineKind::Context),
        other => Err(corrupt(format!("вид строки «{other}»"))),
    }
}

fn anchor_state_name(state: AnchorState) -> &'static str {
    match state {
        AnchorState::Current => "current",
        AnchorState::Stale(StaleReason::FileMissing) => "file_missing",
        AnchorState::Stale(StaleReason::PathChanged) => "path_changed",
        AnchorState::Stale(StaleReason::LinesChanged) => "lines_changed",
        AnchorState::Stale(StaleReason::NotInOwnDiff) => "not_in_own_diff",
    }
}

fn parse_anchor_state(name: &str) -> Result<AnchorState, JournalError> {
    match name {
        "current" => Ok(AnchorState::Current),
        "file_missing" => Ok(AnchorState::Stale(StaleReason::FileMissing)),
        "path_changed" => Ok(AnchorState::Stale(StaleReason::PathChanged)),
        "lines_changed" => Ok(AnchorState::Stale(StaleReason::LinesChanged)),
        "not_in_own_diff" => Ok(AnchorState::Stale(StaleReason::NotInOwnDiff)),
        other => Err(corrupt(format!("состояние якоря «{other}»"))),
    }
}

#[cfg(test)]
mod tests;
