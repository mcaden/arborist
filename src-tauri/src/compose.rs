//! Pure command composition, label dedup, worktree validation, and
//! shell-quoting helpers (Phase 5 of the implementation plan).
//!
//! This module is **pure**: it performs no filesystem or process I/O. The
//! caller (Phase 7's `session_create`) is responsible for:
//!
//! 1. Calling [`validate_worktree`] to canonicalize the user-supplied path.
//! 2. Reading the selected instruction set file from disk.
//! 3. Calling [`compose_command`] with the canonical worktree path and the
//!    instruction set contents already in memory.
//! 4. Materialising the returned [`ComposedInvocation::temp_files`] to disk
//!    before spawning the PTY.
//!
//! Keeping composition pure makes it trivially unit-testable and lets
//! `respawn_existing` (Phase 7) re-materialise temp files without re-running
//! composition.
//!
//! Spec/design references:
//! - SPEC §5.2 C-05 (label dedup)
//! - SPEC §5.4 I-04 (instruction-set delivery)
//! - SPEC §5.6 (Shell Commands at Launch)
//! - DESIGN §5.1 step 2 (platform shell selection)
//! - DESIGN §5.4 (restart reuses `composed_command` verbatim)
//! - DESIGN §5.6 (per-tool CLI launch table)
//! - DESIGN §8 (security: quoting, canonicalization, path-as-cwd)

use std::path::{Path, PathBuf};

use crate::types::{Error, InstructionSet, SessionId, TempFileSpec, Tool};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// The product of [`compose_command`]: the verbatim shell string that will be
/// passed to the platform shell as its `-c` / `/c` argument, plus any temp
/// files the caller must materialise before spawning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposedInvocation {
    /// Full shell command — prelaunch commands joined with `&&` followed by
    /// the per-tool CLI launch command. This is stored verbatim on
    /// `Session.composed_command` and reused by `session_restart` and
    /// restore-on-launch (DESIGN §5.4 / §5.5).
    pub composed_command: String,
    /// Files the backend must write to disk before spawning the PTY. For
    /// Claude this contains exactly one entry (the `--system-prompt` file).
    /// For Copilot this is empty.
    pub temp_files: Vec<TempFileSpec>,
}

/// Inputs to [`compose_command`]. Borrowed to keep the call site allocation-
/// free; the function itself returns owned data.
pub struct ComposeInputs<'a> {
    pub session_id: SessionId,
    pub tool: Tool,
    /// **Already canonicalized** by [`validate_worktree`]. Composition does
    /// not re-canonicalize and never touches disk.
    pub worktree_path: &'a Path,
    pub worktree_label: &'a str,
    /// Optional user-curated instruction set. When `None`:
    /// * Claude launches with no `--system-prompt` (auto-loads `CLAUDE.md` from
    ///   `cwd`).
    /// * Copilot ignores this field — it is launched bare regardless, and reads
    ///   `.github/copilot-instructions.md` from `cwd`.
    pub instruction_set: Option<&'a InstructionSet>,
    /// Verbatim, in declaration order. They are joined with ` && ` ahead of
    /// the CLI command and passed through *without* re-quoting — they are
    /// already user-authored shell snippets (DESIGN §5.6).
    pub prelaunch_commands: &'a [String],
    /// In-memory contents of the selected instruction set file. The caller
    /// reads the file (and enforces the size cap from DESIGN §8); compose
    /// stays pure. Must be `Some` when [`Self::instruction_set`] is `Some`,
    /// `None` otherwise. Ignored for Copilot.
    pub instruction_set_contents: Option<&'a str>,
    /// Optional user override for the CLI launch command. When `Some` and
    /// non-empty, replaces the bare program token (`claude` / `copilot`)
    /// in the composed command. The string is treated as a verbatim shell
    /// snippet — *not* a single quoted token — so users can put extra
    /// arguments in it (e.g. `"npx claude --model sonnet"`). Empty strings
    /// behave the same as `None` (use the default).
    pub cli_launch_command: Option<&'a str>,
}

/// Compose the shell command for a session.
///
/// Implements the per-tool table in DESIGN §5.6 / SPEC I-04:
///
/// | Tool    | CLI                                              | Temp file |
/// |---------|--------------------------------------------------|-----------|
/// | Claude  | `claude --system-prompt <quoted-temp-file-path>` | yes       |
/// | Copilot | `copilot` (bare; interactive mode is the default) | no        |
///
/// **Worktree handling**: the worktree path is *never* interpolated into the
/// composed command. In both cases the real `cwd` for `portable-pty` is set
/// separately by the PTY pool; we never emit `cd "<path>" && …` (DESIGN §8).
pub fn compose_command(inputs: &ComposeInputs<'_>) -> Result<ComposedInvocation, Error> {
    let quoter = platform_shell().quoter;

    let (cli_cmd, temp_files) = match inputs.tool {
        Tool::Claude => build_claude(inputs, quoter),
        Tool::Copilot => build_copilot(inputs, quoter),
    };

    let mut parts: Vec<String> = inputs.prelaunch_commands.iter().map(String::clone).collect();
    parts.push(cli_cmd);
    let composed_command = parts.join(" && ");

    Ok(ComposedInvocation {
        composed_command,
        temp_files,
    })
}

/// Per SPEC C-05: produce a label that does not collide with `existing`.
///
/// Algorithm: if `base` is not in `existing`, return it unchanged. Otherwise
/// pick the lowest integer `n >= 2` such that `format!("{base} {n}")` is not
/// in `existing`. Existing gaps are **not** refilled in the sense that this
/// function only returns `base` itself when nothing collides — but among
/// suffixes it always picks the *lowest* free integer, so e.g. given
/// `[foo, foo 3]` and base `foo` it returns `foo 2`.
#[must_use]
pub fn dedupe_label(existing: &[&str], base: &str) -> String {
    if !existing.contains(&base) {
        return base.to_owned();
    }
    let mut n: u32 = 2;
    loop {
        let candidate = format!("{base} {n}");
        if !existing.contains(&candidate.as_str()) {
            return candidate;
        }
        n += 1;
    }
}

/// Validate a user-supplied **worktree / branch name** (Roadmap §2.3).
///
/// Returns `Ok(name.to_owned())` when valid, or `Err(message)` describing
/// the first rule violated. The rules deliberately mirror git's branch
/// naming rules subset that makes sense for the in-app create-worktree
/// flow:
///
/// * No spaces.
/// * No `..`, `~`, `^`, `:`, `?`, `*`, `[`, `\`.
/// * Cannot start or end with `.` or `/`.
/// * Cannot end with `.lock`.
/// * Cannot be `@` alone.
/// * 1–255 characters.
///
/// Pure: no IO. Used both by the Tauri command (server-side check before
/// shelling out to `git worktree add`) and re-implemented identically on
/// the TS side as `validateWorktreeName` for inline picker feedback.
pub fn validate_worktree_name(name: &str) -> Result<String, String> {
    if name.is_empty() {
        return Err("name cannot be empty".to_owned());
    }
    // Use char count (Unicode scalars) on both sides of the boundary to
    // keep Rust and TS rejecting / accepting the same inputs.
    if name.chars().count() > 255 {
        return Err("name cannot exceed 255 characters".to_owned());
    }
    if name == "@" {
        return Err("name cannot be '@'".to_owned());
    }
    if name.starts_with('-') {
        return Err("name cannot start with '-'".to_owned());
    }
    if name.contains("..") {
        return Err("name cannot contain '..'".to_owned());
    }
    if name.contains("@{") {
        return Err("name cannot contain '@{'".to_owned());
    }
    if name.contains("//") {
        return Err("name cannot contain '//'".to_owned());
    }
    if name.contains(' ') {
        return Err("name cannot contain spaces".to_owned());
    }
    for ch in ['~', '^', ':', '?', '*', '[', '\\'] {
        if name.contains(ch) {
            return Err(format!("name cannot contain '{ch}'"));
        }
    }
    if name.chars().any(|c| c.is_control() || c == '\u{007f}') {
        return Err("name cannot contain control characters".to_owned());
    }
    if name.starts_with('.') || name.starts_with('/') {
        return Err("name cannot start with '.' or '/'".to_owned());
    }
    if name.ends_with('.') || name.ends_with('/') {
        return Err("name cannot end with '.' or '/'".to_owned());
    }
    if name.ends_with(".lock") {
        return Err("name cannot end with '.lock'".to_owned());
    }
    // Per-component checks: every '/'-separated segment must independently
    // satisfy git's refs(7) rules.
    for component in name.split('/') {
        if component.is_empty() {
            return Err("name cannot contain empty path components".to_owned());
        }
        if component.starts_with('.') {
            return Err("name path components cannot start with '.'".to_owned());
        }
        if component.ends_with(".lock") {
            return Err("name path components cannot end with '.lock'".to_owned());
        }
    }
    Ok(name.to_owned())
}

/// Validate a user-supplied worktree path.
///
/// - Missing path → [`Error::WorktreeMissing`].
/// - Exists but not a directory → [`Error::InvalidPath`].
/// - Otherwise returns the canonical (symlink-resolved) path.
///
/// On Windows we go through `dunce::canonicalize` to avoid `\\?\` UNC
/// prefixes that confuse downstream tooling and string comparisons.
pub fn validate_worktree(path: &Path) -> Result<PathBuf, Error> {
    if !path.exists() {
        return Err(Error::WorktreeMissing(path.to_path_buf()));
    }
    let canonical = dunce::canonicalize(path).map_err(|e| Error::InvalidPath(format!("{}: {e}", path.display())))?;
    if !canonical.is_dir() {
        return Err(Error::InvalidPath(format!("{} is not a directory", canonical.display())));
    }
    Ok(canonical)
}

/// Deterministic per-session temp directory: `<os-temp>/arborist/<uuid>/`.
///
/// Phase 6's `cleanup_orphans` walks `<os-temp>/arborist/` and removes child
/// directories whose UUID does not match a known session, so this scheme
/// must stay stable.
#[must_use]
pub fn session_temp_dir(id: &SessionId) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push("arborist");
    p.push(id.0.to_string());
    p
}

/// Single source of truth for the path to a Copilot session's OTel JSONL
/// file. Used by [`env_for_tool`] (to point Copilot's exporter at it),
/// by `pty_pool` spawn prep (to wipe a stale copy from a prior run), and
/// by `session_metrics::MetricsRegistry::start` (to tail it). Anywhere
/// else that needs this path **must** call this helper — never reconstruct
/// the filename inline.
#[must_use]
pub fn copilot_otel_path(session_id: &SessionId) -> PathBuf {
    session_temp_dir(session_id).join("otel.jsonl")
}

/// Per-session **environment variables** to inject into the spawned CLI
/// process, separately from the shell command. These are *additions* on top
/// of the parent process's inherited env (the spawner does not call
/// `env_clear`). They are recomputed on every spawn from the session id and
/// **never** stored on `Session` — only the path to the OTel JSONL would be
/// derivable anyway, and persisting it would let stale paths from a previous
/// install leak across upgrades.
///
/// Values are `OsString` so paths with non-UTF-8 segments (rare on Windows
/// but possible) round-trip without lossy `�` substitution.
///
/// Per-tool behaviour:
///
/// * `Tool::Copilot` — enable Copilot's OpenTelemetry **file exporter**
///   pointing at `<session_temp_dir>/otel.jsonl`. Arborist tails that file to
///   surface real-time token usage / context-window state in the sidebar (see
///   [`crate::session_metrics::run_copilot_watcher`]). The
///   `OTEL_BSP_SCHEDULE_DELAY=1000` (ms) tightens the SDK's batch flush from
///   its 5s default to ~1Hz so the sidebar updates feel live. Older Copilot
///   CLIs that don't recognise these vars silently ignore them.
/// * `Tool::Claude` — empty. Claude Code has no file-exporter mode (only OTLP,
///   which would require an in-process receiver). Reuses the existing
///   transcript-tailing watcher.
#[must_use]
pub fn env_for_tool(tool: Tool, session_id: &SessionId) -> Vec<(String, std::ffi::OsString)> {
    match tool {
        Tool::Copilot => {
            let path = copilot_otel_path(session_id);
            vec![
                ("COPILOT_OTEL_FILE_EXPORTER_PATH".to_owned(), path.into_os_string()),
                ("COPILOT_OTEL_ENABLED".to_owned(), "true".into()),
                ("OTEL_BSP_SCHEDULE_DELAY".to_owned(), "1000".into()),
            ]
        }
        Tool::Claude => Vec::new(),
    }
}

/// Augment a stored `composed_command` with `--resume <ai_session_id>` so
/// the AI conversation continues across an app restart, a user-initiated
/// restart, or — for Copilot — pre-binds the brand-new session to a
/// pre-allocated uuid at create time.
///
/// Used by every spawn site that has an `ai_session_id` to honor:
/// `session_create` (Copilot only — pre-allocated uuid), `session_restart`
/// (Copilot only — freshly re-allocated uuid), and `restore_all_sessions`
/// (both tools when an id is persisted). The persisted `composed_command`
/// itself stays bare (DESIGN §5.4 — the immutable record never contains
/// `--resume`); the splice happens on a clone at every spawn.
///
/// We append at the end of the command rather than parse and re-emit the
/// CLI invocation. Both `claude` and `copilot` accept positional flags
/// in any order, and the trailing token of `composed_command` is always
/// the CLI invocation (DESIGN §5.6 step 3 — `[prelaunch && ]<cli>`),
/// so appending binds correctly even when the user has prelaunch hooks
/// that themselves contain `&&` inside quoted strings.
///
/// `ai_session_id` is shell-quoted using the host quoter only when it
/// contains characters that could be interpreted by the shell. In
/// practice CLI session ids are UUIDs (ASCII alphanumerics + `-`), and
/// quoting them is actively harmful on Windows: the `cmd.exe` quoter
/// wraps the value in literal `"…"`, and CLIs distributed as `.cmd`
/// shims (like `copilot.cmd`) forward `%*` verbatim to the underlying
/// `node` process, which then sees the surrounding quotes as part of
/// the argument value — `--resume "<uuid>"` reaches the CLI with the
/// quotes attached and resume fails. Defensive quoting is still applied
/// for any future non-safe id (e.g. Copilot's session-by-name resume).
#[must_use]
pub fn with_resume(composed_command: &str, _tool: Tool, ai_session_id: &str) -> String {
    if is_shell_safe_token(ai_session_id) {
        format!("{composed_command} --resume {ai_session_id}")
    } else {
        let quoter = platform_shell().quoter;
        format!("{composed_command} --resume {}", quoter(ai_session_id))
    }
}

/// True for non-empty values composed entirely of characters that have
/// no special meaning to either `sh` or `cmd.exe`: ASCII letters,
/// digits, `-`, `_`, and `.`. UUIDs and similar slugs trivially satisfy
/// this and can be appended to a composed command without quoting.
///
/// The set is intentionally conservative — other characters (`@ + : = ,`
/// etc.) are also unquoted-safe on both shells, but the conservative
/// floor covers every realistic AI-session id today (UUIDs from both
/// Claude and Copilot) and any value outside it falls through to the
/// host quoter, which is correct, just verbose. Do not expand this set
/// without auditing every shell context the resulting command flows
/// through (`cmd.exe /c`, `.cmd` shim → CRT re-parse, `sh -c`).
fn is_shell_safe_token(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

// ---------------------------------------------------------------------------
// Shell quoting
// ---------------------------------------------------------------------------

/// POSIX single-quoted form. Round-trip rule: `sh -c "echo <output>"` prints
/// the original byte sequence unchanged.
///
/// Algorithm (well-known): wrap the value in single quotes, and replace any
/// internal `'` with the four-character sequence `'\''`. This works for
/// every byte except NUL (which no shell can carry as an argument anyway).
/// Empty strings become `''` so they remain a single argument.
#[must_use]
pub fn shell_quote_posix(value: &str) -> String {
    if value.is_empty() {
        return "''".to_owned();
    }
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            // Close the quoted run, emit an escaped quote, reopen.
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// `cmd.exe` quoting.
///
/// The `cmd.exe` parser splits arguments on whitespace and then invokes the
/// program loader, which re-parses the command tail according to the C
/// runtime rules. Two separate layers of escaping are required:
///
/// 1. **CRT layer** (so the spawned program sees the value as one argument):
///    wrap in `"…"`, double any embedded `"` and any backslash run that
///    immediately precedes a `"` or the closing quote.
/// 2. **`cmd.exe` layer** (so the metacharacters `^ & | < > ( ) % !` aren't
///    interpreted by the shell *before* the target program ever sees them):
///    caret-escape every reserved character *outside* of the double quotes —
///    but because we put the whole value inside one pair of double quotes, the
///    only metacharacter that needs a caret is `"` itself when it appears at
///    the boundary. We additionally caret-escape `^` inside the value to defend
///    against `cmd /v:on`-style delayed expansion gotchas, and caret-escape `%`
///    and `!` which the shell expands inside double quotes.
///
/// The implementation here errs on the side of over-quoting: every reserved
/// character gets a `^` prefix and the whole value is wrapped in `"…"`.
/// This is verbose but safe, and it's only used for argv values we ourselves
/// supply.
#[must_use]
pub fn shell_quote_cmd(value: &str) -> String {
    // First, CRT-style escape inside the eventual double quotes.
    let mut crt = String::with_capacity(value.len() + 2);
    let mut backslashes: usize = 0;
    for ch in value.chars() {
        match ch {
            '\\' => {
                backslashes += 1;
                crt.push('\\');
            }
            '"' => {
                // Double every preceding backslash, then escape the quote.
                for _ in 0..backslashes {
                    crt.push('\\');
                }
                crt.push('\\');
                crt.push('"');
                backslashes = 0;
            }
            _ => {
                backslashes = 0;
                crt.push(ch);
            }
        }
    }
    // Backslashes immediately before the closing quote also need doubling.
    for _ in 0..backslashes {
        crt.push('\\');
    }

    // Now caret-escape cmd.exe metacharacters. Since the whole value is
    // wrapped in `"…"` below, most metacharacters are inert — but `%` and
    // `!` are still expanded inside double quotes, and `^` itself is the
    // escape character so it must be doubled. We caret-escape these three
    // *inside* the quoted region; everything else is safe.
    let mut out = String::with_capacity(crt.len() + 2);
    out.push('"');
    for ch in crt.chars() {
        match ch {
            '%' | '!' | '^' => {
                out.push('^');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// A function pointer to the appropriate quoter for the host shell.
pub type Quoter = fn(&str) -> String;

/// Platform shell selection per DESIGN §5.1 step 2.
pub struct PlatformShell {
    /// Program to spawn (e.g. `/bin/sh`, `cmd.exe`).
    pub program: String,
    /// Single argument flag (`-c` or `/c`).
    pub flag: &'static str,
    /// Quoter to use when interpolating dynamic values into the composed
    /// command we hand to `program flag`.
    pub quoter: Quoter,
}

/// Resolve the host's interactive shell.
///
/// - Unix: `$SHELL` if set, else `/bin/sh`; flag `-c`; POSIX quoting.
/// - Windows: `%COMSPEC%` if set, else `cmd.exe`; flag `/c`; cmd quoting.
#[must_use]
pub fn platform_shell() -> PlatformShell {
    #[cfg(windows)]
    {
        let program = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_owned());
        PlatformShell {
            program,
            flag: "/c",
            quoter: shell_quote_cmd,
        }
    }
    #[cfg(not(windows))]
    {
        let program = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_owned());
        PlatformShell {
            program,
            flag: "-c",
            quoter: shell_quote_posix,
        }
    }
}

// ---------------------------------------------------------------------------
// Per-tool builders
// ---------------------------------------------------------------------------

fn worktree_context_block(label: &str, worktree_path: &Path) -> String {
    format!(
        "You are operating in Git worktree **{label}** at {path}.",
        label = label,
        path = worktree_path.display(),
    )
}

fn build_claude(inputs: &ComposeInputs<'_>, quoter: Quoter) -> (String, Vec<TempFileSpec>) {
    let program = cli_program_for_tool(Tool::Claude, inputs.cli_launch_command, quoter);

    // No user instruction set: launch plain `claude`. The agent still
    // auto-discovers `CLAUDE.md` from its `cwd` (the worktree). Skipping
    // `--system-prompt` keeps the launch surface minimal and lets the
    // agent derive its location from `pwd`/`git` without us having to
    // fabricate a worktree-context system prompt.
    let Some(contents) = inputs.instruction_set_contents else {
        return (program, Vec::new());
    };

    let dir = session_temp_dir(&inputs.session_id);
    let temp_path = dir.join("system-prompt.md");

    let header = worktree_context_block(inputs.worktree_label, inputs.worktree_path);
    let body = format!("{header}\n---\n{contents}", header = header, contents = contents,);

    let cli_cmd = format!(
        "{program} --system-prompt {quoted}",
        program = program,
        quoted = quoter(&temp_path.to_string_lossy()),
    );

    (
        cli_cmd,
        vec![TempFileSpec {
            path: temp_path,
            contents: body,
        }],
    )
}

fn build_copilot(inputs: &ComposeInputs<'_>, quoter: Quoter) -> (String, Vec<TempFileSpec>) {
    // Modern `copilot` (the standalone GitHub Copilot CLI) starts in
    // interactive mode by default. The legacy `--interactive <string>`
    // flag was removed and now triggers a "too many arguments" usage
    // error from the CLI itself. We therefore spawn `copilot` with no
    // arguments and rely on its `cwd`-based discovery of
    // `.github/copilot-instructions.md` for repository guidance — the
    // PTY pool already passes the worktree as `cwd`. The worktree
    // context block (label + path) is intentionally dropped here; the
    // agent can derive its location from `pwd`/`git` if it needs to.
    let cli_cmd = cli_program_for_tool(Tool::Copilot, inputs.cli_launch_command, quoter);
    (cli_cmd, Vec::new())
}

/// Environment variable consulted by [`cli_program_for_tool`] to override
/// the `claude` executable. **Test-only seam** — production code never sets
/// this, but integration tests point it at `arborist-test-child` so they can
/// drive the full Tauri command/event surface end-to-end without a real
/// Claude install. Documented here so the override path is auditable.
pub const CLAUDE_OVERRIDE_ENV: &str = "ARBORIST_CLI_OVERRIDE_CLAUDE";

/// Sibling of [`CLAUDE_OVERRIDE_ENV`] for the `copilot` executable.
pub const COPILOT_OVERRIDE_ENV: &str = "ARBORIST_CLI_OVERRIDE_COPILOT";

/// Resolve the program token for `tool`. Precedence (highest first):
///
/// 1. **User config override** (`config_override`, when `Some` and non-empty):
///    inserted **verbatim** into the composed command. This is a shell snippet
///    authored by the user — not a single argument — so callers can add flags
///    like `--model sonnet` directly. Persisted by the Settings dialog into
///    `AppConfig.ai_launch_commands`.
/// 2. **Test-seam env var** (`ARBORIST_CLI_OVERRIDE_*`): set by integration
///    tests to point at `arborist-test-child`. Returned **shell-quoted** so
///    paths with spaces still work.
/// 3. **Default**: the bare CLI name (`claude` / `copilot`).
///
/// The override path is invisible to the persisted `composed_command` once
/// the env var is unset, so do not rely on the env-var path across restarts.
fn cli_program_for_tool(tool: Tool, config_override: Option<&str>, quoter: Quoter) -> String {
    // 1. Config override (verbatim).
    if let Some(s) = config_override {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
    }
    // 2. Test-seam env override (quoted).
    let var = match tool {
        Tool::Claude => CLAUDE_OVERRIDE_ENV,
        Tool::Copilot => COPILOT_OVERRIDE_ENV,
    };
    if let Ok(path) = std::env::var(var) {
        if !path.is_empty() {
            return quoter(&path);
        }
    }
    // 3. Default.
    match tool {
        Tool::Claude => "claude".to_owned(),
        Tool::Copilot => "copilot".to_owned(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::InstructionSetId;
    use pretty_assertions::assert_eq;
    use rstest::rstest;
    use std::fs;
    use uuid::Uuid;

    // -- helpers ----------------------------------------------------------

    fn fixed_id() -> SessionId {
        SessionId(Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").expect("uuid"))
    }

    fn instr_set(tool: Tool) -> InstructionSet {
        InstructionSet {
            id: InstructionSetId::new("set-1"),
            name: "Set 1".into(),
            tool,
            file_path: PathBuf::from("/some/instructions.md"),
            is_default: true,
        }
    }

    fn inputs<'a>(
        tool: Tool,
        worktree: &'a Path,
        label: &'a str,
        is: Option<&'a InstructionSet>,
        prelaunch: &'a [String],
        body: Option<&'a str>,
    ) -> ComposeInputs<'a> {
        ComposeInputs {
            session_id: fixed_id(),
            tool,
            worktree_path: worktree,
            worktree_label: label,
            instruction_set: is,
            prelaunch_commands: prelaunch,
            instruction_set_contents: body,
            cli_launch_command: None,
        }
    }

    // The host's quoter — used in tests so assertions work on both Unix
    // and Windows runners.
    fn host_quote(s: &str) -> String {
        (platform_shell().quoter)(s)
    }

    // -- composition: Claude --------------------------------------------

    #[test]
    fn claude_compose_no_prelaunch() {
        let wt = PathBuf::from(if cfg!(windows) { "C:\\repos\\my-feature" } else { "/repos/my-feature" });
        let is = instr_set(Tool::Claude);
        let body = "# Instructions\nBe helpful.\n";
        let r = compose_command(&inputs(Tool::Claude, &wt, "my-feature", Some(&is), &[], Some(body))).expect("compose");

        let expected_path = session_temp_dir(&fixed_id()).join("system-prompt.md");
        let quoted = host_quote(&expected_path.to_string_lossy());
        assert_eq!(r.composed_command, format!("claude --system-prompt {quoted}"));

        assert_eq!(r.temp_files.len(), 1);
        assert_eq!(r.temp_files[0].path, expected_path);
        assert!(r.temp_files[0]
            .contents
            .starts_with("You are operating in Git worktree **my-feature** at "));
        assert!(r.temp_files[0].contents.ends_with(body));
        assert!(r.temp_files[0].contents.contains("\n---\n"));

        // Worktree path must not leak into the composed command for Claude.
        assert!(!r.composed_command.contains(&*wt.to_string_lossy()));
        // No `cd "..." &&` shenanigans either.
        assert!(!r.composed_command.contains("cd "));
    }

    #[test]
    fn claude_compose_with_prelaunch() {
        let wt = PathBuf::from(if cfg!(windows) { "C:\\wt" } else { "/wt" });
        let is = instr_set(Tool::Claude);
        let pre = vec!["nvm use 20".to_owned(), "source .env".to_owned()];
        let r = compose_command(&inputs(Tool::Claude, &wt, "wt", Some(&is), &pre, Some("body"))).expect("compose");

        assert!(r.composed_command.starts_with("nvm use 20 && source .env && claude --system-prompt "));
    }

    #[test]
    fn claude_temp_path_is_quoted_when_it_contains_a_space() {
        // Force a temp path with a space by overriding TMPDIR (Unix) /
        // TEMP (Windows) — but env-var hacks are messy under parallel tests.
        // Instead, exercise the quoter directly on the kind of path we'd
        // build, and assert the composed command would contain the quoted
        // form for such a path.
        let space_path = if cfg!(windows) {
            "C:\\Users\\Some User\\AppData\\Local\\Temp\\arborist\\x\\system-prompt.md"
        } else {
            "/tmp/Some User/arborist/x/system-prompt.md"
        };
        let q = host_quote(space_path);
        // Either POSIX single-quoted or cmd double-quoted — both contain
        // the relevant quote character.
        if cfg!(windows) {
            assert!(q.starts_with('"') && q.ends_with('"'));
        } else {
            assert!(q.starts_with('\'') && q.ends_with('\''));
            // Round-trip: nothing inside is an unescaped single quote.
            let inner = &q[1..q.len() - 1];
            assert!(!inner.contains('\''));
        }
    }

    #[test]
    fn claude_compose_without_instruction_set_drops_system_prompt() {
        let wt = PathBuf::from(if cfg!(windows) { "C:\\repos\\my-feature" } else { "/repos/my-feature" });
        let r = compose_command(&inputs(Tool::Claude, &wt, "my-feature", None, &[], None)).expect("compose");

        assert_eq!(r.composed_command, "claude");
        assert!(r.temp_files.is_empty());
        // Worktree path is supplied as `cwd` by the PTY pool, never via the
        // command string.
        assert!(!r.composed_command.contains(&*wt.to_string_lossy()));
    }

    #[test]
    fn claude_compose_without_instruction_set_keeps_prelaunch() {
        let wt = PathBuf::from(if cfg!(windows) { "C:\\wt" } else { "/wt" });
        let pre = vec!["nvm use 20".to_owned()];
        let r = compose_command(&inputs(Tool::Claude, &wt, "wt", None, &pre, None)).expect("compose");
        assert_eq!(r.composed_command, "nvm use 20 && claude");
        assert!(r.temp_files.is_empty());
    }

    // -- composition: Copilot -------------------------------------------

    #[test]
    fn copilot_compose_no_prelaunch() {
        let wt = PathBuf::from(if cfg!(windows) { "C:\\wt" } else { "/wt" });
        let is = instr_set(Tool::Copilot);
        let r = compose_command(&inputs(Tool::Copilot, &wt, "wt", Some(&is), &[], Some("ignored"))).expect("compose");

        // Modern `copilot` starts in interactive mode by default. The legacy
        // `--interactive <string>` flag was removed and now triggers a
        // "too many arguments" error from the CLI itself, so we spawn it
        // bare and rely on `cwd`-based discovery for repo guidance.
        assert_eq!(r.composed_command, "copilot");
        assert!(r.temp_files.is_empty());
        assert!(!r.composed_command.contains("--instructions"));
        assert!(!r.composed_command.contains("--interactive"));
        // Worktree path must not leak into the composed command — it is
        // supplied to `portable-pty` as `cwd`.
        assert!(!r.composed_command.contains(&*wt.to_string_lossy()));
        assert!(!r.composed_command.contains("cd "));
    }

    #[test]
    fn copilot_compose_with_prelaunch() {
        let wt = PathBuf::from(if cfg!(windows) { "C:\\wt" } else { "/wt" });
        let is = instr_set(Tool::Copilot);
        let pre = vec!["echo hi".to_owned(), "true".to_owned()];
        let r = compose_command(&inputs(Tool::Copilot, &wt, "wt", Some(&is), &pre, Some(""))).expect("compose");
        assert_eq!(r.composed_command, "echo hi && true && copilot");
    }

    #[test]
    fn copilot_compose_does_not_leak_worktree_path_when_path_contains_spaces() {
        let wt = PathBuf::from(if cfg!(windows) { "C:\\my repos\\feature" } else { "/my repos/feature" });
        let is = instr_set(Tool::Copilot);
        let r = compose_command(&inputs(Tool::Copilot, &wt, "feature", Some(&is), &[], Some(""))).expect("compose");
        // Bare `copilot` — no worktree path, no `--interactive`, regardless
        // of how spicy the path looks. This is the regression test for the
        // pre-removal behaviour where we used to interpolate the path into
        // a quoted `--interactive` argument.
        assert_eq!(r.composed_command, "copilot");
    }

    // -- composition: cli_launch_command override ----------------------

    #[test]
    fn claude_compose_uses_cli_launch_command_override() {
        let wt = PathBuf::from(if cfg!(windows) { "C:\\wt" } else { "/wt" });
        let is = instr_set(Tool::Claude);
        let mut i = inputs(Tool::Claude, &wt, "wt", Some(&is), &[], Some("body"));
        i.cli_launch_command = Some("npx claude --model sonnet");
        let r = compose_command(&i).expect("compose");
        // Override is inserted verbatim in place of the bare `claude`
        // token; the `--system-prompt` flag is still appended.
        assert!(r.composed_command.starts_with("npx claude --model sonnet --system-prompt "));
    }

    #[test]
    fn copilot_compose_uses_cli_launch_command_override() {
        let wt = PathBuf::from(if cfg!(windows) { "C:\\wt" } else { "/wt" });
        let mut i = inputs(Tool::Copilot, &wt, "wt", None, &[], None);
        i.cli_launch_command = Some("gh copilot");
        let r = compose_command(&i).expect("compose");
        assert_eq!(r.composed_command, "gh copilot");
    }

    #[test]
    fn empty_or_whitespace_override_falls_back_to_default() {
        let wt = PathBuf::from(if cfg!(windows) { "C:\\wt" } else { "/wt" });
        for s in [Some(""), Some("   "), None] {
            let mut i = inputs(Tool::Copilot, &wt, "wt", None, &[], None);
            i.cli_launch_command = s;
            let r = compose_command(&i).expect("compose");
            assert_eq!(r.composed_command, "copilot", "override={:?}", s);
        }
    }

    #[test]
    fn override_composes_with_prelaunch_commands() {
        let wt = PathBuf::from(if cfg!(windows) { "C:\\wt" } else { "/wt" });
        let pre = vec!["nvm use 20".to_owned()];
        let mut i = inputs(Tool::Copilot, &wt, "wt", None, &pre, None);
        i.cli_launch_command = Some("copilot --foo bar");
        let r = compose_command(&i).expect("compose");
        assert_eq!(r.composed_command, "nvm use 20 && copilot --foo bar");
    }

    // -- with_resume ----------------------------------------------------

    #[test]
    fn with_resume_appends_verbatim_id_to_bare_claude() {
        let out = with_resume("claude", Tool::Claude, "abc-123");
        // Slug-safe id: appended verbatim, no shell quoting (avoids
        // `.cmd`-shim quote leakage on Windows).
        assert_eq!(out, "claude --resume abc-123");
    }

    #[test]
    fn with_resume_appends_after_system_prompt() {
        let base = "claude --system-prompt /tmp/x.txt";
        let out = with_resume(base, Tool::Claude, "uuid-1");
        assert_eq!(out, format!("{base} --resume uuid-1"), "resume must be appended after existing flags");
    }

    #[test]
    fn with_resume_keeps_prelaunch_chain_intact() {
        // Prelaunch commands chained via && precede the CLI invocation.
        // Appending --resume at the end binds to the trailing CLI token.
        let base = "echo hi && nvm use 20 && copilot";
        let out = with_resume(base, Tool::Copilot, "sess-9");
        assert_eq!(out, format!("{base} --resume sess-9"));
    }

    #[test]
    fn with_resume_does_not_quote_uuid_ids() {
        // Regression: cmd.exe quoting of a plain UUID wraps it in
        // literal `"…"`, which `.cmd` shims (e.g. copilot.cmd) forward
        // verbatim to node, making the CLI see `--resume "<uuid>"` and
        // fail to resume. Safe tokens must be appended unquoted on
        // every platform.
        let uuid = "1eed651b-6a1b-4c7e-9646-7132eae8c6e9";
        let out = with_resume("copilot", Tool::Copilot, uuid);
        assert_eq!(out, format!("copilot --resume {uuid}"));
        assert!(!out.contains('"'), "no double quotes around safe id: {out}");
        assert!(!out.contains('\''), "no single quotes around safe id: {out}");
    }

    #[test]
    fn with_resume_quotes_ids_with_shell_metachars() {
        // Defensive: a future name-based resume id must not corrupt the
        // command. The host quoter should fully escape it.
        let nasty = "id with space&danger";
        let out = with_resume("claude", Tool::Claude, nasty);
        assert_eq!(out, format!("claude --resume {}", host_quote(nasty)));
        // Extra sanity: the raw id substring must not appear unquoted.
        assert!(!out.ends_with(nasty));
    }

    // -- POSIX quoting --------------------------------------------------

    #[rstest]
    #[case("")]
    #[case("simple")]
    #[case("with space")]
    #[case("with'quote")]
    #[case("with\"dquote")]
    #[case("with\\backslash")]
    #[case("with$dollar")]
    #[case("with&amp")]
    #[case("with|pipe")]
    #[case("with>redirect")]
    #[case("with^caret")]
    fn posix_quote_round_trip_properties(#[case] input: &str) {
        let q = shell_quote_posix(input);
        // Must start and end with a single quote.
        assert!(q.starts_with('\''), "{q:?} must start with '");
        assert!(q.ends_with('\''), "{q:?} must end with '");

        // The "interior" — between the leading and trailing single quotes
        // — must contain no unescaped single quote. Single quotes inside
        // are encoded as `'\''`, which means the only ' chars we should
        // see inside are part of `'\''` patterns. Concretely: every
        // run of consecutive `'` characters in the encoded output must
        // have even length-or-be-part-of-the-escape: the simplest
        // structural check is that splitting on `'\''` and re-joining
        // recovers the original by replacing back.
        let re_decoded = q[1..q.len() - 1].replace("'\\''", "'");
        assert_eq!(re_decoded, input);
    }

    // -- cmd.exe quoting ------------------------------------------------

    #[rstest]
    #[case("")]
    #[case("simple")]
    #[case("with space")]
    #[case("with'quote")]
    #[case("with\"dquote")]
    #[case("with\\backslash")]
    #[case("with$dollar")]
    #[case("with&amp")]
    #[case("with|pipe")]
    #[case("with>redirect")]
    #[case("with^caret")]
    #[case("with%percent")]
    #[case("with!bang")]
    fn cmd_quote_structural_properties(#[case] input: &str) {
        let q = shell_quote_cmd(input);
        // Outer wrapper.
        assert!(q.starts_with('"'), "{q:?} must start with \"");
        assert!(q.ends_with('"'), "{q:?} must end with \"");
        let inner = &q[1..q.len() - 1];

        // Reserved cmd metacharacters that survive double-quoting must
        // each be preceded by a caret in the encoded form. We verify this
        // by reconstructing the input from `inner`: walking the encoded
        // bytes, every `^X` where `X ∈ {%, !, ^}` collapses to `X`, and
        // every other char is itself. The result must equal the original
        // input modulo the CRT-layer `\"`/`\\` encoding (which we decode
        // separately below).
        let mut decoded_cmd = String::new();
        let mut iter = inner.chars().peekable();
        while let Some(c) = iter.next() {
            if c == '^' {
                if let Some(&n) = iter.peek() {
                    if matches!(n, '%' | '!' | '^') {
                        decoded_cmd.push(n);
                        iter.next();
                        continue;
                    }
                }
                decoded_cmd.push(c);
            } else {
                // Any bare `%`, `!`, or `^` here means the encoder failed.
                assert!(!matches!(c, '%' | '!'), "{c:?} not caret-escaped in {inner:?}");
                decoded_cmd.push(c);
            }
        }

        // Now reverse the CRT layer: every `\"` → `"`, every run of `\\`
        // before a `"` or end-of-string halves; bare `\` outside of those
        // contexts is kept. Equivalent operational decode: replace `\"`
        // with `"`, then replace `\\` with `\`.
        let crt_decoded = decoded_cmd.replace("\\\"", "\"");
        // Halve any trailing/embedded doubled backslashes that were doubled
        // because they preceded a `"` or end-of-string. We doubled *all*
        // backslashes that immediately preceded a `"` and any trailing run;
        // for inputs in this test set the simple `\\` → `\` replacement on
        // the remaining string round-trips correctly.
        let final_decoded = crt_decoded.replace("\\\\", "\\");
        assert_eq!(final_decoded, input, "round-trip failed for {input:?}");
    }

    #[test]
    fn cmd_quote_preserves_trailing_backslash_before_closing_quote() {
        // `foo\` inside `"…"` must become `foo\\` so the loader doesn't
        // see the `\` as escaping our closing quote.
        let q = shell_quote_cmd("foo\\");
        assert_eq!(q, "\"foo\\\\\"");
    }

    #[test]
    fn cmd_quote_doubles_backslashes_before_inner_quote() {
        let q = shell_quote_cmd("a\\\"b");
        // `\` then `"` → `\\\"`; final wrapper adds `"` on each end.
        assert_eq!(q, "\"a\\\\\\\"b\"");
    }

    // -- worktree validation --------------------------------------------

    #[test]
    fn validate_worktree_missing_path() {
        let p = PathBuf::from(if cfg!(windows) {
            "C:\\definitely\\does\\not\\exist\\arborist-test"
        } else {
            "/definitely/does/not/exist/arborist-test"
        });
        match validate_worktree(&p) {
            Err(Error::WorktreeMissing(got)) => assert_eq!(got, p),
            other => panic!("expected WorktreeMissing, got {other:?}"),
        }
    }

    #[test]
    fn validate_worktree_file_not_dir() {
        let dir = tempfile::tempdir().expect("tmp");
        let file = dir.path().join("not-a-dir.txt");
        fs::write(&file, b"hi").expect("write");
        match validate_worktree(&file) {
            Err(Error::InvalidPath(msg)) => assert!(msg.contains("not a directory")),
            other => panic!("expected InvalidPath, got {other:?}"),
        }
    }

    #[test]
    fn validate_worktree_happy_path() {
        let dir = tempfile::tempdir().expect("tmp");
        let canonical = dunce::canonicalize(dir.path()).expect("canon");
        let got = validate_worktree(dir.path()).expect("ok");
        assert_eq!(got, canonical);
    }

    #[cfg(unix)]
    #[test]
    fn validate_worktree_resolves_symlink() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().expect("tmp");
        let target = dir.path().join("target");
        fs::create_dir(&target).expect("mkdir");
        let link = dir.path().join("link");
        symlink(&target, &link).expect("symlink");
        let got = validate_worktree(&link).expect("ok");
        assert_eq!(got, dunce::canonicalize(&target).expect("canon"));
    }

    // -- label dedup ----------------------------------------------------

    #[test]
    fn dedupe_label_no_collision() {
        assert_eq!(dedupe_label(&[], "foo"), "foo");
        assert_eq!(dedupe_label(&["bar"], "foo"), "foo");
    }

    #[test]
    fn dedupe_label_one_collision() {
        assert_eq!(dedupe_label(&["foo"], "foo"), "foo 2");
    }

    #[test]
    fn dedupe_label_consecutive_collisions() {
        assert_eq!(dedupe_label(&["foo", "foo 2"], "foo"), "foo 3");
        assert_eq!(dedupe_label(&["foo", "foo 2", "foo 3"], "foo"), "foo 4");
    }

    #[test]
    fn dedupe_label_fills_lowest_gap() {
        // Documented rule: pick the lowest free integer >= 2 — so a gap
        // at "foo 2" gets filled even though "foo 3" exists.
        assert_eq!(dedupe_label(&["foo", "foo 3"], "foo"), "foo 2");
    }

    #[test]
    fn dedupe_label_stress_many_existing() {
        let owned: Vec<String> = (1..100u32).map(|n| format!("foo {n}")).collect();
        let mut refs: Vec<&str> = owned.iter().map(String::as_str).collect();
        refs.push("foo");
        // "foo 1" exists in `owned`; the lowest free n >= 2 is 100 since
        // 2..=99 are all taken.
        assert_eq!(dedupe_label(&refs, "foo"), "foo 100");
    }

    // -- session_temp_dir ----------------------------------------------

    #[test]
    fn session_temp_dir_is_deterministic() {
        let id = fixed_id();
        let p1 = session_temp_dir(&id);
        let p2 = session_temp_dir(&id);
        assert_eq!(p1, p2);
        // Path layout: <os-temp>/arborist/<uuid>/
        assert!(p1.ends_with(id.0.to_string()));
        assert_eq!(p1.parent().and_then(|p| p.file_name()), Some(std::ffi::OsStr::new("arborist")));
    }

    // --- validate_worktree_name (Roadmap §2.3) ---------------------------

    #[rstest]
    #[case("my-feature")]
    #[case("feature/sub")] // slash in middle is OK (becomes branch refs/heads/feature/sub)
    #[case("a")]
    #[case("v1.2.3")]
    fn validate_worktree_name_accepts_valid_inputs(#[case] name: &str) {
        assert_eq!(validate_worktree_name(name).as_deref(), Ok(name));
    }

    #[rstest]
    #[case("", "empty")]
    #[case("@", "'@'")]
    #[case("foo bar", "spaces")]
    #[case("foo..bar", "..")]
    #[case("foo~bar", "'~'")]
    #[case("foo^bar", "'^'")]
    #[case("foo:bar", "':'")]
    #[case("foo?bar", "'?'")]
    #[case("foo*bar", "'*'")]
    #[case("foo[bar", "'['")]
    #[case("foo\\bar", "'\\'")]
    #[case(".hidden", "start with '.'")]
    #[case("/abs", "start with '.' or '/'")]
    #[case("trailing.", "end with '.'")]
    #[case("trailing/", "end with '.' or '/'")]
    #[case("branch.lock", ".lock")]
    #[case("-bad", "'-'")]
    #[case("foo@{bar", "'@{'")]
    #[case("foo//bar", "'//'")]
    #[case("foo\tbar", "control characters")]
    #[case("foo\nbar", "control characters")]
    #[case("foo\x7fbar", "control characters")]
    #[case("feature/.hidden", "start with '.'")]
    #[case("feature/foo.lock/bar", ".lock")]
    fn validate_worktree_name_rejects_invalid_inputs(#[case] name: &str, #[case] reason_substring: &str) {
        let err = validate_worktree_name(name).expect_err("should reject");
        assert!(err.contains(reason_substring), "error {err:?} did not mention {reason_substring:?}",);
    }

    #[test]
    fn validate_worktree_name_rejects_overlong_names() {
        let long = "a".repeat(256);
        assert!(validate_worktree_name(&long).is_err());
        let max = "a".repeat(255);
        assert!(validate_worktree_name(&max).is_ok());
    }

    // -- env_for_tool --------------------------------------------------------

    #[test]
    fn env_for_tool_claude_is_empty() {
        assert!(env_for_tool(Tool::Claude, &fixed_id()).is_empty());
    }

    #[test]
    fn env_for_tool_copilot_returns_otel_keys() {
        let id = fixed_id();
        let env = env_for_tool(Tool::Copilot, &id);
        let map: std::collections::HashMap<String, std::ffi::OsString> = env.iter().cloned().collect();

        // Path is deterministic and matches the single-source-of-truth helper.
        let expected = copilot_otel_path(&id).into_os_string();
        assert_eq!(map.get("COPILOT_OTEL_FILE_EXPORTER_PATH"), Some(&expected),);
        assert_eq!(map.get("COPILOT_OTEL_ENABLED"), Some(&std::ffi::OsString::from("true")));
        // Standard OTel SDK env var; literal "1000" (ms) tightens batch
        // flush from the 5s default to ~1Hz.
        assert_eq!(map.get("OTEL_BSP_SCHEDULE_DELAY"), Some(&std::ffi::OsString::from("1000")),);
        assert_eq!(env.len(), 3, "exactly three env vars for Copilot");
    }

    #[test]
    fn env_for_tool_copilot_path_changes_with_session_id() {
        let a = SessionId(Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap());
        let b = SessionId(Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap());
        let path_a: std::collections::HashMap<_, _> = env_for_tool(Tool::Copilot, &a).into_iter().collect();
        let path_b: std::collections::HashMap<_, _> = env_for_tool(Tool::Copilot, &b).into_iter().collect();
        assert_ne!(
            path_a.get("COPILOT_OTEL_FILE_EXPORTER_PATH"),
            path_b.get("COPILOT_OTEL_FILE_EXPORTER_PATH"),
        );
    }
}
