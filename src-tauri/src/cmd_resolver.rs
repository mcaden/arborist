//! Command-string → executable-path resolution.
//!
//! Used by the icon extractor to find the **right** executable to
//! query for an icon, given just the user-typed command string. The
//! captured PID is unreliable for icon purposes:
//!
//! - Terminal sub-sessions on Windows are wrapped in `cmd /c <cmd>`,
//!   so the PID belongs to `cmd.exe` — the user typed `pwsh` and
//!   expects pwsh's icon.
//! - Application sub-sessions whose launcher is a script shim
//!   (`code.cmd`, `gh.cmd`, npm-installed CLI shims) all show
//!   `cmd.exe`'s generic shell icon if you ask the OS for the script
//!   file's icon directly.
//!
//! The resolution is intentionally a pure function of the command
//! string + cwd + PATH/PATHEXT. No subprocess-spawning, no
//! window-enumeration. Failures return `None` and the frontend falls
//! back to its emoji glyph.
//!
//! ## Resolution pipeline
//!
//! 1. `parse_program(cmd)` — quote-aware first token, skipping
//!    `env`/`KEY=value` shell prefixes.
//! 2. `resolve_executable(program, cwd)` — absolute / cwd-relative /
//!    `PATH` lookup, applying Windows `PATHEXT` for bare names.
//! 3. `unwrap_script_wrapper(path)` — if the resolved file is a
//!    `.cmd`/`.bat`/`.ps1`/`.sh` script, peek at its contents to find
//!    the first existing executable referenced inside (e.g.
//!    `code.cmd` → `..\Code.exe`).
//!
//! [`resolve_command_executable`] composes all three and returns the
//! best path it could find, or `None`.

use std::path::{Path, PathBuf};

/// End-to-end resolution: `command` string → absolute executable path
/// suitable for icon extraction. `cwd` is used for relative-path
/// resolution. Returns `None` if every step in the pipeline fails.
#[must_use]
pub fn resolve_command_executable(command: &str, cwd: &Path) -> Option<PathBuf> {
    resolve_command_executable_detailed(command, cwd).map(|(p, _)| p)
}

/// Like [`resolve_command_executable`] but also reports whether the
/// returned path was reached via [`unwrap_script_wrapper`]. Callers
/// that branch on "did we follow a script shim" (e.g.
/// [`resolve_command_icon_path`], which only suppresses interpreter
/// icons when the user *didn't* type the interpreter directly) use
/// this richer return.
#[must_use]
pub fn resolve_command_executable_detailed(command: &str, cwd: &Path) -> Option<(PathBuf, bool)> {
    let program = parse_program(command)?;
    let resolved = resolve_executable(&program, cwd)?;
    if let Some(unwrapped) = unwrap_script_wrapper(&resolved) {
        return Some((unwrapped, true));
    }
    Some((resolved, false))
}

/// Parse the leading program token from a shell-style command line.
/// Honours `"…"` and `'…'` quoting (not backslash escapes — those
/// are path separators on Windows). Skips leading shell-style env
/// prefixes (`env`, `KEY=value`) so `env FOO=1 pwsh` returns
/// `Some("pwsh")`.
#[must_use]
pub fn parse_program(command: &str) -> Option<String> {
    for t in ShellTokens::new(command) {
        if t == "env" {
            continue;
        }
        if !t.starts_with('/')
            && !t.starts_with('\\')
            && !t.contains(['\\', '/'])
            && t.contains('=')
        {
            // KEY=value env prefix — skip.
            continue;
        }
        return Some(t);
    }
    None
}

/// Resolve `program` to an absolute path:
/// 1. If it's already absolute, return it (with Windows `PATHEXT`
///    completion if no extension).
/// 2. If it contains a path separator, resolve it relative to `cwd`.
/// 3. Otherwise look it up in `PATH` (Windows: also tries each
///    `PATHEXT` suffix).
#[must_use]
pub fn resolve_executable(program: &str, cwd: &Path) -> Option<PathBuf> {
    let p = Path::new(program);
    if p.is_absolute() {
        return existing_with_pathext(p);
    }
    if program.contains('/') || program.contains('\\') {
        let joined = cwd.join(program);
        return existing_with_pathext(&joined);
    }
    search_path(program)
}

fn existing_with_pathext(path: &Path) -> Option<PathBuf> {
    if path.is_file() {
        return Some(path.to_path_buf());
    }
    if cfg!(target_os = "windows") {
        // Re-try with each PATHEXT suffix only if the path has no
        // extension — `code` → `code.cmd`/`code.exe`, but
        // `Code.exe` → `Code.exe.cmd` would be nonsense.
        if path.extension().is_none() {
            for ext in pathext_entries() {
                let with_ext: PathBuf = format!("{}{}", path.display(), ext).into();
                if with_ext.is_file() {
                    return Some(with_ext);
                }
            }
        }
    }
    None
}

fn search_path(program: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(program);
        if let Some(found) = existing_with_pathext(&candidate) {
            return Some(found);
        }
    }
    None
}

fn pathext_entries() -> Vec<String> {
    std::env::var("PATHEXT")
        .ok()
        .map(|s| {
            s.split(';')
                .filter(|e| !e.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_else(|| vec![".COM".into(), ".EXE".into(), ".BAT".into(), ".CMD".into()])
}

/// True for filenames that look like shell-script shims (rather than
/// real binaries). When the OS is asked for the icon of one of these,
/// it returns a generic "script" icon — useless for sidebar
/// branding.
#[must_use]
pub fn is_script_wrapper(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|s| s.to_str()) else {
        return false;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "cmd" | "bat" | "ps1" | "sh"
    )
}

/// If `path` is a script wrapper, peek inside (read the first ~8KB)
/// and look for an executable reference. Returns the first one that
/// resolves to an existing file.
///
/// The heuristic is intentionally simple: tokenise the script, look
/// for tokens ending in `.exe` (Windows) or shebang interpreters
/// (Unix), expand `%~dp0`-style references relative to the script's
/// directory, and return the first existing match.
///
/// On Unix, also handles `#!/usr/bin/env <prog>` shebangs by re-running
/// `resolve_executable` against the named program.
#[must_use]
pub fn unwrap_script_wrapper(path: &Path) -> Option<PathBuf> {
    if !is_script_wrapper(path) {
        return None;
    }
    let script_dir = path.parent()?;
    let content = read_head(path, 8 * 1024)?;

    // Unix shebang fast path.
    if let Some(first_line) = content.lines().next() {
        if let Some(rest) = first_line.strip_prefix("#!") {
            // Strip optional `/usr/bin/env ` indirection.
            let trimmed = rest.trim();
            let after_env = trimmed.strip_prefix("/usr/bin/env ").unwrap_or(trimmed);
            if let Some(prog) = parse_program(after_env) {
                if let Some(resolved) = resolve_executable(&prog, script_dir) {
                    if resolved != path {
                        return Some(resolved);
                    }
                }
            }
        }
    }

    // Look for `.exe` references in the body. Scan all whitespace-
    // and quote-delimited tokens.
    for token in script_tokens(&content) {
        let lower = token.to_ascii_lowercase();
        if !lower.ends_with(".exe") {
            continue;
        }
        let expanded = expand_script_refs(&token, script_dir);
        if expanded.is_file() {
            // Don't return the wrapper itself if it self-references.
            if same_canonical(&expanded, path) {
                continue;
            }
            return Some(expanded);
        }
    }
    None
}

fn read_head(path: &Path, max_bytes: usize) -> Option<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; max_bytes];
    let n = f.read(&mut buf).ok()?;
    buf.truncate(n);
    Some(String::from_utf8_lossy(&buf).into_owned())
}

fn same_canonical(a: &Path, b: &Path) -> bool {
    let ca = a.canonicalize().ok();
    let cb = b.canonicalize().ok();
    matches!((ca, cb), (Some(x), Some(y)) if x == y)
}

/// Expand cmd.exe-style `%~dp0` (script directory) references in a
/// path token. Other `%VAR%` substitutions are left intact and
/// will simply fail the `is_file()` check downstream.
fn expand_script_refs(token: &str, script_dir: &Path) -> PathBuf {
    let stripped = token.trim_matches(|c| c == '"' || c == '\'');
    let with_dp0 = stripped.replace("%~dp0", &format!("{}\\", script_dir.display()));
    let cleaned = with_dp0.replace("%~dp0/", &format!("{}/", script_dir.display()));
    let p = Path::new(&cleaned);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        script_dir.join(p)
    }
}

/// Tokenise script content into whitespace- and quote-delimited
/// fragments. Less strict than `ShellTokens` because we're scavenging,
/// not executing.
fn script_tokens(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for ch in content.chars() {
        match (quote, ch) {
            (Some(q), c) if q == c => {
                quote = None;
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            (Some(_), c) => cur.push(c),
            (None, '"') | (None, '\'') => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                quote = Some(ch);
            }
            (None, c) if c.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            (None, c) => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Quote-aware token iterator: splits on whitespace but keeps
/// `"…"` and `'…'` runs together. Just enough for parsing the leading
/// program of a user-supplied command line — not a full POSIX
/// parser. Backslash escapes are not interpreted (they're path
/// separators on Windows).
pub struct ShellTokens<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> ShellTokens<'a> {
    #[must_use]
    pub fn new(input: &'a str) -> Self {
        Self { input, pos: 0 }
    }
}

impl Iterator for ShellTokens<'_> {
    type Item = String;

    fn next(&mut self) -> Option<String> {
        let bytes = self.input.as_bytes();
        while self.pos < bytes.len() && bytes[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
        if self.pos >= bytes.len() {
            return None;
        }
        let mut out = String::new();
        let mut quote: Option<u8> = None;
        while self.pos < bytes.len() {
            let b = bytes[self.pos];
            match quote {
                Some(q) if b == q => {
                    quote = None;
                    self.pos += 1;
                }
                Some(_) => {
                    out.push(b as char);
                    self.pos += 1;
                }
                None if b == b'"' || b == b'\'' => {
                    quote = Some(b);
                    self.pos += 1;
                }
                None if b.is_ascii_whitespace() => break,
                None => {
                    out.push(b as char);
                    self.pos += 1;
                }
            }
        }
        Some(out)
    }
}

// ---------------------------------------------------------------------------
// Icon-oriented helpers
// ---------------------------------------------------------------------------

/// Like [`resolve_command_executable`] but tuned for icon extraction:
///
/// 1. Walks `bin/<stem>.exe` launchers up to the parent dir's main
///    application exe (the VS Code case — `bin/code.exe` is a thin
///    CLI launcher with no embedded icon, while `../Code.exe` carries
///    the branded icon resource).
/// 2. Suppresses results that land on a generic language interpreter
///    wrapper (`node.exe`, `python.exe`, …) **only when the resolution
///    went through a script shim** ([`unwrap_script_wrapper`] followed
///    it). That's the npm/pip-installed-CLI case where the wrapper
///    hands us back the interpreter and its icon is useless branding
///    for the actual tool.
///
/// Direct user invocations (`pwsh`, `node`, `cmd`, `bash`, …) are
/// **not** suppressed — the user explicitly named that runtime as
/// their custom-process command, so showing its real icon is exactly
/// what they want.
///
/// Returns `None` in three cases:
/// 1. [`resolve_command_executable_detailed`] returned `None`.
/// 2. The path was reached via script unwrap **and** the resolved
///    path's basename matches a known interpreter (see
///    [`is_interpreter_basename`]).
/// 3. The basename is missing entirely (defensive).
#[must_use]
pub fn resolve_command_icon_path(command: &str, cwd: &Path) -> Option<PathBuf> {
    let (resolved, was_unwrapped) = resolve_command_executable_detailed(command, cwd)?;
    // Prefer a sibling main-app exe in the parent directory if the
    // resolved path is a `bin/<stem>.exe` launcher. The launcher
    // typically carries no icon resource — its parent does.
    let candidate = unwrap_bin_launcher_for_icon(&resolved).unwrap_or(resolved);
    let name = candidate
        .file_name()?
        .to_string_lossy()
        .to_ascii_lowercase();
    if was_unwrapped && is_interpreter_basename(&name) {
        return None;
    }
    Some(candidate)
}

/// If `path` looks like `<root>/bin/<stem>.exe` (a CLI launcher under
/// a `bin/` subdirectory), look for a sibling `<root>/<stem>.exe` and
/// return it. This is the "VS Code launcher" pattern: `bin/code.exe`
/// is a thin shim that exec's the main `..\Code.exe` (which has the
/// branded icon resource).
///
/// Match rules:
/// * Parent directory must be named `bin` (case-insensitive). We
///   intentionally do not match other generic dir names (`cli`,
///   `launchers`) until we have evidence those layouts exist in the
///   wild — matching too eagerly risks false positives that pick up
///   the wrong binary's icon.
/// * The candidate exe in `<root>` must share the same file stem
///   (case-insensitive) as the launcher. Capitalisation may differ
///   (`bin/code.exe` ↔ `Code.exe`).
/// * The candidate must not canonicalise to the launcher itself
///   (defensive — avoid loops when `bin/code.exe` happens to also
///   exist as `<root>/code.exe`).
///
/// Returns `None` when the heuristic does not apply or the parent dir
/// is unreadable / contains no matching exe.
#[must_use]
fn unwrap_bin_launcher_for_icon(path: &Path) -> Option<PathBuf> {
    let bin_dir = path.parent()?;
    let bin_name = bin_dir.file_name().and_then(|s| s.to_str())?;
    if !bin_name.eq_ignore_ascii_case("bin") {
        return None;
    }
    let app_dir = bin_dir.parent()?;
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())?
        .to_ascii_lowercase();
    let launcher_canonical = path.canonicalize().ok();

    for entry in std::fs::read_dir(app_dir).ok()? {
        let entry = entry.ok()?;
        let entry_path = entry.path();
        let is_exe = entry_path
            .extension()
            .and_then(|s| s.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("exe"));
        if !is_exe {
            continue;
        }
        let entry_stem = entry_path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_ascii_lowercase);
        if entry_stem.as_deref() != Some(&stem) {
            continue;
        }
        // Don't return the launcher itself.
        if let (Some(a), Ok(b)) = (launcher_canonical.as_ref(), entry_path.canonicalize()) {
            if a == &b {
                continue;
            }
        }
        return Some(entry_path);
    }
    None
}

/// True if `basename_lc` is a known generic language-runtime/interpreter
/// executable name (lower-cased, with extension as on disk). Matches
/// the user-visible launcher binaries that appear *inside* npm/pip
/// CLI shims after [`unwrap_script_wrapper`] follows them.
///
/// Kept conservative on purpose — being wrong here means we either
/// (a) miss a useful icon (false positive: harmless, falls back to
/// SVG glyph), or (b) show the runtime's icon for a third-party CLI
/// (false negative: ugly but not broken). When in doubt, *include*
/// the name here.
#[must_use]
pub fn is_interpreter_basename(basename_lc: &str) -> bool {
    matches!(
        basename_lc,
        "node"
            | "node.exe"
            | "deno"
            | "deno.exe"
            | "bun"
            | "bun.exe"
            | "python"
            | "python.exe"
            | "python3"
            | "python3.exe"
            | "py"
            | "py.exe"
            | "ruby"
            | "ruby.exe"
            | "perl"
            | "perl.exe"
            | "lua"
            | "lua.exe"
            | "java"
            | "java.exe"
            | "javaw"
            | "javaw.exe"
            // Shell hosts — same logic: showing cmd's / pwsh's icon
            // for `pwsh -c whatever` is worse than falling back to
            // the SVG. PowerShell ships under two basenames:
            // `pwsh` (PowerShell 7+, cross-platform) and
            // `powershell` (Windows PowerShell 5.x, legacy).
            | "cmd"
            | "cmd.exe"
            | "pwsh"
            | "pwsh.exe"
            | "powershell"
            | "powershell.exe"
            | "sh"
            | "bash"
            | "zsh"
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn parse_program_handles_quoted_paths() {
        assert_eq!(parse_program("pwsh"), Some("pwsh".into()));
        assert_eq!(parse_program("  pwsh   -nop"), Some("pwsh".into()));
        assert_eq!(
            parse_program("\"C:\\Program Files\\PowerShell\\7\\pwsh.exe\" -nop"),
            Some("C:\\Program Files\\PowerShell\\7\\pwsh.exe".into())
        );
        assert_eq!(
            parse_program("'/usr/local/bin/code'"),
            Some("/usr/local/bin/code".into())
        );
    }

    #[test]
    fn parse_program_skips_env_prefix() {
        assert_eq!(parse_program("env FOO=1 BAR=2 pwsh"), Some("pwsh".into()));
        assert_eq!(parse_program("FOO=1 BAR=2 pwsh -nop"), Some("pwsh".into()));
        assert_eq!(parse_program(""), None);
        assert_eq!(parse_program("   "), None);
    }

    #[test]
    fn parse_program_does_not_mistake_path_for_env() {
        // `/usr/bin/foo` contains a slash but isn't an env assignment.
        assert_eq!(parse_program("/usr/bin/foo"), Some("/usr/bin/foo".into()));
        // `C:\bin\foo` likewise.
        assert_eq!(parse_program("C:\\bin\\foo"), Some("C:\\bin\\foo".into()));
    }

    #[test]
    fn is_script_wrapper_recognises_common_extensions() {
        assert!(is_script_wrapper(Path::new("code.cmd")));
        assert!(is_script_wrapper(Path::new("foo.bat")));
        assert!(is_script_wrapper(Path::new("foo.ps1")));
        assert!(is_script_wrapper(Path::new("foo.sh")));
        assert!(is_script_wrapper(Path::new("FOO.CMD")));
        assert!(!is_script_wrapper(Path::new("code.exe")));
        assert!(!is_script_wrapper(Path::new("pwsh")));
    }

    #[test]
    fn resolve_executable_absolute_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("foo.exe");
        std::fs::write(&exe, b"binary").unwrap();
        assert_eq!(
            resolve_executable(exe.to_str().unwrap(), tmp.path()),
            Some(exe)
        );
    }

    #[test]
    fn resolve_executable_relative_with_separator() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("bin")).unwrap();
        let exe = tmp.path().join("bin").join("foo.exe");
        std::fs::write(&exe, b"binary").unwrap();
        assert_eq!(resolve_executable("bin/foo.exe", tmp.path()), Some(exe));
    }

    #[test]
    fn unwrap_script_wrapper_finds_referenced_exe() {
        let tmp = tempfile::tempdir().unwrap();
        // Layout mirrors VS Code: bin/code.cmd, ../Code.exe.
        let bin_dir = tmp.path().join("bin");
        std::fs::create_dir(&bin_dir).unwrap();
        let real_exe = tmp.path().join("Code.exe");
        std::fs::write(&real_exe, b"binary").unwrap();
        let cmd = bin_dir.join("code.cmd");
        let mut f = std::fs::File::create(&cmd).unwrap();
        writeln!(
            f,
            "@echo off\r\nSETLOCAL\r\n\"%~dp0..\\Code.exe\" --foo %*\r\n"
        )
        .unwrap();

        let resolved = unwrap_script_wrapper(&cmd).expect("should resolve to Code.exe");
        assert_eq!(
            resolved.canonicalize().unwrap(),
            real_exe.canonicalize().unwrap(),
            "expected unwrap to point at the real exe"
        );
    }

    #[test]
    fn unwrap_script_wrapper_returns_none_for_real_exe() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("foo.exe");
        std::fs::write(&exe, b"binary").unwrap();
        assert!(unwrap_script_wrapper(&exe).is_none());
    }

    #[test]
    fn unwrap_script_wrapper_returns_none_when_no_target_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let cmd = tmp.path().join("foo.cmd");
        std::fs::write(&cmd, b"@echo off\r\n\"%~dp0nonexistent.exe\" %*\r\n").unwrap();
        assert!(unwrap_script_wrapper(&cmd).is_none());
    }

    #[test]
    fn unwrap_script_wrapper_handles_unix_shebang() {
        let tmp = tempfile::tempdir().unwrap();
        // `target` is an existing executable that the shebang names.
        // We just need a valid file.
        let target = tmp.path().join("python3");
        std::fs::write(&target, b"binary").unwrap();
        let script = tmp.path().join("foo.sh");
        std::fs::write(
            &script,
            format!("#!/usr/bin/env {}\necho hi\n", target.display()),
        )
        .unwrap();
        assert_eq!(
            unwrap_script_wrapper(&script)
                .unwrap()
                .canonicalize()
                .unwrap(),
            target.canonicalize().unwrap()
        );
    }

    #[test]
    fn shell_tokens_handles_quotes_and_whitespace() {
        let toks: Vec<String> = ShellTokens::new("a 'b c' \"d e\" f").collect();
        assert_eq!(toks, vec!["a", "b c", "d e", "f"]);
    }

    #[test]
    fn is_interpreter_basename_covers_known_runtimes() {
        for name in [
            "node.exe",
            "python.exe",
            "python3",
            "ruby",
            "bun.exe",
            "deno",
            "java",
            "cmd.exe",
            "bash",
            // PowerShell hosts: showing pwsh's terminal icon for a
            // user's `pwsh -c …` command would be misleading.
            "pwsh",
            "pwsh.exe",
            "powershell",
            "powershell.exe",
        ] {
            assert!(is_interpreter_basename(name), "expected {name} to match");
        }
        for name in ["code.exe", "explorer.exe", "vim", "claude.exe"] {
            assert!(
                !is_interpreter_basename(name),
                "expected {name} to NOT match"
            );
        }
    }

    #[test]
    fn resolve_command_icon_path_filters_interpreter_targets() {
        let dir = tempfile::tempdir().unwrap();
        // A "node" exe and a wrapper script that points to it.
        let node = dir.path().join("node.exe");
        std::fs::File::create(&node).unwrap();
        let wrapper = dir.path().join("foo.cmd");
        let mut f = std::fs::File::create(&wrapper).unwrap();
        writeln!(f, r#"@"%~dp0node.exe" %*"#).unwrap();
        drop(f);

        // Sanity: the executable resolver finds node.exe via the wrapper.
        let exe = resolve_command_executable(wrapper.to_str().unwrap(), dir.path()).unwrap();
        assert_eq!(
            exe.canonicalize().unwrap(),
            node.canonicalize().unwrap(),
            "precondition: wrapper unwrap should yield node.exe"
        );
        // The icon-path resolver should reject it because it's an interpreter.
        assert!(
            resolve_command_icon_path(wrapper.to_str().unwrap(), dir.path()).is_none(),
            "interpreter targets must be filtered for icon use"
        );
    }

    /// Regression: when the user *directly* names an interpreter as
    /// their custom-process command (e.g. typed `pwsh` in the
    /// Settings UI), the icon resolver must return its real path —
    /// the interpreter blacklist exists for unwrapped script shims,
    /// not direct invocations.
    #[test]
    fn resolve_command_icon_path_allows_direct_interpreter_invocation() {
        let dir = tempfile::tempdir().unwrap();
        // Simulate a directly-resolvable `pwsh.exe` binary in `dir`.
        let pwsh = dir.path().join("pwsh.exe");
        std::fs::File::create(&pwsh).unwrap();

        // Direct path → no script unwrap → must NOT be filtered, even
        // though `pwsh.exe` is in `is_interpreter_basename`.
        let icon = resolve_command_icon_path(pwsh.to_str().unwrap(), dir.path())
            .expect("direct interpreter invocation must yield its real exe path");
        assert_eq!(
            icon.canonicalize().unwrap(),
            pwsh.canonicalize().unwrap(),
            "icon path must point at the directly-invoked interpreter"
        );
    }

    /// Regression: a wrapper that unwraps to a *non-interpreter* (the
    /// VS Code `code.cmd` → `Code.exe` case) must still yield an
    /// icon path. Pairs with [`unwrap_script_wrapper_finds_referenced_exe`]
    /// but exercises the full icon-path filter.
    #[test]
    fn resolve_command_icon_path_passes_unwrapped_non_interpreter_through() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let real_exe = dir.path().join("Code.exe");
        std::fs::write(&real_exe, b"binary").unwrap();
        let wrapper = bin.join("code.cmd");
        let mut f = std::fs::File::create(&wrapper).unwrap();
        writeln!(f, r#"@"%~dp0..\Code.exe" %*"#).unwrap();
        drop(f);

        let icon = resolve_command_icon_path(wrapper.to_str().unwrap(), dir.path())
            .expect("non-interpreter unwrap targets must resolve for icons");
        assert_eq!(
            icon.canonicalize().unwrap(),
            real_exe.canonicalize().unwrap()
        );
    }

    #[test]
    fn resolve_command_executable_detailed_reports_unwrap_flag() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("plain.exe");
        std::fs::File::create(&exe).unwrap();
        let (_p, was_unwrapped) =
            resolve_command_executable_detailed(exe.to_str().unwrap(), dir.path()).unwrap();
        assert!(!was_unwrapped, "direct exe invocations are not unwrapped");

        let wrapper = dir.path().join("shim.cmd");
        let mut f = std::fs::File::create(&wrapper).unwrap();
        writeln!(f, r#"@"%~dp0plain.exe" %*"#).unwrap();
        drop(f);
        let (_p2, was_unwrapped2) =
            resolve_command_executable_detailed(wrapper.to_str().unwrap(), dir.path()).unwrap();
        assert!(was_unwrapped2, "shim invocations report unwrap=true");
    }

    /// Regression: VS Code 1.69+ ships `bin/code.exe` as a thin CLI
    /// launcher with no embedded icon resource; the branded icon
    /// lives on the parent-dir `Code.exe`. The icon resolver must
    /// walk that one level up.
    #[test]
    fn resolve_command_icon_path_walks_up_from_bin_launcher() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let launcher = bin.join("code.exe");
        std::fs::write(&launcher, b"thin launcher").unwrap();
        // Note: capitalisation differs (Code.exe vs code.exe) — the
        // walk-up is case-insensitive on stem comparison.
        let main_exe = dir.path().join("Code.exe");
        std::fs::write(&main_exe, b"branded ui exe").unwrap();

        let icon = resolve_command_icon_path(launcher.to_str().unwrap(), dir.path())
            .expect("bin/launcher should walk up to the parent main exe");
        assert_eq!(
            icon.canonicalize().unwrap(),
            main_exe.canonicalize().unwrap(),
            "icon path must point at the branded parent-dir exe"
        );
    }

    /// Negative: a `bin/foo.exe` whose parent dir contains no
    /// matching exe should resolve to itself (no spurious walk-up).
    #[test]
    fn resolve_command_icon_path_keeps_bin_launcher_when_no_parent_match() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let launcher = bin.join("solo.exe");
        std::fs::write(&launcher, b"solo binary").unwrap();
        // No `solo.exe` in `dir` — must not walk up.
        std::fs::write(dir.path().join("unrelated.exe"), b"x").unwrap();

        let icon = resolve_command_icon_path(launcher.to_str().unwrap(), dir.path()).unwrap();
        assert_eq!(
            icon.canonicalize().unwrap(),
            launcher.canonicalize().unwrap(),
            "icon path must stay on the launcher when no parent twin exists"
        );
    }

    /// Negative: do not match arbitrary subdir names — only literal
    /// `bin`. Matching `cli/code.exe` could regress fine cases (e.g.
    /// the `cli` exe really is the user-facing binary).
    #[test]
    fn resolve_command_icon_path_does_not_walk_up_from_non_bin_subdir() {
        let dir = tempfile::tempdir().unwrap();
        let cli = dir.path().join("cli");
        std::fs::create_dir(&cli).unwrap();
        let launcher = cli.join("code.exe");
        std::fs::write(&launcher, b"x").unwrap();
        let parent_exe = dir.path().join("Code.exe");
        std::fs::write(&parent_exe, b"y").unwrap();

        let icon = resolve_command_icon_path(launcher.to_str().unwrap(), dir.path()).unwrap();
        assert_eq!(
            icon.canonicalize().unwrap(),
            launcher.canonicalize().unwrap(),
            "non-`bin` subdir parents must not trigger walk-up"
        );
    }
}
