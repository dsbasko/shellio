use serde_json::{Value, json};

use super::anchor::{Anchor, LinePoint, Side, line_code};
use super::diff::FileDiff;
use super::revision::{
    AnchorFate, HeadCheck, HeadPath, Revision, RevisionField, Sha, StaleReason, TripleOrigin,
    triple_origin,
};
use super::*;

type TestResult = Result<(), Box<dyn std::error::Error>>;

const PATH: &str = "src/lib.rs";
/// `printf 'src/lib.rs' | shasum -a 1` — независимый оракул `line_code`.
const PATH_SHA1: &str = "b24749917179fb5e3e613ed2a703fcdcc6cdf9da";

const HELD: &str = "\
@@ -1,5 +1,6 @@
 fn a() {}
-fn b() {}
+fn b2() {}
+fn c() {}
 fn d() {}
 fn e() {}
 fn f() {}
@@ -20,2 +21,2 @@
-fn t() {}
+fn t2() {}
 fn u() {}
";

/// Новый head: вставлена строка после `d`, `t2` заменена на `t3`, README из diff исчез.
const FRESH: &str = "\
@@ -1,5 +1,7 @@
 fn a() {}
-fn b() {}
+fn b2() {}
+fn c() {}
 fn d() {}
+fn d2() {}
 fn e() {}
 fn f() {}
@@ -20,2 +22,2 @@
-fn t() {}
+fn t3() {}
 fn u() {}
";

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

fn draft(id: u64, session: &ReviewSession, anchor: Anchor) -> DraftComment {
    DraftComment::new(DraftId(id), session, Placement::Line(anchor), "текст")
}

#[test]
fn sha_is_validated_and_normalized() -> TestResult {
    let upper = "ABCDEF0123456789ABCDEF0123456789ABCDEF01";
    assert_eq!(Sha::parse(upper)?.as_str(), upper.to_ascii_lowercase());
    assert!(Sha::parse(&"a".repeat(64)).is_ok());
    for bad in ["", "abc", &"g".repeat(40), &"a".repeat(41)] {
        assert!(
            matches!(Sha::parse(bad), Err(ModelError::Sha { .. })),
            "{bad}"
        );
    }
    Ok(())
}

#[test]
fn provider_id_is_opaque() {
    let id = ProviderId::new("PRR_kwDOAbc123");
    assert_eq!(id.as_str(), "PRR_kwDOAbc123");
    assert_ne!(ProviderId::new("1"), ProviderId::new("01"));
}

#[test]
fn line_point_side_must_fit_line_kind() -> TestResult {
    let file = held_file()?;
    let added = file.line_at(false, 3).ok_or("нет строки")?;
    let removed = file.line_at(true, 2).ok_or("нет строки")?;
    let context = file.line_at(false, 4).ok_or("нет строки")?;
    assert!(matches!(
        LinePoint::new(Side::Old, added),
        Err(ModelError::Side { .. })
    ));
    assert!(matches!(
        LinePoint::new(Side::New, removed),
        Err(ModelError::Side { .. })
    ));
    assert!(LinePoint::new(Side::Old, context).is_ok());
    assert!(LinePoint::new(Side::New, context).is_ok());
    assert!(matches!(
        LinePoint::at(&file, Side::New, 99),
        Err(ModelError::AnchorMissing { line: 99, .. })
    ));
    Ok(())
}

#[test]
fn range_must_be_ordered_within_one_hunk() -> TestResult {
    let file = held_file()?;
    let b = LinePoint::at(&file, Side::Old, 2)?;
    let c = LinePoint::at(&file, Side::New, 3)?;
    let t2 = LinePoint::at(&file, Side::New, 21)?;
    assert_eq!(Anchor::range(&file, c, b), Err(ModelError::RangeOrder));
    assert_eq!(Anchor::range(&file, c, c), Err(ModelError::RangeOrder));
    assert_eq!(Anchor::range(&file, b, t2), Err(ModelError::RangeHunk));
    assert!(Anchor::range(&file, b, c).is_ok());
    Ok(())
}

#[test]
fn github_anchor_sides_and_ranges() -> TestResult {
    let file = held_file()?;
    let gh = session(Provider::GitHub, rev('1', '2', '3')?);
    let head = "3".repeat(40);
    let cases: Vec<(Anchor, Value)> = vec![
        (
            Anchor::line(&file, LinePoint::at(&file, Side::New, 3)?)?,
            json!({"commit_id": head, "path": PATH, "line": 3, "side": "RIGHT"}),
        ),
        (
            Anchor::line(&file, LinePoint::at(&file, Side::Old, 2)?)?,
            json!({"commit_id": head, "path": PATH, "line": 2, "side": "LEFT"}),
        ),
        (
            Anchor::line(&file, LinePoint::at(&file, Side::Old, 3)?)?,
            json!({"commit_id": head, "path": PATH, "line": 3, "side": "LEFT"}),
        ),
        (
            Anchor::range(
                &file,
                LinePoint::at(&file, Side::Old, 2)?,
                LinePoint::at(&file, Side::New, 3)?,
            )?,
            json!({"commit_id": head, "path": PATH, "line": 3, "side": "RIGHT",
                   "start_line": 2, "start_side": "LEFT"}),
        ),
        (
            Anchor::range(
                &file,
                LinePoint::at(&file, Side::New, 4)?,
                LinePoint::at(&file, Side::New, 6)?,
            )?,
            json!({"commit_id": head, "path": PATH, "line": 6, "side": "RIGHT",
                   "start_line": 4, "start_side": "RIGHT"}),
        ),
    ];
    for (anchor, expected) in cases {
        let out = gh.github_anchor(&draft(1, &gh, anchor))?;
        assert_eq!(out.commit_id(), gh.revision().head_sha());
        assert_eq!(serde_json::to_value(&out)?, expected);
    }
    Ok(())
}

#[test]
fn github_anchor_uses_new_path_of_renamed_file() -> TestResult {
    let file = FileDiff::parse("old.rs", "new.rs", "@@ -1 +1 @@\n-x\n+y\n")?;
    let gh = session(Provider::GitHub, rev('1', '2', '3')?);
    let anchor = Anchor::line(&file, LinePoint::at(&file, Side::Old, 1)?)?;
    let out = serde_json::to_value(gh.github_anchor(&draft(1, &gh, anchor))?)?;
    assert_eq!(out["path"], "new.rs");
    Ok(())
}

#[test]
fn gitlab_position_lines_and_line_range() -> TestResult {
    let file = held_file()?;
    let gl = session(Provider::GitLab, rev('1', '2', '3')?);
    let triple = json!({
        "base_sha": "1".repeat(40),
        "start_sha": "2".repeat(40),
        "head_sha": "3".repeat(40),
        "position_type": "text",
        "old_path": PATH,
        "new_path": PATH,
    });
    let with = |extra: Value| -> Value {
        let mut value = triple.clone();
        if let (Some(map), Some(extra)) = (value.as_object_mut(), extra.as_object()) {
            map.extend(extra.clone());
        }
        value
    };
    let code = |old: u32, new: u32| format!("{PATH_SHA1}_{old}_{new}");
    let cases: Vec<(Anchor, Value)> = vec![
        (
            Anchor::line(&file, LinePoint::at(&file, Side::New, 3)?)?,
            with(json!({"new_line": 3})),
        ),
        (
            Anchor::line(&file, LinePoint::at(&file, Side::Old, 2)?)?,
            with(json!({"old_line": 2})),
        ),
        (
            Anchor::line(&file, LinePoint::at(&file, Side::New, 4)?)?,
            with(json!({"old_line": 3, "new_line": 4})),
        ),
        (
            Anchor::range(
                &file,
                LinePoint::at(&file, Side::Old, 2)?,
                LinePoint::at(&file, Side::New, 3)?,
            )?,
            with(json!({"new_line": 3, "line_range": {
                "start": {"line_code": code(2, 2), "type": "old", "old_line": 2},
                "end": {"line_code": code(3, 3), "type": "new", "new_line": 3},
            }})),
        ),
        (
            Anchor::range(
                &file,
                LinePoint::at(&file, Side::New, 4)?,
                LinePoint::at(&file, Side::New, 6)?,
            )?,
            with(json!({"old_line": 5, "new_line": 6, "line_range": {
                "start": {"line_code": code(3, 4), "old_line": 3, "new_line": 4},
                "end": {"line_code": code(5, 6), "old_line": 5, "new_line": 6},
            }})),
        ),
    ];
    for (anchor, expected) in cases {
        let position = gl.gitlab_position(&draft(1, &gl, anchor))?;
        assert_eq!(&position.revision(), gl.revision());
        assert_eq!(serde_json::to_value(&position)?, expected);
    }
    let added = file.line_at(false, 3).ok_or("нет строки")?;
    assert_eq!(line_code(PATH, added), code(3, 3));
    Ok(())
}

#[test]
fn provider_anchor_needs_matching_provider_and_line_placement() -> TestResult {
    let file = held_file()?;
    let gl = session(Provider::GitLab, rev('1', '2', '3')?);
    let anchor = Anchor::line(&file, LinePoint::at(&file, Side::New, 3)?)?;
    assert_eq!(
        gl.github_anchor(&draft(1, &gl, anchor)),
        Err(ModelError::Provider {
            expected: Provider::GitHub,
            found: Provider::GitLab,
        })
    );
    let reply = DraftComment::new(
        DraftId(2),
        &gl,
        Placement::Reply {
            thread: ProviderId::new("d1"),
        },
        "ответ",
    );
    assert_eq!(gl.gitlab_position(&reply), Err(ModelError::NotAnchored));
    Ok(())
}

/// ⚠️ `detached/` в репозиториях нет: образец `gitlab.rs:346–349` воспроизведён по словам
/// ADR-0015 — `base_sha` и `start_sha` из свежих `diff_refs`, `head_sha` из переданного ранее.
#[test]
fn fresh_refs_with_held_head_sha_are_rejected() -> TestResult {
    let held = rev('a', 'b', 'c')?;
    let fresh = rev('d', 'e', 'f')?;
    let mut gl = session(Provider::GitLab, held.clone());
    let mixed = Revision::new(
        fresh.base_sha().clone(),
        fresh.start_sha().clone(),
        held.head_sha().clone(),
    );
    assert_eq!(
        triple_origin(&held, &fresh, &mixed),
        TripleOrigin::Mixed {
            fresh: vec![RevisionField::Base, RevisionField::Start],
            held: vec![RevisionField::Head],
        }
    );
    assert_eq!(
        gl.verify_outgoing(&mixed),
        Err(ModelError::RevisionMismatch {
            fields: vec![RevisionField::Base, RevisionField::Start],
        })
    );

    let file = held_file()?;
    let anchor = Anchor::line(&file, LinePoint::at(&file, Side::New, 3)?)?;
    let mut drafts = vec![draft(1, &gl, anchor)];
    let position = gl.gitlab_position(&drafts[0])?;
    assert_eq!(
        triple_origin(&held, &fresh, &position.revision()),
        TripleOrigin::Held
    );

    let fresh_diffs = vec![FileDiff::parse(PATH, PATH, FRESH)?];
    let HeadCheck::Changed(change) = gl.check_head(&fresh, &[file], &fresh_diffs, &drafts) else {
        return Err("смена head не замечена".into());
    };
    gl.apply_head(&change, HeadPath::Move, &mut drafts, &fresh_diffs)?;
    let position = gl.gitlab_position(&drafts[0])?;
    assert_eq!(
        triple_origin(&held, &fresh, &position.revision()),
        TripleOrigin::Fresh
    );
    assert_eq!(
        gl.verify_outgoing(&mixed),
        Err(ModelError::RevisionMismatch {
            fields: vec![RevisionField::Head],
        })
    );
    Ok(())
}

#[test]
fn triple_origin_ignores_unchanged_fields() -> TestResult {
    let held = rev('a', 'b', 'c')?;
    let fresh = rev('a', 'b', 'f')?;
    assert_eq!(triple_origin(&held, &fresh, &held), TripleOrigin::Held);
    assert_eq!(triple_origin(&held, &fresh, &fresh), TripleOrigin::Fresh);
    assert_eq!(
        triple_origin(&held, &fresh, &rev('a', '9', 'c')?),
        TripleOrigin::Foreign
    );
    Ok(())
}

#[test]
fn draft_from_other_revision_is_refused() -> TestResult {
    let file = held_file()?;
    let old = session(Provider::GitHub, rev('a', 'b', 'c')?);
    let new = session(Provider::GitHub, rev('a', 'b', 'f')?);
    let anchor = Anchor::line(&file, LinePoint::at(&file, Side::New, 3)?)?;
    assert_eq!(
        new.github_anchor(&draft(1, &old, anchor)),
        Err(ModelError::RevisionMismatch {
            fields: vec![RevisionField::Head],
        })
    );
    Ok(())
}

struct HeadFixture {
    session: ReviewSession,
    fresh: Revision,
    held_diff: Vec<FileDiff>,
    fresh_diff: Vec<FileDiff>,
    drafts: Vec<DraftComment>,
}

fn head_fixture() -> Result<HeadFixture, ModelError> {
    let session = session(Provider::GitHub, rev('a', 'b', 'c')?);
    let file = held_file()?;
    let readme = FileDiff::parse("README.md", "README.md", "@@ -1 +1 @@\n-old\n+new\n")?;
    let drafts = vec![
        draft(
            1,
            &session,
            Anchor::range(
                &file,
                LinePoint::at(&file, Side::Old, 2)?,
                LinePoint::at(&file, Side::New, 3)?,
            )?,
        ),
        draft(
            2,
            &session,
            Anchor::line(&file, LinePoint::at(&file, Side::New, 5)?)?,
        ),
        draft(
            3,
            &session,
            Anchor::line(&file, LinePoint::at(&file, Side::New, 21)?)?,
        ),
        DraftComment::new(
            DraftId(4),
            &session,
            Placement::Reply {
                thread: ProviderId::new("t1"),
            },
            "ответ",
        ),
        draft(
            5,
            &session,
            Anchor::line(&readme, LinePoint::at(&readme, Side::New, 1)?)?,
        ),
    ];
    Ok(HeadFixture {
        session,
        fresh: rev('a', 'b', 'f')?,
        held_diff: vec![file, readme],
        fresh_diff: vec![FileDiff::parse(PATH, PATH, FRESH)?],
        drafts,
    })
}

#[test]
fn head_change_classifies_anchors_without_renumbering() -> TestResult {
    let fx = head_fixture()?;
    assert_eq!(
        fx.session.check_head(
            fx.session.revision(),
            &fx.held_diff,
            &fx.held_diff,
            &fx.drafts
        ),
        HeadCheck::Same
    );
    let HeadCheck::Changed(change) =
        fx.session
            .check_head(&fx.fresh, &fx.held_diff, &fx.fresh_diff, &fx.drafts)
    else {
        return Err("смена head не замечена".into());
    };
    assert_eq!(change.from(), fx.session.revision());
    assert_eq!(change.to(), &fx.fresh);
    assert_eq!(
        change.fates(),
        &[
            (DraftId(1), AnchorFate::Kept),
            // `e` сдвинулась на строку ниже: номер не пересчитывается.
            (DraftId(2), AnchorFate::Stale(StaleReason::LinesChanged)),
            (DraftId(3), AnchorFate::Stale(StaleReason::LinesChanged)),
            (DraftId(5), AnchorFate::Stale(StaleReason::FileMissing)),
        ]
    );
    assert_eq!(
        change.stale().collect::<Vec<_>>(),
        vec![DraftId(2), DraftId(3), DraftId(5)]
    );
    Ok(())
}

#[test]
fn same_counters_with_other_text_and_renamed_file_are_stale() -> TestResult {
    let gh = session(Provider::GitHub, rev('a', 'b', 'c')?);
    let held = vec![FileDiff::parse(
        "old.rs",
        "new.rs",
        "@@ -1 +1 @@\n-x\n+y\n",
    )?];
    let edited = vec![FileDiff::parse(
        "old.rs",
        "new.rs",
        "@@ -1 +1 @@\n-x\n+z\n",
    )?];
    let renamed = vec![FileDiff::parse(
        "other.rs",
        "new.rs",
        "@@ -1 +1 @@\n-x\n+y\n",
    )?];
    let anchor = Anchor::line(&held[0], LinePoint::at(&held[0], Side::New, 1)?)?;
    assert_eq!(
        anchor.fate(&held, &edited),
        AnchorFate::Stale(StaleReason::LinesChanged)
    );
    assert_eq!(
        anchor.fate(&held, &renamed),
        AnchorFate::Stale(StaleReason::PathChanged)
    );
    for own in [&renamed[..], &[]] {
        assert_eq!(
            anchor.fate(own, &held),
            AnchorFate::Stale(StaleReason::NotInOwnDiff)
        );
    }
    assert_eq!(anchor.fate(&held, &held), AnchorFate::Kept);
    let drafts = vec![draft(1, &gh, anchor)];
    assert!(matches!(
        gh.check_head(&rev('a', 'b', 'f')?, &held, &edited, &drafts),
        HeadCheck::Changed(_)
    ));
    Ok(())
}

#[test]
fn stay_keeps_session_and_drafts() -> TestResult {
    let mut fx = head_fixture()?;
    let HeadCheck::Changed(change) =
        fx.session
            .check_head(&fx.fresh, &fx.held_diff, &fx.fresh_diff, &fx.drafts)
    else {
        return Err("смена head не замечена".into());
    };
    let before = (fx.session.clone(), fx.drafts.clone());
    fx.session
        .apply_head(&change, HeadPath::Stay, &mut fx.drafts, &fx.fresh_diff)?;
    assert_eq!((fx.session, fx.drafts), before);
    Ok(())
}

#[test]
fn move_carries_kept_anchors_and_marks_stale_ones() -> TestResult {
    let mut fx = head_fixture()?;
    let HeadCheck::Changed(change) =
        fx.session
            .check_head(&fx.fresh, &fx.held_diff, &fx.fresh_diff, &fx.drafts)
    else {
        return Err("смена head не замечена".into());
    };
    fx.session
        .apply_head(&change, HeadPath::Move, &mut fx.drafts, &fx.fresh_diff)?;
    assert_eq!(fx.session.revision(), &fx.fresh);
    let kept = fx.session.github_anchor(&fx.drafts[0])?;
    assert_eq!(kept.commit_id(), fx.fresh.head_sha());
    assert_eq!(fx.drafts[3].revision(), &fx.fresh);
    for index in [1, 2, 4] {
        assert!(!fx.drafts[index].is_current());
        assert_eq!(fx.drafts[index].revision(), change.from());
        assert!(matches!(
            fx.session.github_anchor(&fx.drafts[index]),
            Err(ModelError::Stale { .. })
        ));
    }
    assert_eq!(
        fx.drafts[1].state(),
        AnchorState::Stale(StaleReason::LinesChanged)
    );
    assert!(matches!(
        fx.session
            .apply_head(&change, HeadPath::Move, &mut fx.drafts, &fx.fresh_diff),
        Err(ModelError::RevisionMismatch { .. })
    ));
    Ok(())
}

#[test]
fn rebind_takes_explicit_anchor_of_fresh_diff_only() -> TestResult {
    let mut fx = head_fixture()?;
    let HeadCheck::Changed(change) =
        fx.session
            .check_head(&fx.fresh, &fx.held_diff, &fx.fresh_diff, &fx.drafts)
    else {
        return Err("смена head не замечена".into());
    };
    let fresh_file = &fx.fresh_diff[0];
    let e_now = Anchor::line(fresh_file, LinePoint::at(fresh_file, Side::New, 6)?)?;

    let not_stale = HeadPath::Rebind(vec![(DraftId(1), e_now.clone())]);
    assert_eq!(
        fx.session
            .apply_head(&change, not_stale, &mut fx.drafts, &fx.fresh_diff),
        Err(ModelError::RebindNotStale { draft: DraftId(1) })
    );
    let held_only = fx.drafts[2].placement().clone();
    let Placement::Line(held_anchor) = held_only else {
        return Err("у черновика 3 нет якоря".into());
    };
    assert!(matches!(
        fx.session.apply_head(
            &change,
            HeadPath::Rebind(vec![(DraftId(3), held_anchor)]),
            &mut fx.drafts,
            &fx.fresh_diff
        ),
        Err(ModelError::AnchorMissing { .. })
    ));
    assert_eq!(fx.session.revision(), change.from());

    fx.session.apply_head(
        &change,
        HeadPath::Rebind(vec![(DraftId(2), e_now)]),
        &mut fx.drafts,
        &fx.fresh_diff,
    )?;
    let rebound = serde_json::to_value(fx.session.github_anchor(&fx.drafts[1])?)?;
    assert_eq!(rebound["line"], 6);
    assert_eq!(rebound["commit_id"], fx.fresh.head_sha().as_str());
    assert!(!fx.drafts[2].is_current());
    assert!(fx.drafts[0].is_current());
    Ok(())
}

#[test]
fn stale_draft_is_rebound_later_to_the_current_revision_only_explicitly() -> TestResult {
    let mut fx = head_fixture()?;
    let HeadCheck::Changed(change) =
        fx.session
            .check_head(&fx.fresh, &fx.held_diff, &fx.fresh_diff, &fx.drafts)
    else {
        return Err("смена head не замечена".into());
    };
    fx.session
        .apply_head(&change, HeadPath::Move, &mut fx.drafts, &fx.fresh_diff)?;
    let fresh_file = &fx.fresh_diff[0];
    let e_now = Anchor::line(fresh_file, LinePoint::at(fresh_file, Side::New, 6)?)?;
    assert_eq!(
        fx.session
            .rebind_stale(&mut fx.drafts[0], e_now.clone(), &fx.fresh_diff),
        Err(ModelError::RebindNotStale { draft: DraftId(1) })
    );
    let Placement::Line(held_anchor) = fx.drafts[2].placement().clone() else {
        return Err("у черновика 3 нет якоря".into());
    };
    let before = fx.drafts[2].clone();
    assert!(matches!(
        fx.session
            .rebind_stale(&mut fx.drafts[2], held_anchor, &fx.fresh_diff),
        Err(ModelError::AnchorMissing { .. })
    ));
    assert_eq!(fx.drafts[2], before, "отказ без изменений");
    fx.session
        .rebind_stale(&mut fx.drafts[1], e_now, &fx.fresh_diff)?;
    assert!(fx.drafts[1].is_current());
    assert_eq!(fx.drafts[1].revision(), &fx.fresh);
    let rebound = serde_json::to_value(fx.session.github_anchor(&fx.drafts[1])?)?;
    assert_eq!(rebound["line"], 6);
    assert_eq!(rebound["commit_id"], fx.fresh.head_sha().as_str());
    Ok(())
}

#[test]
fn missing_held_diff_makes_anchors_stale_never_kept() -> TestResult {
    let fx = head_fixture()?;
    let HeadCheck::Changed(change) =
        fx.session
            .check_head(&fx.fresh, &[], &fx.fresh_diff, &fx.drafts)
    else {
        return Err("смена head не замечена".into());
    };
    assert!(!change.fates().is_empty());
    for (id, fate) in change.fates() {
        assert_eq!(
            fate,
            &AnchorFate::Stale(StaleReason::NotInOwnDiff),
            "черновик {}",
            id.0
        );
    }
    Ok(())
}
