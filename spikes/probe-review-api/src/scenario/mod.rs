//! Каталог сценариев пробы. Сценарий — строка матрицы возможностей или самопроверка стенда;
//! процесс сценария идёт в одном столбце и пишет один `result.json`.

pub mod fake;
pub mod live;

use std::fs;
use std::path::PathBuf;

use crate::results::{Column, Execution, ResultFile, Status};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scenario {
    /// Пустой прогон стенда: временный каталог состояния, `result.json`, агрегирование.
    Smoke,
    /// Строка матрицы `gh-local-create`.
    GhLocalCreate,
    /// Строка матрицы `gh-pending-cycle`.
    GhPendingCycle,
    /// Строка матрицы `gh-pending-foreign`.
    GhPendingForeign,
    /// Строка матрицы `gh-anchors`.
    GhAnchors,
    /// Строка матрицы `gh-viewed`.
    GhViewed,
    /// Строка матрицы `gl-draft-publish`.
    GlDraftPublish,
    /// Строка матрицы `gl-foreign-draft`.
    GlForeignDraft,
    /// Строка матрицы `gl-approve-sha`.
    GlApproveSha,
    /// Строка матрицы `gl-request-changes`.
    GlRequestChanges,
    /// Строка матрицы `gl-anchors`.
    GlAnchors,
    /// Строка матрицы `head-change`.
    HeadChange,
    /// Строка матрицы `journal-crash`.
    JournalCrash,
    /// Строка матрицы `reconcile-ambiguous`.
    ReconcileAmbiguous,
    /// Строка матрицы `reconcile-foreign`.
    ReconcileForeign,
    /// Строка матрицы `reconcile-deleted`.
    ReconcileDeleted,
    /// Строка матрицы `reconcile-paged`.
    ReconcilePaged,
    /// Строка матрицы `partial`.
    Partial,
    /// Строка матрицы `capabilities`.
    Capabilities,
}

impl Scenario {
    pub const ALL: [Scenario; 19] = [
        Scenario::Smoke,
        Scenario::GhLocalCreate,
        Scenario::GhPendingCycle,
        Scenario::GhPendingForeign,
        Scenario::GhAnchors,
        Scenario::GhViewed,
        Scenario::GlDraftPublish,
        Scenario::GlForeignDraft,
        Scenario::GlApproveSha,
        Scenario::GlRequestChanges,
        Scenario::GlAnchors,
        Scenario::HeadChange,
        Scenario::JournalCrash,
        Scenario::ReconcileAmbiguous,
        Scenario::ReconcileForeign,
        Scenario::ReconcileDeleted,
        Scenario::ReconcilePaged,
        Scenario::Partial,
        Scenario::Capabilities,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Scenario::Smoke => "smoke",
            Scenario::GhLocalCreate => "gh-local-create",
            Scenario::GhPendingCycle => "gh-pending-cycle",
            Scenario::GhPendingForeign => "gh-pending-foreign",
            Scenario::GhAnchors => "gh-anchors",
            Scenario::GhViewed => "gh-viewed",
            Scenario::GlDraftPublish => "gl-draft-publish",
            Scenario::GlForeignDraft => "gl-foreign-draft",
            Scenario::GlApproveSha => "gl-approve-sha",
            Scenario::GlRequestChanges => "gl-request-changes",
            Scenario::GlAnchors => "gl-anchors",
            Scenario::HeadChange => "head-change",
            Scenario::JournalCrash => "journal-crash",
            Scenario::ReconcileAmbiguous => "reconcile-ambiguous",
            Scenario::ReconcileForeign => "reconcile-foreign",
            Scenario::ReconcileDeleted => "reconcile-deleted",
            Scenario::ReconcilePaged => "reconcile-paged",
            Scenario::Partial => "partial",
            Scenario::Capabilities => "capabilities",
        }
    }

    pub fn from_name(name: &str) -> Option<Scenario> {
        Scenario::ALL.into_iter().find(|s| s.name() == name)
    }

    /// Столбцы, в которых сценарий прогоняется.
    pub fn columns(self) -> &'static [Column] {
        match self {
            Scenario::Smoke => &[Column::Fake],
            // Строки GitHub; строки провайдера другого хоста в столбце — «—» матрицы.
            Scenario::GhLocalCreate
            | Scenario::GhPendingCycle
            | Scenario::GhPendingForeign
            | Scenario::GhAnchors
            | Scenario::GhViewed
            | Scenario::ReconcileForeign => &[Column::Fake, Column::GithubCloud, Column::Ghes],
            // Строки GitLab; столбец `gitlab_ce` добавляется вместе с их живыми сценариями.
            Scenario::GlDraftPublish | Scenario::GlForeignDraft => {
                &[Column::Fake, Column::GitlabCloud, Column::GitlabCe]
            }
            Scenario::GlApproveSha
            | Scenario::GlRequestChanges
            | Scenario::GlAnchors
            | Scenario::HeadChange
            | Scenario::JournalCrash
            | Scenario::ReconcileAmbiguous
            | Scenario::ReconcileDeleted
            | Scenario::ReconcilePaged
            | Scenario::Partial => &[Column::Fake, Column::GitlabCloud],
            Scenario::Capabilities => &[
                Column::Fake,
                Column::GithubCloud,
                Column::GitlabCloud,
                Column::GitlabCe,
                Column::Ghes,
            ],
        }
    }

    /// Самопроверка стенда в матрицу возможностей не входит.
    pub fn is_stand(self) -> bool {
        match self {
            Scenario::Smoke => true,
            Scenario::GhLocalCreate
            | Scenario::GhPendingCycle
            | Scenario::GhPendingForeign
            | Scenario::GhAnchors
            | Scenario::GhViewed
            | Scenario::GlDraftPublish
            | Scenario::GlForeignDraft
            | Scenario::GlApproveSha
            | Scenario::GlRequestChanges
            | Scenario::GlAnchors
            | Scenario::HeadChange
            | Scenario::JournalCrash
            | Scenario::ReconcileAmbiguous
            | Scenario::ReconcileForeign
            | Scenario::ReconcileDeleted
            | Scenario::ReconcilePaged
            | Scenario::Partial
            | Scenario::Capabilities => false,
        }
    }

    pub fn run(self, ctx: &Context, result: &mut ResultFile) {
        match self {
            Scenario::Smoke => smoke(ctx, result),
            Scenario::GhLocalCreate
            | Scenario::GhPendingCycle
            | Scenario::GhPendingForeign
            | Scenario::GhAnchors
            | Scenario::GhViewed
            | Scenario::GlDraftPublish
            | Scenario::GlForeignDraft
            | Scenario::GlApproveSha
            | Scenario::GlRequestChanges
            | Scenario::GlAnchors
            | Scenario::HeadChange
            | Scenario::JournalCrash
            | Scenario::ReconcileAmbiguous
            | Scenario::ReconcileForeign
            | Scenario::ReconcileDeleted
            | Scenario::ReconcilePaged
            | Scenario::Partial
            | Scenario::Capabilities => match ctx.column {
                Column::Fake => fake::run(self, &ctx.state_dir, result),
                Column::GithubCloud | Column::GitlabCloud | Column::GitlabCe | Column::Ghes => {
                    live::run(self, ctx, result)
                }
            },
        }
    }
}

pub struct Context {
    pub column: Column,
    /// Каталог процесса, он же `--out`.
    pub out: PathBuf,
    /// Временный `SHELLIO_STATE_DIR` процесса.
    pub state_dir: PathBuf,
}

fn smoke(ctx: &Context, result: &mut ResultFile) {
    if !ctx.state_dir.is_dir() {
        result.execution = Execution::error(format!(
            "предусловие: SHELLIO_STATE_DIR {} не каталог",
            ctx.state_dir.display()
        ));
        return;
    }
    let probe = ctx.state_dir.join("smoke.probe");
    let written = fs::write(&probe, b"smoke").and_then(|()| fs::remove_file(&probe));
    if let Err(err) = written {
        result.execution =
            Execution::error(format!("запись во временный каталог состояния: {err}"));
        return;
    }
    if !ctx.out.is_dir() {
        result.execution = Execution::error(format!(
            "каталог процесса {} не существует",
            ctx.out.display()
        ));
        return;
    }
    result.status = Status::Pass;
    result.reason = format!(
        "стенд в столбце {}: временный каталог состояния доступен для записи, result.json пишется в каталог процесса",
        ctx.column.name()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for scenario in Scenario::ALL {
            assert_eq!(Scenario::from_name(scenario.name()), Some(scenario));
            assert!(!scenario.columns().is_empty());
        }
        assert_eq!(Scenario::from_name("nope"), None);
    }

    #[test]
    fn every_matrix_row_has_one_fake_scenario() {
        use crate::results::MATRIX_ROWS;
        let rows: Vec<&str> = Scenario::ALL
            .iter()
            .filter(|s| !s.is_stand())
            .map(|s| s.name())
            .collect();
        assert_eq!(
            rows,
            MATRIX_ROWS.to_vec(),
            "строки матрицы и сценарии в одном порядке"
        );
        for scenario in Scenario::ALL.into_iter().filter(|s| !s.is_stand()) {
            assert!(
                scenario.columns().contains(&Column::Fake),
                "{} без столбца fake",
                scenario.name()
            );
        }
        let stands: Vec<&str> = Scenario::ALL
            .iter()
            .filter(|s| s.is_stand())
            .map(|s| s.name())
            .collect();
        assert_eq!(stands, ["smoke"]);
    }

    #[test]
    fn matrix_row_outside_fake_is_execution_error() -> Result<(), String> {
        let state = tempfile::tempdir().map_err(|e| e.to_string())?;
        let ctx = Context {
            column: Column::GithubCloud,
            out: state.path().to_path_buf(),
            state_dir: state.path().to_path_buf(),
        };
        let mut result = ResultFile::new("s", "b", "partial", Column::GithubCloud);
        Scenario::Partial.run(&ctx, &mut result);
        assert!(!result.execution.is_ok());
        Ok(())
    }

    #[test]
    fn smoke_passes_in_writable_dirs() -> Result<(), String> {
        let state = tempfile::tempdir().map_err(|e| e.to_string())?;
        let out = tempfile::tempdir().map_err(|e| e.to_string())?;
        let ctx = Context {
            column: Column::Fake,
            out: out.path().to_path_buf(),
            state_dir: state.path().to_path_buf(),
        };
        let mut result = ResultFile::new("s", "b", "smoke", Column::Fake);
        Scenario::Smoke.run(&ctx, &mut result);
        assert_eq!(result.execution, Execution::Ok);
        assert_eq!(result.status, Status::Pass);
        assert!(result.problems().is_empty(), "{:?}", result.problems());
        Ok(())
    }

    #[test]
    fn smoke_without_state_dir_is_execution_error() {
        let ctx = Context {
            column: Column::Fake,
            out: PathBuf::from("/nonexistent-out"),
            state_dir: PathBuf::from("/nonexistent-state"),
        };
        let mut result = ResultFile::new("s", "b", "smoke", Column::Fake);
        Scenario::Smoke.run(&ctx, &mut result);
        assert!(!result.execution.is_ok());
    }
}
