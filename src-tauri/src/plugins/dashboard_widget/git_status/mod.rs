//! Git Status dashboard-widget backend descriptor and command implementation.

use crate::commands::session::AppContext;
use crate::plugins::dashboard_widget::DashboardWidgetBackend;
use crate::plugins::Plugin;
use crate::types::AppError;

pub mod pr_info;

pub struct GitStatusBackend;

impl Plugin for GitStatusBackend {
    fn id(&self) -> &'static str {
        "git-status"
    }

    fn display_name(&self) -> &'static str {
        "Git Status"
    }
}

impl DashboardWidgetBackend for GitStatusBackend {
    fn required_commands(&self) -> &'static [&'static str] {
        &["worktree_git_status", "worktree_pr_info"]
    }
}

/// Snapshot `git status` for a single worktree (Issue #55: worktree dashboard).
///
/// Always returns `Ok(...)` even when discovery fails — on missing dir / non-repo / `git` binary missing / non-zero `git` exit, the result is a
/// default-valued `WorktreeGitStatus` with `error: Some(message)` populated so the dashboard can distinguish a clean tree (`error == None`, all
/// counts zero) from an unreadable one and surface an inline "unable to read git status" hint rather than blocking the user.
///
/// The path is canonicalized via `compose::validate_worktree` before being handed to the runner so behaviour matches the rest of the command
/// surface (e.g. `session_create`) and a relative or non-existent path can't have `git` invoked against an unintended directory. A validation
/// failure is converted to the same `Ok(error_struct)` shape rather than an `AppError` to preserve the "always resolves" contract the frontend
/// depends on.
pub fn worktree_git_status_impl(ctx: &AppContext, worktree_path: &std::path::Path) -> Result<crate::types::WorktreeGitStatus, AppError> {
    match crate::compose::validate_worktree(worktree_path) {
        Ok(canonical) => Ok(ctx.git_runner.git_status(&canonical).unwrap_or_else(|e| crate::types::WorktreeGitStatus {
            error: Some(format!("git status failed: {e}")),
            ..Default::default()
        })),
        Err(e) => Ok(crate::types::WorktreeGitStatus {
            error: Some(format!("invalid worktree path: {e}")),
            ..Default::default()
        }),
    }
}

/// Look up the pull/merge request associated with a worktree's current branch (provider detected from the `origin` remote; data sourced from the
/// matching provider CLI). See [`pr_info`] for the provider matrix and the always-`Ok` contract.
///
/// Mirrors [`worktree_git_status_impl`]: the path is canonicalised via `compose::validate_worktree` and any validation failure is folded into the
/// `Ok(error_struct)` shape so the frontend never sees a thrown `AppError` for an unreadable worktree.
pub fn worktree_pr_info_impl(_ctx: &AppContext, worktree_path: &std::path::Path) -> Result<crate::types::WorktreePrInfo, AppError> {
    match crate::compose::validate_worktree(worktree_path) {
        Ok(canonical) => Ok(pr_info::compute_pr_info(&pr_info::RealPrInfoRunner, &canonical)),
        Err(e) => Ok(crate::types::WorktreePrInfo {
            error: Some(format!("invalid worktree path: {e}")),
            ..Default::default()
        }),
    }
}
