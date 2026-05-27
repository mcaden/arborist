//! Git-related wire types shared between the host backend, the MCP sidecar, and the
//! frontend. These structures cross every Tauri / MCP boundary, so they live in
//! `arborist-types` rather than in the host's `src-tauri/src/git.rs` to guarantee a single
//! canonical definition. The host's `git.rs` implements the porcelain parsing that produces
//! these values; the MCP tools serialize them straight to clients.

use serde::{Deserialize, Serialize};

/// Result of `git remote show origin` / `symbolic-ref refs/remotes/origin/HEAD` lookups. The
/// `source` field tells callers how the branch was discovered so that the UI / MCP responses
/// can disclose whether they're guessing (`Main` / `Master` fallbacks) or have a definitive
/// answer from the remote.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DefaultBranchInfo {
    pub branch: String,
    pub source: DefaultBranchSource,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum DefaultBranchSource {
    OriginHead,
    Main,
    Master,
}

/// Outcome of a `git merge` invocation issued by the MCP `merge_main_into_worktrees` tool.
/// `AutoAborted` / `LeftConflicted` distinguish "we cleaned up after a conflict" from "the
/// working tree is still in MERGING state" so the host can surface the right remediation hint.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum MergeFromBranchOutcome {
    MergedCleanly { ahead: u32, behind: u32 },
    AlreadyUpToDate,
    AutoAborted { files: Vec<String> },
    LeftConflicted { files: Vec<String> },
    TimedOut { recovered: bool },
}

/// Outcome of a `git merge-tree --write-tree` dry-run. `Unsupported` is returned when the
/// installed git is too old to support the form we use; callers may fall back to the live
/// merge path.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum MergeTreeOutcome {
    Clean,
    Conflict { files: Vec<String> },
    AlreadyUpToDate,
    Unsupported,
}

/// Summary of `git status` for the MCP `workspace_status` / `list_worktrees` tools.
///
/// `error` captures parsing / IO failures without sinking the entire response: the worktree
/// still appears in the listing, but with `error: Some(...)` so the client can show a degraded
/// row instead of dropping it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeGitStatusSummary {
    pub dirty: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub ahead_of_upstream: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub behind_upstream: Option<u32>,
    pub file_count: u32,
    pub has_upstream: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn assert_roundtrip<T>(value: &T, fixture: Value)
    where
        T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
    {
        let serialized: Value = serde_json::to_value(value).expect("serialize");
        assert_eq!(serialized, fixture, "serialized form drifted from fixture");

        let deserialized: T = serde_json::from_value(fixture).expect("deserialize");
        assert_eq!(&deserialized, value, "deserialized value drifted");
    }

    #[test]
    fn default_branch_info_roundtrip() {
        assert_roundtrip(
            &DefaultBranchInfo {
                branch: "main".to_owned(),
                source: DefaultBranchSource::OriginHead,
            },
            json!({ "branch": "main", "source": "originHead" }),
        );
    }

    #[test]
    fn merge_from_branch_outcome_roundtrip_variants() {
        assert_roundtrip(
            &MergeFromBranchOutcome::MergedCleanly { ahead: 2, behind: 0 },
            json!({ "mergedCleanly": { "ahead": 2, "behind": 0 } }),
        );
        assert_roundtrip(
            &MergeFromBranchOutcome::AutoAborted {
                files: vec!["a.txt".to_owned()],
            },
            json!({ "autoAborted": { "files": ["a.txt"] } }),
        );
        assert_roundtrip(&MergeFromBranchOutcome::AlreadyUpToDate, json!("alreadyUpToDate"));
        assert_roundtrip(
            &MergeFromBranchOutcome::TimedOut { recovered: true },
            json!({ "timedOut": { "recovered": true } }),
        );
    }

    #[test]
    fn merge_tree_outcome_roundtrip_variants() {
        assert_roundtrip(&MergeTreeOutcome::Clean, json!("clean"));
        assert_roundtrip(
            &MergeTreeOutcome::Conflict {
                files: vec!["src/a.rs".to_owned()],
            },
            json!({ "conflict": { "files": ["src/a.rs"] } }),
        );
        assert_roundtrip(&MergeTreeOutcome::Unsupported, json!("unsupported"));
    }

    #[test]
    fn worktree_git_status_summary_roundtrip() {
        assert_roundtrip(
            &WorktreeGitStatusSummary {
                dirty: true,
                ahead_of_upstream: Some(3),
                behind_upstream: Some(1),
                file_count: 7,
                has_upstream: true,
                error: None,
            },
            json!({
                "dirty": true,
                "aheadOfUpstream": 3,
                "behindUpstream": 1,
                "fileCount": 7,
                "hasUpstream": true
            }),
        );
    }

    #[test]
    fn worktree_git_status_summary_with_error_roundtrip() {
        assert_roundtrip(
            &WorktreeGitStatusSummary {
                dirty: false,
                ahead_of_upstream: None,
                behind_upstream: None,
                file_count: 0,
                has_upstream: false,
                error: Some("worktree vanished".to_owned()),
            },
            json!({
                "dirty": false,
                "fileCount": 0,
                "hasUpstream": false,
                "error": "worktree vanished"
            }),
        );
    }
}
