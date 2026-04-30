use std::process::Command;

/// Build a `git` Command with repo-selection environment variables stripped.
///
/// build.rs runs as a child of `cargo`, which under a husky pre-push hook
/// inherits `GIT_DIR`/`GIT_WORK_TREE`/etc. from the outer `git push`. Without
/// stripping them, our branch detection would report the *outer* repo's
/// branch and bake the wrong value into `ARBORIST_BUILD_BRANCH`. Mirrors
/// `git::git_command()` (kept duplicated because build scripts can't depend
/// on the crate being built).
fn git_command() -> Command {
    let mut cmd = Command::new("git");
    for var in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
        "GIT_PREFIX",
    ] {
        cmd.env_remove(var);
    }
    cmd
}

/// Detect the git branch name this build is being produced from.
///
/// Resolution order:
///   1. `ARBORIST_BUILD_BRANCH` (explicit override)
///   2. `GITHUB_HEAD_REF` (set in PR builds — source branch, not `merge`)
///   3. `GITHUB_REF_NAME` (set in branch/tag builds)
///   4. `git rev-parse --abbrev-ref HEAD`
///
/// Returns an empty string if none succeed or the branch is detached / `HEAD`.
fn detect_branch() -> String {
    if let Ok(b) = std::env::var("ARBORIST_BUILD_BRANCH") {
        return b.trim().to_string();
    }
    for var in ["GITHUB_HEAD_REF", "GITHUB_REF_NAME"] {
        if let Ok(v) = std::env::var(var) {
            let v = v.trim();
            if !v.is_empty() {
                return v.to_string();
            }
        }
    }
    let out = git_command()
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output();
    if let Ok(out) = out {
        if out.status.success() {
            if let Ok(s) = String::from_utf8(out.stdout) {
                let s = s.trim();
                if !s.is_empty() && s != "HEAD" {
                    return s.to_string();
                }
            }
        }
    }
    String::new()
}

/// Sanitize a branch name before embedding it in a `cargo:` directive.
///
/// Branch names can in principle contain control characters (notably
/// newlines via the `ARBORIST_BUILD_BRANCH` override).  Without sanitization
/// a malicious or malformed value could inject extra `cargo:` lines into
/// the build output.  Restrict to a single-line, conservative character set:
/// ASCII alphanumerics plus `-_./+:`.  Anything else is dropped.
fn sanitize_branch(raw: &str) -> String {
    raw.lines()
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | '+' | ':'))
        .collect()
}

fn main() {
    let branch = sanitize_branch(&detect_branch());
    println!("cargo:rustc-env=ARBORIST_BUILD_BRANCH={branch}");

    // Re-run when the override or CI env vars change.
    println!("cargo:rerun-if-env-changed=ARBORIST_BUILD_BRANCH");
    println!("cargo:rerun-if-env-changed=GITHUB_HEAD_REF");
    println!("cargo:rerun-if-env-changed=GITHUB_REF_NAME");

    // Re-run when HEAD moves. In a linked worktree `.git` is a file, so resolve
    // the real HEAD path via `git rev-parse --git-path HEAD`.
    if let Ok(out) = git_command()
        .args(["rev-parse", "--git-path", "HEAD"])
        .output()
    {
        if out.status.success() {
            if let Ok(p) = String::from_utf8(out.stdout) {
                let p = p.trim();
                if !p.is_empty() {
                    println!("cargo:rerun-if-changed={p}");
                }
            }
        }
    }

    tauri_build::build()
}
