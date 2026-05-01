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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use portable_pty::PtySize;
use tracing::{debug, info, warn};

use crate::compose::{self, ComposeInputs};
use crate::config_store::{
    discover_instructions, list_instructions_for, ConfigStore, MAX_INSTRUCTION_FILE_BYTES,
};
use crate::git::{GitRunner, RealGitRunner};
use crate::pty_pool::{cleanup_orphans, PtyPool, PtySink};
use crate::session_metrics::{AiSessionDiscoveryCb, MetricsCb, MetricsRegistry, TurnCb};
use crate::types::{
    AppError, Error, InstructionSet, PartialAppConfig, Session, SessionCloseResult,
    SessionCreateArgs, SessionId, SessionInputArgs, SessionResizeArgs, SessionRestartArgs,
    SessionStatus, SessionView, Tool,
};
use crate::workspace_scope::WorkspaceScope;

/// Wiring shared by every Phase 7 session command. Built once at startup
/// (production) or per-test (integration tests).
pub struct AppContext {
    pub pool: Arc<PtyPool>,
    /// Per-(branch, workspace) binding. Held behind an `RwLock` so the
    /// in-app workspace switch (phase 7) can transactionally swap the
    /// entire scope under a write lock without any caller seeing a
    /// torn intermediate state. Read-side callers should prefer the
    /// [`Self::store`] helper below — it takes a brief read lock and
    /// returns an owned [`ConfigStore`] clone, never holding the
    /// lock across a downstream operation.
    pub workspace: Arc<RwLock<WorkspaceScope>>,
    pub sink: PtySink,
    /// Injected git seam (Phase 10). Production wires [`RealGitRunner`];
    /// tests pass a fake to avoid depending on the real `git` binary.
    pub git_runner: Arc<dyn GitRunner>,
    /// Restore-on-launch is a one-shot operation gated on the frontend
    /// signalling readiness. We use a CAS instead of a mutex so a frenzied
    /// frontend that calls `frontend_ready` more than once cannot trigger
    /// duplicate restores.
    pub restored: AtomicBool,
    /// Per-session token-usage / context-window watcher pool (Issue #3).
    /// A no-op for sessions whose tool isn't supported; see
    /// [`crate::session_metrics`].
    pub metrics: Arc<MetricsRegistry>,
    /// Callback the metrics watchers invoke for every new snapshot.
    /// Production wires this into `app.emit("session://metrics", …)`;
    /// tests substitute a capturing closure.
    pub metrics_emit: MetricsCb,
    /// Callback the metrics watchers invoke when they discover (or learn
    /// of a change to) the AI-side session id. Production wires this to
    /// persist via `ConfigStore::update_session_ai_session_id` so the next
    /// app-restart restore can `--resume <id>`. Tests substitute a
    /// capturing closure.
    pub ai_session_discover: AiSessionDiscoveryCb,
    /// Callback the metrics watchers invoke when an agent turn completes
    /// (Copilot OTel `invoke_agent` close; Claude transcript assistant
    /// line). Production wires this into the existing
    /// `session://activity` channel as a `TurnEnd` variant; tests
    /// substitute a capturing closure.
    pub turn_emit: TurnCb,
    /// Sessions that have been persisted but not yet PTY-spawned. Used by
    /// `restore_all_sessions` to defer the actual `pool.spawn` until the
    /// frontend reports the real terminal dimensions via `session_resize`.
    /// The first `session_resize` for a pending id atomically claims it
    /// (removing it from the map) and triggers the spawn at the right
    /// size — so the CLI's first paint never happens at the wrong width.
    /// The map value is the spawn-ready `Session` (already augmented with
    /// `--resume <ai-session-id>` if applicable) so the claim path doesn't
    /// have to re-derive it. `Mutex<HashMap>` (not `RwLock`) because the
    /// only access pattern is a single-step claim (`remove`) under the same
    /// lock as the membership check, which a `RwLock` cannot give us
    /// atomically.
    pub pending_spawn: Arc<Mutex<HashMap<SessionId, Session>>>,
    /// Phase 7 in-app workspace switch — serialises concurrent
    /// `workspace_switch` invocations so the close-all + bind + swap
    /// pipeline cannot interleave with itself. Held across `.await`s
    /// inside `workspace_switch_impl`, hence `tokio::sync::Mutex`.
    pub switch_serialise: tokio::sync::Mutex<()>,
    /// Phase 7 in-app workspace switch — set to `true` while a switch
    /// is mid-pipeline (after `switch_serialise` is acquired and before
    /// the swap commits). Other workspace-mutating commands
    /// (`session_create`, `session_restart`, `frontend_ready`) check
    /// this flag and return [`AppError::WorkspaceSwitchInProgress`]
    /// if set. Read-only / per-session commands (`session_input`,
    /// `session_resize`, `session_close`) intentionally pass — they
    /// must succeed during teardown so the close-all loop in the
    /// switch can complete, and they cannot create *new* orphan state.
    pub switch_in_progress: AtomicBool,
}

impl AppContext {
    #[must_use]
    pub fn new(
        pool: Arc<PtyPool>,
        store: ConfigStore,
        sink: PtySink,
        git_runner: Arc<dyn GitRunner>,
        metrics_emit: MetricsCb,
        ai_session_discover: AiSessionDiscoveryCb,
        turn_emit: TurnCb,
    ) -> Self {
        // Production code that wants to bind to a real (branch,
        // workspace) tuple should use [`Self::with_workspace`] so the
        // OS-level lock guard is held for the lifetime of the context.
        // This constructor wraps the supplied store in a
        // [`WorkspaceScope::for_test`] (no OS lock) — historically all
        // callers were tests and the boot path, and the boot path will
        // migrate to `with_workspace` in phase 6.
        let scope = WorkspaceScope::for_test(store, None);
        Self::with_workspace_internal(
            pool,
            Arc::new(RwLock::new(scope)),
            sink,
            git_runner,
            metrics_emit,
            ai_session_discover,
            turn_emit,
        )
    }

    /// Production constructor (phase 6 boot wiring): bind the context
    /// to an already-acquired [`WorkspaceScope`] held behind the
    /// shared `RwLock` that workspace-switch (phase 7) will mutate.
    #[must_use]
    pub fn with_workspace(
        pool: Arc<PtyPool>,
        workspace: Arc<RwLock<WorkspaceScope>>,
        sink: PtySink,
        git_runner: Arc<dyn GitRunner>,
        metrics_emit: MetricsCb,
        ai_session_discover: AiSessionDiscoveryCb,
        turn_emit: TurnCb,
    ) -> Self {
        Self::with_workspace_internal(
            pool,
            workspace,
            sink,
            git_runner,
            metrics_emit,
            ai_session_discover,
            turn_emit,
        )
    }

    fn with_workspace_internal(
        pool: Arc<PtyPool>,
        workspace: Arc<RwLock<WorkspaceScope>>,
        sink: PtySink,
        git_runner: Arc<dyn GitRunner>,
        metrics_emit: MetricsCb,
        ai_session_discover: AiSessionDiscoveryCb,
        turn_emit: TurnCb,
    ) -> Self {
        Self {
            pool,
            workspace,
            sink,
            git_runner,
            restored: AtomicBool::new(false),
            metrics: Arc::new(MetricsRegistry::new()),
            metrics_emit,
            ai_session_discover,
            turn_emit,
            pending_spawn: Arc::new(Mutex::new(HashMap::new())),
            switch_serialise: tokio::sync::Mutex::new(()),
            switch_in_progress: AtomicBool::new(false),
        }
    }

    /// Convenience constructor for call sites (notably integration tests
    /// from earlier phases) that don't care about git discovery — defaults
    /// to the real runner. New tests should prefer [`Self::new`] with a
    /// fake [`GitRunner`].
    #[must_use]
    pub fn with_real_git(pool: Arc<PtyPool>, store: ConfigStore, sink: PtySink) -> Self {
        Self::new(
            pool,
            store,
            sink,
            Arc::new(RealGitRunner),
            Arc::new(|_| {}),
            Arc::new(|_, _| {}),
            Arc::new(|_, _| {}),
        )
    }

    /// Snapshot the current [`ConfigStore`] for this context. Cheap
    /// (read lock + `Arc` clone). Never holds the workspace lock
    /// across a downstream operation — call this once per command,
    /// then operate on the returned owned handle.
    ///
    /// Will `panic!` if the workspace lock is poisoned (which can only
    /// happen if a writer panicked mid-mutation; recovery is
    /// impossible because the swap is not idempotent).
    #[must_use]
    pub fn store(&self) -> ConfigStore {
        self.workspace
            .read()
            .expect("workspace lock poisoned")
            .store
            .clone()
    }
}

// ---------------------------------------------------------------------------
// session_create
// ---------------------------------------------------------------------------

/// Reject zero-sized PTY dimensions at the command boundary. Raw `u16`
/// allows `0`; passing `PtySize { cols: 0, rows: 0, ... }` to
/// `portable_pty::openpty` fails with an opaque OS error on the spawn
/// thread. We catch it here so the frontend gets a stable, branchable
/// error code (`InvalidArgs`) it can surface as a real diagnostic
/// instead of a generic "PTY spawn failed".
///
/// In normal use, [`crate::types::SessionCreateArgs`]/`SessionResizeArgs`
/// are populated by the frontend's `measureInitialPtyDimensions` /
/// `getTerminalDimensions`, which clamp upward. This guard is purely
/// defensive against a future refactor that bypasses those helpers, or
/// a buggy direct caller.
fn validate_pty_dims(cols: u16, rows: u16) -> Result<(), AppError> {
    if cols == 0 || rows == 0 {
        return Err(AppError::new(
            "InvalidArgs",
            format!("pty dimensions must be > 0 (got cols={cols}, rows={rows})"),
        ));
    }
    Ok(())
}

/// Create a new session, materialise its temp files, persist it, and spawn
/// the PTY child. Returns the [`SessionView`] the frontend can stash in its
/// store.
pub fn session_create_impl(
    ctx: &AppContext,
    args: SessionCreateArgs,
) -> Result<SessionView, AppError> {
    ensure_no_switch_in_progress(ctx)?;
    validate_pty_dims(args.cols, args.rows)?;

    // 1. Validate worktree (canonicalises; rejects relative/missing).
    let worktree = compose::validate_worktree(&args.worktree_path).map_err(AppError::from)?;

    // 2. Optionally resolve the instruction set & enforce tool match.
    //    Empty-string IDs from the frontend are treated as "no selection"
    //    so an over-eager wizard can't trigger a NotFound for a `none`
    //    sentinel.
    let cfg = ctx.store().load_config();
    let id_opt = args
        .instruction_set_id
        .as_ref()
        .filter(|id| !id.as_str().is_empty());
    let set_opt = match id_opt {
        Some(id) => Some(lookup_instruction_set(&cfg, id, args.tool)?),
        None => None,
    };

    // 3. Read instruction file contents (re-checking the size cap because
    //    `discover_instructions` *skips* oversized files but a user could
    //    have plumbed an ID through that bypassed discovery).
    let contents_opt = match &set_opt {
        Some(set) => Some(read_instruction_file(&set.file_path)?),
        None => None,
    };

    // 4. Derive a non-colliding label from the worktree basename.
    let existing_sessions = ctx.store().load_sessions();
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
    let cli_override = match args.tool {
        crate::types::Tool::Claude => cfg.ai_launch_commands.claude.as_str(),
        crate::types::Tool::Copilot => cfg.ai_launch_commands.copilot.as_str(),
    };
    let composed = compose::compose_command(&ComposeInputs {
        session_id,
        tool: args.tool,
        worktree_path: &worktree,
        worktree_label: &label,
        instruction_set: set_opt.as_ref(),
        prelaunch_commands: prelaunch_for(&cfg, &worktree),
        instruction_set_contents: contents_opt.as_deref(),
        cli_launch_command: Some(cli_override),
    })
    .map_err(AppError::from)?;

    // 6. Materialise temp files on disk.
    materialise_temp_files(&composed.temp_files)?;

    // 7. Build the persisted record. `tab_index` puts the new session at
    //    the end of the current order.
    let tab_index = ctx
        .store()
        .load_config()
        .tab_order
        .len()
        .min(usize::MAX - 1);
    let session = Session {
        id: session_id,
        tool: args.tool,
        worktree_path: worktree.clone(),
        worktree_name: basename,
        label: label.clone(),
        instruction_set_id: set_opt.as_ref().map(|s| s.id.clone()),
        composed_command: composed.composed_command,
        status: SessionStatus::Starting,
        pid: None,
        created_at: now_unix_seconds(),
        tab_index,
        temp_files: composed.temp_files,
        ai_session_id: None,
    };

    // 8. Persist before spawning so a crash mid-spawn still leaves an
    //    auditable record we can clean up on restart.
    ctx.store().save_session(&session).map_err(AppError::from)?;

    // 9. Append to lastOpenSessions / tabOrder.
    let mut last = cfg.last_open_sessions.clone();
    if !last.contains(&session.id) {
        last.push(session.id);
    }
    let mut order = cfg.tab_order.clone();
    if !order.contains(&session.id) {
        order.push(session.id);
    }
    ctx.store()
        .save_config(PartialAppConfig {
            last_open_sessions: Some(last),
            tab_order: Some(order),
            active_session_id: Some(Some(session.id)),
            ..Default::default()
        })
        .map_err(AppError::from)?;

    // 10. Announce Starting (synchronous; the pool will follow with Running).
    (ctx.sink.status)(&session.id, SessionStatus::Starting, None, None);

    // 11. Spawn. The pool emits Running with PID through the same sink.
    let pid = ctx
        .pool
        .spawn(
            &session,
            ctx.sink.clone(),
            PtySize {
                cols: args.cols,
                rows: args.rows,
                pixel_width: 0,
                pixel_height: 0,
            },
        )
        .map_err(AppError::from)?;

    // 12. Start the per-session metrics watcher (Issue #3). No-op for tools
    //     we can't introspect; never fatal — surface as a debug log only.
    ctx.metrics.start(
        session.id,
        session.tool,
        session.worktree_path.clone(),
        SystemTime::now(),
        Arc::clone(&ctx.metrics_emit),
        Arc::clone(&ctx.turn_emit),
        Arc::clone(&ctx.ai_session_discover),
    );

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
    let mut sessions: Vec<Session> = ctx.store().load_sessions().into_values().collect();
    sessions.sort_by_key(|s| s.tab_index);
    Ok(sessions.iter().map(SessionView::from).collect())
}

// ---------------------------------------------------------------------------
// session_close
// ---------------------------------------------------------------------------

pub async fn session_close_impl(
    ctx: &AppContext,
    id: SessionId,
    delete_worktree: bool,
) -> Result<SessionCloseResult, AppError> {
    // 0. Stop the metrics watcher (Issue #3) before tearing the rest down
    //    so it never observes a half-cleaned session.
    ctx.metrics.stop(&id);

    // 0b. Drop any deferred-spawn registration so a session closed before
    //     the frontend ever measured it doesn't leak — and so a later
    //     `session_resize` for a stale id can't trigger a phantom spawn
    //     of an already-removed record.
    if let Ok(mut g) = ctx.pending_spawn.lock() {
        g.remove(&id);
    }

    // Capture the worktree path *before* we drop the persisted record —
    // we need it for the optional `git worktree remove` step at the end.
    // Use a *strict* read here so that a corrupt or unreadable
    // sessions.json doesn't silently translate "delete the worktree"
    // into "skip silently and report success". On read failure or
    // missing-record we surface a `worktree_delete_error` later instead
    // of attempting deletion.
    let worktree_intent: WorktreeDeleteIntent = if delete_worktree {
        match ctx.store().try_load_sessions() {
            Ok(map) => match map.get(&id) {
                Some(s) => WorktreeDeleteIntent::Path(s.worktree_path.clone()),
                None => WorktreeDeleteIntent::Refused(format!(
                    "session {id} not found in store; cannot determine worktree to delete"
                )),
            },
            Err(e) => WorktreeDeleteIntent::Refused(format!(
                "could not read sessions snapshot reliably; refusing to attempt worktree deletion: {e}"
            )),
        }
    } else {
        WorktreeDeleteIntent::None
    };

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
    ctx.store().remove_session(&id).map_err(AppError::from)?;

    // 4. Trim AppConfig ordering & active selection.
    let cfg = ctx.store().load_config();
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
    ctx.store()
        .save_config(PartialAppConfig {
            last_open_sessions: Some(new_last),
            tab_order: Some(new_order),
            active_session_id: active_patch,
            ..Default::default()
        })
        .map_err(AppError::from)?;

    // 5. Optional: remove the git worktree from disk. The session is
    //    already gone from the store at this point, so deletion failure
    //    must NOT fail the overall close (that would leave the frontend
    //    unable to converge on a "tab gone" state). Surface the error in
    //    the result instead.
    let mut result = SessionCloseResult::default();
    match worktree_intent {
        WorktreeDeleteIntent::Path(wt) => {
            if let Err(error) = delete_worktree_after_close(ctx, &id, &wt, &cfg.workspace_root) {
                warn!(
                    session_id = %id,
                    worktree_path = %wt.display(),
                    error = %error.message,
                    "worktree deletion failed after session close",
                );
                result.worktree_delete_error = Some(error.message);
            }
        }
        WorktreeDeleteIntent::Refused(msg) => {
            warn!(
                session_id = %id,
                error = %msg,
                "worktree deletion was requested but refused before reaching git",
            );
            result.worktree_delete_error = Some(msg);
        }
        WorktreeDeleteIntent::None => {}
    }
    Ok(result)
}

/// What `session_close_impl` decided to do about the optional `delete_worktree`
/// flag, captured *before* the persisted session record is dropped so the
/// decision is based on the pre-close state.
enum WorktreeDeleteIntent {
    /// Caller did not request a worktree deletion.
    None,
    /// Deletion was requested and the worktree path was resolved successfully.
    Path(PathBuf),
    /// Deletion was requested but cannot be attempted (e.g. the sessions
    /// snapshot couldn't be read strictly, or the session record is missing).
    /// The contained string is reported back to the frontend verbatim as
    /// `worktree_delete_error` so the user knows why nothing was deleted.
    Refused(String),
}

/// Helper: validate and execute `git worktree remove --force`. Refuses to
/// touch the configured `workspace_root` itself (i.e. the main checkout),
/// any path that is not contained under the workspace root, or any path
/// still claimed by another live session.
fn delete_worktree_after_close(
    ctx: &AppContext,
    id: &SessionId,
    worktree_path: &Path,
    workspace_root: &Option<PathBuf>,
) -> Result<(), AppError> {
    // Require an explicit workspace root. Without it we have neither a
    // safe `-C` directory to invoke git from (running git inside the
    // worktree we're about to delete fails on Windows because the OS
    // locks a process's CWD) nor a basis for the containment check below.
    let root = workspace_root.as_ref().ok_or_else(|| {
        AppError::from(Error::Internal(
            "cannot delete worktree without a configured workspace root".to_owned(),
        ))
    })?;
    if !root.is_dir() {
        return Err(AppError::from(Error::WorktreeMissing(root.clone())));
    }

    // Compare canonical forms so case differences, trailing slashes, and
    // 8.3 short names don't fool us. For a destructive operation we
    // refuse on canonicalization failure rather than fall back to the raw
    // path: a non-normalized form (`..`, dangling symlink, junction with
    // a missing target) could otherwise slip past the equality and
    // containment checks below.
    let canon_wt = dunce::canonicalize(worktree_path).map_err(|e| {
        AppError::from(Error::Internal(format!(
            "cannot canonicalize worktree path {}: {e}",
            worktree_path.display()
        )))
    })?;
    let canon_root = dunce::canonicalize(root).map_err(|e| {
        AppError::from(Error::Internal(format!(
            "cannot canonicalize workspace root {}: {e}",
            root.display()
        )))
    })?;

    // Safety 1: never remove the main worktree.
    if canon_wt == canon_root {
        return Err(AppError::from(Error::Internal(
            "refusing to delete the workspace root (main worktree)".to_owned(),
        )));
    }
    // Safety 2: only remove paths *under* the workspace root. A corrupted
    // session record (or hostile caller) must not be able to use this code
    // path to delete arbitrary directories.
    if !canon_wt.starts_with(&canon_root) {
        return Err(AppError::from(Error::Internal(format!(
            "refusing to delete worktree outside workspace root: {}",
            worktree_path.display()
        ))));
    }
    // Safety 3: refuse if any *other* live session still references the
    // same worktree. The session being closed has already been removed
    // from the store at this point, so it cannot match itself. If a
    // foreign session's path fails to canonicalize we conservatively
    // treat it as a match — for a destructive operation, an
    // un-canonicalizable path could refer to the same directory we're
    // about to delete (a different textual form, dangling junction, or
    // path made temporarily inaccessible) and we'd rather refuse than
    // delete a worktree another session may still depend on. The session
    // snapshot itself must be loaded *strictly* — for a destructive
    // operation, an unreadable or quarantined sessions.json cannot be
    // silently treated as "no other sessions exist".
    let sessions = ctx.store().try_load_sessions().map_err(|e| {
        AppError::from(Error::Internal(format!(
            "refusing to delete worktree because the sessions snapshot could not be read reliably: {e}"
        )))
    })?;
    let still_in_use = sessions
        .values()
        .any(|s| match dunce::canonicalize(&s.worktree_path) {
            Ok(other) => other == canon_wt,
            Err(_) => true,
        });
    if still_in_use {
        return Err(AppError::from(Error::Internal(format!(
            "refusing to delete worktree still in use by another session: {}",
            worktree_path.display()
        ))));
    }

    let repo_root: PathBuf = root.clone();

    ctx.git_runner
        .remove_worktree(&repo_root, worktree_path)
        .map_err(AppError::from)?;
    info!(
        session_id = %id,
        worktree = %worktree_path.display(),
        "worktree removed after session close",
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// session_focus
// ---------------------------------------------------------------------------

pub fn session_focus_impl(ctx: &AppContext, id: SessionId) -> Result<(), AppError> {
    let sessions = ctx.store().load_sessions();
    if !sessions.contains_key(&id) {
        return Err(AppError::from(Error::NotFound(format!(
            "session {id} not found"
        ))));
    }
    ctx.store()
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
    let SessionResizeArgs {
        session_id,
        cols,
        rows,
    } = args;

    // Reject 0×0 up front. Without this, the deferred-spawn branch below
    // would forward zeros into `pool.spawn` and the live-resize branch
    // into `pool.resize`, both of which surface OS-level openpty/ioctl
    // errors. See [`validate_pty_dims`].
    validate_pty_dims(cols, rows)?;

    // Atomically claim a pending-spawn entry for this session, if any.
    // `restore_all_sessions` registers restored sessions here without
    // spawning, deferring the actual `pool.spawn` until the frontend
    // measures its host and fires the first `session_resize`. That way
    // the CLI's first paint sees the correct PTY width rather than the
    // OS-default 80×24 — see DESIGN §5.5 (restore-on-launch) and the
    // PR notes for the long-standing splash-screen-too-narrow bug.
    let pending = {
        let mut guard = ctx
            .pending_spawn
            .lock()
            .map_err(|_| AppError::new("Internal", "pending_spawn mutex poisoned"))?;
        guard.remove(&session_id)
    };

    if let Some(session) = pending {
        let size = PtySize {
            cols,
            rows,
            pixel_width: 0,
            pixel_height: 0,
        };
        match ctx.pool.spawn(&session, ctx.sink.clone(), size) {
            Ok(pid) => {
                info!(
                    session_id = %session.id,
                    pid,
                    cols,
                    rows,
                    "deferred spawn fired by first session_resize",
                );
                // Start the metrics watcher (Issue #3) — mirrors what
                // `restore_all_sessions` used to do immediately after
                // the inline spawn.
                ctx.metrics.start(
                    session.id,
                    session.tool,
                    session.worktree_path.clone(),
                    SystemTime::now(),
                    Arc::clone(&ctx.metrics_emit),
                    Arc::clone(&ctx.turn_emit),
                    Arc::clone(&ctx.ai_session_discover),
                );
                return Ok(());
            }
            Err(e) => {
                // The deferred spawn failed (e.g. PTY allocation OS error).
                // Surface as Error status so the UI shows the overlay
                // with a Restart button — same shape as a restart-time
                // failure (the user can retry once they've fixed
                // whatever caused the spawn to fail).
                let msg = format!("Failed to start restored session: {e}");
                let _ = ctx
                    .store()
                    .update_session_status(&session.id, SessionStatus::Error, None);
                (ctx.sink.status)(&session.id, SessionStatus::Error, None, Some(msg));
                return Err(AppError::from(e));
            }
        }
    }

    ctx.pool
        .resize(&session_id, cols, rows)
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

pub fn session_restart_impl(ctx: &AppContext, args: SessionRestartArgs) -> Result<(), AppError> {
    ensure_no_switch_in_progress(ctx)?;
    validate_pty_dims(args.cols, args.rows)?;

    let id = args.session_id;
    let sessions = ctx.store().load_sessions();
    let session = sessions
        .get(&id)
        .cloned()
        .ok_or_else(|| AppError::from(Error::NotFound(format!("session {id} not found"))))?;

    // Pre-check: if the worktree directory is gone the spawn will fail
    // with an opaque OS error. Surface a friendly message and persist
    // Error so the overlay re-renders with context (Roadmap §4.3).
    if !session.worktree_path.is_dir() {
        let msg = stale_worktree_message(&session.worktree_path);
        let _ = ctx
            .store()
            .update_session_status(&id, SessionStatus::Error, None);
        (ctx.sink.status)(&id, SessionStatus::Error, None, Some(msg));
        return Err(AppError::from(Error::WorktreeMissing(
            session.worktree_path.clone(),
        )));
    }

    // Re-materialise temp files in case they were deleted (e.g. by a prior
    // close path that ran while the session was still open in another
    // window — defensive). Composed command is reused verbatim per
    // DESIGN §5.4 — *never* recompose at restart time.
    if let Err(e) = materialise_temp_files(&session.temp_files) {
        let msg = format!("Failed to prepare session temp files: {e}");
        let _ = ctx
            .store()
            .update_session_status(&id, SessionStatus::Error, None);
        (ctx.sink.status)(&id, SessionStatus::Error, None, Some(msg));
        return Err(e);
    }

    // Mark Starting in the persisted record up front so a UI poll right
    // after restart doesn't see stale Running/pid.
    ctx.store()
        .update_session_status(&id, SessionStatus::Starting, None)
        .map_err(AppError::from)?;
    (ctx.sink.status)(&id, SessionStatus::Starting, None, None);

    // Restart starts a fresh AI conversation (DESIGN §5.4). The previous
    // ai_session_id refers to a transcript the new CLI invocation will
    // not be writing to, so we clear it eagerly — otherwise a crash
    // between restart and the new watcher's first discovery would let
    // the next restore `--resume` the *pre-restart* conversation.
    //
    // Order matters: stop the OLD watcher first, *then* clear. We need
    // `stop_and_join` (not just `stop`) because the worker only re-checks
    // its `running` flag at the top of each poll iteration — a fire-and-
    // forget stop would let the in-flight iteration call `discover()` one
    // more time and persist the stale id back, undoing the clear. After
    // join returns, the worker thread has fully exited; the new watcher
    // started below by `metrics.start` will repopulate the field with
    // the new conversation's id once the CLI starts writing.
    ctx.metrics.stop_and_join(&id);
    if let Err(e) = ctx.store().update_session_ai_session_id(&id, None) {
        warn!(session_id = %id, error = ?e, "restart: failed to clear ai_session_id");
    }

    if let Err(e) = ctx.pool.respawn_existing(
        &session,
        ctx.sink.clone(),
        PtySize {
            cols: args.cols,
            rows: args.rows,
            pixel_width: 0,
            pixel_height: 0,
        },
    ) {
        let msg = format!("Failed to restart session: {e}");
        let _ = ctx
            .store()
            .update_session_status(&id, SessionStatus::Error, None);
        (ctx.sink.status)(&id, SessionStatus::Error, None, Some(msg));
        return Err(AppError::from(e));
    }
    // Issue #3: restart the metrics watcher with a fresh spawn instant so
    // the freshness filter on Claude project JSONL files re-anchors.
    ctx.metrics.start(
        session.id,
        session.tool,
        session.worktree_path.clone(),
        SystemTime::now(),
        Arc::clone(&ctx.metrics_emit),
        Arc::clone(&ctx.turn_emit),
        Arc::clone(&ctx.ai_session_discover),
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// frontend_ready / restore_all_sessions
// ---------------------------------------------------------------------------

/// Idempotent: returns `true` if this call won the CAS and triggered the
/// restore path, `false` if restore had already been kicked off.
///
/// While a workspace switch is in progress (Phase 7), this returns
/// `false` without touching the CAS — the frontend will re-issue
/// `frontend_ready` after the `workspace://changed` event arrives, at
/// which point the gate is open and the CAS can fire for the new
/// workspace.
pub fn frontend_ready_impl(ctx: &AppContext) -> bool {
    if ctx.switch_in_progress.load(Ordering::SeqCst) {
        return false;
    }
    ctx.restored
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
}

/// Phase 7 helper — refuse new-state-creating commands while a workspace
/// switch is in progress. Returns the wire-shape `WorkspaceSwitchInProgress`
/// error so the frontend can branch on it (typically: ignore + retry once
/// the `workspace://changed` event arrives).
fn ensure_no_switch_in_progress(ctx: &AppContext) -> Result<(), AppError> {
    if ctx.switch_in_progress.load(Ordering::SeqCst) {
        return Err(AppError::new(
            "WorkspaceSwitchInProgress",
            "A workspace switch is in progress; retry once it completes.",
        ));
    }
    Ok(())
}

/// Friendly UI message when a session's worktree directory is no longer
/// available on disk (deleted or replaced with a non-directory). Used by
/// both restore and restart so the wording stays consistent.
fn stale_worktree_message(path: &std::path::Path) -> String {
    format!("Worktree path is no longer available: {}", path.display())
}

/// Best-effort preflight check that the AI tool's transcript for
/// `ai_session_id` still exists on disk. If it doesn't (user deleted it
/// between launches, OS tmp clean, etc.), `restore_all_sessions` skips
/// the `--resume` augmentation rather than handing the CLI a stale id
/// that would error out before the user sees anything useful.
///
/// On any I/O failure we conservatively return `true` so the worst case
/// is the CLI reports its own "no such session" error, which is no worse
/// than today's behaviour.
fn ai_session_transcript_exists(
    tool: Tool,
    worktree_path: &std::path::Path,
    ai_session_id: &str,
) -> bool {
    let Some(home) = crate::session_metrics::home_dir() else {
        return true;
    };
    let path = match tool {
        Tool::Claude => home
            .join(".claude")
            .join("projects")
            .join(crate::session_metrics::encode_cwd(worktree_path))
            .join(format!("{ai_session_id}.jsonl")),
        Tool::Copilot => home
            .join(".copilot")
            .join("session-state")
            .join(ai_session_id),
    };
    // `try_exists` distinguishes "definitely missing" from "couldn't tell"
    // (e.g. permission denied on a parent dir). `Path::is_file`/`is_dir`
    // would conflate both as `false`, which would silently strip a valid
    // `--resume` whenever the home dir is briefly unreadable.
    match path.try_exists() {
        Ok(true) => true,
        Ok(false) => false,
        Err(_) => true,
    }
}

/// Re-spawn every persisted session. Called once after the frontend signals
/// readiness. Failures on individual sessions are logged but do not abort
/// the rest — a single broken session must not strand the whole app.
///
/// Idempotent on a per-session basis: any session already live in the
/// PTY pool *or* already registered in `pending_spawn` is skipped. This
/// matters for the Phase 7 in-app workspace switch, which resets
/// `restored = false` so a subsequent `frontend_ready` fires the
/// restore for the new workspace — but if the user races a manual
/// session_create against that, restore must not double-spawn or
/// overwrite the live record.
pub fn restore_all_sessions(ctx: &AppContext) {
    let sessions = ctx.store().load_sessions();
    let ids: Vec<SessionId> = sessions.keys().copied().collect();

    // Sweep stale temp dirs whose UUIDs no longer correspond to any
    // persisted session. Stale dirs whose UUID *is* still persisted are
    // intentionally kept (DESIGN §5.6 / Phase 6 spec).
    if let Err(e) = cleanup_orphans(&ids) {
        warn!(error = %e, "cleanup_orphans failed during restore");
    }

    // Snapshot pending_spawn membership once so the per-session check
    // doesn't re-acquire the mutex N times. Pool membership IS checked
    // per-iteration because pool.contains is cheap and the value can
    // change as the loop progresses (a session in the pool now might
    // exit before we get to the next id).
    //
    // Race note: a concurrent `session_create` could insert a new
    // pending entry *after* this snapshot, in which case the new id
    // would not appear here. That's benign — the new id is also not
    // in `sessions` (which we already loaded above), so the loop
    // never visits it. The `restored` CAS guarantees this loop body
    // runs at most once per workspace binding, so there's no
    // double-spawn risk from re-entry either.
    let pending_ids: std::collections::HashSet<SessionId> = ctx
        .pending_spawn
        .lock()
        .map(|g| g.keys().copied().collect())
        .unwrap_or_default();

    for (id, session) in sessions {
        if ctx.pool.contains(&id) || pending_ids.contains(&id) {
            debug!(session_id = %id, "restore: skipping already-live or already-pending session");
            continue;
        }

        // Worktree path validation — Roadmap §4.3. If the directory is
        // gone (deleted between launches), spawning would fail with an
        // opaque OS error. Surface a friendly message instead so the
        // terminal overlay can explain the situation.
        if !session.worktree_path.is_dir() {
            let msg = stale_worktree_message(&session.worktree_path);
            warn!(session_id = %id, path = %session.worktree_path.display(), "restore: worktree missing");
            let _ = ctx
                .store()
                .update_session_status(&id, SessionStatus::Error, None);
            (ctx.sink.status)(&id, SessionStatus::Error, None, Some(msg));
            continue;
        }

        // Re-materialise temp files in case they were swept by an OS-level
        // tmp clean. `respawn_existing` reuses `composed_command` verbatim.
        if let Err(e) = materialise_temp_files(&session.temp_files) {
            warn!(session_id = %id, error = ?e, "restore: temp-file materialise failed");
            let msg = format!("Failed to restore session temp files: {e}");
            let _ = ctx
                .store()
                .update_session_status(&id, SessionStatus::Error, None);
            (ctx.sink.status)(&id, SessionStatus::Error, None, Some(msg));
            continue;
        }

        if let Err(e) = ctx
            .store()
            .update_session_status(&id, SessionStatus::Starting, None)
        {
            warn!(session_id = %id, error = ?e, "restore: status update failed");
            continue;
        }
        (ctx.sink.status)(&id, SessionStatus::Starting, None, None);

        // AI-session resume — DESIGN §5.5. Augment composed_command with
        // `--resume <ai_session_id>` so the underlying CLI continues the
        // prior conversation. We *augment*, never *recompose from inputs*
        // (DESIGN §5.4 still holds for the persisted record). We only
        // resume on app-restart restore — user-initiated `session_restart`
        // intentionally launches a fresh CLI conversation.
        //
        // Known limitation (ROADMAP §4.5): for Claude, the `ai_session_id`
        // is discovered heuristically from the newest JSONL in the project
        // dir post-spawn. If two Arborist sessions share the same worktree
        // (same `<encoded-cwd>`), the watchers can converge on the same
        // file and persist the same id for both. On restart, both sessions
        // would then try to `--resume` the same Claude conversation; only
        // one resumes faithfully and the other will see Claude's own "no
        // such session" / "conversation in use" error in its terminal.
        // The fix is a hook-driven session-id source (tracked in #4); the
        // single-session-per-worktree case (the common one) is unaffected.
        // Copilot is not affected — its OTel file is per-Arborist-session,
        // so `gen_ai.conversation.id` is unambiguous.
        let mut session_to_spawn = session.clone();
        if let Some(aid) = session.ai_session_id.as_deref() {
            if ai_session_transcript_exists(session.tool, &session.worktree_path, aid) {
                session_to_spawn.composed_command =
                    compose::with_resume(&session.composed_command, session.tool, aid);
            } else {
                // Transcript was deleted between launches. Drop the stored
                // id so we don't keep trying to resume it, and start fresh.
                warn!(
                    session_id = %id,
                    ai_session_id = %aid,
                    "restore: AI transcript missing on disk; starting fresh conversation",
                );
                let _ = ctx.store().update_session_ai_session_id(&id, None);
            }
        }

        match ctx
            .pending_spawn
            .lock()
            .map(|mut g| g.insert(id, session_to_spawn))
        {
            Ok(_) => {
                info!(
                    session_id = %id,
                    "restore: registered for deferred spawn (will fire on first session_resize)",
                );
            }
            Err(_) => {
                warn!(session_id = %id, "restore: pending_spawn mutex poisoned; skipping deferred registration");
                let _ = ctx
                    .store()
                    .update_session_status(&id, SessionStatus::Error, None);
                (ctx.sink.status)(
                    &id,
                    SessionStatus::Error,
                    None,
                    Some("Internal error: could not register session for restore".to_owned()),
                );
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
// workspace_validate / worktree_create (Roadmap §1, §2)
// ---------------------------------------------------------------------------

/// Validate a candidate workspace root for the first-boot picker. Never
/// returns an `AppError` for the "invalid" case — the picker shows inline
/// feedback. Real `AppError`s are reserved for unexpected backend failures.
pub fn workspace_validate_impl(
    ctx: &AppContext,
    path: &std::path::Path,
) -> Result<crate::types::WorkspaceValidateResult, AppError> {
    use crate::types::WorkspaceValidateResult;

    let invalid = |msg: &str| WorkspaceValidateResult {
        valid: false,
        error: Some(msg.to_owned()),
    };

    if path.as_os_str().is_empty() {
        return Ok(invalid("path is empty"));
    }
    if path.is_relative() {
        return Ok(invalid("path must be absolute"));
    }
    let canon = match dunce::canonicalize(path) {
        Ok(c) => c,
        Err(e) => return Ok(invalid(&format!("path could not be resolved: {e}"))),
    };
    if !canon.is_dir() {
        return Ok(invalid("path is not a directory"));
    }
    let toplevel = ctx
        .git_runner
        .git_toplevel(&canon)
        .map_err(AppError::from)?;
    let Some(toplevel) = toplevel else {
        return Ok(invalid("path is not a git repository"));
    };
    if toplevel != canon {
        return Ok(invalid(&format!(
            "path must be the repository root ({})",
            toplevel.display()
        )));
    }
    Ok(WorkspaceValidateResult {
        valid: true,
        error: None,
    })
}

// ---------------------------------------------------------------------------
// workspace_switch (Phase 7)
// ---------------------------------------------------------------------------

// In-app workspace switch — transactional swap of the active
// [`WorkspaceScope`] without restarting the process.
//
// Pipeline:
// 1. Acquire `switch_serialise` (so two concurrent switches cannot
//    interleave their close-all + bind + swap steps).
// 2. Validate the new path is a real git-repository directory and
//    canonicalise it. Reject early on any failure — no state mutated.
// 3. No-op fast path: if the canonical new path equals the current
//    workspace, return `{ no_op: true }` immediately. The existing
//    binding stays valid; no event is emitted.
// 4. Set `switch_in_progress = true` so [`session_create_impl`],
//    [`session_restart_impl`], and [`frontend_ready_impl`] refuse new
//    work while the switch is mid-pipeline.
// 5. Acquire the OS lock + open `ConfigStore` for the new workspace
//    via [`crate::boot::bind_workspace`]. While step 5 runs we hold
//    BOTH the old lock (inside the current `WorkspaceScope`) and the
//    new lock (inside `WorkspaceBinding`). On
//    [`crate::boot::BootError::Contention`] we abort the switch with
//    `WorkspaceLocked` — the user stays on the current workspace.
// 6. Stop and join all metrics watchers
//    ([`MetricsRegistry::stop_all_and_join`]) BEFORE closing the
//    sessions. `session_close_impl` calls `metrics.stop()` per
//    session which **removes** the handle from the registry — if we
//    closed first, `stop_all_and_join` would find nothing to join
//    and worker threads could still fire one final
//    discover/turn callback after the swap, writing an old session
//    id into the new store. Stopping first while the PTYs are still
//    alive is harmless: workers observe `running = false` on their
//    next poll and exit.
// 7. Quiesce the old workspace's sessions: enumerate from the *old*
//    store, then [`session_close_impl`] each. **Hard-fail** on the
//    first close error — partial close + swap would leave mixed
//    state. The new binding (and its lock) is dropped on failure so
//    the old workspace remains the only bound one.
// 8. Clear `pending_spawn` (any restored-but-not-yet-spawned ids from
//    the old workspace) and reset `restored = false` so the new
//    workspace's restore-on-launch can fire when the frontend
//    re-issues `frontend_ready`.
// 9. Swap [`AppContext::workspace`] under `RwLock` write — the old
//    `WorkspaceLockGuard` inside the old scope is dropped here, in
//    one atomic moment, releasing the OS lock on the old workspace.
//    Other readers were unblocked from step 4 onward only in the no-
//    op gate — during the swap itself, the very brief write lock
//    blocks them.
// 10. Persist `workspace_root` into the *new* store's `config.json`
//     (so the React picker UI doesn't fire on top of the swap), and
//     update the per-branch `last-workspace.json` hint so the next
//     launch resumes the new workspace by default.
// 11. Clear `switch_in_progress = false` and emit
//     `workspace://changed`. The frontend reacts by re-fetching
//     config + sessions and re-issuing `frontend_ready` for the new
//     workspace's restore.

/// Tauri-shaped wrapper around the inner switch implementation —
/// converts the [`AppHandle`] into the testable seams the inner
/// function needs (an `app_data_dir` path and an emit closure).
///
/// Production callers go through this; tests can call
/// [`workspace_switch_impl_inner`] directly with a tempdir and a
/// capturing closure to avoid having to build a real Tauri app.
pub async fn workspace_switch_impl(
    ctx: &AppContext,
    app_handle: &tauri::AppHandle,
    new_path: &Path,
) -> Result<crate::types::WorkspaceSwitchResult, AppError> {
    use tauri::{Emitter, Manager};

    let app_data_dir = app_handle
        .path()
        .app_data_dir()
        .map_err(|e| AppError::new("Io", format!("app_data_dir: {e}")))?;
    let app_for_emit = app_handle.clone();
    let emit_fn: Arc<dyn Fn(&crate::types::WorkspaceChangedEvent) + Send + Sync> =
        Arc::new(move |payload| {
            if let Err(e) = app_for_emit.emit("workspace://changed", payload.clone()) {
                warn!(error = %e, "emit workspace://changed failed; frontend may not refresh");
            }
        });
    workspace_switch_impl_inner(ctx, &app_data_dir, crate::BUILD_BRANCH, new_path, emit_fn).await
}

/// Testable inner of [`workspace_switch_impl`]. See module-level docs
/// on the public wrapper for the full pipeline. Split out so unit tests
/// can drive the swap with a tempdir-backed `app_data_dir` and a
/// capturing emit closure, without standing up a real Tauri app.
pub async fn workspace_switch_impl_inner(
    ctx: &AppContext,
    app_data_dir: &Path,
    branch: &str,
    new_path: &Path,
    emit_changed: Arc<dyn Fn(&crate::types::WorkspaceChangedEvent) + Send + Sync>,
) -> Result<crate::types::WorkspaceSwitchResult, AppError> {
    use crate::types::{WorkspaceChangedEvent, WorkspaceSwitchResult};

    // Step 1 — serialise concurrent switches.
    let _serialise = ctx.switch_serialise.lock().await;

    // Step 2 — validate + canonicalise. We re-use workspace_validate_impl
    // for parity with what the frontend already showed the user. Empty /
    // relative / non-dir / non-repo all turn into a clean error here.
    let validate = workspace_validate_impl(ctx, new_path)?;
    if !validate.valid {
        return Err(AppError::new(
            "InvalidPath",
            validate
                .error
                .unwrap_or_else(|| "workspace validation failed".to_owned()),
        ));
    }
    let canonical = dunce::canonicalize(new_path).map_err(|e| {
        AppError::new(
            "InvalidPath",
            format!("could not canonicalise workspace path: {e}"),
        )
    })?;

    // Step 3 — no-op fast path.
    let current_root = ctx
        .workspace
        .read()
        .expect("workspace lock poisoned")
        .workspace_root
        .clone();
    if current_root.as_ref() == Some(&canonical) {
        return Ok(WorkspaceSwitchResult {
            workspace_root: canonical,
            no_op: true,
        });
    }

    // Step 4 — flip the gate. From here until the `false` store at the
    // end, session_create / session_restart / frontend_ready will refuse.
    ctx.switch_in_progress.store(true, Ordering::SeqCst);

    // Helper: tear down the gate on any early-return error path.
    let restore_gate_on_error = |e: AppError| -> AppError {
        ctx.switch_in_progress.store(false, Ordering::SeqCst);
        e
    };

    // Step 5 — acquire OS lock + ConfigStore for the new workspace.
    if let Err(e) = std::fs::create_dir_all(app_data_dir) {
        return Err(restore_gate_on_error(AppError::new(
            "Io",
            format!("create_dir_all({}): {e}", app_data_dir.display()),
        )));
    }
    let binding =
        match crate::boot::bind_workspace(&canonical, app_data_dir, branch) {
            Ok(b) => b,
            Err(crate::boot::BootError::Contention { branch, workspace }) => {
                return Err(restore_gate_on_error(AppError::new(
                    "WorkspaceLocked",
                    format!(
                    "Workspace is already open in another Arborist window (branch: {}, path: {}).",
                    if branch.trim().is_empty() { "main" } else { &branch },
                    workspace.display(),
                ),
                )));
            }
            Err(other) => {
                return Err(restore_gate_on_error(AppError::new(
                    "Internal",
                    format!("workspace bind failed: {other}"),
                )));
            }
        };

    // Step 6 — quiesce metrics watchers. We do this BEFORE
    // session_close so that the per-session `metrics.stop()` calls
    // inside session_close_impl don't drain the registry first
    // (`stop()` removes the handle), leaving stop_all_and_join with
    // nothing to join. Stopping watchers here while the old PTYs are
    // still alive is harmless: the worker's next poll iteration sees
    // `running = false` and exits cleanly. The join barrier is the
    // actual "no callback after the swap" guarantee — without it,
    // an in-flight discover/turn callback could race the swap and
    // write an old session id into the new store.
    ctx.metrics.stop_all_and_join();

    // Step 7 — quiesce old workspace sessions. Enumerate from the
    // *current* store (still the old one until the swap in step 9).
    // session_close_impl uses ctx.store() which clones from the old
    // scope; safe.
    let old_session_ids: Vec<SessionId> = ctx.store().load_sessions().keys().copied().collect();
    for id in old_session_ids {
        if let Err(e) = session_close_impl(ctx, id, false).await {
            // Hard-fail: partial close + swap would leave mixed-workspace
            // state. Drop the new binding (releases its OS lock via
            // WorkspaceLockGuard's Drop), restore the gate, surface a
            // useful error.
            drop(binding);
            return Err(restore_gate_on_error(AppError::new(
                "WorkspaceSwitchFailed",
                format!("could not close session {id} before workspace switch: {e}"),
            )));
        }
    }

    // Step 8 — drop pending_spawn entries from the old workspace and
    // reset the restore gate so the new workspace's
    // restore_all_sessions can fire when the frontend re-issues
    // frontend_ready.
    if let Ok(mut g) = ctx.pending_spawn.lock() {
        g.clear();
    }
    ctx.restored.store(false, Ordering::SeqCst);

    // Step 9 — swap WorkspaceScope. The OLD WorkspaceLockGuard inside
    // the old scope is dropped at this assignment, releasing the OS
    // lock on the old workspace.
    let new_scope = crate::boot::into_scope(binding);
    {
        let mut w = ctx.workspace.write().expect("workspace lock poisoned");
        *w = new_scope;
    }

    // Step 10 — persist single-source-of-truth markers for the new
    // workspace. Both are best-effort: their failure does not undo the
    // swap.
    let new_store = ctx.store();
    crate::boot::ensure_workspace_root_in_config(&new_store, &canonical);
    if let Err(e) = crate::boot::write_hint(app_data_dir, branch, &canonical) {
        warn!(error = %e, "failed to persist last-workspace hint after switch; non-fatal");
    }

    // Step 11 — open the gate and notify the frontend.
    ctx.switch_in_progress.store(false, Ordering::SeqCst);
    emit_changed(&WorkspaceChangedEvent {
        workspace_root: canonical.clone(),
    });

    info!(workspace = %canonical.display(), "workspace switch complete");
    Ok(WorkspaceSwitchResult {
        workspace_root: canonical,
        no_op: false,
    })
}

/// Create a new linked worktree at `<workspaceRoot>/.worktrees/<name>` on a
/// fresh branch named `<name>`.
pub fn worktree_create_impl(
    ctx: &AppContext,
    name: &str,
) -> Result<crate::types::WorktreeCreateResult, AppError> {
    use crate::types::WorktreeCreateResult;

    // Validate the name with the same rules the frontend used (defence in depth).
    let validated = compose::validate_worktree_name(name)
        .map_err(|msg| AppError::from(Error::InvalidPath(msg)))?;

    let cfg = ctx.store().load_config();
    let Some(workspace) = cfg.workspace_root.as_ref() else {
        return Err(AppError::from(Error::NotFound(
            "workspaceRoot is not set; cannot create worktree".to_owned(),
        )));
    };
    let workspace = workspace.clone();
    if !workspace.is_dir() {
        return Err(AppError::from(Error::WorktreeMissing(workspace)));
    }

    let relative = std::path::PathBuf::from(".worktrees").join(&validated);
    let absolute = workspace.join(&relative);
    // Use symlink_metadata so dangling symlinks/junctions are still treated
    // as "exists" (Roadmap critique #4).
    if std::fs::symlink_metadata(&absolute).is_ok() {
        return Err(AppError::from(Error::InvalidPath(format!(
            "{} already exists",
            absolute.display()
        ))));
    }

    // Containment guard (critique #3): ensure `<workspace>/.worktrees`
    // canonicalizes back inside the workspace. Refuses symlink/junction
    // escapes that would otherwise place the new worktree outside the
    // declared workspace root.
    let worktrees_dir = workspace.join(".worktrees");
    if let Ok(meta) = std::fs::symlink_metadata(&worktrees_dir) {
        if meta.file_type().is_symlink() {
            return Err(AppError::from(Error::InvalidPath(format!(
                "{} is a symlink; refusing to create worktree outside workspace",
                worktrees_dir.display()
            ))));
        }
    } else {
        // Create the .worktrees parent ourselves so `git worktree add` does
        // not have to, and so the canonicalize/containment check below has
        // something to resolve.
        std::fs::create_dir_all(&worktrees_dir).map_err(|e| {
            AppError::from(Error::Internal(format!(
                "could not create {}: {e}",
                worktrees_dir.display()
            )))
        })?;
    }
    let canon_worktrees = dunce::canonicalize(&worktrees_dir).map_err(|e| {
        AppError::from(Error::Internal(format!(
            "could not canonicalize {}: {e}",
            worktrees_dir.display()
        )))
    })?;
    if !canon_worktrees.starts_with(&workspace) {
        return Err(AppError::from(Error::InvalidPath(format!(
            "{} resolves outside the workspace",
            worktrees_dir.display()
        ))));
    }

    let new_path = ctx
        .git_runner
        .create_worktree(&workspace, &relative, &validated)
        .map_err(AppError::from)?;

    // Post-condition: the canonical new path must still lie under the
    // canonical workspace root.
    if !new_path.starts_with(&workspace) {
        return Err(AppError::from(Error::InvalidPath(format!(
            "created worktree {} resolved outside workspace {}",
            new_path.display(),
            workspace.display()
        ))));
    }

    Ok(WorktreeCreateResult { path: new_path })
}

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
