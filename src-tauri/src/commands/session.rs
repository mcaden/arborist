//! Phase 7 session lifecycle commands.
//!
//! This module hosts the **business-logic** functions (`*_impl`) for every
//! `session_*` command listed in DESIGN §6. The thin `#[tauri::command]`
//! wrappers in [`super`] forward into these so the integration tests can
//! exercise the same code paths without spinning up Tauri.
//!
//! ## AppContext
//!
//! Every impl takes an [`AppContext`] borrowed reference. In production a
//! single `Arc<AppContext>` is built in `tauri::Builder::setup` and stored
//! via `app.manage(...)`; tests build their own with a [`FakePtySpawner`]
//! and an output-capturing [`PtySink`].
//!
//! ## Status emission rule
//!
//! `Starting` is emitted **here**, synchronously, before we hand off to the
//! pool. The pool emits `Running` (with PID) at the end of its spawn
//! sequence. Both flow through the same [`PtySink::status`] callback, so
//! the on-disk session record converges automatically when the production
//! sink is wired (see [`crate::lib::run`]).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tracing::{debug, info, warn};

use crate::compose::{self, ComposeInputs};
use crate::config_store::{
    discover_instructions, list_instructions_for, ConfigStore, MAX_INSTRUCTION_FILE_BYTES,
};
use crate::git::{GitRunner, RealGitRunner};
use crate::pty_pool::{cleanup_orphans, PtyPool, PtySink};
use crate::types::{
    AppError, Error, InstructionSet, PartialAppConfig, Session, SessionCreateArgs, SessionId,
    SessionInputArgs, SessionResizeArgs, SessionStatus, SessionView, Tool,
};

/// Wiring shared by every Phase 7 session command. Built once at startup
/// (production) or per-test (integration tests).
pub struct AppContext {
    pub pool: Arc<PtyPool>,
    pub store: ConfigStore,
    pub sink: PtySink,
    /// Injected git seam (Phase 10). Production wires [`RealGitRunner`];
    /// tests pass a fake to avoid depending on the real `git` binary.
    pub git_runner: Arc<dyn GitRunner>,
    /// Restore-on-launch is a one-shot operation gated on the frontend
    /// signalling readiness. We use a CAS instead of a mutex so a frenzied
    /// frontend that calls `frontend_ready` more than once cannot trigger
    /// duplicate restores.
    pub restored: AtomicBool,
}

impl AppContext {
    #[must_use]
    pub fn new(
        pool: Arc<PtyPool>,
        store: ConfigStore,
        sink: PtySink,
        git_runner: Arc<dyn GitRunner>,
    ) -> Self {
        Self {
            pool,
            store,
            sink,
            git_runner,
            restored: AtomicBool::new(false),
        }
    }

    /// Convenience constructor for call sites (notably integration tests
    /// from earlier phases) that don't care about git discovery — defaults
    /// to the real runner. New tests should prefer [`Self::new`] with a
    /// fake [`GitRunner`].
    #[must_use]
    pub fn with_real_git(pool: Arc<PtyPool>, store: ConfigStore, sink: PtySink) -> Self {
        Self::new(pool, store, sink, Arc::new(RealGitRunner))
    }
}

// ---------------------------------------------------------------------------
// session_create
// ---------------------------------------------------------------------------

/// Create a new session, materialise its temp files, persist it, and spawn
/// the PTY child. Returns the [`SessionView`] the frontend can stash in its
/// store.
pub fn session_create_impl(
    ctx: &AppContext,
    args: SessionCreateArgs,
) -> Result<SessionView, AppError> {
    // 1. Validate worktree (canonicalises; rejects relative/missing).
    let worktree = compose::validate_worktree(&args.worktree_path).map_err(AppError::from)?;

    // 2. Resolve instruction set & enforce tool match.
    let cfg = ctx.store.load_config();
    let set = lookup_instruction_set(&cfg, &args.instruction_set_id, args.tool)?;

    // 3. Read instruction file contents (re-checking the size cap because
    //    `discover_instructions` *skips* oversized files but a user could
    //    have plumbed an ID through that bypassed discovery).
    let contents = read_instruction_file(&set.file_path)?;

    // 4. Derive a non-colliding label from the worktree basename.
    let existing_sessions = ctx.store.load_sessions();
    let existing_labels: Vec<&str> = existing_sessions
        .values()
        .map(|s| s.label.as_str())
        .collect();
    let basename = worktree
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "session".to_owned());
    let label = compose::dedupe_label(&existing_labels, &basename);

    // 5. Compose command + temp files.
    let session_id = SessionId::new();
    let composed = compose::compose_command(&ComposeInputs {
        session_id,
        tool: args.tool,
        worktree_path: &worktree,
        worktree_label: &label,
        instruction_set: &set,
        prelaunch_commands: prelaunch_for(&cfg, &worktree),
        instruction_set_contents: &contents,
    })
    .map_err(AppError::from)?;

    // 6. Materialise temp files on disk.
    materialise_temp_files(&composed.temp_files)?;

    // 7. Build the persisted record. `tab_index` puts the new session at
    //    the end of the current order.
    let tab_index = ctx.store.load_config().tab_order.len().min(usize::MAX - 1);
    let session = Session {
        id: session_id,
        tool: args.tool,
        worktree_path: worktree.clone(),
        worktree_name: basename,
        label: label.clone(),
        instruction_set_id: args.instruction_set_id.clone(),
        composed_command: composed.composed_command,
        status: SessionStatus::Starting,
        pid: None,
        created_at: now_unix_seconds(),
        tab_index,
        temp_files: composed.temp_files,
    };

    // 8. Persist before spawning so a crash mid-spawn still leaves an
    //    auditable record we can clean up on restart.
    ctx.store.save_session(&session).map_err(AppError::from)?;

    // 9. Append to lastOpenSessions / tabOrder.
    let mut last = cfg.last_open_sessions.clone();
    if !last.contains(&session.id) {
        last.push(session.id);
    }
    let mut order = cfg.tab_order.clone();
    if !order.contains(&session.id) {
        order.push(session.id);
    }
    ctx.store
        .save_config(PartialAppConfig {
            last_open_sessions: Some(last),
            tab_order: Some(order),
            active_session_id: Some(Some(session.id)),
            ..Default::default()
        })
        .map_err(AppError::from)?;

    // 10. Announce Starting (synchronous; the pool will follow with Running).
    (ctx.sink.status)(&session.id, SessionStatus::Starting, None);

    // 11. Spawn. The pool emits Running with PID through the same sink.
    let pid = ctx
        .pool
        .spawn(&session, ctx.sink.clone())
        .map_err(AppError::from)?;

    info!(session_id = %session.id, pid, label = %label, "session created");

    let mut view = SessionView::from(&session);
    view.status = SessionStatus::Running;
    view.pid = Some(pid);
    Ok(view)
}

// ---------------------------------------------------------------------------
// session_list
// ---------------------------------------------------------------------------

pub fn session_list_impl(ctx: &AppContext) -> Result<Vec<SessionView>, AppError> {
    let mut sessions: Vec<Session> = ctx.store.load_sessions().into_values().collect();
    sessions.sort_by_key(|s| s.tab_index);
    Ok(sessions.iter().map(SessionView::from).collect())
}

// ---------------------------------------------------------------------------
// session_close
// ---------------------------------------------------------------------------

pub async fn session_close_impl(ctx: &AppContext, id: SessionId) -> Result<(), AppError> {
    // 1. Best-effort kill (NotFound from the pool is fine — the session
    //    may have exited on its own already).
    if ctx.pool.contains(&id) {
        ctx.pool.kill(&id).await.map_err(AppError::from)?;
    }

    // 2. Belt-and-braces temp-dir cleanup. `pool.kill` also does this when
    //    the session is live; we re-attempt for the "already exited" path.
    let dir = compose::session_temp_dir(&id);
    if dir.exists() {
        if let Err(e) = std::fs::remove_dir_all(&dir) {
            debug!(session_id = %id, error = %e, "session temp dir removal failed (post-close)");
        }
    }

    // 3. Drop the persisted record.
    ctx.store.remove_session(&id).map_err(AppError::from)?;

    // 4. Trim AppConfig ordering & active selection.
    let cfg = ctx.store.load_config();
    let new_last: Vec<SessionId> = cfg
        .last_open_sessions
        .iter()
        .copied()
        .filter(|s| s != &id)
        .collect();
    let new_order: Vec<SessionId> = cfg.tab_order.iter().copied().filter(|s| s != &id).collect();
    let active_patch: Option<Option<SessionId>> = match cfg.active_session_id {
        Some(active) if active == id => Some(new_order.first().copied()),
        _ => None,
    };
    ctx.store
        .save_config(PartialAppConfig {
            last_open_sessions: Some(new_last),
            tab_order: Some(new_order),
            active_session_id: active_patch,
            ..Default::default()
        })
        .map_err(AppError::from)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// session_focus
// ---------------------------------------------------------------------------

pub fn session_focus_impl(ctx: &AppContext, id: SessionId) -> Result<(), AppError> {
    let sessions = ctx.store.load_sessions();
    if !sessions.contains_key(&id) {
        return Err(AppError::from(Error::NotFound(format!(
            "session {id} not found"
        ))));
    }
    ctx.store
        .save_config(PartialAppConfig {
            active_session_id: Some(Some(id)),
            ..Default::default()
        })
        .map_err(AppError::from)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// session_resize / session_input
// ---------------------------------------------------------------------------

pub fn session_resize_impl(ctx: &AppContext, args: SessionResizeArgs) -> Result<(), AppError> {
    ctx.pool
        .resize(&args.session_id, args.cols, args.rows)
        .map_err(AppError::from)
}

pub fn session_input_impl(ctx: &AppContext, args: SessionInputArgs) -> Result<(), AppError> {
    ctx.pool
        .write(&args.session_id, args.data.as_bytes())
        .map_err(AppError::from)
}

// ---------------------------------------------------------------------------
// session_restart
// ---------------------------------------------------------------------------

pub fn session_restart_impl(ctx: &AppContext, id: SessionId) -> Result<(), AppError> {
    let sessions = ctx.store.load_sessions();
    let session = sessions
        .get(&id)
        .cloned()
        .ok_or_else(|| AppError::from(Error::NotFound(format!("session {id} not found"))))?;

    // Re-materialise temp files in case they were deleted (e.g. by a prior
    // close path that ran while the session was still open in another
    // window — defensive). Composed command is reused verbatim per
    // DESIGN §5.4 — *never* recompose at restart time.
    materialise_temp_files(&session.temp_files)?;

    // Mark Starting in the persisted record up front so a UI poll right
    // after restart doesn't see stale Running/pid.
    ctx.store
        .update_session_status(&id, SessionStatus::Starting, None)
        .map_err(AppError::from)?;
    (ctx.sink.status)(&id, SessionStatus::Starting, None);

    ctx.pool
        .respawn_existing(&session, ctx.sink.clone())
        .map_err(AppError::from)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// frontend_ready / restore_all_sessions
// ---------------------------------------------------------------------------

/// Idempotent: returns `true` if this call won the CAS and triggered the
/// restore path, `false` if restore had already been kicked off.
pub fn frontend_ready_impl(ctx: &AppContext) -> bool {
    ctx.restored
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
}

/// Re-spawn every persisted session. Called once after the frontend signals
/// readiness. Failures on individual sessions are logged but do not abort
/// the rest — a single broken session must not strand the whole app.
pub fn restore_all_sessions(ctx: &AppContext) {
    let sessions = ctx.store.load_sessions();
    let ids: Vec<SessionId> = sessions.keys().copied().collect();

    // Sweep stale temp dirs whose UUIDs no longer correspond to any
    // persisted session. Stale dirs whose UUID *is* still persisted are
    // intentionally kept (DESIGN §5.6 / Phase 6 spec).
    if let Err(e) = cleanup_orphans(&ids) {
        warn!(error = %e, "cleanup_orphans failed during restore");
    }

    for (id, session) in sessions {
        // Re-materialise temp files in case they were swept by an OS-level
        // tmp clean. `respawn_existing` reuses `composed_command` verbatim.
        if let Err(e) = materialise_temp_files(&session.temp_files) {
            warn!(session_id = %id, error = ?e, "restore: temp-file materialise failed");
            continue;
        }

        if let Err(e) = ctx
            .store
            .update_session_status(&id, SessionStatus::Starting, None)
        {
            warn!(session_id = %id, error = ?e, "restore: status update failed");
            continue;
        }
        (ctx.sink.status)(&id, SessionStatus::Starting, None);

        match ctx.pool.respawn_existing(&session, ctx.sink.clone()) {
            Ok(pid) => {
                info!(session_id = %id, pid, "restored session");
            }
            Err(e) => {
                warn!(session_id = %id, error = ?e, "restore: respawn failed");
                // Best-effort: surface as Error status so the UI can show it.
                let _ = ctx
                    .store
                    .update_session_status(&id, SessionStatus::Error, None);
                (ctx.sink.status)(&id, SessionStatus::Error, None);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// worktrees_list (Phase 10)
// ---------------------------------------------------------------------------

/// Enumerate worktrees rooted at `repo_root`. Returns `Ok(vec![])` on any
/// failure (missing dir, not a repo, git unavailable) — graceful
/// degradation lets the frontend always fall back to the manual "Browse…"
/// button without surfacing an error toast.
pub fn worktrees_list_impl(
    ctx: &AppContext,
    repo_root: &std::path::Path,
) -> Result<Vec<crate::types::WorktreeInfo>, AppError> {
    if !repo_root.is_dir() {
        debug!(
            code = "GitUnavailable",
            repo_root = %repo_root.display(),
            "worktrees_list: repo_root not a directory; returning empty list"
        );
        return Ok(Vec::new());
    }
    ctx.git_runner
        .list_worktrees(repo_root)
        .map_err(AppError::from)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Look up an instruction set by ID, validating that its tool matches the
/// requested one.
fn lookup_instruction_set(
    cfg: &crate::types::AppConfig,
    id: &crate::types::InstructionSetId,
    tool: Tool,
) -> Result<InstructionSet, AppError> {
    // Use the same discovery the frontend sees so behaviour is consistent
    // (in particular, oversize and symlink-out-of-dir filtering).
    let sets: Vec<InstructionSet> = if cfg.instruction_sets_dir.as_os_str().is_empty() {
        Vec::new()
    } else {
        discover_instructions(&cfg.instruction_sets_dir).map_err(AppError::from)?
    };
    let Some(set) = sets.into_iter().find(|s| &s.id == id) else {
        // Fall back to the helper that surfaces a friendly error shape.
        // `list_instructions_for` may further filter; if absent, NotFound.
        let helper = list_instructions_for(cfg).unwrap_or_default();
        if let Some(s) = helper.into_iter().find(|s| &s.id == id) {
            if s.tool != tool {
                return Err(AppError::from(Error::ToolMismatch(format!(
                    "instruction set {id} is for {:?}, not {tool:?}",
                    s.tool
                ))));
            }
            return Ok(s);
        }
        return Err(AppError::from(Error::NotFound(format!(
            "instruction set {id} not found"
        ))));
    };
    if set.tool != tool {
        return Err(AppError::from(Error::ToolMismatch(format!(
            "instruction set {id} is for {:?}, not {tool:?}",
            set.tool
        ))));
    }
    Ok(set)
}

fn read_instruction_file(path: &std::path::Path) -> Result<String, AppError> {
    let meta = std::fs::metadata(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => AppError::from(Error::InstructionFileMissing(path.into())),
        _ => AppError::from(Error::Io(e)),
    })?;
    if meta.len() > MAX_INSTRUCTION_FILE_BYTES {
        return Err(AppError::from(Error::InstructionFileTooLarge(path.into())));
    }
    std::fs::read_to_string(path).map_err(|e| AppError::from(Error::Io(e)))
}

fn prelaunch_for<'a>(cfg: &'a crate::types::AppConfig, worktree: &std::path::Path) -> &'a [String] {
    let key = worktree.to_string_lossy().into_owned();
    cfg.worktree_prelaunch_commands
        .get(&key)
        .map(Vec::as_slice)
        .unwrap_or(cfg.prelaunch_commands.as_slice())
}

fn materialise_temp_files(files: &[crate::types::TempFileSpec]) -> Result<(), AppError> {
    for f in files {
        if let Some(parent) = f.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| AppError::from(Error::Io(e)))?;
        }
        std::fs::write(&f.path, &f.contents).map_err(|e| AppError::from(Error::Io(e)))?;
    }
    Ok(())
}

fn now_unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// Re-export some common path types so call sites don't have to remember the
// import paths.
pub use std::path::Path as _SessionPath;
#[allow(dead_code)]
type _PathBufAlias = PathBuf;
