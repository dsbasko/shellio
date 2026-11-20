use super::reconcile::{
    GitHubLedger, GitLabLedger, Verdict, apply, reconcile_github, reconcile_gitlab,
};
use crate::fake::FakeServer;
use crate::fake::github::{FakeCommit, FakeFile, GithubFake, Permission};
use crate::fake::gitlab::{Access, FakeDiff, GitlabFake};
use crate::github::{self, GitHub, ReviewEvent};
use crate::gitlab::{self, GitLab};
use crate::journal::{
    Confirmation, ItemState, Journal, Operation, PlannedItem, ProjectId, SubmissionSpec,
};
use crate::model::anchor::{Anchor, LinePoint, Side};
use crate::model::{Placement, Provider, ReviewSession, ReviewTarget};
use crate::transport::http::{Credential, HttpConfig, HttpTransport, Secret};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const BASE: &str = "1111111111111111111111111111111111111111";
const HEAD: &str = "2222222222222222222222222222222222222222";
const PATH: &str = "src/a.rs";
const DIFF: &str = "@@ -1,4 +1,4 @@\n ctx1\n-old2\n+new2\n ctx3\n ctx4\n";

fn planned(operation: Operation, drafts: &[crate::model::DraftId]) -> PlannedItem {
    PlannedItem {
        operation,
        drafts: drafts.to_vec(),
    }
}

#[test]
fn gitlab_publish_is_refuted_left_unresolved_or_candidate_but_never_confirmed() -> TestResult {
    let mut fake = GitlabFake::new("g/p");
    fake.add_user("bob", "tok-reviewer", Some(Access::Developer));
    fake.add_user("alice", "tok-author", Some(Access::Developer));
    fake.add_mr(3, "alice", BASE, HEAD, vec![FakeDiff::modified(PATH, DIFF)]);
    fake.add_reviewer(3, "bob")?;
    let server = FakeServer::start(fake)?;
    let target = ReviewTarget {
        provider: Provider::GitLab,
        host: "gitlab.test".to_owned(),
        repo: "g/p".to_owned(),
        number: 3,
    };
    let gitlab = GitLab::new(
        HttpTransport::new(
            HttpConfig::default(),
            Some(Credential::private_token(Secret::new("tok-reviewer"))),
        ),
        gitlab::Endpoints::instance(server.base()),
        &target,
    )?;
    let snapshot = gitlab.merge_request()?;
    let session = ReviewSession::new("bob", target, snapshot.revision.clone());
    let dir = tempfile::tempdir()?;
    let journal = Journal::open(&dir.path().join("state.db"))?;
    let project = ProjectId::new("p");
    let scope = journal.scope(&project);
    let session_id = scope.create_session(&session)?;
    let file = &snapshot.files[0];
    let mut ids = Vec::new();
    for (line, body) in [(1, "первый"), (2, "второй"), (3, "третий\r\n")] {
        let anchor = Anchor::line(file, LinePoint::at(file, Side::New, line)?)?;
        ids.push(
            scope
                .add_draft(session_id, Placement::Line(anchor), body)?
                .id,
        );
    }
    let mut items: Vec<PlannedItem> = ids
        .iter()
        .map(|id| planned(Operation::GitLabCreateDraftNote, &[*id]))
        .collect();
    items.extend(
        ids.iter()
            .map(|id| planned(Operation::GitLabPublishDraftNote, &[*id])),
    );
    let submission = scope.plan_submission(session_id, &items)?;
    let mut known = Vec::new();
    for (item, id) in submission.items[..3].iter().zip(&ids) {
        scope.begin(item.id)?;
        let draft = scope.draft(*id)?;
        let (created, _) = gitlab.create_draft(&session, &draft, false)?;
        scope.confirm_with(
            item.id,
            &Confirmation {
                remote: Some(created.id().clone()),
                drafts: vec![(*id, created.id().clone())],
                pending_review: None,
            },
        )?;
        known.push(created);
    }
    let publishes = &submission.items[3..];
    for item in publishes {
        scope.begin(item.id)?;
        scope.mark_unknown(item.id, "нет ответа")?;
    }
    let removed: u64 = known[1].id().as_str().parse()?;
    server.with(|fake| fake.remove_draft(3, removed))?;
    gitlab.publish_draft(&known[2])?;

    let submission = scope.submission(submission.id)?;
    let verdicts: Vec<Verdict> = publishes
        .iter()
        .zip(&ids)
        .map(
            |(item, id)| -> Result<Verdict, Box<dyn std::error::Error>> {
                let draft = scope.draft(*id)?;
                let item = scope.item(item.id)?;
                Ok(reconcile_gitlab(
                    &gitlab,
                    &GitLabLedger {
                        session: &session,
                        submission: &submission,
                        item: &item,
                        drafts: std::slice::from_ref(&draft),
                        own_drafts: &[],
                        own_notes: &[],
                    },
                ))
            },
        )
        .collect::<Result<_, _>>()?;
    assert!(
        matches!(&verdicts[0], Verdict::Refuted { .. }),
        "{:?}",
        verdicts[0]
    );
    assert!(
        matches!(&verdicts[1], Verdict::Unresolved { .. }),
        "удалён вторым клиентом: {:?}",
        verdicts[1]
    );
    assert!(
        matches!(&verdicts[2], Verdict::Candidate { .. }),
        "опубликован, но связи нет: {:?}",
        verdicts[2]
    );
    for (item, verdict) in publishes.iter().zip(&verdicts) {
        apply(&scope, item.id, verdict)?;
    }
    let states: Vec<ItemState> = publishes
        .iter()
        .map(|item| scope.item(item.id).map(|item| item.state))
        .collect::<Result<_, _>>()?;
    assert_eq!(
        states,
        [ItemState::Failed, ItemState::Unknown, ItemState::Unknown]
    );
    assert!(scope.item(publishes[2].id)?.candidate.is_some());
    assert!(scope.submission(submission.id)?.closed);
    assert_eq!(server.count(crate::fake::gitlab::BULK_PUBLISH), 0);
    assert_eq!(
        server.count(crate::fake::gitlab::PUBLISH),
        1,
        "сверка запись не повторяет"
    );
    Ok(())
}

#[test]
fn pending_id_alone_does_not_confirm_submit() -> TestResult {
    let mut fake = GithubFake::new("o", "r");
    fake.add_user("bob", "tok-reviewer", Permission::Write);
    fake.add_user("alice", "tok-author", Permission::Write);
    fake.add_pull(
        7,
        "alice",
        BASE,
        FakeCommit {
            sha: HEAD.to_owned(),
            files: vec![FakeFile::modified(PATH, DIFF)],
        },
    );
    let server = FakeServer::start(fake)?;
    let target = ReviewTarget {
        provider: Provider::GitHub,
        host: "github.com".to_owned(),
        repo: "o/r".to_owned(),
        number: 7,
    };
    let github = GitHub::new(
        HttpTransport::new(
            HttpConfig::default(),
            Some(Credential::bearer(Secret::new("tok-reviewer"))),
        ),
        github::Endpoints::single(server.base()),
        &target,
    )?;
    let caps = github.capabilities()?;
    let snapshot = github.pull()?;
    let session = ReviewSession::new("bob", target, snapshot.revision.clone());
    let dir = tempfile::tempdir()?;
    let journal = Journal::open(&dir.path().join("state.db"))?;
    let project = ProjectId::new("p");
    let scope = journal.scope(&project);
    let session_id = scope.create_session(&session)?;
    let spec = SubmissionSpec {
        event: Some(ReviewEvent::Comment.name().to_owned()),
        summary: "Итог".to_owned(),
        ..SubmissionSpec::default()
    };
    let submission = scope.plan(
        session_id,
        &spec,
        &[
            planned(Operation::GitHubCreatePending, &[]),
            planned(Operation::GitHubSubmitReview, &[]),
        ],
    )?;
    let (create, submit) = (submission.items[0].id, submission.items[1].id);
    scope.begin(create)?;
    let clear = github.check_no_pending(&caps)?;
    let pending = github.start_pending(&session, &caps, &clear)?;
    scope.confirm_with(
        create,
        &Confirmation {
            remote: Some(pending.id().clone()),
            drafts: Vec::new(),
            pending_review: Some(pending.id().clone()),
        },
    )?;
    scope.begin(submit)?;
    scope.mark_unknown(submit, "нет ответа")?;
    let read = |scope: &crate::journal::Scope<'_>| -> Result<Verdict, Box<dyn std::error::Error>> {
        let submission = scope.submission(submission.id)?;
        let item = scope.item(submit)?;
        Ok(reconcile_github(
            &github,
            &GitHubLedger {
                session: &session,
                submission: &submission,
                item: &item,
                drafts: &[],
                own_comments: &[],
            },
        ))
    };
    assert!(
        matches!(read(&scope)?, Verdict::Refuted { .. }),
        "PENDING не отправлен: ID создания Submit не подтверждает"
    );
    github.submit_pending(&pending, ReviewEvent::Comment, "Итог")?;
    let verdict = read(&scope)?;
    assert_eq!(
        verdict,
        Verdict::Confirmed {
            remote: Some(pending.id().clone())
        }
    );
    apply(&scope, submit, &verdict)?;
    assert_eq!(scope.item(submit)?.state, ItemState::Confirmed);
    Ok(())
}

mod run {
    use std::time::Duration;

    use super::{BASE, DIFF, HEAD, PATH, TestResult};
    use crate::fake::github::{FakeCommit, FakeFile, GithubFake, Permission};
    use crate::fake::gitlab::{Access, BULK_PUBLISH, DRAFTS, FakeDiff, GitlabFake, NOTES, PUBLISH};
    use crate::fake::{FakeServer, Fault, Reply};
    use crate::github::{self, GitHub, ReviewEvent};
    use crate::gitlab::{self, GitLab};
    use crate::journal::{ItemState, Journal, Operation, ProjectId, SessionId};
    use crate::model::anchor::{Anchor, LinePoint, Side};
    use crate::model::{Placement, Provider, ReviewSession, ReviewTarget};
    use crate::submit::reconcile::Verdict;
    use crate::submit::{
        Delivery, GitLabPlan, SubmitError, plan_github_local, plan_github_pending, plan_gitlab,
        reconcile_github, reconcile_gitlab, report, run_github, run_gitlab,
    };
    use crate::transport::http::{Credential, HttpConfig, HttpTransport, Secret};
    use crate::transport::{Failure, Request, Response, Transport};

    const SUBMIT: &str = "POST /repos/{owner}/{repo}/pulls/{number}/reviews/{review_id}/events";

    fn gitlab_target() -> ReviewTarget {
        ReviewTarget {
            provider: Provider::GitLab,
            host: "gitlab.test".to_owned(),
            repo: "g/p".to_owned(),
            number: 3,
        }
    }

    fn gitlab_server() -> Result<FakeServer<GitlabFake>, String> {
        let mut fake = GitlabFake::new("g/p");
        fake.add_user("bob", "tok-reviewer", Some(Access::Developer));
        fake.add_user("alice", "tok-author", Some(Access::Developer));
        fake.add_mr(3, "alice", BASE, HEAD, vec![FakeDiff::modified(PATH, DIFF)]);
        fake.add_reviewer(3, "bob")?;
        FakeServer::start(fake)
    }

    fn gitlab_adapter<T: Transport>(
        transport: T,
        server: &FakeServer<GitlabFake>,
    ) -> Result<GitLab<T>, gitlab::ApiError> {
        GitLab::new(
            transport,
            gitlab::Endpoints::instance(server.base()),
            &gitlab_target(),
        )
    }

    fn http(token: &str) -> HttpTransport {
        HttpTransport::new(
            HttpConfig::default(),
            Some(Credential::private_token(Secret::new(token))),
        )
    }

    /// Сессия и черновики на строках `lines` (новая сторона) с текстами `bodies`.
    fn gitlab_session<T: Transport>(
        journal: &Journal,
        gitlab: &GitLab<T>,
        drafts: &[(u32, &str)],
    ) -> Result<SessionId, Box<dyn std::error::Error>> {
        let snapshot = gitlab.merge_request()?;
        let session = ReviewSession::new("bob", gitlab_target(), snapshot.revision.clone());
        let scope = journal.scope(&ProjectId::new("p"));
        let id = scope.create_session(&session)?;
        let file = &snapshot.files[0];
        for (line, body) in drafts {
            let anchor = Anchor::line(file, LinePoint::at(file, Side::New, *line)?)?;
            scope.add_draft(id, Placement::Line(anchor), body)?;
        }
        Ok(id)
    }

    fn states(report: &crate::submit::Report) -> Vec<(Operation, ItemState)> {
        report
            .items
            .iter()
            .map(|item| (item.operation, item.state))
            .collect()
    }

    #[test]
    fn gitlab_run_creates_publishes_and_summarises_with_full_report() -> TestResult {
        let server = gitlab_server()?;
        let gitlab = gitlab_adapter(http("tok-reviewer"), &server)?;
        let caps = gitlab.capabilities()?;
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("state.db");
        let journal = Journal::open(&path)?;
        let session = gitlab_session(&journal, &gitlab, &[(1, "первый"), (2, "второй")])?;
        let scope = journal.scope(&ProjectId::new("p"));
        let plan = GitLabPlan {
            summary: "Итог".to_owned(),
            ..GitLabPlan::default()
        };
        let submission = plan_gitlab(&scope, session, &plan)?;
        let done = run_gitlab(&scope, submission.id, &gitlab, &caps)?;
        assert_eq!(done.delivery, Delivery::Full);
        drop(journal);
        let journal = Journal::open(&path)?;
        let scope = journal.scope(&ProjectId::new("p"));
        let reopened = report(&scope, submission.id)?;
        assert_eq!(reopened.delivery, Delivery::Full);
        assert!(journal.recovered().is_empty());
        assert_eq!(server.count(BULK_PUBLISH), 0);
        assert_eq!(server.count(PUBLISH), 2);
        assert_eq!(server.count(NOTES), 1);
        assert!(server.with(|fake| fake.drafts(3)).is_empty());
        assert!(
            matches!(
                plan_gitlab(&scope, session, &GitLabPlan::default()),
                Err(SubmitError::Empty)
            ),
            "опубликованные черновики в новую отправку не входят"
        );
        Ok(())
    }

    #[test]
    fn refused_publish_closes_submission_and_reports_partial() -> TestResult {
        let server = gitlab_server()?;
        server.inject(Fault::new(PUBLISH, Reply::Status(403)).skip(1));
        let gitlab = gitlab_adapter(http("tok-reviewer"), &server)?;
        let caps = gitlab.capabilities()?;
        let dir = tempfile::tempdir()?;
        let journal = Journal::open(&dir.path().join("state.db"))?;
        let session = gitlab_session(&journal, &gitlab, &[(1, "первый"), (2, "второй")])?;
        let scope = journal.scope(&ProjectId::new("p"));
        let submission = plan_gitlab(&scope, session, &GitLabPlan::default())?;
        let partial = run_gitlab(&scope, submission.id, &gitlab, &caps)?;
        assert_eq!(partial.delivery, Delivery::Partial);
        assert!(partial.closed);
        assert_eq!(
            states(&partial),
            [
                (Operation::GitLabCreateDraftNote, ItemState::Confirmed),
                (Operation::GitLabCreateDraftNote, ItemState::Confirmed),
                (Operation::GitLabPublishDraftNote, ItemState::Confirmed),
                (Operation::GitLabPublishDraftNote, ItemState::Failed),
            ]
        );
        assert!(matches!(
            run_gitlab(&scope, submission.id, &gitlab, &caps),
            Err(SubmitError::Closed(_))
        ));
        let again = plan_gitlab(&scope, session, &GitLabPlan::default())?;
        let ops: Vec<Operation> = again.items.iter().map(|item| item.operation).collect();
        assert_eq!(
            ops,
            [Operation::GitLabPublishDraftNote],
            "черновику с ID второе создание не планируется, опубликованный не входит"
        );
        let done = run_gitlab(&scope, again.id, &gitlab, &caps)?;
        assert_eq!(done.delivery, Delivery::Full);
        assert_eq!(server.count(DRAFTS), 2);
        server.finish()?;
        Ok(())
    }

    #[test]
    fn rate_limit_fails_and_lost_response_stays_unknown_without_retry() -> TestResult {
        let server = gitlab_server()?;
        server.inject(Fault::new(DRAFTS, Reply::RateLimited));
        let gitlab = gitlab_adapter(http("tok-reviewer"), &server)?;
        let caps = gitlab.capabilities()?;
        let dir = tempfile::tempdir()?;
        let journal = Journal::open(&dir.path().join("state.db"))?;
        let session = gitlab_session(&journal, &gitlab, &[(1, "первый")])?;
        let scope = journal.scope(&ProjectId::new("p"));
        let limited = plan_gitlab(&scope, session, &GitLabPlan::default())?;
        let refused = run_gitlab(&scope, limited.id, &gitlab, &caps)?;
        assert_eq!(refused.delivery, Delivery::NotSent);
        assert_eq!(refused.items[0].state, ItemState::Failed);

        server.inject(Fault::new(DRAFTS, Reply::Status(502)).after_apply());
        let lost = plan_gitlab(&scope, session, &GitLabPlan::default())?;
        let unknown = run_gitlab(&scope, lost.id, &gitlab, &caps)?;
        assert_eq!(unknown.delivery, Delivery::Partial);
        assert_eq!(unknown.unresolved, 1);
        assert_eq!(unknown.items[0].state, ItemState::Unknown);
        assert!(matches!(
            run_gitlab(&scope, lost.id, &gitlab, &caps),
            Err(SubmitError::Unresolved(_))
        ));
        assert!(
            matches!(
                plan_gitlab(&scope, session, &GitLabPlan::default()),
                Err(SubmitError::Empty)
            ),
            "черновик под unknown в новую отправку не входит"
        );
        let verdicts = reconcile_gitlab(&scope, lost.id, &gitlab)?;
        assert!(
            matches!(verdicts[..], [Verdict::Candidate { .. }]),
            "{verdicts:?}"
        );
        assert_eq!(scope.item(lost.items[0].id)?.state, ItemState::Unknown);
        assert_eq!(server.count(DRAFTS), 2, "после сбоя записей без сверки нет");
        server.finish()?;
        Ok(())
    }

    #[test]
    fn crash_before_request_reopens_as_unknown_without_server_write() -> TestResult {
        let server = gitlab_server()?;
        let gitlab = gitlab_adapter(http("tok-reviewer"), &server)?;
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("state.db");
        let journal = Journal::open(&path)?;
        let session = gitlab_session(&journal, &gitlab, &[(1, "первый")])?;
        let scope = journal.scope(&ProjectId::new("p"));
        let submission = plan_gitlab(&scope, session, &GitLabPlan::default())?;
        let first = submission.items[0].id;
        scope.begin(first)?;
        drop(scope);
        drop(journal);

        let journal = Journal::open(&path)?;
        assert_eq!(journal.recovered(), [first]);
        let scope = journal.scope(&ProjectId::new("p"));
        let verdicts = reconcile_gitlab(&scope, submission.id, &gitlab)?;
        assert!(
            matches!(verdicts[..], [Verdict::Unresolved { .. }]),
            "{verdicts:?}"
        );
        assert_eq!(scope.item(first)?.state, ItemState::Unknown);
        assert_eq!(server.count(DRAFTS), 0);
        Ok(())
    }

    /// Транспорт, который паникует после доставки записи с путём `path`: процесс умер между
    /// принятой записью и записью ответа в журнал.
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

    #[test]
    fn crash_after_delivery_reopens_as_unknown_and_reconciles_to_candidate() -> TestResult {
        let server = gitlab_server()?;
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("state.db");
        let (session, submission) = {
            let gitlab = gitlab_adapter(http("tok-reviewer"), &server)?;
            let journal = Journal::open(&path)?;
            let session = gitlab_session(&journal, &gitlab, &[(1, "первый")])?;
            let scope = journal.scope(&ProjectId::new("p"));
            (
                session,
                plan_gitlab(&scope, session, &GitLabPlan::default())?,
            )
        };
        let base = server.base().to_owned();
        let crashed = std::thread::spawn(move || {
            let run = || -> Result<(), Box<dyn std::error::Error>> {
                let transport = PanicAfter {
                    inner: http("tok-reviewer"),
                    path: "/draft_notes",
                };
                let gitlab = GitLab::new(
                    transport,
                    gitlab::Endpoints::instance(&base),
                    &gitlab_target(),
                )?;
                let caps = gitlab.capabilities()?;
                let journal = Journal::open(&path)?;
                let scope = journal.scope(&ProjectId::new("p"));
                run_gitlab(&scope, submission.id, &gitlab, &caps)?;
                Ok(())
            };
            let _ = run();
        })
        .join();
        assert!(crashed.is_err(), "поток должен упасть после доставки");
        server.wait_calls(1, Duration::from_secs(10))?;
        assert_eq!(server.count(DRAFTS), 1);

        let journal = Journal::open(&dir.path().join("state.db"))?;
        let first = submission.items[0].id;
        assert_eq!(journal.recovered(), [first]);
        let scope = journal.scope(&ProjectId::new("p"));
        let gitlab = gitlab_adapter(http("tok-reviewer"), &server)?;
        let verdicts = reconcile_gitlab(&scope, submission.id, &gitlab)?;
        assert!(
            matches!(verdicts[..], [Verdict::Candidate { .. }]),
            "записанный сервером черновик — кандидат, а не подтверждение: {verdicts:?}"
        );
        let item = scope.item(first)?;
        assert_eq!(item.state, ItemState::Unknown);
        assert!(item.candidate.is_some());
        assert_eq!(server.count(DRAFTS), 1, "повтора записи нет");
        let _ = session;
        Ok(())
    }

    #[test]
    fn two_identical_published_notes_leave_lost_publish_ambiguous() -> TestResult {
        let server = gitlab_server()?;
        server.inject(
            Fault::new(PUBLISH, Reply::Status(502))
                .after_apply()
                .skip(1),
        );
        let gitlab = gitlab_adapter(http("tok-reviewer"), &server)?;
        let caps = gitlab.capabilities()?;
        let dir = tempfile::tempdir()?;
        let journal = Journal::open(&dir.path().join("state.db"))?;
        let session = gitlab_session(&journal, &gitlab, &[(2, "одинаковый"), (2, "одинаковый")])?;
        let scope = journal.scope(&ProjectId::new("p"));
        let submission = plan_gitlab(&scope, session, &GitLabPlan::default())?;
        let partial = run_gitlab(&scope, submission.id, &gitlab, &caps)?;
        assert_eq!(partial.unresolved, 1);
        let verdicts = reconcile_gitlab(&scope, submission.id, &gitlab)?;
        assert!(
            matches!(&verdicts[..], [Verdict::Ambiguous { candidates }] if candidates.len() == 2),
            "{verdicts:?}"
        );
        let lost = &partial.items[3];
        assert_eq!(scope.item(lost.id)?.state, ItemState::Unknown);
        assert_eq!(server.count(PUBLISH), 2);
        server.finish()?;
        Ok(())
    }

    #[test]
    fn foreign_twin_on_second_page_with_normalised_text_is_only_a_candidate() -> TestResult {
        let server = gitlab_server()?;
        server.inject(Fault::new(DRAFTS, Reply::Status(502)));
        let gitlab = gitlab_adapter(http("tok-reviewer"), &server)?;
        let caps = gitlab.capabilities()?;
        let dir = tempfile::tempdir()?;
        let journal = Journal::open(&dir.path().join("state.db"))?;
        let session = gitlab_session(&journal, &gitlab, &[(2, "текст ревью  \r\n")])?;
        let scope = journal.scope(&ProjectId::new("p"));
        let submission = plan_gitlab(&scope, session, &GitLabPlan::default())?;
        let lost = run_gitlab(&scope, submission.id, &gitlab, &caps)?;
        assert_eq!(lost.items[0].state, ItemState::Unknown);
        assert!(
            server.with(|fake| fake.drafts(3)).is_empty(),
            "запись не применена"
        );

        let draft = scope.draft(submission.items[0].drafts[0])?;
        let at = scope.session(session)?;
        let position = serde_json::to_value(at.gitlab_position(&draft)?)?;
        server.with(|fake| -> Result<(), String> {
            for index in 0..100 {
                fake.plant_draft(3, "bob", &format!("другой {index}"), Some(&position))?;
            }
            fake.plant_draft(3, "bob", "текст ревью", Some(&position))?;
            Ok(())
        })?;
        let verdicts = reconcile_gitlab(&scope, submission.id, &gitlab)?;
        assert!(
            matches!(verdicts[..], [Verdict::Candidate { .. }]),
            "чужой близнец на второй странице: {verdicts:?}"
        );
        assert_eq!(
            scope.item(submission.items[0].id)?.state,
            ItemState::Unknown
        );
        assert_eq!(server.count(DRAFTS), 1);
        server.finish()?;
        Ok(())
    }

    #[test]
    fn held_draft_skips_head_change_and_is_rebound_after_refute_through_reopen() -> TestResult {
        use crate::journal::JournalError;
        use crate::model::DraftComment;
        use crate::model::revision::{HeadCheck, HeadPath};

        const NEW_HEAD: &str = "3333333333333333333333333333333333333333";
        let server = gitlab_server()?;
        server.inject(Fault::new(PUBLISH, Reply::Status(502)));
        let gitlab = gitlab_adapter(http("tok-reviewer"), &server)?;
        let caps = gitlab.capabilities()?;
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("state.db");
        let journal = Journal::open(&path)?;
        let session_id = gitlab_session(&journal, &gitlab, &[(2, "держит элемент")])?;
        let scope = journal.scope(&ProjectId::new("p"));
        let first = plan_gitlab(&scope, session_id, &GitLabPlan::default())?;
        let stopped = run_gitlab(&scope, first.id, &gitlab, &caps)?;
        assert_eq!(stopped.unresolved, 1);
        let held = first.items[0].drafts[0];
        let held_snapshot = gitlab.merge_request()?;

        server.with(|fake| fake.push_head(3, NEW_HEAD, vec![FakeDiff::modified(PATH, DIFF)]))?;
        assert!(matches!(
            run_gitlab(&scope, first.id, &gitlab, &caps),
            Err(SubmitError::HeadChanged { .. })
        ));
        let fresh = gitlab.merge_request()?;
        let mut session = scope.session(session_id)?;
        let before = session.revision().clone();
        let mut free: Vec<DraftComment> = Vec::new();
        for draft in scope.drafts(session_id)? {
            if scope.draft_hold(draft.id)?.is_none() {
                free.push(draft);
            }
        }
        assert!(
            free.is_empty(),
            "черновик под unknown в смену head не передаётся"
        );
        let HeadCheck::Changed(change) =
            session.check_head(&fresh.revision, &held_snapshot.files, &fresh.files, &free)
        else {
            return Err("смена head не замечена".into());
        };
        session.apply_head(&change, HeadPath::Move, &mut free, &fresh.files)?;
        assert!(matches!(
            scope.save_head(session_id, &session, &[scope.draft(held)?]),
            Err(JournalError::DraftHeld { .. })
        ));
        scope.save_head(session_id, &session, &free)?;
        assert_eq!(scope.session(session_id)?.revision(), &fresh.revision);
        assert_eq!(
            scope.draft(held)?.revision(),
            &before,
            "держит элемент — прежняя ревизия"
        );

        let verdicts = reconcile_gitlab(&scope, first.id, &gitlab)?;
        assert!(
            matches!(verdicts[..], [Verdict::Refuted { .. }]),
            "позиция и чтение по ID — на ревизии черновика: {verdicts:?}"
        );
        assert!(
            matches!(
                plan_gitlab(&scope, session_id, &GitLabPlan::default()),
                Err(SubmitError::Empty)
            ),
            "черновик на прежней ревизии не отправляется до перепривязки"
        );
        let mut draft = scope.draft(held)?;
        let file = &fresh.files[0];
        let anchor = Anchor::line(file, LinePoint::at(file, Side::New, 2)?)?;
        session.rebind_stale(&mut draft, anchor, &fresh.files)?;
        scope.save_head(session_id, &session, std::slice::from_ref(&draft))?;
        drop(scope);
        drop(journal);

        let journal = Journal::open(&path)?;
        let scope = journal.scope(&ProjectId::new("p"));
        let reopened = scope.draft(held)?;
        assert!(reopened.is_current());
        assert_eq!(reopened.revision(), &fresh.revision);
        assert_eq!(reopened.placement(), draft.placement());
        let plan = GitLabPlan {
            update: vec![held],
            ..GitLabPlan::default()
        };
        let retry = plan_gitlab(&scope, session_id, &plan)?;
        let ops: Vec<Operation> = retry.items.iter().map(|item| item.operation).collect();
        assert_eq!(
            ops,
            [
                Operation::GitLabUpdateDraftNote,
                Operation::GitLabPublishDraftNote
            ]
        );
        let done = run_gitlab(&scope, retry.id, &gitlab, &caps)?;
        assert_eq!(done.delivery, Delivery::Full);
        assert_eq!(server.count(DRAFTS), 1);
        assert_eq!(server.count(PUBLISH), 2);
        server.finish()?;
        Ok(())
    }

    fn github_setup() -> Result<FakeServer<GithubFake>, String> {
        let mut fake = GithubFake::new("o", "r");
        fake.add_user("bob", "tok-reviewer", Permission::Write);
        fake.add_user("alice", "tok-author", Permission::Write);
        fake.add_pull(
            7,
            "alice",
            BASE,
            FakeCommit {
                sha: HEAD.to_owned(),
                files: vec![FakeFile::modified(PATH, DIFF)],
            },
        );
        FakeServer::start(fake)
    }

    fn github_adapter(
        server: &FakeServer<GithubFake>,
    ) -> Result<GitHub<HttpTransport>, github::ApiError> {
        GitHub::new(
            HttpTransport::new(
                HttpConfig::default(),
                Some(Credential::bearer(Secret::new("tok-reviewer"))),
            ),
            github::Endpoints::single(server.base()),
            &ReviewTarget {
                provider: Provider::GitHub,
                host: "github.com".to_owned(),
                repo: "o/r".to_owned(),
                number: 7,
            },
        )
    }

    fn github_session(
        journal: &Journal,
        github: &GitHub<HttpTransport>,
        lines: &[(u32, &str)],
    ) -> Result<SessionId, Box<dyn std::error::Error>> {
        let snapshot = github.pull()?;
        let target = ReviewTarget {
            provider: Provider::GitHub,
            host: "github.com".to_owned(),
            repo: "o/r".to_owned(),
            number: 7,
        };
        let session = ReviewSession::new("bob", target, snapshot.revision.clone());
        let scope = journal.scope(&ProjectId::new("p"));
        let id = scope.create_session(&session)?;
        let file = &snapshot.files[0];
        for (line, body) in lines {
            let anchor = Anchor::line(file, LinePoint::at(file, Side::New, *line)?)?;
            scope.add_draft(id, Placement::Line(anchor), body)?;
        }
        Ok(id)
    }

    #[test]
    fn github_local_branch_sends_one_create_review() -> TestResult {
        let server = github_setup()?;
        let github = github_adapter(&server)?;
        let caps = github.capabilities()?;
        let dir = tempfile::tempdir()?;
        let journal = Journal::open(&dir.path().join("state.db"))?;
        let session = github_session(&journal, &github, &[(1, "первый"), (2, "второй")])?;
        let scope = journal.scope(&ProjectId::new("p"));
        let submission = plan_github_local(&scope, session, ReviewEvent::Comment, "Итог")?;
        assert_eq!(submission.items.len(), 1);
        assert_eq!(submission.items[0].drafts.len(), 2);
        let done = run_github(&scope, submission.id, &github, &caps)?;
        assert_eq!(done.delivery, Delivery::Full);
        let reviews = server.with(|fake| fake.reviews(7));
        assert_eq!(reviews.len(), 1);
        assert_eq!(server.with(|fake| fake.comments(7)).len(), 2);
        let again = plan_github_local(&scope, session, ReviewEvent::Comment, "Ещё")?;
        assert!(
            again.items[0].drafts.is_empty(),
            "отправленные не повторяются"
        );
        Ok(())
    }

    #[test]
    fn refuted_submit_is_retried_on_the_same_pending_after_reopen() -> TestResult {
        let server = github_setup()?;
        server.inject(Fault::new(SUBMIT, Reply::Status(502)));
        let github = github_adapter(&server)?;
        let caps = github.capabilities()?;
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("state.db");
        let journal = Journal::open(&path)?;
        let session = github_session(&journal, &github, &[(1, "первый"), (2, "второй")])?;
        let scope = journal.scope(&ProjectId::new("p"));
        let first = plan_github_pending(&scope, session, ReviewEvent::Comment, "Итог", None, &[])?;
        let stopped = run_github(&scope, first.id, &github, &caps)?;
        assert_eq!(
            states(&stopped),
            [
                (Operation::GitHubCreatePending, ItemState::Confirmed),
                (Operation::GitHubAddPendingComment, ItemState::Confirmed),
                (Operation::GitHubAddPendingComment, ItemState::Confirmed),
                (Operation::GitHubSubmitReview, ItemState::Unknown),
            ]
        );
        let verdicts = reconcile_github(&scope, first.id, &github)?;
        assert!(
            matches!(verdicts[..], [Verdict::Refuted { .. }]),
            "{verdicts:?}"
        );
        assert!(scope.submission(first.id)?.closed);
        drop(scope);
        drop(journal);

        let journal = Journal::open(&path)?;
        let scope = journal.scope(&ProjectId::new("p"));
        let retry = plan_github_pending(&scope, session, ReviewEvent::Comment, "Итог", None, &[])?;
        let ops: Vec<Operation> = retry.items.iter().map(|item| item.operation).collect();
        assert_eq!(ops, [Operation::GitHubSubmitReview]);
        assert_eq!(
            retry.spec.pending_review,
            first.spec.pending_review.clone().or_else(|| {
                scope
                    .submission(first.id)
                    .ok()
                    .and_then(|s| s.spec.pending_review)
            })
        );
        let done = run_github(&scope, retry.id, &github, &caps)?;
        assert_eq!(done.delivery, Delivery::Full);
        let reviews = server.with(|fake| fake.reviews(7));
        assert_eq!(reviews.len(), 1, "тот же PENDING, второго review нет");
        assert_eq!(server.count(SUBMIT), 2);
        server.finish()?;
        Ok(())
    }

    #[test]
    fn silent_submit_waits_for_receipt_and_reconciles_to_confirmed() -> TestResult {
        let server = github_setup()?;
        server.inject(Fault::new(SUBMIT, Reply::Silence).after_apply());
        let github = GitHub::new(
            HttpTransport::new(
                HttpConfig {
                    call_timeout: Duration::from_millis(300),
                    ..HttpConfig::default()
                },
                Some(Credential::bearer(Secret::new("tok-reviewer"))),
            ),
            github::Endpoints::single(server.base()),
            &ReviewTarget {
                provider: Provider::GitHub,
                host: "github.com".to_owned(),
                repo: "o/r".to_owned(),
                number: 7,
            },
        )?;
        let caps = github.capabilities()?;
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("state.db");
        let journal = Journal::open(&path)?;
        let session = github_session(&journal, &github, &[(1, "первый")])?;
        let scope = journal.scope(&ProjectId::new("p"));
        let submission =
            plan_github_pending(&scope, session, ReviewEvent::Comment, "Итог", None, &[])?;
        let stopped = run_github(&scope, submission.id, &github, &caps)?;
        assert_eq!(stopped.unresolved, 1);
        // Квитанция сервера, а не срок клиента: до неё молчащий Submit мог ещё не дойти.
        let logged = server.log().len();
        if server.count(SUBMIT) == 0 {
            server.wait_calls(logged + 1, Duration::from_secs(10))?;
        }
        assert_eq!(server.count(SUBMIT), 1);
        assert_eq!(server.parked(), 1);
        drop(scope);
        drop(journal);

        let journal = Journal::open(&path)?;
        let scope = journal.scope(&ProjectId::new("p"));
        let verdicts = reconcile_github(&scope, submission.id, &github)?;
        assert!(
            matches!(verdicts[..], [Verdict::Confirmed { .. }]),
            "известный review в отправленном состоянии: {verdicts:?}"
        );
        assert_eq!(report(&scope, submission.id)?.delivery, Delivery::Full);
        assert_eq!(server.count(SUBMIT), 1);
        server.release_parked();
        server.finish()?;
        Ok(())
    }
}
