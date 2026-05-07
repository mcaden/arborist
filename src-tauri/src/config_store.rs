//! Persistence layer for Arborist (Phase 4).
//!
//! Two logical JSON stores live side-by-side in a single directory (typically
//! the OS-specific `app_data_dir` provided by Tauri):
//!
//! * `config.json`   → [`AppConfig`]
//! * `sessions.json` → `BTreeMap<SessionId, Session>`
//!
//! Both files are written atomically using `tempfile::NamedTempFile::persist`
//! so an interrupted write never leaves a truncated file. On Unix the parent
//! directory is `fsync`-ed after `persist` so the rename itself is durable.
//!
//! ## Crash & corruption handling
//!
//! `load_config` / `load_sessions` **never panic** on malformed input. If the
//! JSON fails to parse (or fails schema validation), the offending file is
//! moved aside to `<name>.bad-<unix-timestamp>` and an empty/default value is
//! returned. A `tracing::warn!` event with the
//! [`Error::ConfigQuarantined`](crate::types::Error::ConfigQuarantined) code
//! describes which file was quarantined and why.
//!
//! ## Path safety (`save_config`)
//!
//! * Relative paths in `instructionSetsDir` or `worktreeRoots[]` are rejected
//!   with [`Error::InvalidPath`].
//! * The keys of `worktreePrelaunchCommands` (canonicalized worktree paths) are
//!   also rejected if relative.
//! * Instruction file paths supplied via the (currently unused) override path
//!   must canonicalize *inside* `instructionSetsDir`.
//!
//! ## Instruction discovery (`discover_instructions`)
//!
//! `*.md` files in `instructionSetsDir` are loaded; each candidate is
//! canonicalized and asserted to lie inside the canonical
//! `instructionSetsDir` (defence against symlinks pointing outside). Files
//! larger than 1 MiB are skipped + warned. Filenames prefixed with `claude-`
//! and `copilot-` are bound to those tools respectively; everything else is
//! ignored. The discovered "default" per tool is `<tool>-default.md` if it
//! exists, else the first alphabetical match for that tool prefix. The
//! [`InstructionSetId`] for each set is its filename stem (e.g.
//! `claude-default`).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use tempfile::NamedTempFile;
use tracing::{debug, warn};

use crate::store_layout::StoreLayout;
use crate::types::{
    AppConfig, AppError, CustomProcessDef, CustomProcessDefId, CustomProcessKind, Error, InstructionSet, InstructionSetId, PartialAppConfig,
    PartialDefaultInstructionSets, Session, SessionId, SessionStatus, SubSessionRecord, Tool, CONFIG_VERSION_CURRENT,
};

const CONFIG_FILENAME: &str = "config.json";
const SESSIONS_FILENAME: &str = "sessions.json";

/// Maximum size (in bytes) of a single instruction file. Files exceeding
/// this cap are skipped during discovery (defence in depth — see DESIGN
/// §8.2).
pub const MAX_INSTRUCTION_FILE_BYTES: u64 = 1024 * 1024;

// ---------------------------------------------------------------------------
// ConfigStore
// ---------------------------------------------------------------------------

/// Handle to the on-disk store directory. Cheap to construct and clone.
///
/// All write paths (`save_config`, `save_session`, `remove_session`,
/// `update_session_status`, `update_session_ai_session_id`,
/// `append_last_open_sub_session`, `remove_last_open_sub_session`) are
/// serialized through a mutex shared by clones of the same handle.
/// Without this, load-modify-write paths called from different threads
/// using the same `ConfigStore` instance (e.g. the PTY wait thread
/// updating `status` while a metrics watcher updates `ai_session_id`)
/// would race and silently lose updates. Atomic file writes
/// (`tempfile::persist`) only protect against torn reads, not against
/// lost updates.
///
/// Scope: this guard covers writes performed through clones of the same
/// `ConfigStore` only. Separately opened `ConfigStore` instances
/// pointing at the same directory do **not** share this mutex and are
/// therefore not serialized against each other — which is why
/// command handlers route through the managed `AppContext`'s store via
/// `AppContext::store()` rather than calling `ConfigStore::open` per
/// request. Concurrent access from a second Arborist process **is**
/// prevented at the `(branch, workspace)` granularity by the OS-level
/// advisory lock acquired in [`crate::boot::bind_workspace`] (held in
/// [`crate::workspace_scope::WorkspaceScope`] for the lifetime of the
/// running instance). Two binaries that bind the *same* `(branch,
/// workspace)` tuple cannot run concurrently. A user editing
/// `sessions.json` by hand while the app is running is still not
/// supported.
///
/// Concurrent reads (`load_config`, `load_sessions`) intentionally do
/// **not** take the lock; if they race a writer they may observe either
/// the pre- or post-write state, which is the same guarantee
/// `write_atomic` already provides.
#[derive(Debug, Clone)]
pub struct ConfigStore {
    dir: PathBuf,
    /// Optional [`StoreLayout`] this store was constructed from. Set by
    /// [`ConfigStore::from_layout`] (the per-(branch, workspace) entry
    /// point used by `WorkspaceScope`); `None` when constructed via the
    /// legacy [`ConfigStore::open`] path (tests, examples, and any
    /// flat-directory caller). Callers that need the layout's
    /// auxiliary paths (lock file, seed lock, legacy seed sources)
    /// should use [`ConfigStore::layout`] and handle the `None` case.
    layout: Option<StoreLayout>,
    write_lock: Arc<Mutex<()>>,
}

impl ConfigStore {
    /// Open (or create) a store rooted at `dir`. The directory will be
    /// created if it does not yet exist.
    ///
    /// Prefer [`ConfigStore::from_layout`] in production code paths
    /// where a [`StoreLayout`] is available — it carries enough
    /// information to resolve the lock-file path, seed-lock path, and
    /// legacy fall-back seed paths. `open` remains the supported entry
    /// point for tests, examples, and other flat-directory callers.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, Error> {
        let dir = dir.into();
        fs::create_dir_all(&dir).map_err(Error::Io)?;
        Ok(Self {
            dir,
            layout: None,
            write_lock: Arc::new(Mutex::new(())),
        })
    }

    /// Open (or create) a store at `layout.workspace_dir()`, retaining
    /// the [`StoreLayout`] for later access via [`Self::layout`]. This
    /// is the canonical entry point used by `WorkspaceScope` at boot
    /// and by the in-app workspace switch.
    pub fn from_layout(layout: StoreLayout) -> Result<Self, Error> {
        let dir = layout.workspace_dir();
        fs::create_dir_all(&dir).map_err(Error::Io)?;
        Ok(Self {
            dir,
            layout: Some(layout),
            write_lock: Arc::new(Mutex::new(())),
        })
    }

    /// The [`StoreLayout`] this store was constructed from, when
    /// available. Returns `None` for stores opened via the legacy
    /// [`Self::open`] path. Use this to reach auxiliary paths
    /// (`lock_path`, `seed_lock_path`, legacy seed sources).
    #[must_use]
    pub fn layout(&self) -> Option<&StoreLayout> {
        self.layout.as_ref()
    }

    /// Filesystem directory backing this store. Mostly useful for tests and
    /// diagnostics.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn config_path(&self) -> PathBuf {
        self.dir.join(CONFIG_FILENAME)
    }

    fn sessions_path(&self) -> PathBuf {
        self.dir.join(SESSIONS_FILENAME)
    }

    // ----- AppConfig ------------------------------------------------------

    /// Load the persisted [`AppConfig`], applying semantic validation:
    ///
    /// * Quarantines and returns defaults if the file is missing or
    ///   unparseable.
    /// * Canonicalizes `instructionSetsDir` and each `worktreeRoots[]`,
    ///   dropping (with a warning) any entry that no longer points at an
    ///   existing directory.
    /// * Drops per-worktree override keys whose paths don't canonicalize to an
    ///   existing directory (logged warning).
    /// * If `defaultInstructionSets.{claude|copilot}` references an ID that
    ///   isn't in the discovered instruction set list, falls back to the
    ///   discovered default for that tool.
    pub fn load_config(&self) -> AppConfig {
        let path = self.config_path();
        let raw = match fs::read_to_string(&path) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return default_seeded_config(),
            Err(e) => {
                warn!(
                    code = "Io",
                    path = %path.display(),
                    error = %e,
                    "config.json could not be read; returning defaults",
                );
                return default_seeded_config();
            }
        };

        let parsed: AppConfig = match serde_json::from_str(&raw) {
            Ok(c) => c,
            Err(e) => {
                let quarantined = quarantine(&path);
                warn!(
                    code = "ConfigQuarantined",
                    path = %path.display(),
                    quarantined = ?quarantined,
                    error = %e,
                    "config.json failed to parse; quarantined and returning defaults",
                );
                return default_seeded_config();
            }
        };

        // Future-version downgrade guard: if a newer build wrote this file
        // (e.g. user downgraded to this branch), don't risk silently
        // rewriting it without the future fields. Quarantine and return
        // defaults so the user notices.
        if parsed.config_version > CONFIG_VERSION_CURRENT {
            let quarantined = quarantine(&path);
            warn!(
                code = "ConfigQuarantined",
                path = %path.display(),
                quarantined = ?quarantined,
                found_version = parsed.config_version,
                supported_version = CONFIG_VERSION_CURRENT,
                "config.json was written by a newer build; quarantined and returning defaults",
            );
            return default_seeded_config();
        }

        let mut cfg = parsed;
        // v1/v2 → v3: `workspace_root` did not exist. If the user already
        // had exactly one `worktree_roots` entry, treat that as the
        // workspace so they don't get pushed back through the first-boot
        // picker for no reason. Multi-root and zero-root configs leave
        // `workspace_root` as `None` and the picker will be shown.
        if cfg.config_version < 3 && cfg.workspace_root.is_none() && cfg.worktree_roots.len() == 1 {
            cfg.workspace_root = Some(cfg.worktree_roots[0].clone());
        }
        // Bump the on-disk version stamp so the next save records the
        // current schema explicitly. (`active_session_id` was the v1→v2
        // addition; `workspace_root` is the v2→v3 addition; `custom_processes`
        // and `last_open_sub_sessions` are the v3→v4 additions. All
        // default via serde, so missing fields hydrate cleanly already.)
        //
        // v3→v4: additively seed the built-in custom-process defs
        // (`shell`, `open-folder`, `vscode`). Only IDs not already present
        // are inserted, so a user who edited / deleted a built-in does not
        // get it silently re-injected on every launch.
        if cfg.config_version < 4 {
            seed_default_custom_processes(&mut cfg.custom_processes);
        }
        if cfg.config_version < CONFIG_VERSION_CURRENT {
            cfg.config_version = CONFIG_VERSION_CURRENT;
        }
        sanitize_loaded_custom_processes(&mut cfg.custom_processes);
        sanitize_loaded_sub_session_records(&mut cfg.last_open_sub_sessions, &cfg.custom_processes);
        validate_loaded_config(&mut cfg);

        // Validate default instruction set IDs against the *discovered* set.
        // Skip validation entirely if we don't have an instructionSetsDir
        // configured — there's nothing to validate against and we'd
        // otherwise clobber legitimate IDs that haven't yet been observed.
        if !cfg.instruction_sets_dir.as_os_str().is_empty() {
            let discovered = discover_instructions(&cfg.instruction_sets_dir).unwrap_or_default();
            let known_ids: BTreeSet<InstructionSetId> = discovered.iter().map(|i| i.id.clone()).collect();
            let claude_default = discovered.iter().find(|i| i.tool == Tool::Claude && i.is_default).map(|i| i.id.clone());
            let copilot_default = discovered.iter().find(|i| i.tool == Tool::Copilot && i.is_default).map(|i| i.id.clone());

            if !cfg.default_instruction_sets.claude.as_str().is_empty() && !known_ids.contains(&cfg.default_instruction_sets.claude) {
                warn!(
                    code = "ConfigQuarantined",
                    missing = %cfg.default_instruction_sets.claude,
                    "defaultInstructionSets.claude not found in discovered sets; falling back",
                );
                cfg.default_instruction_sets.claude = claude_default.unwrap_or_default();
            }
            if !cfg.default_instruction_sets.copilot.as_str().is_empty() && !known_ids.contains(&cfg.default_instruction_sets.copilot) {
                warn!(
                    code = "ConfigQuarantined",
                    missing = %cfg.default_instruction_sets.copilot,
                    "defaultInstructionSets.copilot not found in discovered sets; falling back",
                );
                cfg.default_instruction_sets.copilot = copilot_default.unwrap_or_default();
            }
        }

        cfg
    }

    /// Apply a partial update to the persisted [`AppConfig`] and write the
    /// merged result back to disk atomically.
    ///
    /// Each path field provided in `patch` is canonicalized; relative paths
    /// are rejected with [`Error::InvalidPath`]. Per-worktree override keys
    /// are canonicalized; keys that fail canonicalization are dropped with a
    /// warning rather than poisoning the whole call.
    pub fn save_config(&self, patch: PartialAppConfig) -> Result<AppConfig, Error> {
        self.save_config_with(patch, |_| false)
    }

    /// Variant of [`Self::save_config`] that also runs an arbitrary
    /// in-place mutation against the merged config **while holding
    /// the write lock**, then persists once. The mutation's return
    /// value is unused — we always write because the patch was
    /// already merged in. The lock spans load → merge → mutate →
    /// write, eliminating the read-modify-write race that would
    /// exist if a caller did `save_config` followed by `write_full`.
    pub fn save_config_with<F>(&self, patch: PartialAppConfig, mut mutate: F) -> Result<AppConfig, Error>
    where
        F: FnMut(&mut AppConfig) -> bool,
    {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut cfg = self.load_config();
        merge_partial(&mut cfg, patch)?;
        let _ = mutate(&mut cfg);
        cfg.config_version = CONFIG_VERSION_CURRENT;
        write_atomic(&self.config_path(), &cfg)?;
        Ok(cfg)
    }

    /// Write the supplied [`AppConfig`] verbatim, bumping the version
    /// stamp. Used by the icon backfill path which mutates the config
    /// in fields the public `PartialAppConfig` patch surface doesn't
    /// expose (`icon_data_uri` is backend-derived, not user-editable).
    ///
    /// **Caution:** holds the write lock for its own duration only;
    /// don't sandwich it with a `load_config` from a separate caller
    /// expecting an atomic read-modify-write — use
    /// [`Self::save_config_with`] for that case.
    pub fn write_full(&self, mut cfg: AppConfig) -> Result<AppConfig, Error> {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        cfg.config_version = CONFIG_VERSION_CURRENT;
        write_atomic(&self.config_path(), &cfg)?;
        Ok(cfg)
    }

    // ----- Sessions -------------------------------------------------------

    /// Load all persisted [`Session`] records, keyed by ID. A missing or
    /// malformed file produces an empty map (with quarantine on parse
    /// failure).
    pub fn load_sessions(&self) -> BTreeMap<SessionId, Session> {
        let path = self.sessions_path();
        let raw = match fs::read_to_string(&path) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return BTreeMap::new(),
            Err(e) => {
                warn!(
                    code = "Io",
                    path = %path.display(),
                    error = %e,
                    "sessions.json could not be read; returning empty map",
                );
                return BTreeMap::new();
            }
        };
        match serde_json::from_str::<BTreeMap<SessionId, Session>>(&raw) {
            Ok(mut m) => {
                migrate_copilot_composed_commands(&mut m);
                m
            }
            Err(e) => {
                let quarantined = quarantine(&path);
                warn!(
                    code = "ConfigQuarantined",
                    path = %path.display(),
                    quarantined = ?quarantined,
                    error = %e,
                    "sessions.json failed to parse; quarantined and returning empty map",
                );
                BTreeMap::new()
            }
        }
    }

    /// Strict variant of [`Self::load_sessions`] for callers that perform
    /// destructive operations and cannot safely treat IO/parse failures as
    /// "no sessions exist". Returns the full session map on success or the
    /// underlying error otherwise. A missing file is still treated as an
    /// empty map (a fresh install has no `sessions.json`).
    pub fn try_load_sessions(&self) -> Result<BTreeMap<SessionId, Session>, Error> {
        let path = self.sessions_path();
        let raw = match fs::read_to_string(&path) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(e) => return Err(Error::Io(e)),
        };
        let mut m: BTreeMap<SessionId, Session> =
            serde_json::from_str(&raw).map_err(|e| Error::Internal(format!("sessions.json failed to parse: {} ({e})", path.display())))?;
        migrate_copilot_composed_commands(&mut m);
        Ok(m)
    }

    /// Persist a single session record (insert-or-replace).
    pub fn save_session(&self, session: &Session) -> Result<(), Error> {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut all = self.load_sessions();
        all.insert(session.id, session.clone());
        write_atomic(&self.sessions_path(), &all)
    }

    /// Remove a session record by ID. Missing IDs are a no-op success.
    pub fn remove_session(&self, id: &SessionId) -> Result<(), Error> {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut all = self.load_sessions();
        if all.remove(id).is_none() {
            return Ok(());
        }
        write_atomic(&self.sessions_path(), &all)
    }

    /// Mutate the persisted status (and optionally PID) of a session record.
    /// Used by the Phase 6 wait thread so reloaded sessions never advertise
    /// stale `running`/`pid` values.
    pub fn update_session_status(&self, id: &SessionId, status: SessionStatus, pid: Option<u32>) -> Result<(), Error> {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut all = self.load_sessions();
        let Some(session) = all.get_mut(id) else {
            return Err(Error::NotFound(format!("session {id} not found")));
        };
        session.status = status;
        session.pid = pid;
        write_atomic(&self.sessions_path(), &all)
    }

    /// Mutate the persisted `ai_session_id` of a session record. Used by
    /// the metrics watchers' discovery callback so app-restart restore
    /// can resume the AI conversation. Returns `Ok(true)` when the value
    /// changed (and was therefore persisted), `Ok(false)` when the value
    /// was already current — the latter avoids a redundant disk write
    /// every poll once the watcher has converged.
    pub fn update_session_ai_session_id(&self, id: &SessionId, ai_session_id: Option<String>) -> Result<bool, Error> {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut all = self.load_sessions();
        let Some(session) = all.get_mut(id) else {
            return Err(Error::NotFound(format!("session {id} not found")));
        };
        if session.ai_session_id == ai_session_id {
            return Ok(false);
        }
        session.ai_session_id = ai_session_id;
        write_atomic(&self.sessions_path(), &all)?;
        Ok(true)
    }

    // ----- Sub-sessions (last_open_sub_sessions list) --------------------

    /// Append a sub-session record to `AppConfig.lastOpenSubSessions`,
    /// replacing any existing entry with the same id. Serialized via the
    /// shared `write_lock`.
    pub fn append_last_open_sub_session(&self, record: crate::types::SubSessionRecord) -> Result<(), Error> {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut cfg = self.load_config();
        cfg.last_open_sub_sessions.retain(|r| r.id != record.id);
        cfg.last_open_sub_sessions.push(record);
        write_atomic(&self.config_path(), &cfg)
    }

    /// Remove a sub-session record by id. Missing ids are a no-op success.
    pub fn remove_last_open_sub_session(&self, id: &crate::types::SubSessionId) -> Result<(), Error> {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut cfg = self.load_config();
        let before = cfg.last_open_sub_sessions.len();
        cfg.last_open_sub_sessions.retain(|r| &r.id != id);
        if cfg.last_open_sub_sessions.len() == before {
            return Ok(());
        }
        write_atomic(&self.config_path(), &cfg)
    }
}

// ---------------------------------------------------------------------------
// Loaded-session migrations
// ---------------------------------------------------------------------------

/// Rewrite Copilot session `composed_command` values that were persisted
/// before we dropped the legacy `--interactive <string>` invocation. The
/// modern `copilot` CLI rejects that flag with "too many arguments". Any
/// trailing `copilot ...` segment is replaced with bare `copilot`, so
/// restart-on-launch and `session_restart` work for sessions created by
/// older builds.
fn migrate_copilot_composed_commands(sessions: &mut BTreeMap<SessionId, Session>) {
    for (id, session) in sessions.iter_mut() {
        if session.tool != Tool::Copilot {
            continue;
        }
        let segments: Vec<&str> = session.composed_command.split(" && ").collect();
        let Some((last, head)) = segments.split_last() else {
            continue;
        };
        if !last.trim_start().starts_with("copilot") || last.trim() == "copilot" {
            continue;
        }
        let mut rebuilt: Vec<String> = head.iter().map(|s| (*s).to_string()).collect();
        rebuilt.push("copilot".to_string());
        let new_cmd = rebuilt.join(" && ");
        warn!(
            session_id = %id,
            old = %session.composed_command,
            new = %new_cmd,
            "migrating stale Copilot composed_command (dropping legacy --interactive flag)",
        );
        session.composed_command = new_cmd;
    }
}

// ---------------------------------------------------------------------------
// Loaded-config validation
// ---------------------------------------------------------------------------

fn validate_loaded_config(cfg: &mut AppConfig) {
    // Canonicalize instructionSetsDir. An empty path = "no directory yet
    // configured" and is left as-is.
    if !cfg.instruction_sets_dir.as_os_str().is_empty() {
        match dunce::canonicalize(&cfg.instruction_sets_dir) {
            Ok(p) if p.is_dir() => cfg.instruction_sets_dir = p,
            Ok(p) => {
                warn!(
                    code = "InvalidPath",
                    path = %p.display(),
                    "instructionSetsDir is not a directory; clearing",
                );
                cfg.instruction_sets_dir = PathBuf::new();
            }
            Err(e) => {
                warn!(
                    code = "InvalidPath",
                    path = %cfg.instruction_sets_dir.display(),
                    error = %e,
                    "instructionSetsDir could not be canonicalized; clearing",
                );
                cfg.instruction_sets_dir = PathBuf::new();
            }
        }
    }

    cfg.worktree_roots = std::mem::take(&mut cfg.worktree_roots)
        .into_iter()
        .filter_map(|p| match dunce::canonicalize(&p) {
            Ok(c) if c.is_dir() => Some(c),
            Ok(c) => {
                warn!(
                    code = "InvalidPath",
                    path = %c.display(),
                    "worktreeRoots entry is not a directory; dropping",
                );
                None
            }
            Err(e) => {
                warn!(
                    code = "InvalidPath",
                    path = %p.display(),
                    error = %e,
                    "worktreeRoots entry could not be canonicalized; dropping",
                );
                None
            }
        })
        .collect();

    // workspace_root: canonicalize, drop on failure (treated like a stale
    // path — the picker will be re-shown on next launch).
    if let Some(ws) = cfg.workspace_root.take() {
        match dunce::canonicalize(&ws) {
            Ok(c) if c.is_dir() => cfg.workspace_root = Some(c),
            Ok(c) => {
                warn!(
                    code = "InvalidPath",
                    path = %c.display(),
                    "workspaceRoot is not a directory; clearing",
                );
            }
            Err(e) => {
                warn!(
                    code = "InvalidPath",
                    path = %ws.display(),
                    error = %e,
                    "workspaceRoot could not be canonicalized; clearing",
                );
            }
        }
    }

    let raw_overrides = std::mem::take(&mut cfg.worktree_prelaunch_commands);
    let mut filtered = BTreeMap::new();
    for (key, cmds) in raw_overrides {
        match dunce::canonicalize(&key) {
            Ok(c) if c.is_dir() => {
                filtered.insert(c.to_string_lossy().into_owned(), cmds);
            }
            Ok(_) | Err(_) => {
                warn!(
                    code = "InvalidPath",
                    key = %key,
                    "worktreePrelaunchCommands key does not canonicalize to an existing dir; dropping",
                );
            }
        }
    }
    cfg.worktree_prelaunch_commands = filtered;
}

// ---------------------------------------------------------------------------
// Partial merge / save validation
// ---------------------------------------------------------------------------

fn merge_partial(cfg: &mut AppConfig, patch: PartialAppConfig) -> Result<(), Error> {
    if let Some(v) = patch.config_version {
        cfg.config_version = v;
    }
    if let Some(d) = patch.default_instruction_sets {
        let PartialDefaultInstructionSets { claude, copilot } = d;
        if let Some(c) = claude {
            cfg.default_instruction_sets.claude = c;
        }
        if let Some(c) = copilot {
            cfg.default_instruction_sets.copilot = c;
        }
    }
    if let Some(dir) = patch.instruction_sets_dir {
        // Empty string = "clear the directory" (revert to the unconfigured
        // default). Otherwise the path must be absolute and exist.
        if dir.as_os_str().is_empty() {
            cfg.instruction_sets_dir = PathBuf::new();
        } else {
            if dir.is_relative() {
                return Err(Error::InvalidPath(format!("instructionSetsDir must be absolute, got {}", dir.display())));
            }
            let canon = dunce::canonicalize(&dir).map_err(|e| Error::InvalidPath(format!("{}: {e}", dir.display())))?;
            if !canon.is_dir() {
                return Err(Error::InvalidPath(format!("instructionSetsDir is not a directory: {}", canon.display())));
            }
            cfg.instruction_sets_dir = canon;
        }
    }
    // workspace_root is tri-state like active_session_id: absent → leave
    // alone; Some(None) → clear; Some(Some(path)) → set after validating it
    // is an absolute, existing directory.
    if let Some(ws) = patch.workspace_root {
        match ws {
            None => cfg.workspace_root = None,
            Some(p) => {
                if p.is_relative() {
                    return Err(Error::InvalidPath(format!("workspaceRoot must be absolute, got {}", p.display())));
                }
                let canon = dunce::canonicalize(&p).map_err(|e| Error::InvalidPath(format!("{}: {e}", p.display())))?;
                if !canon.is_dir() {
                    return Err(Error::InvalidPath(format!("workspaceRoot is not a directory: {}", canon.display())));
                }
                cfg.workspace_root = Some(canon);
            }
        }
    }
    if let Some(roots) = patch.worktree_roots {
        let mut out = Vec::with_capacity(roots.len());
        for p in roots {
            if p.is_relative() {
                return Err(Error::InvalidPath(format!("worktreeRoots entries must be absolute, got {}", p.display())));
            }
            let canon = dunce::canonicalize(&p).map_err(|e| Error::InvalidPath(format!("{}: {e}", p.display())))?;
            if !canon.is_dir() {
                return Err(Error::InvalidPath(format!("worktreeRoots entry is not a directory: {}", canon.display())));
            }
            out.push(canon);
        }
        cfg.worktree_roots = out;
    }
    if let Some(cmds) = patch.prelaunch_commands {
        cfg.prelaunch_commands = cmds;
    }
    if let Some(launch) = patch.ai_launch_commands {
        let crate::types::PartialAiLaunchCommands { claude, copilot } = launch;
        if let Some(c) = claude {
            // Clear cached icon when the command changes — re-resolution
            // will re-populate it from a post-save backfill pass.
            if c != cfg.ai_launch_commands.claude {
                cfg.ai_launch_commands.claude_icon_data_uri = None;
            }
            cfg.ai_launch_commands.claude = c;
        }
        if let Some(c) = copilot {
            if c != cfg.ai_launch_commands.copilot {
                cfg.ai_launch_commands.copilot_icon_data_uri = None;
            }
            cfg.ai_launch_commands.copilot = c;
        }
    }
    if let Some(overrides) = patch.worktree_prelaunch_commands {
        let mut out = BTreeMap::new();
        for (key, cmds) in overrides {
            let p = PathBuf::from(&key);
            if p.is_relative() {
                return Err(Error::InvalidPath(format!("worktreePrelaunchCommands key must be absolute, got {key}",)));
            }
            match dunce::canonicalize(&p) {
                Ok(c) if c.is_dir() => {
                    out.insert(c.to_string_lossy().into_owned(), cmds);
                }
                Ok(_) | Err(_) => {
                    warn!(
                        code = "InvalidPath",
                        key = %key,
                        "worktreePrelaunchCommands key does not canonicalize to an existing dir; dropping",
                    );
                }
            }
        }
        cfg.worktree_prelaunch_commands = out;
    }
    if let Some(s) = patch.last_open_sessions {
        cfg.last_open_sessions = s;
    }
    if let Some(t) = patch.tab_order {
        cfg.tab_order = t;
    }
    // Tri-state: `None` → don't touch; `Some(None)` → clear; `Some(Some(id))` →
    // set.
    if let Some(active) = patch.active_session_id {
        cfg.active_session_id = active;
    }
    if let Some(mut defs) = patch.custom_processes {
        validate_custom_processes(&defs)?;
        // Preserve cached `icon_data_uri` across patches that don't
        // carry it (the frontend never sends it — it's a backend
        // derived field). Drop the cache when `command` changes so
        // the next backfill pass re-resolves.
        let prev: BTreeMap<crate::types::CustomProcessDefId, &crate::types::CustomProcessDef> =
            cfg.custom_processes.iter().map(|d| (d.id.clone(), d)).collect();
        for def in defs.iter_mut() {
            if def.icon_data_uri.is_some() {
                continue;
            }
            if let Some(old) = prev.get(&def.id) {
                if old.command == def.command {
                    def.icon_data_uri = old.icon_data_uri.clone();
                }
            }
        }
        cfg.custom_processes = defs;
    }
    if let Some(records) = patch.last_open_sub_sessions {
        cfg.last_open_sub_sessions = records;
    }
    Ok(())
}

/// Reject obviously-invalid [`CustomProcessDef`] lists at the
/// `config_set` boundary so corrupt state can't reach the runtime.
///
/// Rules (also enforced by the Settings UI in Phase 6, but the backend is
/// the source of truth):
///
/// * Non-empty `id` and `name`.
/// * `id` matches `[a-zA-Z0-9_-]+` (so it can be safely used as a wire key and
///   React `key` without escaping).
/// * Non-empty `command`.
/// * IDs are unique within the list.
fn validate_custom_processes(defs: &[CustomProcessDef]) -> Result<(), Error> {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for def in defs {
        if def.id.as_str().is_empty() {
            return Err(Error::InvalidCustomProcessDef("customProcesses[]: id must be non-empty".into()));
        }
        if !def.id.as_str().chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return Err(Error::InvalidCustomProcessDef(format!(
                "customProcesses[]: id {:?} must match [a-zA-Z0-9_-]+",
                def.id.as_str()
            )));
        }
        if def.name.trim().is_empty() {
            return Err(Error::InvalidCustomProcessDef(format!(
                "customProcesses[{}]: name must be non-empty",
                def.id
            )));
        }
        if def.command.trim().is_empty() {
            return Err(Error::InvalidCustomProcessDef(format!(
                "customProcesses[{}]: command must be non-empty",
                def.id
            )));
        }
        if !seen.insert(def.id.as_str()) {
            return Err(Error::InvalidCustomProcessDef(format!(
                "customProcesses[]: duplicate id {:?}",
                def.id.as_str()
            )));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Atomic write
// ---------------------------------------------------------------------------

fn write_atomic<T: serde::Serialize>(target: &Path, value: &T) -> Result<(), Error> {
    let parent = target
        .parent()
        .ok_or_else(|| Error::InvalidPath(format!("target has no parent: {}", target.display())))?;
    fs::create_dir_all(parent).map_err(Error::Io)?;

    let serialized = serde_json::to_vec_pretty(value).map_err(Error::Serde)?;

    let mut tmp = NamedTempFile::new_in(parent).map_err(Error::Io)?;
    tmp.write_all(&serialized).map_err(Error::Io)?;
    tmp.flush().map_err(Error::Io)?;
    tmp.as_file().sync_all().map_err(Error::Io)?;
    tmp.persist(target).map_err(|e| Error::Io(e.error))?;

    #[cfg(unix)]
    {
        if let Ok(dir) = fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Quarantine
// ---------------------------------------------------------------------------

fn quarantine(path: &Path) -> Option<PathBuf> {
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let target = path.with_extension(format!("{}.bad-{ts}", path.extension().and_then(|e| e.to_str()).unwrap_or("json"),));
    match fs::rename(path, &target) {
        Ok(_) => Some(target),
        Err(e) => {
            warn!(
                code = "Io",
                path = %path.display(),
                error = %e,
                "failed to quarantine corrupt file",
            );
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Instruction discovery
// ---------------------------------------------------------------------------

const CLAUDE_PREFIX: &str = "claude-";
const COPILOT_PREFIX: &str = "copilot-";
const CLAUDE_DEFAULT_FILENAME: &str = "claude-default.md";
const COPILOT_DEFAULT_FILENAME: &str = "copilot-default.md";

/// Scan `dir` for `*.md` files and return the discovered [`InstructionSet`]
/// list. Returns `Err` only if `dir` itself can't be read; per-file errors
/// are logged and the offending file is skipped.
///
/// The returned list is sorted by `file_path` so the per-tool default
/// selection (`<tool>-default.md` if present, else first alphabetical with
/// the right prefix) is deterministic.
pub fn discover_instructions(dir: &Path) -> Result<Vec<InstructionSet>, Error> {
    if dir.as_os_str().is_empty() {
        return Ok(Vec::new());
    }
    let canon_dir = match dunce::canonicalize(dir) {
        Ok(p) => p,
        Err(e) => {
            warn!(
                code = "InvalidPath",
                dir = %dir.display(),
                error = %e,
                "instructionSetsDir cannot be canonicalized; returning empty list",
            );
            return Ok(Vec::new());
        }
    };
    if !canon_dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut sets: Vec<InstructionSet> = Vec::new();
    let entries = fs::read_dir(&canon_dir).map_err(Error::Io)?;
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                warn!(error = %e, "failed to read directory entry");
                continue;
            }
        };
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()).map(str::to_owned) else {
            continue;
        };

        // Symlink defence — canonicalize and confirm we're still inside
        // canon_dir.
        let canon = match dunce::canonicalize(&path) {
            Ok(p) => p,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "failed to canonicalize candidate");
                continue;
            }
        };
        if !canon.starts_with(&canon_dir) {
            warn!(
                code = "InvalidPath",
                path = %canon.display(),
                base = %canon_dir.display(),
                "instruction file canonicalizes outside instructionSetsDir; skipping",
            );
            continue;
        }

        let meta = match fs::metadata(&canon) {
            Ok(m) => m,
            Err(e) => {
                warn!(path = %canon.display(), error = %e, "failed to stat candidate");
                continue;
            }
        };
        if !meta.is_file() {
            continue;
        }
        if meta.len() > MAX_INSTRUCTION_FILE_BYTES {
            warn!(
                code = "InvalidPath",
                path = %canon.display(),
                size = meta.len(),
                cap = MAX_INSTRUCTION_FILE_BYTES,
                "instruction file exceeds 1 MiB cap; skipping",
            );
            continue;
        }

        let tool = if stem.starts_with(CLAUDE_PREFIX) {
            Tool::Claude
        } else if stem.starts_with(COPILOT_PREFIX) {
            Tool::Copilot
        } else {
            // Files not matching either prefix are ignored — the prefix
            // convention is documented in CONFIGURATION.md.
            continue;
        };

        let name = humanize_stem(&stem);
        sets.push(InstructionSet {
            id: InstructionSetId::new(stem),
            name,
            tool,
            file_path: canon,
            is_default: false, // assigned below
        });
    }

    sets.sort_by(|a, b| a.file_path.cmp(&b.file_path));

    mark_defaults(&mut sets, Tool::Claude, CLAUDE_DEFAULT_FILENAME);
    mark_defaults(&mut sets, Tool::Copilot, COPILOT_DEFAULT_FILENAME);

    Ok(sets)
}

fn humanize_stem(stem: &str) -> String {
    // "claude-default" → "Claude default"; mostly a UX nicety. Kept simple
    // — this is not a translation layer.
    let mut chars = stem.replace(['-', '_'], " ");
    if let Some(c) = chars.get_mut(0..1) {
        c.make_ascii_uppercase();
    }
    chars
}

fn mark_defaults(sets: &mut [InstructionSet], tool: Tool, preferred_filename: &str) {
    let preferred_idx = sets
        .iter()
        .position(|s| s.tool == tool && filename_matches(&s.file_path, preferred_filename));
    let chosen = preferred_idx.or_else(|| sets.iter().position(|s| s.tool == tool));
    if let Some(idx) = chosen {
        sets[idx].is_default = true;
    }
}

fn filename_matches(path: &Path, filename: &str) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.eq_ignore_ascii_case(filename))
}

// ---------------------------------------------------------------------------
// Tauri command surface helpers
// ---------------------------------------------------------------------------

/// Public helper used by the `instructions_list` Tauri command. Wraps
/// [`discover_instructions`] and converts the inner error to [`AppError`].
pub fn list_instructions_for(cfg: &AppConfig) -> Result<Vec<InstructionSet>, AppError> {
    discover_instructions(&cfg.instruction_sets_dir).map_err(AppError::from)
}

// ---------------------------------------------------------------------------
// Built-in custom-process defs (configVersion 3→4 seeding)
// ---------------------------------------------------------------------------

/// Reserved ID for the built-in "Shell" terminal launcher.
pub const BUILTIN_DEF_ID_SHELL: &str = "shell";
/// Reserved ID for the built-in "Open Folder" application launcher.
pub const BUILTIN_DEF_ID_OPEN_FOLDER: &str = "open-folder";
/// Reserved ID for the built-in "VS Code" application launcher.
pub const BUILTIN_DEF_ID_VSCODE: &str = "vscode";

/// Construct the on-first-launch [`AppConfig`] with the built-in
/// custom-process defs already seeded. Used both for the missing-file
/// path and the quarantine-and-default-on-load path so a fresh install
/// always sees the documented Launch menu entries.
fn default_seeded_config() -> AppConfig {
    let mut cfg = AppConfig::default();
    seed_default_custom_processes(&mut cfg.custom_processes);
    cfg
}

/// Drop persisted [`CustomProcessDef`]s that fail validation. Unlike the
/// strict `config_set` boundary, the load path is *graceful*: an
/// individually-corrupt def (empty command, bad id) is logged and removed
/// rather than nuking the whole config. Duplicate IDs keep the first
/// occurrence and drop later ones.
fn sanitize_loaded_custom_processes(defs: &mut Vec<CustomProcessDef>) {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let original_len = defs.len();
    defs.retain(|def| {
        if let Err(e) = validate_custom_processes(std::slice::from_ref(def)) {
            warn!(
                code = "CustomProcessDefDropped",
                id = %def.id,
                error = %e,
                "dropping invalid custom process def from loaded config",
            );
            return false;
        }
        if !seen.insert(def.id.as_str().to_owned()) {
            warn!(
                code = "CustomProcessDefDropped",
                id = %def.id,
                "dropping duplicate custom process def id from loaded config",
            );
            return false;
        }
        true
    });
    if defs.len() != original_len {
        debug!(
            removed = original_len - defs.len(),
            kept = defs.len(),
            "sanitized custom process defs from loaded config",
        );
    }
}

/// Sanitize persisted [`SubSessionRecord`]s on load: drop any whose
/// `def_id` no longer exists in the user's `custom_processes` (the def
/// was deleted between sessions), and backfill `composed_command` from
/// the def for legacy v3→v4 records that didn't persist it. Both are
/// silent — restore-on-launch is best-effort.
fn sanitize_loaded_sub_session_records(records: &mut Vec<SubSessionRecord>, defs: &[CustomProcessDef]) {
    let by_id: std::collections::BTreeMap<&CustomProcessDefId, &CustomProcessDef> = defs.iter().map(|d| (&d.id, d)).collect();
    let original_len = records.len();
    records.retain_mut(|rec| {
        let Some(def) = by_id.get(&rec.def_id) else {
            warn!(
                code = "SubSessionRecordDropped",
                id = %rec.id,
                def_id = %rec.def_id,
                "dropping sub-session record whose def is no longer present",
            );
            return false;
        };
        if rec.composed_command.trim().is_empty() {
            rec.composed_command = def.command.clone();
        }
        true
    });
    if records.len() != original_len {
        debug!(
            removed = original_len - records.len(),
            kept = records.len(),
            "sanitized sub-session records from loaded config",
        );
    }
}

/// table) into `defs`. Only IDs not already present are appended;
/// existing entries (including ones the user has edited or disabled) are
/// left untouched. Insertion order mirrors the plan table so a fresh
/// install renders the menu in the documented order.
///
/// `vscode` is enabled by default iff the `code` binary is discoverable
/// on `PATH` at seed time. The probe is best-effort: a transient PATH
/// hiccup just leaves it disabled (the user can flip the toggle in the
/// Settings dialog).
pub fn seed_default_custom_processes(defs: &mut Vec<CustomProcessDef>) {
    let existing: BTreeSet<CustomProcessDefId> = defs.iter().map(|d| d.id.clone()).collect();
    for built_in in default_custom_processes() {
        if !existing.contains(&built_in.id) {
            defs.push(built_in);
        }
    }
}

/// The full ordered list of built-in defs, regardless of whether they
/// are already present in any particular config. Test-only callers may
/// use this for assertions; production code should call
/// [`seed_default_custom_processes`] which is additive.
#[must_use]
pub fn default_custom_processes() -> Vec<CustomProcessDef> {
    vec![
        CustomProcessDef {
            id: CustomProcessDefId::new(BUILTIN_DEF_ID_SHELL),
            name: "Shell".to_owned(),
            kind: CustomProcessKind::Terminal,
            command: default_shell_command(),
            enabled: true,
            icon: None,
            icon_data_uri: None,
        },
        CustomProcessDef {
            id: CustomProcessDefId::new(BUILTIN_DEF_ID_OPEN_FOLDER),
            name: "Open Folder".to_owned(),
            kind: CustomProcessKind::Application,
            command: default_open_folder_command().to_owned(),
            enabled: true,
            icon: None,
            icon_data_uri: None,
        },
        CustomProcessDef {
            id: CustomProcessDefId::new(BUILTIN_DEF_ID_VSCODE),
            name: "VS Code".to_owned(),
            kind: CustomProcessKind::Application,
            command: "code .".to_owned(),
            enabled: command_on_path("code"),
            icon: None,
            icon_data_uri: None,
        },
    ]
}

fn default_shell_command() -> String {
    // Phase 1 keeps this minimal: launch the platform shell interactively.
    // The PTY pool will spawn it via `$SHELL -c <cmd>` (Unix) or
    // `%COMSPEC% /c <cmd>` (Windows), so the inner command is a fresh
    // login-ish invocation of the same shell. We deliberately don't pass
    // `--login` so we don't fight the user's profile order.
    if cfg!(target_os = "windows") {
        "cmd".to_owned()
    } else {
        // Use $SHELL when set, but only if it looks like a sane absolute
        // path with no shell-metacharacters. A weird $SHELL (containing
        // spaces, quotes, `;`, `&`, `|`, `$`, backticks, newlines, …) would
        // be re-interpreted by the launcher's `sh -c`, so we fall back to
        // `sh -i` rather than persist a footgun into the user's seed.
        let shell = std::env::var("SHELL")
            .ok()
            .filter(|s| {
                let s = s.trim();
                !s.is_empty() && std::path::Path::new(s).is_absolute() && !s.chars().any(|c| c.is_whitespace() || "\"'`$&|;<>()\\*?[]{}".contains(c))
            })
            .unwrap_or_else(|| "sh".to_owned());
        format!("{shell} -i")
    }
}

const fn default_open_folder_command() -> &'static str {
    if cfg!(target_os = "windows") {
        "explorer ."
    } else if cfg!(target_os = "macos") {
        "open ."
    } else {
        "xdg-open ."
    }
}

/// Return `true` if `cmd` resolves to an executable on the current
/// process's `PATH`. Pure-std implementation so we don't have to pull in
/// the `which` crate just for this best-effort probe. Errors and missing
/// `PATH` both yield `false`.
///
/// On Unix, requires at least one executable bit (`0o111`) so a stray
/// non-executable file named like the command on `PATH` doesn't enable a
/// launcher that will fail to spawn.
#[must_use]
pub fn command_on_path(cmd: &str) -> bool {
    let path = match std::env::var_os("PATH") {
        Some(p) => p,
        None => return false,
    };
    let exe_suffixes: &[&str] = if cfg!(target_os = "windows") {
        &["", ".exe", ".cmd", ".bat"]
    } else {
        &[""]
    };
    for dir in std::env::split_paths(&path) {
        for suffix in exe_suffixes {
            let candidate = if suffix.is_empty() {
                dir.join(cmd)
            } else {
                dir.join(format!("{cmd}{suffix}"))
            };
            if !candidate.is_file() {
                continue;
            }
            if is_executable(&candidate) {
                return true;
            }
        }
    }
    false
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).map(|m| m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> bool {
    // On Windows, `is_file()` + a recognized suffix from PATHEXT-ish list
    // is the practical equivalent. We don't crack `PATHEXT` here yet.
    true
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{CustomProcessDef, CustomProcessDefId, CustomProcessKind, SessionStatus, SubSessionId, SubSessionRecord, TempFileSpec};
    use pretty_assertions::assert_eq;
    use std::collections::BTreeMap;
    use std::fs::{self, File};
    use std::io::Write;
    use tempfile::TempDir;
    use uuid::Uuid;

    fn touch(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        let mut f = File::create(path).expect("create");
        f.write_all(contents.as_bytes()).expect("write");
    }

    fn canon(p: &Path) -> PathBuf {
        dunce::canonicalize(p).expect("canon")
    }

    fn make_session(id: Uuid, label: &str, dir: &Path) -> Session {
        Session {
            id: SessionId(id),
            tool: Tool::Claude,
            worktree_path: dir.to_path_buf(),
            worktree_name: label.to_owned(),
            label: label.to_owned(),
            instruction_set_id: Some(InstructionSetId::new("claude-default")),
            composed_command: format!("claude {label}"),
            status: SessionStatus::Running,
            pid: Some(42),
            created_at: 1_700_000_000,
            tab_index: 0,
            temp_files: vec![TempFileSpec {
                path: dir.join("sp.md"),
                contents: "ctx".to_owned(),
            }],
            ai_session_id: None,
        }
    }

    // ----- discover_instructions ----------------------------------------

    #[test]
    fn discovery_picks_named_defaults() {
        let dir = TempDir::new().expect("td");
        touch(&dir.path().join("claude-default.md"), "c");
        touch(&dir.path().join("claude-other.md"), "x");
        touch(&dir.path().join("copilot-default.md"), "p");

        let sets = discover_instructions(dir.path()).expect("ok");
        let claude_default = sets.iter().find(|s| s.tool == Tool::Claude && s.is_default).expect("claude default");
        assert_eq!(claude_default.id.as_str(), "claude-default");
        let copilot_default = sets.iter().find(|s| s.tool == Tool::Copilot && s.is_default).expect("copilot default");
        assert_eq!(copilot_default.id.as_str(), "copilot-default");
        assert_eq!(sets.len(), 3);
    }

    #[test]
    fn discovery_falls_back_to_first_alphabetical_when_no_named_default() {
        let dir = TempDir::new().expect("td");
        touch(&dir.path().join("claude-other.md"), "x");
        touch(&dir.path().join("claude-zeta.md"), "y");

        let sets = discover_instructions(dir.path()).expect("ok");
        let default = sets.iter().find(|s| s.is_default).expect("a default exists");
        assert_eq!(default.id.as_str(), "claude-other");
    }

    #[test]
    fn discovery_skips_oversized_files() {
        let dir = TempDir::new().expect("td");
        let big = dir.path().join("claude-default.md");
        let mut f = File::create(&big).expect("create");
        let chunk = vec![b'x'; 1024];
        // Write 1 MiB + 1 byte so we cross the cap.
        for _ in 0..1024 {
            f.write_all(&chunk).expect("write");
        }
        f.write_all(b"x").expect("write last");
        f.sync_all().expect("sync");
        drop(f);

        let sets = discover_instructions(dir.path()).expect("ok");
        assert!(sets.is_empty(), "oversized file must be skipped, got {sets:?}",);
    }

    #[test]
    fn discovery_skips_files_without_known_prefix() {
        let dir = TempDir::new().expect("td");
        touch(&dir.path().join("readme.md"), "x");
        touch(&dir.path().join("notes.md"), "x");

        let sets = discover_instructions(dir.path()).expect("ok");
        assert!(sets.is_empty(), "no claude-/copilot- prefix → ignored");
    }

    #[cfg(unix)]
    #[test]
    fn discovery_rejects_symlink_pointing_outside_dir() {
        use std::os::unix::fs::symlink;
        let outside = TempDir::new().expect("outer");
        let target = outside.path().join("claude-evil.md");
        touch(&target, "x");

        let dir = TempDir::new().expect("inner");
        let link = dir.path().join("claude-default.md");
        symlink(&target, &link).expect("symlink");

        let sets = discover_instructions(dir.path()).expect("ok");
        assert!(sets.is_empty(), "symlink escaping instructionSetsDir must be skipped, got {sets:?}",);
    }

    // ----- ConfigStore: load/save ---------------------------------------

    #[test]
    fn load_config_returns_defaults_when_missing() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        // Fresh-install path seeds the built-in custom-process defs;
        // every other field must equal AppConfig::default().
        let mut expected = AppConfig::default();
        seed_default_custom_processes(&mut expected.custom_processes);
        assert_eq!(store.load_config(), expected);
    }

    #[test]
    fn malformed_config_is_quarantined_and_defaults_returned() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        fs::write(td.path().join(CONFIG_FILENAME), b"{not json").expect("write");

        let cfg = store.load_config();
        let mut expected = AppConfig::default();
        seed_default_custom_processes(&mut expected.custom_processes);
        assert_eq!(cfg, expected);

        let badfiles: Vec<_> = fs::read_dir(td.path())
            .expect("rd")
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with("config.json.bad-"))
            .collect();
        assert_eq!(badfiles.len(), 1, "expected exactly one quarantine file");
    }

    #[test]
    fn malformed_sessions_is_quarantined_and_empty_map_returned() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        fs::write(td.path().join(SESSIONS_FILENAME), b"###bad###").expect("write");

        let map = store.load_sessions();
        assert!(map.is_empty());

        let badfiles: Vec<_> = fs::read_dir(td.path())
            .expect("rd")
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with("sessions.json.bad-"))
            .collect();
        assert_eq!(badfiles.len(), 1);
    }

    #[test]
    fn try_load_sessions_returns_ok_empty_when_file_missing() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let map = store.try_load_sessions().expect("ok on missing file");
        assert!(map.is_empty());
    }

    #[test]
    fn try_load_sessions_surfaces_parse_failure_without_quarantine() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let path = td.path().join(SESSIONS_FILENAME);
        fs::write(&path, b"###bad###").expect("write");

        let err = store.try_load_sessions().expect_err("expected parse failure");
        assert!(matches!(err, Error::Internal(_)), "got {err:?}");

        // Strict variant must NOT quarantine — the caller (a destructive
        // operation) needs the file intact so it can be inspected/repaired.
        assert!(path.exists(), "try_load_sessions must not quarantine the bad file");
        let badfiles: Vec<_> = fs::read_dir(td.path())
            .expect("rd")
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with("sessions.json.bad-"))
            .collect();
        assert!(badfiles.is_empty(), "try_load_sessions must not produce quarantine files");
    }

    #[test]
    fn save_config_rejects_relative_instruction_dir() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let patch = PartialAppConfig {
            instruction_sets_dir: Some(PathBuf::from("relative/path")),
            ..Default::default()
        };
        let err = store.save_config(patch).expect_err("rejected");
        assert!(matches!(err, Error::InvalidPath(_)), "got {err:?}");
    }

    #[test]
    fn save_config_canonicalizes_instruction_dir() {
        let td = TempDir::new().expect("td");
        let store_dir = td.path().join("store");
        let inst_dir = td.path().join("instr");
        fs::create_dir_all(&inst_dir).expect("mkdir");
        let store = ConfigStore::open(&store_dir).expect("open");
        let patch = PartialAppConfig {
            instruction_sets_dir: Some(inst_dir.clone()),
            ..Default::default()
        };
        let cfg = store.save_config(patch).expect("ok");
        assert_eq!(cfg.instruction_sets_dir, canon(&inst_dir));
        assert_eq!(cfg.config_version, CONFIG_VERSION_CURRENT);
    }

    #[test]
    fn save_config_accepts_empty_instruction_dir_as_clear() {
        let td = TempDir::new().expect("td");
        let store_dir = td.path().join("store");
        let inst_dir = td.path().join("instr");
        fs::create_dir_all(&inst_dir).expect("mkdir");
        let store = ConfigStore::open(&store_dir).expect("open");
        // First set a non-empty dir...
        store
            .save_config(PartialAppConfig {
                instruction_sets_dir: Some(inst_dir.clone()),
                ..Default::default()
            })
            .expect("set");
        // ...then clear it with an empty PathBuf.
        let cfg = store
            .save_config(PartialAppConfig {
                instruction_sets_dir: Some(PathBuf::new()),
                ..Default::default()
            })
            .expect("clear");
        assert!(cfg.instruction_sets_dir.as_os_str().is_empty());
    }

    #[test]
    fn save_config_drops_bad_override_keys_silently() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let good = td.path().join("good-wt");
        fs::create_dir_all(&good).expect("mkdir");
        let mut overrides = BTreeMap::new();
        overrides.insert(good.to_string_lossy().into_owned(), vec!["nvm use".to_owned()]);
        // Absolute, but does not exist on disk.
        overrides.insert(td.path().join("ghost-wt").to_string_lossy().into_owned(), vec!["nope".to_owned()]);
        let patch = PartialAppConfig {
            worktree_prelaunch_commands: Some(overrides),
            ..Default::default()
        };
        let cfg = store.save_config(patch).expect("ok");
        assert_eq!(cfg.worktree_prelaunch_commands.len(), 1);
        let canon_good = canon(&good).to_string_lossy().into_owned();
        assert!(cfg.worktree_prelaunch_commands.contains_key(&canon_good));
    }

    #[test]
    fn merge_preserves_unspecified_fields() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");

        // First write: set prelaunch_commands.
        let first = store
            .save_config(PartialAppConfig {
                prelaunch_commands: Some(vec!["echo hi".to_owned()]),
                ..Default::default()
            })
            .expect("ok");
        assert_eq!(first.prelaunch_commands, vec!["echo hi".to_owned()]);

        // Second write: set tab_order only — prelaunch_commands must
        // survive.
        let id = SessionId::new();
        let second = store
            .save_config(PartialAppConfig {
                tab_order: Some(vec![id]),
                ..Default::default()
            })
            .expect("ok");
        assert_eq!(second.prelaunch_commands, vec!["echo hi".to_owned()]);
        assert_eq!(second.tab_order, vec![id]);
    }

    #[test]
    fn default_instruction_sets_deep_merges() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        store
            .save_config(PartialAppConfig {
                default_instruction_sets: Some(PartialDefaultInstructionSets {
                    claude: Some(InstructionSetId::new("claude-default")),
                    copilot: Some(InstructionSetId::new("copilot-default")),
                }),
                ..Default::default()
            })
            .expect("ok");

        let after = store
            .save_config(PartialAppConfig {
                default_instruction_sets: Some(PartialDefaultInstructionSets {
                    claude: Some(InstructionSetId::new("claude-other")),
                    copilot: None,
                }),
                ..Default::default()
            })
            .expect("ok");
        assert_eq!(after.default_instruction_sets.claude.as_str(), "claude-other");
        assert_eq!(
            after.default_instruction_sets.copilot.as_str(),
            "copilot-default",
            "copilot must survive when only claude is patched",
        );
    }

    #[test]
    fn missing_default_instruction_id_falls_back_to_discovered_default() {
        let td = TempDir::new().expect("td");
        let inst = td.path().join("instr");
        fs::create_dir_all(&inst).expect("mkdir");
        touch(&inst.join("claude-default.md"), "c");
        touch(&inst.join("copilot-default.md"), "p");

        let store = ConfigStore::open(td.path().join("store")).expect("open");
        // Hand-write a config.json that points at a non-existent claude
        // ID and the real instructionSetsDir.
        let canon_inst = canon(&inst);
        let raw = serde_json::json!({
            "configVersion": 1,
            "defaultInstructionSets": {
                "claude": "ghost",
                "copilot": "copilot-default"
            },
            "instructionSetsDir": canon_inst.to_string_lossy(),
            "worktreeRoots": [],
            "prelaunchCommands": [],
            "worktreePrelaunchCommands": {},
            "lastOpenSessions": [],
            "tabOrder": []
        });
        fs::write(store.config_path(), serde_json::to_vec_pretty(&raw).expect("ser")).expect("write");

        let cfg = store.load_config();
        assert_eq!(cfg.default_instruction_sets.claude.as_str(), "claude-default");
    }

    #[test]
    fn load_drops_invalid_per_worktree_override_keys() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let raw = serde_json::json!({
            "configVersion": 1,
            "defaultInstructionSets": { "claude": "", "copilot": "" },
            "instructionSetsDir": "",
            "worktreeRoots": [],
            "prelaunchCommands": [],
            "worktreePrelaunchCommands": {
                "/definitely/not/a/real/path/arborist-test": ["echo nope"]
            },
            "lastOpenSessions": [],
            "tabOrder": []
        });
        fs::write(store.config_path(), serde_json::to_vec_pretty(&raw).expect("ser")).expect("write");
        let cfg = store.load_config();
        assert!(cfg.worktree_prelaunch_commands.is_empty());
    }

    // ----- Session round-trip & status update ---------------------------

    #[test]
    fn session_round_trip_preserves_record() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let s = make_session(
            Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").expect("uuid"),
            "feat-x",
            td.path(),
        );
        store.save_session(&s).expect("save");
        let all = store.load_sessions();
        assert_eq!(all.get(&s.id), Some(&s));
    }

    #[test]
    fn remove_session_idempotent() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let s = make_session(Uuid::new_v4(), "x", td.path());
        store.save_session(&s).expect("save");
        store.remove_session(&s.id).expect("remove");
        store.remove_session(&s.id).expect("remove again");
        assert!(store.load_sessions().is_empty());
    }

    #[test]
    fn update_session_status_mutates_record() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let s = make_session(Uuid::new_v4(), "x", td.path());
        store.save_session(&s).expect("save");

        store.update_session_status(&s.id, SessionStatus::Exited, None).expect("update");
        let after = store.load_sessions();
        let updated = after.get(&s.id).expect("present");
        assert_eq!(updated.status, SessionStatus::Exited);
        assert_eq!(updated.pid, None);
    }

    #[test]
    fn update_session_status_returns_not_found_for_unknown_id() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let err = store
            .update_session_status(&SessionId::new(), SessionStatus::Exited, None)
            .expect_err("must fail");
        assert!(matches!(err, Error::NotFound(_)));
    }

    // ----- update_session_ai_session_id --------------------------------

    #[test]
    fn update_ai_session_id_persists_value() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let s = make_session(Uuid::new_v4(), "x", td.path());
        store.save_session(&s).expect("save");

        let changed = store.update_session_ai_session_id(&s.id, Some("ai-123".to_owned())).expect("update");
        assert!(changed, "first set must report a change");

        let after = store.load_sessions();
        assert_eq!(after.get(&s.id).expect("present").ai_session_id.as_deref(), Some("ai-123"),);
    }

    #[test]
    fn update_ai_session_id_is_idempotent() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let s = make_session(Uuid::new_v4(), "x", td.path());
        store.save_session(&s).expect("save");

        store.update_session_ai_session_id(&s.id, Some("same".to_owned())).expect("first update");
        let changed = store.update_session_ai_session_id(&s.id, Some("same".to_owned())).expect("second update");
        assert!(!changed, "no-op write must report no change");
    }

    #[test]
    fn update_ai_session_id_returns_not_found_for_unknown_id() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let err = store
            .update_session_ai_session_id(&SessionId::new(), Some("x".to_owned()))
            .expect_err("must fail");
        assert!(matches!(err, Error::NotFound(_)));
    }

    #[test]
    fn update_ai_session_id_can_clear_value() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let s = make_session(Uuid::new_v4(), "x", td.path());
        store.save_session(&s).expect("save");

        store.update_session_ai_session_id(&s.id, Some("ai-1".to_owned())).expect("set");
        let changed = store.update_session_ai_session_id(&s.id, None).expect("clear");
        assert!(changed);
        assert_eq!(store.load_sessions().get(&s.id).expect("present").ai_session_id, None,);
    }

    // ----- Copilot composed_command migration --------------------------

    #[test]
    fn load_sessions_strips_legacy_copilot_interactive_flag() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let id = SessionId(Uuid::parse_str("11111111-1111-1111-1111-111111111111").expect("uuid"));

        // Hand-write sessions.json with the legacy invocation a pre-fix
        // build would have persisted.
        let raw = serde_json::json!({
            id.to_string(): {
                "id": id,
                "tool": "copilot",
                "worktreePath": td.path(),
                "worktreeName": "feat-x",
                "label": "feat-x",
                "composedCommand": "echo hi && copilot --interactive \"context block\"",
                "status": "exited",
                "createdAt": 1_700_000_000_u64,
                "tabIndex": 0,
                "tempFiles": []
            }
        });
        fs::write(store.sessions_path(), serde_json::to_vec_pretty(&raw).expect("ser")).expect("write");

        let loaded = store.load_sessions();
        let session = loaded.get(&id).expect("present");
        assert_eq!(session.composed_command, "echo hi && copilot");
    }

    #[test]
    fn load_sessions_leaves_bare_copilot_alone() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let id = SessionId(Uuid::parse_str("22222222-2222-2222-2222-222222222222").expect("uuid"));
        let raw = serde_json::json!({
            id.to_string(): {
                "id": id,
                "tool": "copilot",
                "worktreePath": td.path(),
                "worktreeName": "feat-y",
                "label": "feat-y",
                "composedCommand": "copilot",
                "status": "exited",
                "createdAt": 1_700_000_000_u64,
                "tabIndex": 0,
                "tempFiles": []
            }
        });
        fs::write(store.sessions_path(), serde_json::to_vec_pretty(&raw).expect("ser")).expect("write");
        let loaded = store.load_sessions();
        assert_eq!(loaded.get(&id).expect("present").composed_command, "copilot");
    }

    #[test]
    fn load_sessions_does_not_touch_claude_sessions() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let id = SessionId(Uuid::parse_str("33333333-3333-3333-3333-333333333333").expect("uuid"));
        let raw = serde_json::json!({
            id.to_string(): {
                "id": id,
                "tool": "claude",
                "worktreePath": td.path(),
                "worktreeName": "feat-z",
                "label": "feat-z",
                "composedCommand": "claude --system-prompt /tmp/x.md",
                "status": "exited",
                "createdAt": 1_700_000_000_u64,
                "tabIndex": 0,
                "tempFiles": []
            }
        });
        fs::write(store.sessions_path(), serde_json::to_vec_pretty(&raw).expect("ser")).expect("write");
        let loaded = store.load_sessions();
        assert_eq!(loaded.get(&id).expect("present").composed_command, "claude --system-prompt /tmp/x.md");
    }

    // ----- Atomic write durability --------------------------------------

    #[test]
    fn atomic_write_leaves_old_file_intact_on_persist_failure() {
        // Simulate a "persist failure" by trying to persist into a path
        // whose parent directory is a *file*, not a dir. This is the
        // closest portable approximation of a cross-filesystem rename
        // failure we can produce without root.
        let td = TempDir::new().expect("td");
        let target = td.path().join("config.json");
        fs::write(&target, b"OLD").expect("seed");

        // Build a doomed write into a path nested under a file.
        let doomed = td.path().join("config.json").join("inner.json");
        let result = write_atomic(&doomed, &serde_json::json!({"x": 1}));
        assert!(result.is_err(), "write to invalid parent must fail");

        // Original file is untouched.
        let still = fs::read(&target).expect("read");
        assert_eq!(still, b"OLD");
    }

    #[test]
    fn atomic_write_replaces_target() {
        let td = TempDir::new().expect("td");
        let target = td.path().join("foo.json");
        fs::write(&target, b"OLD").expect("seed");
        write_atomic(&target, &serde_json::json!({"hello": "world"})).expect("ok");
        let raw = fs::read_to_string(&target).expect("read");
        let parsed: serde_json::Value = serde_json::from_str(&raw).expect("json");
        assert_eq!(parsed, serde_json::json!({"hello": "world"}));
    }

    // ----- active_session_id (Phase 7) ---------------------------------

    #[test]
    fn save_config_sets_and_clears_active_session_id() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let id = SessionId::new();

        // Set.
        let after_set = store
            .save_config(PartialAppConfig {
                active_session_id: Some(Some(id)),
                ..Default::default()
            })
            .expect("set");
        assert_eq!(after_set.active_session_id, Some(id));

        // Absent in patch → preserved.
        let after_noop = store
            .save_config(PartialAppConfig {
                prelaunch_commands: Some(vec!["echo hi".to_owned()]),
                ..Default::default()
            })
            .expect("noop");
        assert_eq!(after_noop.active_session_id, Some(id));

        // Clear via Some(None).
        let after_clear = store
            .save_config(PartialAppConfig {
                active_session_id: Some(None),
                ..Default::default()
            })
            .expect("clear");
        assert_eq!(after_clear.active_session_id, None);
    }

    #[test]
    fn load_config_migrates_v1_to_current() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let raw = serde_json::json!({
            "configVersion": 1,
            "defaultInstructionSets": { "claude": "", "copilot": "" },
            "instructionSetsDir": "",
            "worktreeRoots": [],
            "prelaunchCommands": [],
            "worktreePrelaunchCommands": {},
            "lastOpenSessions": [],
            "tabOrder": []
        });
        fs::write(store.config_path(), serde_json::to_vec_pretty(&raw).expect("ser")).expect("write");
        let cfg = store.load_config();
        assert_eq!(cfg.config_version, CONFIG_VERSION_CURRENT);
        assert_eq!(cfg.active_session_id, None);
        assert_eq!(cfg.workspace_root, None);
    }

    // ----- workspace_root (v3, Roadmap §1) ----------------------------

    #[test]
    fn save_config_sets_and_clears_workspace_root() {
        let td = TempDir::new().expect("td");
        let store_dir = td.path().join("store");
        let ws_dir = td.path().join("workspace");
        fs::create_dir_all(&ws_dir).expect("mk ws");
        let store = ConfigStore::open(&store_dir).expect("open");

        // Set.
        let after_set = store
            .save_config(PartialAppConfig {
                workspace_root: Some(Some(ws_dir.clone())),
                ..Default::default()
            })
            .expect("set");
        assert_eq!(after_set.workspace_root, Some(canon(&ws_dir)));

        // Absent → preserved.
        let after_noop = store
            .save_config(PartialAppConfig {
                prelaunch_commands: Some(vec!["echo hi".to_owned()]),
                ..Default::default()
            })
            .expect("noop");
        assert_eq!(after_noop.workspace_root, Some(canon(&ws_dir)));

        // Clear via Some(None).
        let after_clear = store
            .save_config(PartialAppConfig {
                workspace_root: Some(None),
                ..Default::default()
            })
            .expect("clear");
        assert_eq!(after_clear.workspace_root, None);
    }

    #[test]
    fn save_config_rejects_relative_workspace_root() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let err = store
            .save_config(PartialAppConfig {
                workspace_root: Some(Some(PathBuf::from("relative/path"))),
                ..Default::default()
            })
            .expect_err("rejected");
        assert!(matches!(err, Error::InvalidPath(_)), "got {err:?}");
    }

    #[test]
    fn save_config_rejects_workspace_root_that_is_not_a_dir() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let file = td.path().join("not-a-dir");
        fs::write(&file, b"x").expect("seed");
        let err = store
            .save_config(PartialAppConfig {
                workspace_root: Some(Some(file)),
                ..Default::default()
            })
            .expect_err("rejected");
        assert!(matches!(err, Error::InvalidPath(_)), "got {err:?}");
    }

    #[test]
    fn load_config_v2_with_single_worktree_root_promotes_to_workspace() {
        let td = TempDir::new().expect("td");
        let store_dir = td.path().join("store");
        let repo = td.path().join("repo");
        fs::create_dir_all(&repo).expect("mk repo");
        let store = ConfigStore::open(&store_dir).expect("open");
        let raw = serde_json::json!({
            "configVersion": 2,
            "defaultInstructionSets": { "claude": "", "copilot": "" },
            "instructionSetsDir": "",
            "worktreeRoots": [repo.to_string_lossy()],
            "prelaunchCommands": [],
            "worktreePrelaunchCommands": {},
            "lastOpenSessions": [],
            "tabOrder": [],
            "activeSessionId": null
        });
        fs::write(store.config_path(), serde_json::to_vec_pretty(&raw).expect("ser")).expect("write");
        let cfg = store.load_config();
        assert_eq!(cfg.config_version, CONFIG_VERSION_CURRENT);
        assert_eq!(cfg.workspace_root, Some(canon(&repo)));
    }

    #[test]
    fn load_config_v2_with_multiple_roots_does_not_promote_workspace() {
        let td = TempDir::new().expect("td");
        let store_dir = td.path().join("store");
        let r1 = td.path().join("r1");
        let r2 = td.path().join("r2");
        fs::create_dir_all(&r1).expect("mk r1");
        fs::create_dir_all(&r2).expect("mk r2");
        let store = ConfigStore::open(&store_dir).expect("open");
        let raw = serde_json::json!({
            "configVersion": 2,
            "defaultInstructionSets": { "claude": "", "copilot": "" },
            "instructionSetsDir": "",
            "worktreeRoots": [r1.to_string_lossy(), r2.to_string_lossy()],
            "prelaunchCommands": [],
            "worktreePrelaunchCommands": {},
            "lastOpenSessions": [],
            "tabOrder": [],
            "activeSessionId": null
        });
        fs::write(store.config_path(), serde_json::to_vec_pretty(&raw).expect("ser")).expect("write");
        let cfg = store.load_config();
        assert_eq!(cfg.workspace_root, None);
    }

    // ----- v3 → v4 migration: built-in custom-process seeding ----------

    #[test]
    fn load_config_migrates_v3_with_user_edited_shell_preserved() {
        // The v3 user has already customised the built-in `shell` (renamed
        // it, swapped to fish, disabled it). The v3→v4 seed pass must
        // NOT clobber the user's edits — only append the missing
        // built-ins (open-folder, vscode). This covers the `load_config`
        // integration path, complementing the
        // `seeding_is_additive_and_does_not_overwrite_user_edits` unit
        // test on the seeding helper itself.
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let raw = serde_json::json!({
            "configVersion": 3,
            "defaultInstructionSets": { "claude": "", "copilot": "" },
            "instructionSetsDir": "",
            "workspaceRoot": null,
            "worktreeRoots": [],
            "prelaunchCommands": [],
            "worktreePrelaunchCommands": {},
            "lastOpenSessions": [],
            "tabOrder": [],
            "activeSessionId": null,
            "customProcesses": [
                {
                    "id": BUILTIN_DEF_ID_SHELL,
                    "name": "My Fish",
                    "kind": "terminal",
                    "command": "fish -i",
                    "enabled": false
                }
            ]
        });
        fs::write(store.config_path(), serde_json::to_vec_pretty(&raw).expect("ser")).expect("write");
        let cfg = store.load_config();
        assert_eq!(cfg.config_version, CONFIG_VERSION_CURRENT);
        // Shell preserved verbatim.
        let shell = cfg
            .custom_processes
            .iter()
            .find(|d| d.id.as_str() == BUILTIN_DEF_ID_SHELL)
            .expect("shell def must remain");
        assert_eq!(shell.name, "My Fish");
        assert_eq!(shell.command, "fish -i");
        assert!(!shell.enabled, "user's enabled=false must survive seeding");
        // Other built-ins appended.
        let ids: Vec<_> = cfg.custom_processes.iter().map(|d| d.id.as_str().to_owned()).collect();
        assert!(
            ids.contains(&BUILTIN_DEF_ID_OPEN_FOLDER.to_owned()),
            "open-folder must be appended by v3→v4 migration"
        );
        assert!(
            ids.contains(&BUILTIN_DEF_ID_VSCODE.to_owned()),
            "vscode must be appended by v3→v4 migration"
        );
    }

    #[test]
    fn load_config_migrates_v3_and_seeds_default_custom_processes() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let raw = serde_json::json!({
            "configVersion": 3,
            "defaultInstructionSets": { "claude": "", "copilot": "" },
            "instructionSetsDir": "",
            "workspaceRoot": null,
            "worktreeRoots": [],
            "prelaunchCommands": [],
            "worktreePrelaunchCommands": {},
            "lastOpenSessions": [],
            "tabOrder": [],
            "activeSessionId": null
        });
        fs::write(store.config_path(), serde_json::to_vec_pretty(&raw).expect("ser")).expect("write");
        let cfg = store.load_config();
        assert_eq!(cfg.config_version, CONFIG_VERSION_CURRENT);
        let ids: Vec<_> = cfg.custom_processes.iter().map(|d| d.id.as_str().to_owned()).collect();
        assert_eq!(ids, vec!["shell", "open-folder", "vscode"]);
        assert!(cfg.last_open_sub_sessions.is_empty());
    }

    #[test]
    fn seeding_is_additive_and_does_not_overwrite_user_edits() {
        let user_edited = CustomProcessDef {
            id: CustomProcessDefId::new(BUILTIN_DEF_ID_SHELL),
            name: "My Custom Shell".to_owned(),
            kind: CustomProcessKind::Terminal,
            command: "fish -i".to_owned(),
            enabled: false,
            icon: None,
            icon_data_uri: None,
        };
        let mut defs = vec![user_edited.clone()];
        seed_default_custom_processes(&mut defs);
        // Shell entry preserved verbatim; the other two built-ins appended.
        assert_eq!(defs.len(), 3);
        assert_eq!(defs[0], user_edited);
        assert_eq!(defs[1].id.as_str(), BUILTIN_DEF_ID_OPEN_FOLDER);
        assert_eq!(defs[2].id.as_str(), BUILTIN_DEF_ID_VSCODE);
    }

    #[test]
    fn seeding_is_idempotent_when_called_repeatedly() {
        let mut defs = Vec::new();
        seed_default_custom_processes(&mut defs);
        let after_first = defs.clone();
        seed_default_custom_processes(&mut defs);
        assert_eq!(defs, after_first);
    }

    #[test]
    fn load_config_v4_does_not_reseed_after_user_deleted_a_builtin() {
        // User on v4 already has only `shell` (deleted vscode + open-folder
        // intentionally); the migration must not run again, so the deletes
        // stick.
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let raw = serde_json::json!({
            "configVersion": 4,
            "defaultInstructionSets": { "claude": "", "copilot": "" },
            "instructionSetsDir": "",
            "workspaceRoot": null,
            "worktreeRoots": [],
            "prelaunchCommands": [],
            "worktreePrelaunchCommands": {},
            "lastOpenSessions": [],
            "tabOrder": [],
            "activeSessionId": null,
            "customProcesses": [
                {
                    "id": "shell",
                    "name": "Shell",
                    "kind": "terminal",
                    "command": "sh -i",
                    "enabled": true
                }
            ],
            "lastOpenSubSessions": []
        });
        fs::write(store.config_path(), serde_json::to_vec_pretty(&raw).expect("ser")).expect("write");
        let cfg = store.load_config();
        let ids: Vec<_> = cfg.custom_processes.iter().map(|d| d.id.as_str().to_owned()).collect();
        assert_eq!(ids, vec!["shell"], "v4+ must not re-run the seed pass");
    }

    // ----- save_config: customProcesses validation --------------------

    #[test]
    fn save_config_rejects_custom_process_with_empty_id() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let err = store
            .save_config(PartialAppConfig {
                custom_processes: Some(vec![CustomProcessDef {
                    id: CustomProcessDefId::new(""),
                    name: "x".to_owned(),
                    kind: CustomProcessKind::Terminal,
                    command: "echo".to_owned(),
                    enabled: true,
                    icon: None,
                    icon_data_uri: None,
                }]),
                ..Default::default()
            })
            .expect_err("rejected");
        assert!(matches!(err, Error::InvalidCustomProcessDef(_)), "got {err:?}");
    }

    #[test]
    fn save_config_rejects_custom_process_with_invalid_id_chars() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let err = store
            .save_config(PartialAppConfig {
                custom_processes: Some(vec![CustomProcessDef {
                    id: CustomProcessDefId::new("has space"),
                    name: "x".to_owned(),
                    kind: CustomProcessKind::Terminal,
                    command: "echo".to_owned(),
                    enabled: true,
                    icon: None,
                    icon_data_uri: None,
                }]),
                ..Default::default()
            })
            .expect_err("rejected");
        assert!(matches!(err, Error::InvalidCustomProcessDef(_)), "got {err:?}");
    }

    #[test]
    fn save_config_rejects_custom_process_with_empty_command() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let err = store
            .save_config(PartialAppConfig {
                custom_processes: Some(vec![CustomProcessDef {
                    id: CustomProcessDefId::new("ok"),
                    name: "ok".to_owned(),
                    kind: CustomProcessKind::Terminal,
                    command: "   ".to_owned(),
                    enabled: true,
                    icon: None,
                    icon_data_uri: None,
                }]),
                ..Default::default()
            })
            .expect_err("rejected");
        assert!(matches!(err, Error::InvalidCustomProcessDef(_)), "got {err:?}");
    }

    #[test]
    fn save_config_rejects_duplicate_custom_process_ids() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let dup = CustomProcessDef {
            id: CustomProcessDefId::new("dup"),
            name: "x".to_owned(),
            kind: CustomProcessKind::Terminal,
            command: "echo".to_owned(),
            enabled: true,
            icon: None,
            icon_data_uri: None,
        };
        let err = store
            .save_config(PartialAppConfig {
                custom_processes: Some(vec![dup.clone(), dup]),
                ..Default::default()
            })
            .expect_err("rejected");
        assert!(matches!(err, Error::InvalidCustomProcessDef(_)), "got {err:?}");
    }

    #[test]
    fn save_config_round_trips_valid_custom_processes_and_records() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let parent = SessionId(Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").expect("uuid"));
        let sub = SubSessionRecord {
            id: SubSessionId(Uuid::parse_str("11111111-1111-1111-1111-111111111111").expect("uuid")),
            parent_session_id: parent,
            def_id: CustomProcessDefId::new("shell"),
            kind: CustomProcessKind::Terminal,
            label: "Shell".to_owned(),
            composed_command: "sh -i".to_owned(),
        };
        let def = CustomProcessDef {
            id: CustomProcessDefId::new("shell"),
            name: "Shell".to_owned(),
            kind: CustomProcessKind::Terminal,
            command: "sh -i".to_owned(),
            enabled: true,
            icon: None,
            icon_data_uri: None,
        };
        let after = store
            .save_config(PartialAppConfig {
                custom_processes: Some(vec![def.clone()]),
                last_open_sub_sessions: Some(vec![sub.clone()]),
                ..Default::default()
            })
            .expect("ok");
        assert_eq!(after.custom_processes, vec![def]);
        assert_eq!(after.last_open_sub_sessions, vec![sub]);
    }

    // ----- command_on_path probe ---------------------------------------

    #[test]
    #[serial_test::serial(env_path)]
    fn command_on_path_finds_seeded_executable() {
        let td = TempDir::new().expect("td");
        let dir = td.path();
        let stem = "arborist-probe-bin";
        let exe_name = if cfg!(target_os = "windows") {
            format!("{stem}.exe")
        } else {
            stem.to_owned()
        };
        let path = dir.join(&exe_name);
        fs::write(&path, b"#!/bin/sh\nexit 0\n").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&path).expect("meta").permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&path, perms).expect("chmod");
        }

        let original_path = std::env::var_os("PATH");
        let mut new_path = std::ffi::OsString::from(dir);
        if let Some(p) = original_path.clone() {
            #[cfg(unix)]
            new_path.push(":");
            #[cfg(windows)]
            new_path.push(";");
            new_path.push(p);
        }
        // Mutating env in tests is racy; this test is `#[serial]` to compensate.
        std::env::set_var("PATH", &new_path);
        let result = command_on_path(stem);
        match original_path {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }
        assert!(result, "{stem} should be discovered on the temporary PATH");
    }

    #[test]
    fn command_on_path_returns_false_for_unknown_binary() {
        assert!(!command_on_path("arborist-definitely-not-on-path-zzz-12345"));
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial(env_path)]
    fn command_on_path_rejects_non_executable_file_on_unix() {
        let td = TempDir::new().expect("td");
        let dir = td.path();
        let stem = "arborist-non-exec";
        let path = dir.join(stem);
        fs::write(&path, b"not really an executable\n").expect("write");
        // Deliberately leave default 0o644-ish perms (no exec bit).

        let original_path = std::env::var_os("PATH");
        let mut new_path = std::ffi::OsString::from(dir);
        if let Some(p) = original_path.clone() {
            new_path.push(":");
            new_path.push(p);
        }
        std::env::set_var("PATH", &new_path);
        let result = command_on_path(stem);
        match original_path {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }
        assert!(!result, "non-executable file with name {stem} must not be reported as on PATH");
    }

    // ----- Fresh-install + load-time hardening -------------------------

    #[test]
    fn load_config_seeds_defaults_when_file_missing() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let cfg = store.load_config();
        let ids: Vec<&str> = cfg.custom_processes.iter().map(|d| d.id.as_str()).collect();
        assert!(
            ids.contains(&BUILTIN_DEF_ID_SHELL),
            "fresh install must include {BUILTIN_DEF_ID_SHELL}, got {ids:?}"
        );
        assert!(
            ids.contains(&BUILTIN_DEF_ID_OPEN_FOLDER),
            "fresh install must include {BUILTIN_DEF_ID_OPEN_FOLDER}, got {ids:?}"
        );
        assert!(
            ids.contains(&BUILTIN_DEF_ID_VSCODE),
            "fresh install must include {BUILTIN_DEF_ID_VSCODE}, got {ids:?}"
        );
    }

    #[test]
    fn load_config_seeds_defaults_when_file_unparseable() {
        let td = TempDir::new().expect("td");
        // Write garbage as config.json so parse fails and triggers
        // quarantine-and-default. Defaults must still include built-ins.
        fs::write(td.path().join("config.json"), b"not json {{ ").expect("write");
        let store = ConfigStore::open(td.path()).expect("open");
        let cfg = store.load_config();
        assert!(!cfg.custom_processes.is_empty(), "defaults must seed");
    }

    #[test]
    fn load_config_quarantines_future_version() {
        let td = TempDir::new().expect("td");
        let path = td.path().join("config.json");
        let future = serde_json::json!({
            "configVersion": CONFIG_VERSION_CURRENT + 1,
            "defaultInstructionSets": { "claude": "", "copilot": "" },
            "instructionSetsDir": "",
            "worktreeRoots": [],
            "prelaunchCommands": [],
            "worktreePrelaunchCommands": {},
            "lastOpenSessions": [],
            "tabOrder": [],
            "futureUnknownField": "danger"
        });
        fs::write(&path, serde_json::to_string(&future).expect("ser")).expect("write");
        let store = ConfigStore::open(td.path()).expect("open");
        let cfg = store.load_config();
        assert_eq!(
            cfg.config_version, CONFIG_VERSION_CURRENT,
            "future-version config must be quarantined and replaced with defaults"
        );
        assert!(
            std::fs::read_dir(td.path())
                .expect("readdir")
                .filter_map(|e| e.ok())
                .any(|e| e.file_name().to_string_lossy().starts_with("config.json.bad-")),
            "quarantine file must exist alongside",
        );
    }

    #[test]
    fn load_config_drops_invalid_persisted_custom_processes() {
        let td = TempDir::new().expect("td");
        let path = td.path().join("config.json");
        // Hand-craft a v4 config with one valid, one empty-command, one
        // duplicate-id def. Sanitize on load should keep only the first
        // valid one.
        let crafted = serde_json::json!({
            "configVersion": CONFIG_VERSION_CURRENT,
            "defaultInstructionSets": { "claude": "", "copilot": "" },
            "instructionSetsDir": "",
            "worktreeRoots": [],
            "prelaunchCommands": [],
            "worktreePrelaunchCommands": {},
            "lastOpenSessions": [],
            "tabOrder": [],
            "customProcesses": [
                { "id": "good", "name": "Good", "kind": "terminal", "command": "sh", "enabled": true },
                { "id": "bad", "name": "Bad", "kind": "terminal", "command": "   ", "enabled": true },
                { "id": "good", "name": "Dup", "kind": "terminal", "command": "sh", "enabled": true },
                { "id": "has space", "name": "Space", "kind": "terminal", "command": "sh", "enabled": true }
            ],
            "lastOpenSubSessions": []
        });
        fs::write(&path, serde_json::to_string(&crafted).expect("ser")).expect("write");
        let store = ConfigStore::open(td.path()).expect("open");
        let cfg = store.load_config();
        let ids: Vec<&str> = cfg.custom_processes.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, vec!["good"], "only the first valid def must survive");
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    #[serial_test::serial(env_shell)]
    fn default_shell_command_falls_back_when_shell_env_is_suspicious() {
        let original = std::env::var_os("SHELL");
        // Suspicious: contains a metacharacter.
        std::env::set_var("SHELL", "/bin/sh; rm -rf /");
        let cmd = default_shell_command();
        match original {
            Some(v) => std::env::set_var("SHELL", v),
            None => std::env::remove_var("SHELL"),
        }
        assert_eq!(cmd, "sh -i", "metacharacter in $SHELL must trigger fallback");
    }

    // ----- from_layout --------------------------------------------------

    /// `from_layout` must round-trip a save/load on the per-(branch,
    /// workspace) settings path resolved from the layout, and the
    /// resulting store must expose its layout via [`ConfigStore::layout`]
    /// so callers (seed-on-first-launch, in-app workspace switch) can
    /// reach auxiliary paths like `lock_path()` and
    /// `legacy_config_path()`.
    #[test]
    fn from_layout_writes_under_layout_workspace_dir() {
        let app_data = TempDir::new().expect("app_data");
        let workspace = TempDir::new().expect("workspace");
        let workspace_canon = canon(workspace.path());

        // Branch build → settings live under
        // `<app_data>/branches/feature-x/workspaces/<key>/config.json`.
        let root = crate::store_layout::StoreRoot::new(app_data.path().to_path_buf(), "feature-x".to_owned());
        let workspace_canon_typed = crate::store_layout::CanonicalPath::assume_canonical(workspace_canon.clone());
        let layout = root.for_workspace(&workspace_canon_typed);
        let expected_settings_path = layout.settings_path();
        let expected_workspace_dir = layout.workspace_dir();

        let store = ConfigStore::from_layout(layout.clone()).expect("from_layout");

        // The store's directory is exactly the layout's workspace dir.
        assert_eq!(store.dir(), expected_workspace_dir.as_path());
        // The retained layout points back at the same workspace.
        let retained = store.layout().expect("layout retained");
        assert_eq!(retained.workspace().as_path(), workspace_canon.as_path());
        assert_eq!(retained.settings_path(), expected_settings_path);

        // Round-trip: a save persists to the layout's settings path.
        let partial = PartialAppConfig {
            config_version: Some(CONFIG_VERSION_CURRENT),
            ..PartialAppConfig::default()
        };
        store.save_config(partial).expect("save");
        assert!(
            expected_settings_path.exists(),
            "save_config should have written {}",
            expected_settings_path.display(),
        );
        // And no file leaked into the legacy top-level path.
        assert!(
            !root.legacy_config_path().exists(),
            "from_layout must not write to the legacy top-level path",
        );
    }

    /// Two `from_layout` stores with identical (branch, workspace)
    /// inputs must resolve to the same directory — proves the workspace
    /// key is deterministic and that branch builds in the same
    /// workspace see one shared on-disk state.
    #[test]
    fn from_layout_is_deterministic_for_same_inputs() {
        let app_data = TempDir::new().expect("app_data");
        let workspace = TempDir::new().expect("workspace");
        let canonical = crate::store_layout::CanonicalPath::assume_canonical(canon(workspace.path()));

        let root_a = crate::store_layout::StoreRoot::new(app_data.path().to_path_buf(), "feature-x".to_owned());
        let root_b = root_a.clone();
        let store_a = ConfigStore::from_layout(root_a.for_workspace(&canonical)).expect("a");
        let store_b = ConfigStore::from_layout(root_b.for_workspace(&canonical)).expect("b");
        assert_eq!(store_a.dir(), store_b.dir());
    }

    /// Different workspaces under the same branch must produce
    /// distinct directories — proves isolation between sibling
    /// workspaces is structural, not best-effort.
    #[test]
    fn from_layout_isolates_distinct_workspaces() {
        let app_data = TempDir::new().expect("app_data");
        let ws_a = TempDir::new().expect("ws_a");
        let ws_b = TempDir::new().expect("ws_b");

        let root = crate::store_layout::StoreRoot::new(app_data.path().to_path_buf(), "feature-x".to_owned());
        let store_a =
            ConfigStore::from_layout(root.for_workspace(&crate::store_layout::CanonicalPath::assume_canonical(canon(ws_a.path())))).expect("a");
        let store_b =
            ConfigStore::from_layout(root.for_workspace(&crate::store_layout::CanonicalPath::assume_canonical(canon(ws_b.path())))).expect("b");
        assert_ne!(store_a.dir(), store_b.dir(), "distinct workspaces must isolate their stores",);
    }
}
