//! Pull-request lookup for the Git Status dashboard widget.
//!
//! Given a worktree, this module detects the git provider from the `origin` remote, finds the matching provider CLI on `PATH` (`gh` / `glab` /
//! `az`), shells out to it in the worktree's directory, and normalises the result into a [`WorktreePrInfo`]. Auth is entirely delegated to the
//! CLI — Arborist never reads or stores credentials.
//!
//! The external interactions (git probes, CLI detection, CLI invocation) sit behind the [`PrInfoRunner`] trait so the orchestration and JSON
//! mapping can be unit-tested with canned output and **without** `gh` / `glab` / `az` installed. Production wiring uses [`RealPrInfoRunner`].
//!
//! Contract: [`compute_pr_info`] never panics and always returns a populated struct. Hard failures (CLI invocation error) set
//! [`WorktreePrInfo::error`]; expected empty results (unknown provider, missing CLI, no PR for the branch) are conveyed via `provider` /
//! `cliAvailable` / `note` with `error == None`.

use std::path::Path;
use std::process::Command;

use serde_json::Value;

#[cfg(windows)]
use crate::cmd_resolver::LaunchMethod;
use crate::cmd_resolver::{resolve_executable, resolve_launchable, ResolvedCommand};
use crate::git_remote::parse_remote_url;
use crate::types::{GitProvider, PrChecksStatus, PrState, PullRequestInfo, WorktreePrInfo};

/// Captured result of a CLI invocation.
#[derive(Debug, Clone)]
pub struct CliOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Seam over every external interaction the PR lookup performs, so [`compute_pr_info`] is unit-testable without git or any provider CLI installed.
pub trait PrInfoRunner: Send + Sync {
    /// `git -C <worktree> remote get-url origin`, trimmed. `None` when there is no `origin` remote (or git is unavailable).
    fn remote_url(&self, worktree: &Path) -> Option<String>;
    /// Short current branch name (e.g. `feature/x`). `None` on detached HEAD or failure.
    fn current_branch(&self, worktree: &Path) -> Option<String>;
    /// Whether `program` resolves to an executable on `PATH` (Windows `PATHEXT` aware).
    fn cli_available(&self, program: &str, worktree: &Path) -> bool;
    /// Run `program args...` in `worktree`. `Err` is reserved for a failure to *spawn* the process; a non-zero exit is reported via
    /// [`CliOutput::success`].
    fn run(&self, program: &str, args: &[&str], worktree: &Path) -> Result<CliOutput, String>;
}

/// Production [`PrInfoRunner`] that shells out to the system `git` and provider CLIs.
#[derive(Default, Debug, Clone, Copy)]
pub struct RealPrInfoRunner;

impl RealPrInfoRunner {
    fn git_trimmed(worktree: &Path, args: &[&str]) -> Option<String> {
        let output = crate::git::git_command()
            .current_dir(worktree)
            .arg("-C")
            .arg(worktree)
            .args(args)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if value.is_empty() {
            None
        } else {
            Some(value)
        }
    }
}

impl PrInfoRunner for RealPrInfoRunner {
    fn remote_url(&self, worktree: &Path) -> Option<String> {
        Self::git_trimmed(worktree, &["remote", "get-url", "origin"])
    }

    fn current_branch(&self, worktree: &Path) -> Option<String> {
        Self::git_trimmed(worktree, &["symbolic-ref", "--short", "HEAD"])
    }

    fn cli_available(&self, program: &str, worktree: &Path) -> bool {
        resolve_executable(program, worktree).is_some()
    }

    fn run(&self, program: &str, args: &[&str], worktree: &Path) -> Result<CliOutput, String> {
        let resolved = resolve_launchable(program, worktree).ok_or_else(|| format!("{program} not found on PATH"))?;
        let mut command = build_cli_command(&resolved, args);
        command.current_dir(worktree);
        let output = command.output().map_err(|e| format!("failed to run {program}: {e}"))?;
        Ok(CliOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        })
    }
}

/// Build a [`Command`] for a [`ResolvedCommand`], honouring its [`crate::cmd_resolver::LaunchMethod`]: directly-launchable native executables are
/// spawned as-is, while script wrappers and extensionless shims (`LaunchMethod::ViaCmdShell`) go through `cmd.exe /c` — launching such a file
/// directly fails with os error 193. The console window is suppressed to match [`crate::git::git_command`].
fn build_cli_command(resolved: &ResolvedCommand, args: &[&str]) -> Command {
    #[cfg(windows)]
    let command = {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        use std::os::windows::process::CommandExt as _;
        let mut command = match resolved.launch {
            LaunchMethod::Direct => {
                let mut c = Command::new(&resolved.path);
                c.args(args);
                c
            }
            LaunchMethod::ViaCmdShell => {
                let mut c = Command::new("cmd.exe");
                c.arg("/c").arg(&resolved.path).args(args);
                c
            }
        };
        command.creation_flags(CREATE_NO_WINDOW);
        command
    };
    #[cfg(not(windows))]
    let command = {
        let mut c = Command::new(&resolved.path);
        c.args(args);
        c
    };
    command
}

/// Orchestrate the PR lookup for `worktree`. See module docs for the always-`Ok`, never-panic contract.
pub fn compute_pr_info(runner: &dyn PrInfoRunner, worktree: &Path) -> WorktreePrInfo {
    let Some(remote) = runner.remote_url(worktree) else {
        return WorktreePrInfo {
            provider: GitProvider::Unknown,
            note: Some("No `origin` remote configured — cannot determine git provider.".to_string()),
            ..Default::default()
        };
    };
    let remote_info = parse_remote_url(&remote);
    let mut out = WorktreePrInfo {
        provider: remote_info.provider,
        cli_available: false,
        repo_web_url: remote_info.repo_web_url,
        pr: None,
        note: None,
        error: None,
    };

    match remote_info.provider {
        GitProvider::Unknown => {
            out.note = Some("Unrecognised git host — no pull request integration for this remote.".to_string());
        }
        GitProvider::GitHub => resolve_via_cli(runner, worktree, "gh", &gh_args(), parse_gh, &mut out),
        GitProvider::GitLab => resolve_via_cli(runner, worktree, "glab", &glab_args(), parse_glab, &mut out),
        GitProvider::AzureDevOps => resolve_azure(runner, worktree, &mut out),
    }
    out
}

fn gh_args() -> Vec<String> {
    ["pr", "view", "--json", "number,url,title,state,isDraft,statusCheckRollup"]
        .iter()
        .map(|s| (*s).to_string())
        .collect()
}

fn glab_args() -> Vec<String> {
    ["mr", "view", "--output", "json"].iter().map(|s| (*s).to_string()).collect()
}

/// Run a provider CLI and fold its result into `out`. `parse` maps CLI stdout to `Some(pr)` (found) or `None` (no PR for the branch).
fn resolve_via_cli(
    runner: &dyn PrInfoRunner,
    worktree: &Path,
    program: &str,
    args: &[String],
    parse: fn(&str) -> Option<PullRequestInfo>,
    out: &mut WorktreePrInfo,
) {
    if !runner.cli_available(program, worktree) {
        out.note = Some(format!("`{program}` CLI not found on PATH — install it to see pull request status."));
        return;
    }
    out.cli_available = true;
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    match runner.run(program, &arg_refs, worktree) {
        Err(e) => out.error = Some(e),
        Ok(output) => {
            if output.success {
                match parse(&output.stdout) {
                    Some(pr) => out.pr = Some(pr),
                    None => out.note = Some("No pull request found for this branch.".to_string()),
                }
            } else {
                out.note = Some(cli_failure_note(program, &output.stderr));
            }
        }
    }
}

/// Reject branch names containing characters that `cmd.exe` (used to launch the `az.cmd` script wrapper on Windows) treats as command
/// separators / metacharacters, or control characters. Such names cannot be safely interpolated into the wrapper invocation, so callers skip
/// the lookup rather than risk command injection. Legitimate Git branch names never contain these characters.
fn branch_safe_for_cli(branch: &str) -> bool {
    !branch
        .chars()
        .any(|c| c.is_control() || matches!(c, '&' | '|' | '<' | '>' | '^' | '(' | ')' | '%' | '"' | '!'))
}

/// Azure DevOps needs the source branch passed explicitly (the `az repos pr list` query is branch-scoped) and returns an array; the first active
/// PR wins. `az repos pr list` exits 0 with `[]` when nothing matches, so absence is the empty-array case rather than a non-zero exit.
fn resolve_azure(runner: &dyn PrInfoRunner, worktree: &Path, out: &mut WorktreePrInfo) {
    if !runner.cli_available("az", worktree) {
        out.note = Some("`az` CLI not found on PATH — install it (with the azure-devops extension) to see pull request status.".to_string());
        return;
    }
    out.cli_available = true;
    let Some(branch) = runner.current_branch(worktree) else {
        out.note = Some("Detached HEAD — no branch to look up a pull request for.".to_string());
        return;
    };
    // The branch is interpolated into the `az` invocation, and on Windows `az` is the `az.cmd` script wrapper, which Arborist must launch through
    // `cmd.exe /c`. cmd performs metacharacter parsing (`&`, `|`, `(`, …) before the program sees its argv, and Rust leaves space-free arguments
    // unquoted, so a branch like `x&calc` would inject a command. Legitimate Git branch names never contain these characters, so we degrade to a
    // note rather than risk command execution on a maliciously named branch.
    if !branch_safe_for_cli(&branch) {
        out.note = Some("Branch name contains characters unsafe to pass to the `az` CLI; skipping pull request lookup.".to_string());
        return;
    }
    let source_ref = format!("refs/heads/{branch}");
    let args = [
        "repos",
        "pr",
        "list",
        "--source-branch",
        &source_ref,
        "--status",
        "active",
        "--output",
        "json",
    ];
    match runner.run("az", &args, worktree) {
        Err(e) => out.error = Some(e),
        Ok(output) => {
            if output.success {
                match parse_azure(&output.stdout) {
                    Some(pr) => out.pr = Some(pr),
                    None => out.note = Some("No active pull request found for this branch.".to_string()),
                }
            } else {
                out.note = Some(cli_failure_note("az", &output.stderr));
            }
        }
    }
}

/// Turn a non-zero CLI exit into a human-readable note. A blank stderr (e.g. `gh pr view` printing its "no pull requests found" line to stdout on
/// some versions) degrades to a generic "no pull request" message rather than an alarming empty error.
fn cli_failure_note(program: &str, stderr: &str) -> String {
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("auth") || lower.contains("logged in") || lower.contains("login") || lower.contains("unauthorized") {
        return format!("`{program}` is not authenticated — run its login command to see pull request status.");
    }
    if stderr.is_empty() || lower.contains("no pull request") || lower.contains("no open") || lower.contains("not found") {
        return "No pull request found for this branch.".to_string();
    }
    // Keep the note bounded — surface the first line only.
    let first_line = stderr.lines().next().unwrap_or(stderr);
    format!("`{program}` could not read pull request status: {first_line}")
}

// --------------------------------------------------------------------------- provider JSON mapping

/// Map a `gh pr view --json …` object into a [`PullRequestInfo`].
fn parse_gh(stdout: &str) -> Option<PullRequestInfo> {
    let v: Value = serde_json::from_str(stdout).ok()?;
    let number = v.get("number")?.as_u64()?;
    let url = v.get("url")?.as_str()?.to_string();
    let title = string_field(&v, "title");
    let is_draft = v.get("isDraft").and_then(Value::as_bool).unwrap_or(false);
    let state = match v.get("state").and_then(Value::as_str).unwrap_or("").to_ascii_uppercase().as_str() {
        "OPEN" if is_draft => PrState::Draft,
        "OPEN" => PrState::Open,
        "MERGED" => PrState::Merged,
        "CLOSED" => PrState::Closed,
        _ => PrState::Unknown,
    };
    let checks = gh_checks(v.get("statusCheckRollup"));
    Some(PullRequestInfo {
        number,
        url,
        title,
        state,
        checks,
        is_draft,
    })
}

/// Aggregate GitHub's `statusCheckRollup` array (a mix of CheckRun and StatusContext nodes) into a single coarse status. Priority:
/// failing > pending > passing; an empty/absent rollup is [`PrChecksStatus::None`].
fn gh_checks(rollup: Option<&Value>) -> PrChecksStatus {
    let Some(items) = rollup.and_then(Value::as_array) else {
        return PrChecksStatus::None;
    };
    if items.is_empty() {
        return PrChecksStatus::None;
    }
    let mut any_pending = false;
    for item in items {
        // CheckRun: `status` (QUEUED/IN_PROGRESS/COMPLETED) + `conclusion` (SUCCESS/FAILURE/…). StatusContext: `state` (SUCCESS/FAILURE/PENDING/ERROR).
        let status = item.get("status").and_then(Value::as_str).unwrap_or("").to_ascii_uppercase();
        let conclusion = item.get("conclusion").and_then(Value::as_str).unwrap_or("").to_ascii_uppercase();
        let context_state = item.get("state").and_then(Value::as_str).unwrap_or("").to_ascii_uppercase();

        let failing = matches!(
            conclusion.as_str(),
            "FAILURE" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE"
        ) || matches!(context_state.as_str(), "FAILURE" | "ERROR");
        if failing {
            return PrChecksStatus::Failing;
        }
        let pending = matches!(status.as_str(), "QUEUED" | "IN_PROGRESS" | "PENDING" | "WAITING" | "REQUESTED")
            || context_state == "PENDING"
            || (conclusion.is_empty() && status.is_empty() && context_state.is_empty());
        if pending {
            any_pending = true;
        }
    }
    if any_pending {
        PrChecksStatus::Pending
    } else {
        PrChecksStatus::Passing
    }
}

/// Map a `glab mr view --output json` object (a GitLab MR API payload) into a [`PullRequestInfo`].
fn parse_glab(stdout: &str) -> Option<PullRequestInfo> {
    let v: Value = serde_json::from_str(stdout).ok()?;
    let number = v.get("iid")?.as_u64()?;
    let url = v.get("web_url")?.as_str()?.to_string();
    let title = string_field(&v, "title");
    let is_draft = v
        .get("draft")
        .and_then(Value::as_bool)
        .or_else(|| v.get("work_in_progress").and_then(Value::as_bool))
        .unwrap_or(false);
    let state = match v.get("state").and_then(Value::as_str).unwrap_or("") {
        "opened" if is_draft => PrState::Draft,
        "opened" => PrState::Open,
        "merged" => PrState::Merged,
        "closed" | "locked" => PrState::Closed,
        _ => PrState::Unknown,
    };
    let pipeline_status = v
        .get("head_pipeline")
        .or_else(|| v.get("pipeline"))
        .and_then(|p| p.get("status"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let checks = match pipeline_status {
        "success" => PrChecksStatus::Passing,
        "failed" => PrChecksStatus::Failing,
        "running" | "pending" | "created" | "scheduled" | "preparing" | "waiting_for_resource" => PrChecksStatus::Pending,
        "" => PrChecksStatus::None,
        _ => PrChecksStatus::Unknown,
    };
    Some(PullRequestInfo {
        number,
        url,
        title,
        state,
        checks,
        is_draft,
    })
}

/// Map the first element of an `az repos pr list … --output json` array into a [`PullRequestInfo`]. The PR web URL is composed from the
/// repository's `webUrl` and the PR id (the list payload carries no direct web link). ADO check/policy status is not fetched in v1, so
/// [`PrChecksStatus::Unknown`] is reported.
fn parse_azure(stdout: &str) -> Option<PullRequestInfo> {
    let v: Value = serde_json::from_str(stdout).ok()?;
    let pr = v.as_array()?.first()?;
    let number = pr.get("pullRequestId")?.as_u64()?;
    let web_url = pr.get("repository").and_then(|r| r.get("webUrl")).and_then(Value::as_str)?;
    let url = format!("{}/pullrequest/{number}", web_url.trim_end_matches('/'));
    let title = string_field(pr, "title");
    let is_draft = pr.get("isDraft").and_then(Value::as_bool).unwrap_or(false);
    let state = match pr.get("status").and_then(Value::as_str).unwrap_or("") {
        "active" if is_draft => PrState::Draft,
        "active" => PrState::Open,
        "completed" => PrState::Merged,
        "abandoned" => PrState::Closed,
        _ => PrState::Unknown,
    };
    Some(PullRequestInfo {
        number,
        url,
        title,
        state,
        checks: PrChecksStatus::Unknown,
        is_draft,
    })
}

/// Extract a non-empty string field, returning `None` for missing or blank values.
fn string_field(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Fake runner driven by canned values keyed off the program name.
    #[derive(Default)]
    struct FakeRunner {
        remote: Option<String>,
        branch: Option<String>,
        available: Vec<String>,
        outputs: HashMap<String, Result<CliOutput, String>>,
    }

    impl FakeRunner {
        fn ok(stdout: &str) -> Result<CliOutput, String> {
            Ok(CliOutput {
                success: true,
                stdout: stdout.to_string(),
                stderr: String::new(),
            })
        }
        fn fail(stderr: &str) -> Result<CliOutput, String> {
            Ok(CliOutput {
                success: false,
                stdout: String::new(),
                stderr: stderr.to_string(),
            })
        }
    }

    impl PrInfoRunner for FakeRunner {
        fn remote_url(&self, _w: &Path) -> Option<String> {
            self.remote.clone()
        }
        fn current_branch(&self, _w: &Path) -> Option<String> {
            self.branch.clone()
        }
        fn cli_available(&self, program: &str, _w: &Path) -> bool {
            self.available.iter().any(|p| p == program)
        }
        fn run(&self, program: &str, _args: &[&str], _w: &Path) -> Result<CliOutput, String> {
            self.outputs
                .get(program)
                .cloned()
                .unwrap_or_else(|| Err(format!("no canned output for {program}")))
        }
    }

    fn wt() -> &'static Path {
        Path::new("/tmp/wt")
    }

    #[test]
    fn no_remote_yields_unknown_with_note() {
        let r = FakeRunner::default();
        let out = compute_pr_info(&r, wt());
        assert_eq!(out.provider, GitProvider::Unknown);
        assert!(out.note.is_some());
        assert!(out.error.is_none());
        assert!(out.pr.is_none());
    }

    #[test]
    fn unknown_host_short_circuits_before_cli() {
        let r = FakeRunner {
            remote: Some("https://example.com/o/r.git".to_string()),
            ..Default::default()
        };
        let out = compute_pr_info(&r, wt());
        assert_eq!(out.provider, GitProvider::Unknown);
        assert!(!out.cli_available);
        assert!(out.note.is_some());
    }

    #[test]
    fn github_cli_missing_keeps_repo_url() {
        let r = FakeRunner {
            remote: Some("git@github.com:o/r.git".to_string()),
            ..Default::default()
        };
        let out = compute_pr_info(&r, wt());
        assert_eq!(out.provider, GitProvider::GitHub);
        assert!(!out.cli_available);
        assert_eq!(out.repo_web_url.as_deref(), Some("https://github.com/o/r"));
        assert!(out.note.unwrap().contains("gh"));
    }

    #[test]
    fn github_open_pr_with_passing_checks() {
        let mut outputs = HashMap::new();
        outputs.insert(
            "gh".to_string(),
            FakeRunner::ok(
                r#"{"number":42,"url":"https://github.com/o/r/pull/42","title":"Add thing","state":"OPEN","isDraft":false,
                    "statusCheckRollup":[{"status":"COMPLETED","conclusion":"SUCCESS"}]}"#,
            ),
        );
        let r = FakeRunner {
            remote: Some("https://github.com/o/r.git".to_string()),
            available: vec!["gh".to_string()],
            outputs,
            ..Default::default()
        };
        let out = compute_pr_info(&r, wt());
        let pr = out.pr.expect("pr");
        assert_eq!(pr.number, 42);
        assert_eq!(pr.url, "https://github.com/o/r/pull/42");
        assert_eq!(pr.state, PrState::Open);
        assert_eq!(pr.checks, PrChecksStatus::Passing);
        assert_eq!(out.note, None);
    }

    #[test]
    fn github_draft_pr_with_failing_check() {
        let mut outputs = HashMap::new();
        outputs.insert(
            "gh".to_string(),
            FakeRunner::ok(
                r#"{"number":7,"url":"u","state":"OPEN","isDraft":true,
                    "statusCheckRollup":[{"status":"COMPLETED","conclusion":"SUCCESS"},{"status":"COMPLETED","conclusion":"FAILURE"}]}"#,
            ),
        );
        let r = FakeRunner {
            remote: Some("https://github.com/o/r.git".to_string()),
            available: vec!["gh".to_string()],
            outputs,
            ..Default::default()
        };
        let pr = compute_pr_info(&r, wt()).pr.expect("pr");
        assert_eq!(pr.state, PrState::Draft);
        assert!(pr.is_draft);
        assert_eq!(pr.checks, PrChecksStatus::Failing);
    }

    #[test]
    fn github_no_pr_for_branch() {
        let mut outputs = HashMap::new();
        outputs.insert("gh".to_string(), FakeRunner::fail("no pull requests found for branch \"x\""));
        let r = FakeRunner {
            remote: Some("https://github.com/o/r.git".to_string()),
            available: vec!["gh".to_string()],
            outputs,
            ..Default::default()
        };
        let out = compute_pr_info(&r, wt());
        assert!(out.cli_available);
        assert!(out.pr.is_none());
        assert!(out.note.unwrap().to_lowercase().contains("no pull request"));
    }

    #[test]
    fn github_not_authenticated_note() {
        let mut outputs = HashMap::new();
        outputs.insert(
            "gh".to_string(),
            FakeRunner::fail("To get started with GitHub CLI, please run: gh auth login"),
        );
        let r = FakeRunner {
            remote: Some("https://github.com/o/r.git".to_string()),
            available: vec!["gh".to_string()],
            outputs,
            ..Default::default()
        };
        let out = compute_pr_info(&r, wt());
        assert!(out.note.unwrap().to_lowercase().contains("authenticated"));
    }

    #[test]
    fn gitlab_merged_mr_with_pipeline() {
        let mut outputs = HashMap::new();
        outputs.insert(
            "glab".to_string(),
            FakeRunner::ok(r#"{"iid":12,"web_url":"https://gitlab.com/g/r/-/merge_requests/12","title":"T","state":"merged","draft":false,"head_pipeline":{"status":"success"}}"#),
        );
        let r = FakeRunner {
            remote: Some("https://gitlab.com/g/r.git".to_string()),
            available: vec!["glab".to_string()],
            outputs,
            ..Default::default()
        };
        let pr = compute_pr_info(&r, wt()).pr.expect("pr");
        assert_eq!(pr.number, 12);
        assert_eq!(pr.state, PrState::Merged);
        assert_eq!(pr.checks, PrChecksStatus::Passing);
    }

    #[test]
    fn gitlab_draft_running_pipeline() {
        let mut outputs = HashMap::new();
        outputs.insert(
            "glab".to_string(),
            FakeRunner::ok(r#"{"iid":3,"web_url":"u","state":"opened","work_in_progress":true,"pipeline":{"status":"running"}}"#),
        );
        let r = FakeRunner {
            remote: Some("https://gitlab.com/g/r.git".to_string()),
            available: vec!["glab".to_string()],
            outputs,
            ..Default::default()
        };
        let pr = compute_pr_info(&r, wt()).pr.expect("pr");
        assert_eq!(pr.state, PrState::Draft);
        assert_eq!(pr.checks, PrChecksStatus::Pending);
    }

    #[test]
    fn azure_active_pr_builds_url() {
        let mut outputs = HashMap::new();
        outputs.insert(
            "az".to_string(),
            FakeRunner::ok(r#"[{"pullRequestId":99,"title":"T","status":"active","isDraft":false,"repository":{"webUrl":"https://dev.azure.com/org/proj/_git/repo"}}]"#),
        );
        let r = FakeRunner {
            remote: Some("https://dev.azure.com/org/proj/_git/repo".to_string()),
            branch: Some("feature/x".to_string()),
            available: vec!["az".to_string()],
            outputs,
        };
        let out = compute_pr_info(&r, wt());
        let pr = out.pr.expect("pr");
        assert_eq!(pr.number, 99);
        assert_eq!(pr.url, "https://dev.azure.com/org/proj/_git/repo/pullrequest/99");
        assert_eq!(pr.state, PrState::Open);
        assert_eq!(pr.checks, PrChecksStatus::Unknown);
    }

    #[test]
    fn azure_empty_array_is_no_pr() {
        let mut outputs = HashMap::new();
        outputs.insert("az".to_string(), FakeRunner::ok("[]"));
        let r = FakeRunner {
            remote: Some("git@ssh.dev.azure.com:v3/org/proj/repo".to_string()),
            branch: Some("main".to_string()),
            available: vec!["az".to_string()],
            outputs,
        };
        let out = compute_pr_info(&r, wt());
        assert_eq!(out.provider, GitProvider::AzureDevOps);
        assert!(out.pr.is_none());
        assert!(out.note.unwrap().to_lowercase().contains("no active pull request"));
    }

    #[test]
    fn azure_detached_head_note() {
        let r = FakeRunner {
            remote: Some("https://dev.azure.com/org/proj/_git/repo".to_string()),
            branch: None,
            available: vec!["az".to_string()],
            ..Default::default()
        };
        let out = compute_pr_info(&r, wt());
        assert!(out.cli_available);
        assert!(out.note.unwrap().to_lowercase().contains("detached"));
    }

    #[test]
    fn azure_unsafe_branch_name_skips_cli() {
        // A branch carrying a cmd metacharacter must NOT reach the `az` invocation (which is `cmd.exe /c az.cmd …` on Windows). Even though a
        // canned `az` output is present, the guard returns a note and never consults it, so no PR is produced.
        let mut outputs = HashMap::new();
        outputs.insert(
            "az".to_string(),
            FakeRunner::ok(r#"[{"pullRequestId":1,"title":"T","status":"active","repository":{"webUrl":"https://dev.azure.com/o/p/_git/r"}}]"#),
        );
        let r = FakeRunner {
            remote: Some("https://dev.azure.com/org/proj/_git/repo".to_string()),
            branch: Some("feature/x&calc".to_string()),
            available: vec!["az".to_string()],
            outputs,
        };
        let out = compute_pr_info(&r, wt());
        assert!(out.cli_available);
        assert!(out.pr.is_none());
        assert!(out.error.is_none());
        assert!(out.note.unwrap().to_lowercase().contains("unsafe"));
    }

    #[test]
    fn branch_safe_for_cli_accepts_normal_names_rejects_metacharacters() {
        for ok in ["main", "feature/x", "release-1.2.3", "user/fix_bug", "déjà-vu"] {
            assert!(branch_safe_for_cli(ok), "{ok} should be allowed");
        }
        for bad in ["x&calc", "a|b", "a(b)", "a%PATH%b", "a^b", "a>b", "a<b", "a!b", "a\"b", "a\nb"] {
            assert!(!branch_safe_for_cli(bad), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn spawn_error_sets_error_field() {
        let mut outputs = HashMap::new();
        outputs.insert("gh".to_string(), Err("failed to run gh: boom".to_string()));
        let r = FakeRunner {
            remote: Some("https://github.com/o/r.git".to_string()),
            available: vec!["gh".to_string()],
            outputs,
            ..Default::default()
        };
        let out = compute_pr_info(&r, wt());
        assert!(out.error.is_some());
        assert!(out.pr.is_none());
    }

    #[test]
    fn gh_checks_empty_is_none() {
        assert_eq!(gh_checks(Some(&serde_json::json!([]))), PrChecksStatus::None);
        assert_eq!(gh_checks(None), PrChecksStatus::None);
    }

    #[test]
    #[cfg(windows)]
    fn build_cli_command_honours_launch_method() {
        use std::ffi::OsStr;

        // A directly-launchable executable is spawned as-is.
        let native = ResolvedCommand {
            path: std::path::PathBuf::from(r"C:\tools\gh.exe"),
            launch: LaunchMethod::Direct,
        };
        let cmd = build_cli_command(&native, &["pr", "view"]);
        assert_eq!(cmd.get_program(), OsStr::new(r"C:\tools\gh.exe"));

        // `ViaCmdShell` routes through `cmd.exe /c <path>` — launching a `.cmd`/extensionless shim directly fails with os error 193.
        let shim = ResolvedCommand {
            path: std::path::PathBuf::from(r"C:\Program Files\Azure\az.cmd"),
            launch: LaunchMethod::ViaCmdShell,
        };
        let routed = build_cli_command(&shim, &["repos", "pr", "list"]);
        assert_eq!(routed.get_program(), OsStr::new("cmd.exe"));
        let args: Vec<&OsStr> = routed.get_args().collect();
        assert_eq!(args.first().copied(), Some(OsStr::new("/c")));
        assert_eq!(args.get(1).copied(), Some(OsStr::new(r"C:\Program Files\Azure\az.cmd")));
    }
}
