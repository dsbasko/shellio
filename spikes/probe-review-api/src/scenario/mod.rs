//! Каталог сценариев пробы. Сценарий — строка матрицы возможностей, самопроверка стенда или
//! сценарий стоимости и устойчивости; процесс сценария идёт в одном столбце и пишет один
//! `result.json`.

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
    /// Стоимость: 200 черновиков в одном ревью по ветвям обоих провайдеров.
    Volume,
    /// Устойчивость: исчерпание лимита посреди отправки.
    RateLimit,
}

/// Место сценария в отчёте.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    /// Самопроверка стенда.
    Stand,
    /// Строка матрицы возможностей.
    Row,
    /// Стоимость и устойчивость: только столбец `fake`, в матрицу не входит.
    Cost,
}

impl Scenario {
    pub const ALL: [Scenario; 21] = [
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
        Scenario::Volume,
        Scenario::RateLimit,
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
            Scenario::Volume => "volume",
            Scenario::RateLimit => "rate-limit",
        }
    }

    pub fn from_name(name: &str) -> Option<Scenario> {
        Scenario::ALL.into_iter().find(|s| s.name() == name)
    }

    /// Столбцы, в которых сценарий прогоняется.
    pub fn columns(self) -> &'static [Column] {
        match self {
            Scenario::Smoke | Scenario::Volume | Scenario::RateLimit => &[Column::Fake],
            // Строки GitHub; строки провайдера другого хоста в столбце — «—» матрицы.
            Scenario::GhLocalCreate
            | Scenario::GhPendingCycle
            | Scenario::GhPendingForeign
            | Scenario::GhAnchors
            | Scenario::GhViewed
            | Scenario::ReconcileForeign => &[Column::Fake, Column::GithubCloud, Column::Ghes],
            // Строки GitLab.
            Scenario::GlDraftPublish
            | Scenario::GlForeignDraft
            | Scenario::GlRequestChanges
            | Scenario::GlAnchors
            | Scenario::GlApproveSha
            | Scenario::HeadChange
            | Scenario::ReconcileDeleted
            | Scenario::ReconcileAmbiguous
            | Scenario::JournalCrash
            | Scenario::ReconcilePaged
            | Scenario::Partial => &[Column::Fake, Column::GitlabCloud, Column::GitlabCe],
            Scenario::Capabilities => &[
                Column::Fake,
                Column::GithubCloud,
                Column::GitlabCloud,
                Column::GitlabCe,
                Column::Ghes,
            ],
        }
    }

    pub fn group(self) -> Group {
        match self {
            Scenario::Smoke => Group::Stand,
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
            | Scenario::Capabilities => Group::Row,
            Scenario::Volume | Scenario::RateLimit => Group::Cost,
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
            | Scenario::Capabilities
            | Scenario::Volume
            | Scenario::RateLimit => match ctx.column {
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
            .filter(|s| s.group() == Group::Row)
            .map(|s| s.name())
            .collect();
        assert_eq!(
            rows,
            MATRIX_ROWS.to_vec(),
            "строки матрицы и сценарии в одном порядке"
        );
        for scenario in Scenario::ALL
            .into_iter()
            .filter(|s| s.group() == Group::Row)
        {
            assert!(
                scenario.columns().contains(&Column::Fake),
                "{} без столбца fake",
                scenario.name()
            );
        }
        let stands: Vec<&str> = Scenario::ALL
            .iter()
            .filter(|s| s.group() == Group::Stand)
            .map(|s| s.name())
            .collect();
        assert_eq!(stands, ["smoke"]);
    }

    /// Стоимость и устойчивость идут только в столбце `fake` и в матрицу не входят.
    #[test]
    fn cost_scenarios_run_only_in_fake_outside_matrix() {
        use crate::results::MATRIX_ROWS;
        let cost: Vec<&str> = Scenario::ALL
            .iter()
            .filter(|s| s.group() == Group::Cost)
            .map(|s| s.name())
            .collect();
        assert_eq!(cost, ["volume", "rate-limit"]);
        for scenario in Scenario::ALL
            .into_iter()
            .filter(|s| s.group() == Group::Cost)
        {
            assert_eq!(scenario.columns(), &[Column::Fake], "{}", scenario.name());
            assert!(!MATRIX_ROWS.contains(&scenario.name()));
        }
    }

    #[test]
    fn cost_scenario_outside_fake_is_execution_error() -> Result<(), String> {
        let state = tempfile::tempdir().map_err(|e| e.to_string())?;
        for column in [
            Column::GithubCloud,
            Column::GitlabCloud,
            Column::GitlabCe,
            Column::Ghes,
        ] {
            for scenario in [Scenario::Volume, Scenario::RateLimit] {
                let ctx = Context {
                    column,
                    out: state.path().to_path_buf(),
                    state_dir: state.path().to_path_buf(),
                };
                let mut result = ResultFile::new("s", "b", scenario.name(), column);
                scenario.run(&ctx, &mut result);
                assert!(
                    !result.execution.is_ok(),
                    "{} {}",
                    scenario.name(),
                    column.name()
                );
                assert!(result.trace.is_empty());
            }
        }
        Ok(())
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
