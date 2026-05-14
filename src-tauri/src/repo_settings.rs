//! Repository-stored Arborist settings (issue #71).
//!
//! Each workspace root may contain a `.arborist/` directory:
//!
//! ```text
//! <workspace_root>/
//!   .arborist/
//!     .gitignore        # auto-generated, contains ".worktrees/"
//!     settings.json     # optional, source-controlled
//!     .worktrees/<name> # linked worktrees (git-ignored by the .gitignore above)
//! ```
//!
//! The `settings.json` file lets a team commit shared Arborist defaults to
//! source control. Fields present here override the user-level [`AppConfig`]
//! per field; absent fields fall back to the user's defaults. The set of
//! overridable fields is intentionally narrow — only those whose meaning is
//! repo-specific (AI launch overrides and worktree-prep commands) — so the
//! user's machine-level config (paths, plugin enable flags, custom processes,
//! tab order, …) is never silently shadowed by a checked-in file.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::types::{AppConfig, PluginSettingValue, AI_LAUNCH_COMMAND_SETTING};

/// Subdirectory name under a workspace root that hosts repo-stored Arborist
/// state. Source-controlled (except for the entries listed in `.gitignore`).
pub const ARBORIST_DIR: &str = ".arborist";

/// Path *relative to the workspace root* where Arborist places linked
/// worktrees (`<workspaceRoot>/.arborist/.worktrees/<name>`).
pub const WORKTREES_REL: &str = ".arborist/.worktrees";

/// File inside `.arborist/` whose presence stores team-shared defaults.
pub const SETTINGS_FILENAME: &str = "settings.json";

/// Body of the auto-generated `.arborist/.gitignore`. Keeps the linked
/// worktrees directory out of source control while still tracking
/// `settings.json` itself.
const GITIGNORE_BODY: &str = ".worktrees/\n";

/// Maximum bytes we will read from a `.arborist/settings.json`. Defence in
/// depth — a hand-edited config should be tiny.
const MAX_SETTINGS_BYTES: u64 = 64 * 1024;

/// Subset of [`AppConfig`] that a repository may override via
/// `.arborist/settings.json`. Every field is optional; missing fields fall
/// through to the user-level config.
///
/// Wire format is camelCase (`pluginSettings`, `worktreePrepCommands`) to match the rest of the on-disk JSON shape.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepoSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_launch_commands: Option<RepoAiLaunchCommands>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_settings: Option<RepoPluginSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_prep_commands: Option<Vec<String>>,
}

/// Repo-level subset of [`AiLaunchCommands`] — only the user-editable command
/// strings; icon caches are always machine-local and never read from
/// `settings.json`.
#[derive(Serialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepoAiLaunchCommands {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub commands: BTreeMap<String, String>,
}

/// Repo-level subset of plugin settings. Repo overlays may set AI launch command strings, but do not control user-level enable/disable choices.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepoPluginSettings {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub ai: BTreeMap<String, RepoPluginAiSettings>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepoPluginAiSettings {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub settings: BTreeMap<String, PluginSettingValue>,
}

impl<'de> Deserialize<'de> for RepoAiLaunchCommands {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize, Default)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct Wire {
            #[serde(default)]
            commands: BTreeMap<String, String>,
            // Legacy fixed fields kept for backwards compatibility.
            #[serde(default)]
            claude: Option<String>,
            #[serde(default)]
            copilot: Option<String>,
        }

        let wire = Wire::deserialize(deserializer)?;
        let mut commands = wire.commands;
        if let Some(v) = wire.claude {
            commands.entry(crate::types::Tool::Claude.as_id().to_owned()).or_insert(v);
        }
        if let Some(v) = wire.copilot {
            commands.entry(crate::types::Tool::Copilot.as_id().to_owned()).or_insert(v);
        }
        Ok(Self { commands })
    }
}

impl RepoSettings {
    /// Best-effort load of `<workspace>/.arborist/settings.json`.
    ///
    /// Returns `Self::default()` (i.e. "no overrides") when:
    ///
    /// - the file does not exist,
    /// - the file is too large,
    /// - the JSON fails to parse, or
    /// - any IO error occurs.
    ///
    /// Parse/IO failures are logged as warnings but never propagated — repo
    /// settings are advisory and must never block session creation.
    #[must_use]
    pub fn load(workspace: &Path) -> Self {
        // Defence in depth: refuse to follow a symlinked `.arborist` or
        // `settings.json`. A repo checkout is potentially untrusted, and a
        // symlink could redirect reads outside the workspace and influence
        // commands via the resulting overlay.
        let arborist = workspace.join(ARBORIST_DIR);
        match fs::symlink_metadata(&arborist) {
            Ok(m) if m.file_type().is_symlink() => {
                warn!(
                    code = "InvalidPath",
                    path = %arborist.display(),
                    "ignoring symlinked .arborist directory; using user-level defaults",
                );
                return Self::default();
            }
            Ok(_) | Err(_) => {}
        }
        let path = arborist.join(SETTINGS_FILENAME);
        let meta = match fs::symlink_metadata(&path) {
            Ok(m) if m.file_type().is_symlink() => {
                warn!(
                    code = "InvalidPath",
                    path = %path.display(),
                    "ignoring symlinked .arborist/settings.json; using user-level defaults",
                );
                return Self::default();
            }
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                warn!(code = "Io", path = %path.display(), error = %e, "could not stat .arborist/settings.json");
                return Self::default();
            }
        };
        if meta.len() > MAX_SETTINGS_BYTES {
            warn!(
                code = "InvalidConfig",
                path = %path.display(),
                size = meta.len(),
                "ignoring oversize .arborist/settings.json (> 64 KiB)",
            );
            return Self::default();
        }
        let raw = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                warn!(code = "Io", path = %path.display(), error = %e, "could not read .arborist/settings.json");
                return Self::default();
            }
        };
        match serde_json::from_str::<Self>(&raw) {
            Ok(parsed) => {
                debug!(path = %path.display(), "loaded .arborist/settings.json");
                parsed
            }
            Err(e) => {
                warn!(
                    code = "InvalidConfig",
                    path = %path.display(),
                    error = %e,
                    "could not parse .arborist/settings.json; ignoring (using user-level defaults)",
                );
                Self::default()
            }
        }
    }

    /// Apply `self` on top of `cfg`. Fields present in `self` overwrite the
    /// corresponding fields on `cfg`; unset fields leave `cfg` untouched.
    ///
    /// `aiLaunchCommands.icon_data_uris[pluginId]` cached icon URIs are preserved on
    /// the user-level `cfg` whenever the repo override does not change the
    /// command string — keeping the icon resolution work-cache valid across repo overlays.
    pub fn apply_to(&self, cfg: &mut AppConfig) {
        if let Some(ai) = &self.ai_launch_commands {
            for (plugin_id, command) in &ai.commands {
                cfg.set_ai_launch_command(plugin_id.clone(), command.clone());
            }
        }
        if let Some(plugin_settings) = &self.plugin_settings {
            for (plugin_id, state) in &plugin_settings.ai {
                if let Some(value) = state.settings.get(AI_LAUNCH_COMMAND_SETTING) {
                    let Some(command) = value.as_str() else {
                        warn!(
                            code = "InvalidConfig",
                            plugin_id,
                            setting = AI_LAUNCH_COMMAND_SETTING,
                            "ignoring repo plugin setting because AI launch command must be a string",
                        );
                        continue;
                    };
                    cfg.set_ai_launch_command(plugin_id.clone(), command.to_owned());
                }
            }
        }
        if let Some(prep) = &self.worktree_prep_commands {
            cfg.worktree_prep_commands = prep.clone();
        }
    }
}

/// Compatibility helper used during `worktree_create`: ensure
/// `<workspace>/.arborist/` exists, and best-effort attempt to keep its
/// `.gitignore` listing `.worktrees/`. Idempotent.
///
/// Returns the absolute path of the materialised `.arborist/` directory.
///
/// **Contract:** the directory itself is required — directory-creation
/// failures (or the symlink/non-directory rejections below) propagate as
/// `Err`. The `.gitignore` maintenance is best-effort: write/read errors on
/// the gitignore are logged and swallowed so a transient filesystem hiccup
/// does not block worktree creation. Callers that need a guaranteed
/// gitignore should check `dir.join(".gitignore")` after this returns.
///
/// Refuses to follow a `.arborist` symlink (defence in depth: a symlinked
/// `.arborist` could redirect worktree creation outside the workspace).
pub fn ensure_arborist_dir(workspace: &Path) -> io::Result<PathBuf> {
    let dir = workspace.join(ARBORIST_DIR);
    if let Ok(meta) = fs::symlink_metadata(&dir) {
        if meta.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is a symlink; refusing to use it as the Arborist directory", dir.display()),
            ));
        }
        if !meta.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} exists but is not a directory", dir.display()),
            ));
        }
    } else {
        fs::create_dir_all(&dir)?;
    }
    let gitignore = dir.join(".gitignore");
    match fs::read_to_string(&gitignore) {
        Ok(existing) => {
            // File exists: append `.worktrees/` if (and only if) it is not
            // already listed as its own line. We treat the line as present
            // when any non-comment, non-negation line equals `.worktrees/`
            // (with or without a trailing slash). Anything fancier — pattern
            // negation, glob trickery — is left to the user; we just want to
            // guarantee the default ignore is not silently missing.
            let already_listed = existing.lines().any(|raw| {
                let line = raw.trim();
                if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
                    return false;
                }
                line.trim_end_matches('/') == ".worktrees"
            });
            if !already_listed {
                let mut updated = existing;
                if !updated.is_empty() && !updated.ends_with('\n') {
                    updated.push('\n');
                }
                updated.push_str(GITIGNORE_BODY);
                if let Err(e) = fs::write(&gitignore, updated) {
                    warn!(code = "Io", path = %gitignore.display(), error = %e, "could not append .worktrees/ to .arborist/.gitignore");
                }
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            // Best-effort: a write failure here is non-fatal for worktree creation.
            if let Err(e) = fs::write(&gitignore, GITIGNORE_BODY) {
                warn!(code = "Io", path = %gitignore.display(), error = %e, "could not write .arborist/.gitignore");
            }
        }
        Err(e) => {
            warn!(code = "Io", path = %gitignore.display(), error = %e, "could not read .arborist/.gitignore; leaving as-is");
        }
    }
    Ok(dir)
}

/// Convenience: load the user-level config and overlay repo settings if a
/// `workspace_root` is set. When `workspace_root` is `None`, returns `cfg`
/// unchanged.
#[must_use]
pub fn apply_repo_overlay(mut cfg: AppConfig) -> AppConfig {
    if let Some(workspace) = cfg.workspace_root.clone() {
        let repo = RepoSettings::load(&workspace);
        repo.apply_to(&mut cfg);
    }
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write_settings(workspace: &Path, body: &str) {
        let dir = workspace.join(ARBORIST_DIR);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(SETTINGS_FILENAME), body).unwrap();
    }

    #[test]
    fn missing_directory_yields_default() {
        let dir = tempdir().unwrap();
        let out = RepoSettings::load(dir.path());
        assert_eq!(out, RepoSettings::default());
    }

    #[test]
    fn missing_settings_file_yields_default() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join(ARBORIST_DIR)).unwrap();
        let out = RepoSettings::load(dir.path());
        assert_eq!(out, RepoSettings::default());
    }

    #[test]
    fn malformed_json_logs_and_returns_default() {
        let dir = tempdir().unwrap();
        write_settings(dir.path(), "{ this is not json");
        let out = RepoSettings::load(dir.path());
        assert_eq!(out, RepoSettings::default());
    }

    #[test]
    fn unknown_fields_are_rejected() {
        // deny_unknown_fields makes typos visible (returns default, with a warn log).
        let dir = tempdir().unwrap();
        write_settings(dir.path(), r#"{ "worktreepreepcommands": ["oops"] }"#);
        let out = RepoSettings::load(dir.path());
        assert_eq!(out, RepoSettings::default());
    }

    #[test]
    fn parses_full_settings() {
        let dir = tempdir().unwrap();
        write_settings(
            dir.path(),
            r#"{
                "aiLaunchCommands": { "commands": { "claude": "npx claude --model sonnet" } },
                "worktreePrepCommands": ["pnpm install", "pnpm run build"]
            }"#,
        );
        let out = RepoSettings::load(dir.path());
        assert_eq!(
            out.worktree_prep_commands.as_deref(),
            Some(&["pnpm install".to_owned(), "pnpm run build".to_owned()][..])
        );
        assert_eq!(
            out.ai_launch_commands.as_ref().unwrap().commands.get("claude").map(String::as_str),
            Some("npx claude --model sonnet")
        );
        assert!(!out.ai_launch_commands.as_ref().unwrap().commands.contains_key("copilot"));
    }

    #[test]
    fn apply_to_overrides_only_set_fields() {
        let mut cfg = AppConfig {
            worktree_prep_commands: vec!["user-cmd".to_owned()],
            ..AppConfig::default()
        };
        cfg.set_ai_launch_command("claude".to_owned(), "user-claude".to_owned());
        cfg.ai_launch_commands
            .icon_data_uris
            .insert("claude".to_owned(), Some("data:image/png;base64,USER".to_owned()));
        cfg.set_ai_launch_command("copilot".to_owned(), "user-copilot".to_owned());

        let repo = RepoSettings {
            ai_launch_commands: Some(RepoAiLaunchCommands {
                commands: BTreeMap::from([("claude".to_owned(), "repo-claude".to_owned())]),
            }),
            plugin_settings: None,
            worktree_prep_commands: Some(vec!["repo-cmd".to_owned()]),
        };
        repo.apply_to(&mut cfg);

        assert_eq!(cfg.worktree_prep_commands, vec!["repo-cmd".to_owned()]);
        assert_eq!(cfg.ai_launch_command_for_id("claude"), "repo-claude");
        // Icon cache was invalidated because the command changed.
        assert!(!cfg.ai_launch_commands.icon_data_uris.contains_key("claude"));
        // Copilot left alone (override didn't set it).
        assert_eq!(cfg.ai_launch_command_for_id("copilot"), "user-copilot");
    }

    #[test]
    fn apply_to_keeps_icon_cache_when_command_unchanged() {
        let mut cfg = AppConfig::default();
        cfg.set_ai_launch_command("claude".to_owned(), "same".to_owned());
        cfg.ai_launch_commands
            .icon_data_uris
            .insert("claude".to_owned(), Some("data:image/png;base64,KEEP".to_owned()));

        let repo = RepoSettings {
            ai_launch_commands: Some(RepoAiLaunchCommands {
                commands: BTreeMap::from([("claude".to_owned(), "same".to_owned())]),
            }),
            ..RepoSettings::default()
        };
        repo.apply_to(&mut cfg);
        assert_eq!(
            cfg.ai_launch_commands.icon_data_uris.get("claude").and_then(Option::as_deref),
            Some("data:image/png;base64,KEEP")
        );
    }

    #[test]
    fn apply_to_keeps_icon_cache_when_repo_command_is_explicit_default() {
        let mut cfg = AppConfig::default();
        cfg.ai_launch_commands
            .icon_data_uris
            .insert("claude".to_owned(), Some("data:image/png;base64,KEEP".to_owned()));

        let repo = RepoSettings {
            ai_launch_commands: Some(RepoAiLaunchCommands {
                commands: BTreeMap::from([("claude".to_owned(), String::new())]),
            }),
            ..RepoSettings::default()
        };
        repo.apply_to(&mut cfg);
        assert_eq!(
            cfg.ai_launch_commands.icon_data_uris.get("claude").and_then(Option::as_deref),
            Some("data:image/png;base64,KEEP")
        );
    }

    #[test]
    fn apply_to_accepts_plugin_settings_ai_launch_command() {
        let mut cfg = AppConfig::default();
        let repo = RepoSettings {
            plugin_settings: Some(RepoPluginSettings {
                ai: BTreeMap::from([(
                    "claude".to_owned(),
                    RepoPluginAiSettings {
                        settings: BTreeMap::from([(
                            AI_LAUNCH_COMMAND_SETTING.to_owned(),
                            PluginSettingValue::String("repo-plugin-claude".to_owned()),
                        )]),
                    },
                )]),
            }),
            ..RepoSettings::default()
        };

        repo.apply_to(&mut cfg);

        assert_eq!(cfg.ai_launch_command_for_id("claude"), "repo-plugin-claude");
    }

    #[test]
    fn apply_to_ignores_non_string_plugin_settings_ai_launch_command() {
        let mut cfg = AppConfig::default();
        cfg.set_ai_launch_command("claude".to_owned(), "user-claude".to_owned());
        let repo = RepoSettings {
            plugin_settings: Some(RepoPluginSettings {
                ai: BTreeMap::from([(
                    "claude".to_owned(),
                    RepoPluginAiSettings {
                        settings: BTreeMap::from([(AI_LAUNCH_COMMAND_SETTING.to_owned(), PluginSettingValue::Bool(true))]),
                    },
                )]),
            }),
            ..RepoSettings::default()
        };

        repo.apply_to(&mut cfg);

        assert_eq!(cfg.ai_launch_command_for_id("claude"), "user-claude");
    }

    #[test]
    fn ensure_arborist_dir_appends_worktrees_line_when_missing() {
        let dir = tempdir().unwrap();
        let arborist = dir.path().join(ARBORIST_DIR);
        fs::create_dir_all(&arborist).unwrap();
        // Pre-existing .gitignore that lists *something else* but not .worktrees/.
        fs::write(arborist.join(".gitignore"), "build/\nlocal-secrets/\n").unwrap();

        ensure_arborist_dir(dir.path()).unwrap();

        let gitignore = fs::read_to_string(arborist.join(".gitignore")).unwrap();
        assert!(gitignore.contains("build/"), "preserves user entries");
        assert!(gitignore.contains("local-secrets/"), "preserves user entries");
        assert!(gitignore.lines().any(|l| l.trim_end_matches('/') == ".worktrees"), "appends .worktrees/");
    }

    #[test]
    fn ensure_arborist_dir_does_not_duplicate_worktrees_line() {
        let dir = tempdir().unwrap();
        ensure_arborist_dir(dir.path()).unwrap();
        ensure_arborist_dir(dir.path()).unwrap();
        let gitignore = fs::read_to_string(dir.path().join(ARBORIST_DIR).join(".gitignore")).unwrap();
        let count = gitignore.lines().filter(|l| l.trim_end_matches('/') == ".worktrees").count();
        assert_eq!(count, 1, "expected exactly one .worktrees/ entry, got {count}: {gitignore:?}");
    }

    #[test]
    fn ensure_arborist_dir_handles_missing_trailing_newline() {
        let dir = tempdir().unwrap();
        let arborist = dir.path().join(ARBORIST_DIR);
        fs::create_dir_all(&arborist).unwrap();
        fs::write(arborist.join(".gitignore"), "build/").unwrap(); // no trailing newline

        ensure_arborist_dir(dir.path()).unwrap();

        let gitignore = fs::read_to_string(arborist.join(".gitignore")).unwrap();
        assert!(gitignore.contains("build/"));
        assert!(gitignore.lines().any(|l| l.trim_end_matches('/') == ".worktrees"));
    }

    #[test]
    fn load_rejects_symlinked_settings_file() {
        // Symlink/junction creation is privileged on Windows; only assert on Unix.
        #[cfg(unix)]
        {
            let dir = tempdir().unwrap();
            let arborist = dir.path().join(ARBORIST_DIR);
            fs::create_dir_all(&arborist).unwrap();
            // A real settings file outside the workspace, then a symlink pointing at it.
            let outside = dir.path().join("attacker.json");
            fs::write(&outside, r#"{ "worktreePrepCommands": ["pwned"] }"#).unwrap();
            std::os::unix::fs::symlink(&outside, arborist.join(SETTINGS_FILENAME)).unwrap();

            let out = RepoSettings::load(dir.path());
            assert_eq!(out, RepoSettings::default(), "symlinked settings.json must not be honoured");
        }
    }

    #[test]
    fn load_rejects_symlinked_arborist_dir() {
        #[cfg(unix)]
        {
            let dir = tempdir().unwrap();
            let other = dir.path().join("elsewhere");
            fs::create_dir_all(&other).unwrap();
            fs::write(other.join(SETTINGS_FILENAME), r#"{ "worktreePrepCommands": ["pwned"] }"#).unwrap();
            std::os::unix::fs::symlink(&other, dir.path().join(ARBORIST_DIR)).unwrap();

            let out = RepoSettings::load(dir.path());
            assert_eq!(out, RepoSettings::default(), "symlinked .arborist must not be honoured");
        }
    }

    #[test]
    fn ensure_arborist_dir_creates_dir_and_gitignore() {
        let dir = tempdir().unwrap();
        let arborist = ensure_arborist_dir(dir.path()).unwrap();
        assert!(arborist.is_dir());
        let gitignore = fs::read_to_string(arborist.join(".gitignore")).unwrap();
        assert!(gitignore.contains(".worktrees/"));
    }

    #[test]
    fn ensure_arborist_dir_is_idempotent_and_preserves_user_gitignore() {
        let dir = tempdir().unwrap();
        let arborist = ensure_arborist_dir(dir.path()).unwrap();
        // User edits the gitignore — we must not clobber it on the next call.
        fs::write(arborist.join(".gitignore"), ".worktrees/\nlocal-secrets/\n").unwrap();
        ensure_arborist_dir(dir.path()).unwrap();
        let gitignore = fs::read_to_string(arborist.join(".gitignore")).unwrap();
        assert!(gitignore.contains("local-secrets/"));
    }

    #[test]
    fn ensure_arborist_dir_rejects_symlinked_arborist() {
        // Symlink/junction creation is privileged on Windows; only assert the rejection on Unix.
        #[cfg(unix)]
        {
            let dir = tempdir().unwrap();
            let target = dir.path().join("elsewhere");
            fs::create_dir_all(&target).unwrap();
            std::os::unix::fs::symlink(&target, dir.path().join(ARBORIST_DIR)).unwrap();
            let err = ensure_arborist_dir(dir.path()).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        }
    }

    #[test]
    fn apply_repo_overlay_is_noop_without_workspace() {
        let cfg = AppConfig::default();
        let out = apply_repo_overlay(cfg.clone());
        assert_eq!(out, cfg);
    }

    #[test]
    fn apply_repo_overlay_layers_settings_when_workspace_set() {
        let dir = tempdir().unwrap();
        write_settings(dir.path(), r#"{ "worktreePrepCommands": ["from-repo"] }"#);

        let cfg = AppConfig {
            workspace_root: Some(dir.path().to_path_buf()),
            worktree_prep_commands: vec!["from-user".to_owned()],
            ..AppConfig::default()
        };
        let out = apply_repo_overlay(cfg);
        assert_eq!(out.worktree_prep_commands, vec!["from-repo".to_owned()]);
    }
}
