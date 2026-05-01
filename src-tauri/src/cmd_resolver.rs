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
    let program = parse_program(command)?;
    let resolved = resolve_executable(&program, cwd)?;
    if let Some(unwrapped) = unwrap_script_wrapper(&resolved) {
        return Some(unwrapped);
    }
    Some(resolved)
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

/// Like [`resolve_command_executable`] but suppresses results that
/// land on a generic language interpreter wrapper (`node.exe`,
/// `python.exe`, …). Those wrappers are usually the *script host*
/// for an npm/pip-installed CLI shim, and their icons are useless
/// branding for the actual tool — the caller (frontend `ToolIcon`)
/// should fall back to its bundled SVG glyph instead.
///
/// Returns `None` in three cases:
/// 1. [`resolve_command_executable`] returned `None`.
/// 2. The resolved path's basename matches a known interpreter
///    (see [`is_interpreter_basename`]).
/// 3. The basename is missing entirely (defensive).
#[must_use]
pub fn resolve_command_icon_path(command: &str, cwd: &Path) -> Option<PathBuf> {
    let resolved = resolve_command_executable(command, cwd)?;
    let name = resolved.file_name()?.to_string_lossy().to_ascii_lowercase();
    if is_interpreter_basename(&name) {
        return None;
    }
    Some(resolved)
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
            // Shell hosts — same logic: showing cmd's icon for `cmd /c whatever`
            // is worse than falling back to the SVG.
            | "cmd"
            | "cmd.exe"
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
        ] {
            assert!(is_interpreter_basename(name), "expected {name} to match");
        }
        for name in ["code.exe", "pwsh.exe", "explorer.exe", "vim", "claude.exe"] {
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
}
