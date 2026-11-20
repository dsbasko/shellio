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
