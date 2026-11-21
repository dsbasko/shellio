use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

use super::*;
use crate::model::ModelError;
use crate::model::diff::FileDiff;
use crate::model::revision::{HeadCheck, HeadPath};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const PATH: &str = "src/lib.rs";

const HELD: &str = "\
@@ -1,4 +1,5 @@
 fn a() {}
-fn b() {}
+fn b2() {}
+fn c() {}
 fn d() {}
 fn e() {}
";

/// Новый head: строка `c` заменена на `c2`, остальное на месте.
const FRESH: &str = "\
@@ -1,4 +1,5 @@
 fn a() {}
-fn b() {}
+fn b2() {}
+fn c2() {}
 fn d() {}
 fn e() {}
";

struct Fixture {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

impl Fixture {
    fn new() -> Result<Fixture, std::io::Error> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("state.db");
        Ok(Fixture { _dir: dir, path })
    }

    fn open(&self) -> Result<Journal, JournalError> {
        Journal::open(&self.path)
    }

    /// Соединение в обход журнала: без восстановления и без записи.
    fn peek(&self) -> rusqlite::Result<Connection> {
        Connection::open_with_flags(&self.path, OpenFlags::SQLITE_OPEN_READ_ONLY)
    }
}

fn sha(fill: char) -> Result<Sha, ModelError> {
    Sha::parse(&fill.to_string().repeat(40))
}

fn rev(base: char, start: char, head: char) -> Result<Revision, ModelError> {
    Ok(Revision::new(sha(base)?, sha(start)?, sha(head)?))
}

fn session(provider: Provider, revision: Revision) -> ReviewSession {
    ReviewSession::new(
        "reviewer",
        ReviewTarget {
            provider,
            host: "example.test".to_owned(),
            repo: "group/project".to_owned(),
            number: 7,
        },
        revision,
    )
}

fn held_file() -> Result<FileDiff, ModelError> {
    FileDiff::parse(PATH, PATH, HELD)
}

fn line(file: &FileDiff, side: Side, number: u32) -> Result<Placement, ModelError> {
    Ok(Placement::Line(Anchor::line(
        file,
        LinePoint::at(file, side, number)?,
    )?))
}

fn peek_state(conn: &Connection, item: ItemId) -> rusqlite::Result<(String, Option<i64>)> {
    conn.query_row(
        "SELECT state, in_flight_at FROM submit_items WHERE id = ?1",
        [item.0],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
}

fn planned(operation: Operation, drafts: &[DraftId]) -> PlannedItem {
    PlannedItem {
        operation,
        drafts: drafts.to_vec(),
    }
}

/// Сессия GitLab с тремя черновиками и отправкой из пяти элементов в `planned`.
fn gitlab_submission(
    scope: &Scope<'_>,
) -> Result<(SessionId, Submission), Box<dyn std::error::Error>> {
    let id = scope.create_session(&session(Provider::GitLab, rev('a', 'b', 'c')?))?;
    let file = held_file()?;
    let mut drafts = Vec::new();
    for number in [2, 3, 4] {
        drafts.push(
            scope
                .add_draft(id, line(&file, Side::New, number)?, "текст")?
                .id,
        );
    }
    let mut items: Vec<PlannedItem> = drafts
        .iter()
        .map(|d| planned(Operation::GitLabPublishDraftNote, &[*d]))
        .collect();
    items.push(planned(Operation::GitLabSummaryNote, &[]));
    items.push(planned(Operation::GitLabApprove, &[]));
    let submission = scope.plan_submission(id, &items)?;
    Ok((id, submission))
}

#[test]
fn transition_table_matches_adr() {
    use ItemState::*;
    let allowed = [
        (Planned, InFlight),
        (InFlight, Confirmed),
        (InFlight, Unknown),
        (InFlight, Failed),
        (Unknown, Confirmed),
    ];
    for from in ItemState::ALL {
        for to in ItemState::ALL {
            assert_eq!(
                from.can_become(to),
                allowed.contains(&(from, to)),
                "{} → {}",
                from.name(),
                to.name()
            );
        }
        assert_eq!(ItemState::from_name(from.name()), Some(from));
    }
    assert!(Confirmed.is_finished() && Failed.is_finished());
    assert!(!Planned.is_finished() && !InFlight.is_finished() && !Unknown.is_finished());
    for operation in Operation::ALL {
        assert_eq!(Operation::from_name(operation.name()), Some(operation));
    }
}

#[test]
fn draft_is_written_before_add_returns() -> TestResult {
    let fx = Fixture::new()?;
    let journal = fx.open()?;
    let project = ProjectId::new("p1");
    let scope = journal.scope(&project);
    let review = session(Provider::GitHub, rev('a', 'b', 'c')?);
    let id = scope.create_session(&review)?;
    let file = held_file()?;
    let range = Placement::Line(Anchor::range(
        &file,
        LinePoint::at(&file, Side::Old, 2)?,
        LinePoint::at(&file, Side::New, 3)?,
    )?);
    let first = scope.add_draft(id, range, "диапазон")?;
    let reply = scope.add_draft(
        id,
        Placement::Reply {
            thread: ProviderId::new("PRRT_1"),
        },
        "ответ",
    )?;

    // Второе соединение видит черновик сразу, без закрытия журнала.
    let peek = fx.peek()?;
    let count: i64 = peek.query_row("SELECT count(*) FROM drafts", [], |row| row.get(0))?;
    assert_eq!(count, 2);
    scope.update_draft_body(first.id, "диапазон, правка")?;
    scope.set_draft_remote_id(reply.id, &ProviderId::new("PRRC_9"))?;
    let body: String = peek.query_row(
        "SELECT body FROM drafts WHERE id = ?1",
        [first.id.0],
        |row| row.get(0),
    )?;
    assert_eq!(body, "диапазон, правка");

    drop(journal);
    let journal = fx.open()?;
    let scope = journal.scope(&project);
    assert_eq!(scope.session(id)?, review);
    let restored = scope.drafts(id)?;
    assert_eq!(restored.len(), 2);
    assert_eq!(restored[0].placement(), first.placement());
    assert_eq!(restored[0].body, "диапазон, правка");
    assert_eq!(restored[0].revision(), review.revision());
    assert!(restored[0].is_current());
    assert_eq!(restored[1].placement(), reply.placement());
    assert_eq!(restored[1].remote_id, Some(ProviderId::new("PRRC_9")));
    assert_eq!(scope.draft(first.id)?, restored[0]);
    // Восстановленный черновик отправляется так же, как исходный.
    assert_eq!(
        review.github_anchor(&restored[0])?,
        review.github_anchor(&first)?
    );
    Ok(())
}

#[test]
fn transitions_record_state_and_in_flight_mark() -> TestResult {
    let fx = Fixture::new()?;
    let journal = fx.open()?;
    let project = ProjectId::new("p1");
    let scope = journal.scope(&project);
    let (_, submission) = gitlab_submission(&scope)?;
    let ids: Vec<ItemId> = submission.items.iter().map(|i| i.id).collect();
    assert!(
        submission
            .items
            .iter()
            .all(|i| i.state == ItemState::Planned && i.in_flight_at.is_none())
    );
    assert_eq!(
        submission.items.iter().map(|i| i.seq).collect::<Vec<_>>(),
        vec![0, 1, 2, 3, 4]
    );

    let peek = fx.peek()?;
    scope.begin(ids[0])?;
    // Отметка in_flight на диске до запроса: её видит другое соединение.
    let (state, mark) = peek_state(&peek, ids[0])?;
    assert_eq!(state, "in_flight");
    assert!(mark.is_some());
    scope.confirm(ids[0], Some(&ProviderId::new("note-1")))?;

    scope.begin(ids[1])?;
    scope.mark_unknown(ids[1], "таймаут после отправки")?;
    scope.confirm(ids[1], Some(&ProviderId::new("note-2")))?;

    scope.begin(ids[2])?;
    scope.fail(ids[2], "429: лимит исчерпан")?;

    let items = scope.submission(submission.id)?.items;
    assert_eq!(items[0].state, ItemState::Confirmed);
    assert_eq!(items[0].remote_id, Some(ProviderId::new("note-1")));
    assert_eq!(items[0].in_flight_at, mark);
    assert_eq!(items[1].state, ItemState::Confirmed);
    assert_eq!(items[1].detail.as_deref(), Some("таймаут после отправки"));
    assert_eq!(items[2].state, ItemState::Failed);
    assert_eq!(items[2].detail.as_deref(), Some("429: лимит исчерпан"));
    assert!(items[2].in_flight_at.is_some());
    assert_eq!(items[3].state, ItemState::Planned);
    Ok(())
}

/// Доводит элемент до состояния `state` допустимыми переходами.
fn drive(scope: &Scope<'_>, item: ItemId, state: ItemState) -> Result<(), JournalError> {
    match state {
        ItemState::Planned => Ok(()),
        ItemState::InFlight => scope.begin(item),
        ItemState::Confirmed => {
            scope.begin(item)?;
            scope.confirm(item, None)
        }
        ItemState::Unknown => {
            scope.begin(item)?;
            scope.mark_unknown(item, "нет ответа")
        }
        ItemState::Failed => {
            scope.begin(item)?;
            scope.fail(item, "отказ")
        }
    }
}

fn attempt(scope: &Scope<'_>, item: ItemId, to: ItemState) -> Result<(), JournalError> {
    match to {
        ItemState::InFlight => scope.begin(item),
        ItemState::Confirmed => scope.confirm(item, Some(&ProviderId::new("x"))),
        ItemState::Unknown => scope.mark_unknown(item, "попытка"),
        ItemState::Failed => scope.fail(item, "попытка"),
        ItemState::Planned => unreachable!("в planned журнал не переводит"),
    }
}

#[test]
fn invalid_transitions_are_rejected_without_change() -> TestResult {
    let fx = Fixture::new()?;
    let journal = fx.open()?;
    let project = ProjectId::new("p1");
    let scope = journal.scope(&project);
    let id = scope.create_session(&session(Provider::GitLab, rev('a', 'b', 'c')?))?;
    let targets = [
        ItemState::InFlight,
        ItemState::Confirmed,
        ItemState::Unknown,
        ItemState::Failed,
    ];
    for from in ItemState::ALL {
        for to in targets {
            if from.can_become(to) {
                continue;
            }
            let submission =
                scope.plan_submission(id, &[planned(Operation::GitLabApprove, &[])])?;
            let item = submission.items[0].id;
            drive(&scope, item, from)?;
            let before = scope.item(item)?;
            match attempt(&scope, item, to) {
                Err(JournalError::Transition {
                    item: rejected,
                    from: found,
                    to: wanted,
                }) => {
                    assert_eq!((rejected, found, wanted), (item, from, to));
                }
                other => panic!("{} → {}: {other:?}", from.name(), to.name()),
            }
            assert_eq!(scope.item(item)?, before, "{} → {}", from.name(), to.name());
        }
    }
    Ok(())
}

#[test]
fn reopen_turns_in_flight_into_unknown_and_shows_unfinished() -> TestResult {
    let fx = Fixture::new()?;
    let project = ProjectId::new("p1");
    let (submission, mark) = {
        let journal = fx.open()?;
        assert!(journal.recovered().is_empty());
        let scope = journal.scope(&project);
        let (_, submission) = gitlab_submission(&scope)?;
        let ids: Vec<ItemId> = submission.items.iter().map(|i| i.id).collect();
        drive(&scope, ids[0], ItemState::Confirmed)?;
        // Сбой после принятой записи до ответа: in_flight без исхода.
        scope.begin(ids[1])?;
        drive(&scope, ids[2], ItemState::Unknown)?;
        let mark = scope.item(ids[1])?.in_flight_at;
        (submission, mark)
    };
    let ids: Vec<ItemId> = submission.items.iter().map(|i| i.id).collect();

    let journal = fx.open()?;
    assert_eq!(journal.recovered(), &[ids[1]]);
    let scope = journal.scope(&project);
    let recovered = scope.item(ids[1])?;
    assert_eq!(recovered.state, ItemState::Unknown);
    assert_eq!(recovered.in_flight_at, mark);
    assert!(recovered.in_flight_at.is_some());
    assert_eq!(recovered.detail.as_deref(), Some(RECOVERED_DETAIL));
    // Слепого повтора нет: unknown обратно в in_flight не идёт.
    assert!(matches!(
        scope.begin(ids[1]),
        Err(JournalError::Transition { .. })
    ));

    let unfinished: Vec<(ItemId, ItemState)> = scope
        .unfinished()?
        .iter()
        .map(|i| (i.id, i.state))
        .collect();
    assert_eq!(
        unfinished,
        vec![
            (ids[1], ItemState::Unknown),
            (ids[2], ItemState::Unknown),
            (ids[3], ItemState::Planned),
            (ids[4], ItemState::Planned),
        ]
    );
    assert_eq!(scope.item(ids[0])?.state, ItemState::Confirmed);

    // Второе переоткрытие ничего не меняет.
    drop(journal);
    let journal = fx.open()?;
    assert!(journal.recovered().is_empty());
    assert_eq!(journal.scope(&project).item(ids[1])?, recovered);
    Ok(())
}

#[test]
fn crash_before_request_leaves_item_planned() -> TestResult {
    let fx = Fixture::new()?;
    let project = ProjectId::new("p1");
    let item = {
        let journal = fx.open()?;
        let (_, submission) = gitlab_submission(&journal.scope(&project))?;
        submission.items[0].id
    };
    let journal = fx.open()?;
    assert!(journal.recovered().is_empty());
    let scope = journal.scope(&project);
    assert_eq!(scope.item(item)?.state, ItemState::Planned);
    scope.begin(item)?;
    scope.confirm(item, Some(&ProviderId::new("note-1")))?;
    Ok(())
}

#[test]
fn candidate_is_recorded_only_for_unknown() -> TestResult {
    let fx = Fixture::new()?;
    let journal = fx.open()?;
    let project = ProjectId::new("p1");
    let scope = journal.scope(&project);
    let (_, submission) = gitlab_submission(&scope)?;
    let item = submission.items[0].id;
    let candidate = ProviderId::new("note-77");
    scope.begin(item)?;
    assert!(matches!(
        scope.record_candidate(item, &candidate),
        Err(JournalError::Candidate {
            state: ItemState::InFlight,
            ..
        })
    ));
    scope.mark_unknown(item, "обрыв после отправки")?;
    scope.record_candidate(item, &candidate)?;
    let stored = scope.item(item)?;
    assert_eq!(stored.state, ItemState::Unknown);
    assert_eq!(stored.candidate, Some(candidate));
    assert_eq!(stored.remote_id, None);
    Ok(())
}

#[test]
fn plan_checks_provider_drafts_and_session() -> TestResult {
    let fx = Fixture::new()?;
    let journal = fx.open()?;
    let project = ProjectId::new("p1");
    let scope = journal.scope(&project);
    let file = held_file()?;
    let gl = scope.create_session(&session(Provider::GitLab, rev('a', 'b', 'c')?))?;
    let other = scope.create_session(&session(Provider::GitLab, rev('a', 'b', 'd')?))?;
    let draft = scope.add_draft(gl, line(&file, Side::New, 2)?, "текст")?.id;
    let foreign = scope
        .add_draft(other, line(&file, Side::New, 2)?, "текст")?
        .id;

    assert!(matches!(
        scope.plan_submission(gl, &[planned(Operation::GitHubSubmitReview, &[])]),
        Err(JournalError::OperationProvider { .. })
    ));
    assert!(matches!(
        scope.plan_submission(gl, &[planned(Operation::GitLabPublishDraftNote, &[])]),
        Err(JournalError::Drafts { found: 0, .. })
    ));
    assert!(matches!(
        scope.plan_submission(gl, &[planned(Operation::GitLabApprove, &[draft])]),
        Err(JournalError::Drafts { found: 1, .. })
    ));
    assert!(matches!(
        scope.plan_submission(
            gl,
            &[
                planned(Operation::GitLabApprove, &[]),
                planned(Operation::GitLabPublishDraftNote, &[foreign]),
            ]
        ),
        Err(JournalError::NotFound { .. })
    ));
    // Отвергнутый план не оставляет ни отправки, ни элементов.
    let peek = fx.peek()?;
    let count: i64 = peek.query_row(
        "SELECT (SELECT count(*) FROM submissions) + (SELECT count(*) FROM submit_items)",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(count, 0);

    let gh = scope.create_session(&session(Provider::GitHub, rev('a', 'b', 'c')?))?;
    let d1 = scope.add_draft(gh, line(&file, Side::New, 2)?, "один")?.id;
    let d2 = scope.add_draft(gh, line(&file, Side::New, 3)?, "два")?.id;
    assert!(matches!(
        scope.plan_submission(gh, &[planned(Operation::GitHubCreateReview, &[d1, d1])]),
        Err(JournalError::DuplicateDraft { .. })
    ));
    let submission =
        scope.plan_submission(gh, &[planned(Operation::GitHubCreateReview, &[d1, d2])])?;
    assert_eq!(submission.session, gh);
    assert_eq!(submission.items[0].drafts, vec![d1, d2]);
    assert_eq!(scope.submission(submission.id)?, submission);
    Ok(())
}

#[test]
fn stale_draft_stays_stale_and_on_old_revision_after_reopen() -> TestResult {
    let fx = Fixture::new()?;
    let project = ProjectId::new("p1");
    let held = rev('a', 'b', 'c')?;
    let fresh = rev('a', 'b', 'f')?;
    let file = held_file()?;
    let fresh_diff = vec![FileDiff::parse(PATH, PATH, FRESH)?];
    let id = {
        let journal = fx.open()?;
        let scope = journal.scope(&project);
        let mut review = session(Provider::GitLab, held.clone());
        let id = scope.create_session(&review)?;
        scope.add_draft(id, line(&file, Side::New, 2)?, "сохранится")?;
        scope.add_draft(id, line(&file, Side::New, 3)?, "устареет")?;
        let mut drafts = scope.drafts(id)?;
        let HeadCheck::Changed(change) =
            review.check_head(&fresh, std::slice::from_ref(&file), &fresh_diff, &drafts)
        else {
            return Err("смена head не замечена".into());
        };
        review.apply_head(&change, HeadPath::Move, &mut drafts, &fresh_diff)?;
        scope.save_head(id, &review, &drafts)?;
        id
    };

    let journal = fx.open()?;
    let scope = journal.scope(&project);
    let review = scope.session(id)?;
    assert_eq!(review.revision(), &fresh);
    let drafts = scope.drafts(id)?;
    assert_eq!(drafts[0].revision(), &fresh);
    assert!(drafts[0].is_current());
    assert_eq!(
        review.gitlab_position(&drafts[0])?.revision(),
        fresh.clone()
    );
    assert_eq!(drafts[1].revision(), &held);
    assert_eq!(
        drafts[1].state(),
        AnchorState::Stale(StaleReason::LinesChanged)
    );
    assert!(matches!(
        review.gitlab_position(&drafts[1]),
        Err(ModelError::Stale { .. })
    ));
    Ok(())
}

#[test]
fn draft_on_other_revision_is_refused_after_reopen() -> TestResult {
    let fx = Fixture::new()?;
    let project = ProjectId::new("p1");
    let held = rev('a', 'b', 'c')?;
    let file = held_file()?;
    let (id, draft) = {
        let journal = fx.open()?;
        let scope = journal.scope(&project);
        let id = scope.create_session(&session(Provider::GitHub, held.clone()))?;
        let draft = scope.add_draft(id, line(&file, Side::New, 2)?, "текст")?;
        // Сессия ушла на свежую ревизию без этого черновика.
        let moved = session(Provider::GitHub, rev('a', 'b', 'f')?);
        scope.save_head(id, &moved, &[])?;
        (id, draft.id)
    };
    let journal = fx.open()?;
    let scope = journal.scope(&project);
    let review = scope.session(id)?;
    let restored = scope.draft(draft)?;
    assert_eq!(restored.revision(), &held);
    assert!(matches!(
        review.github_anchor(&restored),
        Err(ModelError::RevisionMismatch { .. })
    ));
    Ok(())
}

#[test]
fn save_head_keeps_account_and_target() -> TestResult {
    let fx = Fixture::new()?;
    let journal = fx.open()?;
    let project = ProjectId::new("p1");
    let scope = journal.scope(&project);
    let id = scope.create_session(&session(Provider::GitHub, rev('a', 'b', 'c')?))?;
    let other_account = ReviewSession::new(
        "author",
        session(Provider::GitHub, rev('a', 'b', 'f')?)
            .target()
            .clone(),
        rev('a', 'b', 'f')?,
    );
    assert!(matches!(
        scope.save_head(id, &other_account, &[]),
        Err(JournalError::SessionIdentity { .. })
    ));
    assert_eq!(scope.session(id)?.revision(), &rev('a', 'b', 'c')?);
    Ok(())
}

/// Два клона одного remote — два проекта (ADR-0025): тот же аккаунт и та же цель на сервере.
#[test]
fn drafts_of_one_project_are_invisible_from_another() -> TestResult {
    let fx = Fixture::new()?;
    let a = ProjectId::new("clone-a");
    let b = ProjectId::new("clone-b");
    let review = session(Provider::GitLab, rev('a', 'b', 'c')?);
    let file = held_file()?;
    let journal = fx.open()?;
    let (sa, da, submission) = {
        let scope = journal.scope(&a);
        let (sa, submission) = gitlab_submission(&scope)?;
        let da = scope.drafts(sa)?[0].id;
        scope.begin(submission.items[0].id)?;
        (sa, da, submission)
    };
    let item_a = submission.items[0].id;
    let sb = journal.scope(&b).create_session(&review)?;

    let mut journal = journal;
    for reopened in [false, true] {
        if reopened {
            drop(journal);
            journal = fx.open()?;
        }
        let scope_b = journal.scope(&b);
        assert!(scope_b.drafts(sb)?.is_empty());
        assert_eq!(
            scope_b
                .sessions()?
                .iter()
                .map(|(id, _)| *id)
                .collect::<Vec<_>>(),
            vec![sb]
        );
        assert!(scope_b.unfinished()?.is_empty());

        let not_found =
            |result: Result<(), JournalError>| matches!(result, Err(JournalError::NotFound { .. }));
        assert!(not_found(scope_b.drafts(sa).map(|_| ())));
        assert!(not_found(scope_b.session(sa).map(|_| ())));
        assert!(not_found(scope_b.draft(da).map(|_| ())));
        assert!(not_found(scope_b.update_draft_body(da, "чужое")));
        assert!(not_found(
            scope_b.set_draft_remote_id(da, &ProviderId::new("n"))
        ));
        assert!(not_found(
            scope_b
                .add_draft(sa, line(&file, Side::New, 2)?, "чужое")
                .map(|_| ())
        ));
        assert!(not_found(
            scope_b
                .plan_submission(sa, &[planned(Operation::GitLabApprove, &[])])
                .map(|_| ())
        ));
        assert!(not_found(
            scope_b
                .plan_submission(sb, &[planned(Operation::GitLabPublishDraftNote, &[da])])
                .map(|_| ())
        ));
        assert!(not_found(scope_b.submission(submission.id).map(|_| ())));
        assert!(not_found(scope_b.item(item_a).map(|_| ())));
        assert!(not_found(scope_b.begin(submission.items[1].id)));
        assert!(not_found(
            scope_b.record_candidate(item_a, &ProviderId::new("n"))
        ));
        assert!(not_found(scope_b.save_head(sa, &review, &[])));

        // Проект A свои черновики видит, правки B их не задели.
        let scope_a = journal.scope(&a);
        let drafts = scope_a.drafts(sa)?;
        assert_eq!(drafts.len(), 3);
        assert!(
            drafts
                .iter()
                .all(|d| d.body == "текст" && d.remote_id.is_none())
        );
        assert_eq!(scope_a.unfinished()?.len(), 5);
    }
    Ok(())
}

#[test]
fn newer_schema_version_is_refused() -> TestResult {
    let fx = Fixture::new()?;
    drop(fx.open()?);
    let conn = Connection::open(&fx.path)?;
    conn.pragma_update(None, "user_version", 99)?;
    drop(conn);
    assert!(matches!(
        fx.open(),
        Err(JournalError::Version { found: 99 })
    ));
    Ok(())
}

#[test]
fn journal_uses_wal_and_full_sync() -> TestResult {
    let fx = Fixture::new()?;
    let journal = fx.open()?;
    let mode: String = journal
        .conn
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
    let sync: i64 = journal
        .conn
        .query_row("PRAGMA synchronous", [], |row| row.get(0))?;
    assert_eq!(mode, "wal");
    assert_eq!(sync, 2, "synchronous = FULL");
    // Привязки libsqlite3-sys по умолчанию требуют не ниже 3.45.3.
    assert!(
        rusqlite::version_number() >= 3_045_003,
        "{}",
        rusqlite::version()
    );
    Ok(())
}

/// Журнал линкуется с системной `libsqlite3` macOS, а не со встроенной или сторонней копией.
#[test]
fn links_system_libsqlite3() -> TestResult {
    let exe = std::env::current_exe()?;
    let output = std::process::Command::new("/usr/bin/otool")
        .arg("-L")
        .arg(&exe)
        .output()?;
    assert!(output.status.success(), "otool -L {}", exe.display());
    let text = String::from_utf8(output.stdout)?;
    let sqlite: Vec<&str> = text
        .lines()
        .skip(1)
        .filter_map(|line| line.split_whitespace().next())
        .filter(|lib| lib.contains("sqlite"))
        .collect();
    assert_eq!(sqlite, vec!["/usr/lib/libsqlite3.dylib"], "{text}");
    Ok(())
}

#[test]
fn open_fails_on_unwritable_path() {
    assert!(Journal::open(Path::new("/nonexistent-dir/state.db")).is_err());
}

#[test]
fn held_draft_is_refused_by_plan_body_and_head_until_closed_or_reconciled() -> TestResult {
    let fx = Fixture::new()?;
    let journal = fx.open()?;
    let project = ProjectId::new("p1");
    let scope = journal.scope(&project);
    let (session_id, submission) = gitlab_submission(&scope)?;
    let d1 = submission.items[0].drafts[0];
    let d2 = submission.items[1].drafts[0];
    let again = [planned(Operation::GitLabPublishDraftNote, &[d2])];
    assert!(matches!(
        scope.plan_submission(session_id, &again),
        Err(JournalError::DraftHeld { draft, state: ItemState::Planned, .. }) if draft == d2
    ));
    assert_eq!(
        scope.draft_hold(d2)?,
        Some((submission.items[1].id, ItemState::Planned))
    );
    scope.begin(submission.items[0].id)?;
    scope.mark_unknown(submission.items[0].id, "нет ответа")?;
    scope.close_submission(submission.id)?;
    assert!(scope.submission(submission.id)?.closed);
    assert_eq!(scope.draft_hold(d2)?, None, "planned закрытой отправки");
    assert!(matches!(
        scope.begin(submission.items[1].id),
        Err(JournalError::Closed { submission: id }) if id == submission.id
    ));
    assert_eq!(
        scope.item(submission.items[1].id)?.state,
        ItemState::Planned
    );
    let held = Err::<(), _>(JournalError::DraftHeld {
        draft: d1,
        item: submission.items[0].id,
        state: ItemState::Unknown,
    });
    assert_eq!(
        format!("{:?}", scope.update_draft_body(d1, "новый текст")),
        format!("{held:?}")
    );
    assert_eq!(scope.draft(d1)?.body, "текст");
    let draft = scope.draft(d1)?;
    let review = scope.session(session_id)?;
    assert!(matches!(
        scope.save_head(session_id, &review, &[draft]),
        Err(JournalError::DraftHeld { draft, .. }) if draft == d1
    ));
    assert!(matches!(
        scope.plan_submission(
            session_id,
            &[planned(Operation::GitLabPublishDraftNote, &[d1])]
        ),
        Err(JournalError::DraftHeld {
            state: ItemState::Unknown,
            ..
        })
    ));
    let next = scope.plan_submission(session_id, &again)?;
    assert_eq!(next.items[0].drafts, [d2]);
    scope.update_draft_body(d2, "правка")?;
    scope.confirm(submission.items[0].id, None)?;
    scope.update_draft_body(d1, "после сверки")?;
    Ok(())
}

#[test]
fn draft_written_by_running_submission_is_not_edited_until_closed_or_done() -> TestResult {
    let fx = Fixture::new()?;
    let journal = fx.open()?;
    let scope = journal.scope(&ProjectId::new("p1"));

    let gl = scope.create_session(&session(Provider::GitLab, rev('a', 'b', 'c')?))?;
    let draft = scope
        .add_draft(gl, line(&held_file()?, Side::New, 2)?, "текст")?
        .id;
    let submission = scope.plan_submission(
        gl,
        &[
            planned(Operation::GitLabCreateDraftNote, &[draft]),
            planned(Operation::GitLabPublishDraftNote, &[draft]),
        ],
    )?;
    scope.update_draft_body(draft, "до записи")?;
    let create = submission.items[0].id;
    scope.begin(create)?;
    scope.confirm(create, Some(&ProviderId::new("71")))?;
    assert!(matches!(
        scope.update_draft_body(draft, "после создания"),
        Err(JournalError::DraftHeld { item, state: ItemState::Confirmed, .. }) if item == create
    ));
    let stored = scope.draft(draft)?;
    assert_eq!(stored.body, "до записи");
    let review = scope.session(gl)?;
    assert!(matches!(
        scope.save_head(gl, &review, &[stored]),
        Err(JournalError::DraftHeld {
            state: ItemState::Confirmed,
            ..
        })
    ));
    scope.close_submission(submission.id)?;
    scope.update_draft_body(draft, "после закрытия")?;

    let gh = scope.create_session(&session(Provider::GitHub, rev('a', 'a', 'c')?))?;
    let comment = scope
        .add_draft(gh, line(&held_file()?, Side::New, 2)?, "текст")?
        .id;
    let pending = scope.plan(
        gh,
        &SubmissionSpec {
            event: Some("COMMENT".to_owned()),
            ..SubmissionSpec::default()
        },
        &[
            planned(Operation::GitHubCreatePending, &[]),
            planned(Operation::GitHubAddPendingComment, &[comment]),
            planned(Operation::GitHubSubmitReview, &[]),
        ],
    )?;
    for item in &pending.items[..2] {
        scope.begin(item.id)?;
        scope.confirm(item.id, None)?;
    }
    assert!(matches!(
        scope.update_draft_body(comment, "Submit впереди"),
        Err(JournalError::DraftHeld {
            state: ItemState::Confirmed,
            ..
        })
    ));
    let submit = pending.items[2].id;
    scope.begin(submit)?;
    scope.confirm(submit, None)?;
    scope.update_draft_body(comment, "после отправки")?;
    Ok(())
}

#[test]
fn refute_only_moves_known_object_operations_out_of_unknown() -> TestResult {
    let fx = Fixture::new()?;
    let journal = fx.open()?;
    let project = ProjectId::new("p1");
    let scope = journal.scope(&project);
    let (session_id, submission) = gitlab_submission(&scope)?;
    let publish = submission.items[0].id;
    let summary = submission.items[3].id;
    assert!(matches!(
        scope.refute(publish, "черновик на месте"),
        Err(JournalError::Transition {
            from: ItemState::Planned,
            ..
        })
    ));
    for item in [publish, summary] {
        scope.begin(item)?;
        scope.mark_unknown(item, "нет ответа")?;
    }
    assert!(matches!(
        scope.refute(summary, "нет заметки"),
        Err(JournalError::Refute {
            operation: Operation::GitLabSummaryNote,
            ..
        })
    ));
    scope.refute(publish, "черновик на месте")?;
    let stored = scope.item(publish)?;
    assert_eq!(stored.state, ItemState::Failed);
    assert_eq!(stored.detail.as_deref(), Some("черновик на месте"));
    assert!(stored.in_flight_at.is_some());
    assert!(scope.submission(submission.id)?.closed);
    let draft = submission.items[0].drafts[0];
    assert_eq!(scope.draft_hold(draft)?, None);
    scope.plan_submission(
        session_id,
        &[planned(Operation::GitLabPublishDraftNote, &[draft])],
    )?;
    assert!(matches!(
        scope.refute(publish, "повтор"),
        Err(JournalError::Transition {
            from: ItemState::Failed,
            ..
        })
    ));
    for operation in Operation::ALL {
        let creates = matches!(
            operation,
            Operation::GitHubCreateReview
                | Operation::GitHubCreatePending
                | Operation::GitHubAddPendingComment
                | Operation::GitLabCreateDraftNote
                | Operation::GitLabSummaryNote
        );
        assert_eq!(
            operation.targets_known_object(),
            !creates,
            "{}",
            operation.name()
        );
    }
    Ok(())
}

#[test]
fn confirm_with_records_remote_ids_in_one_commit_and_spec_survives_reopen() -> TestResult {
    let fx = Fixture::new()?;
    let project = ProjectId::new("p1");
    let (create, publish, draft, submission_id) = {
        let journal = fx.open()?;
        let scope = journal.scope(&project);
        let id = scope.create_session(&session(Provider::GitHub, rev('a', 'a', 'c')?))?;
        let draft = scope
            .add_draft(id, line(&held_file()?, Side::New, 2)?, "текст")?
            .id;
        let spec = SubmissionSpec {
            event: Some("APPROVE".to_owned()),
            summary: "Итог".to_owned(),
            pending_review: None,
            pending_foreign: Some(vec![ProviderId::new("PRRC_9")]),
        };
        let submission = scope.plan(
            id,
            &spec,
            &[
                planned(Operation::GitHubCreatePending, &[]),
                planned(Operation::GitHubAddPendingComment, &[draft]),
            ],
        )?;
        assert_eq!(submission.spec, spec);
        let (create, publish) = (submission.items[0].id, submission.items[1].id);
        scope.begin(create)?;
        scope.confirm_with(
            create,
            &Confirmation {
                remote: Some(ProviderId::new("55")),
                drafts: Vec::new(),
                pending_review: Some(ProviderId::new("55")),
            },
        )?;
        scope.begin(publish)?;
        let foreign = Confirmation {
            remote: Some(ProviderId::new("x")),
            drafts: vec![(DraftId(draft.0 + 100), ProviderId::new("PRRC_1"))],
            pending_review: None,
        };
        assert!(matches!(
            scope.confirm_with(publish, &foreign),
            Err(JournalError::NotFound { .. })
        ));
        assert_eq!(scope.item(publish)?.state, ItemState::InFlight, "откат");
        (create, publish, draft, submission.id)
    };
    let journal = fx.open()?;
    let scope = journal.scope(&project);
    let submission = scope.submission(submission_id)?;
    assert_eq!(submission.spec.event.as_deref(), Some("APPROVE"));
    assert_eq!(submission.spec.summary, "Итог");
    assert_eq!(submission.spec.pending_review, Some(ProviderId::new("55")));
    assert_eq!(
        submission.spec.pending_foreign,
        Some(vec![ProviderId::new("PRRC_9")])
    );
    assert_eq!(scope.item(create)?.state, ItemState::Confirmed);
    assert_eq!(scope.item(publish)?.state, ItemState::Unknown);
    assert_eq!(scope.draft(draft)?.remote_id, None);
    scope.confirm_with(
        publish,
        &Confirmation {
            remote: Some(ProviderId::new("PRRC_1")),
            drafts: vec![(draft, ProviderId::new("PRRC_1"))],
            pending_review: None,
        },
    )?;
    assert_eq!(
        scope.draft(draft)?.remote_id,
        Some(ProviderId::new("PRRC_1"))
    );
    assert_eq!(scope.submissions(submission.session)?.len(), 1);
    Ok(())
}

#[test]
fn note_reconcile_keeps_unknown_and_records_candidate_and_detail() -> TestResult {
    let fx = Fixture::new()?;
    let journal = fx.open()?;
    let project = ProjectId::new("p1");
    let scope = journal.scope(&project);
    let (_, submission) = gitlab_submission(&scope)?;
    let item = submission.items[3].id;
    assert!(matches!(
        scope.note_reconcile(item, None, "рано"),
        Err(JournalError::Candidate {
            state: ItemState::Planned,
            ..
        })
    ));
    scope.begin(item)?;
    scope.mark_unknown(item, "нет ответа")?;
    scope.note_reconcile(item, None, "неоднозначно: 2 совпадения")?;
    let stored = scope.item(item)?;
    assert_eq!(
        (
            stored.state,
            stored.candidate.clone(),
            stored.detail.as_deref()
        ),
        (ItemState::Unknown, None, Some("неоднозначно: 2 совпадения"))
    );
    scope.note_reconcile(item, Some(&ProviderId::new("n7")), "кандидат")?;
    assert_eq!(scope.item(item)?.candidate, Some(ProviderId::new("n7")));
    Ok(())
}
