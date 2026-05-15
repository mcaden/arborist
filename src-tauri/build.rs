use std::path::PathBuf;
use std::process::Command;

/// Build a `git` Command with repo-selection environment variables stripped.
///
/// build.rs runs as a child of `cargo`, which under a husky pre-push hook
/// inherits `GIT_DIR`/`GIT_WORK_TREE`/etc. from the outer `git push`. Without
/// stripping them, our branch detection would report the *outer* repo's
/// branch and bake the wrong value into the generated `build_branch.txt`.
/// Mirrors `git::git_command()` (kept duplicated because build scripts can't
/// depend on the crate being built).
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
///   1. `GITHUB_HEAD_REF` (set in PR builds — source branch, not `merge`)
///   2. `GITHUB_REF_NAME` (set in branch/tag builds)
///   3. `git rev-parse --abbrev-ref HEAD`
///
/// Returns an empty string if none succeed or the branch is detached / `HEAD`.
///
/// Note: there is intentionally no env-var override input. CI vars + git
/// already cover every legitimate case, and an env-var input would silently
/// leak into PTY children spawned by the running app (which inherit the
/// build/dev shell's env), baking the wrong branch into any nested
/// `tauri:dev` invocation.
fn detect_branch() -> String {
    for var in ["GITHUB_HEAD_REF", "GITHUB_REF_NAME"] {
        if let Ok(v) = std::env::var(var) {
            let v = v.trim();
            if !v.is_empty() {
                return v.to_string();
            }
        }
    }
    let out = git_command().args(["rev-parse", "--abbrev-ref", "HEAD"]).output();
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
/// CI-provided env vars (`GITHUB_*`) are normally well-formed, but defense
/// in depth: restrict to a single line and a conservative character set
/// (ASCII alphanumerics plus `-_./+:`) so a malformed value can't inject
/// extra `cargo:` lines into the build output.
fn sanitize_branch(raw: &str) -> String {
    raw.lines()
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | '+' | ':'))
        .collect()
}

/// Ensure the frontend `dist/` directory exists with a minimal placeholder.
///
/// `tauri_build::build()` expects `frontendDist` (configured as `../dist`) to
/// be present. In production the `beforeBuildCommand` runs `pnpm run build`
/// first, but during `cargo test` or ad-hoc `cargo build` the directory may
/// not exist yet. Creating a stub avoids a hard failure without affecting
/// production bundles.
fn ensure_frontend_dist() {
    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo provides CARGO_MANIFEST_DIR"));
    let dist_dir = manifest_dir.join("..").join("dist");

    if dist_dir.exists() {
        if !dist_dir.is_dir() {
            panic!("expected frontend dist path to be a directory: {}", dist_dir.display());
        }
    } else {
        std::fs::create_dir_all(&dist_dir).expect("create dist/ stub directory");
    }

    let index = dist_dir.join("index.html");
    if index.exists() {
        if !index.is_file() {
            panic!("expected frontend dist entry to be a file: {}", index.display());
        }
    } else {
        std::fs::write(&index, "<!doctype html><html><head></head><body></body></html>\n").expect("write dist/index.html stub");
    }
}

fn main() {
    ensure_frontend_dist();

    let branch = sanitize_branch(&detect_branch());

    // Bake the branch into a generated file under OUT_DIR; lib.rs reads it via
    // `include_str!`. We deliberately avoid `cargo:rustc-env=` (which would
    // require an `env!()` read at compile time) so the entire codebase is free
    // of project-specific environment variables.
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo provides OUT_DIR"));
    let branch_file = out_dir.join("build_branch.txt");
    std::fs::write(&branch_file, &branch).expect("write build_branch.txt");

    // Re-run when CI env vars change.
    println!("cargo:rerun-if-env-changed=GITHUB_HEAD_REF");
    println!("cargo:rerun-if-env-changed=GITHUB_REF_NAME");

    // Re-run when HEAD moves. In a linked worktree `.git` is a file, so resolve
    // the real HEAD path via `git rev-parse --git-path HEAD`.
    if let Ok(out) = git_command().args(["rev-parse", "--git-path", "HEAD"]).output() {
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
