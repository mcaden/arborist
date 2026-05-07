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

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use portable_pty::PtySize;
use tracing::{debug, info, warn};

use crate::compose::{self, ComposeInputs};
use crate::config_store::{discover_instructions, list_instructions_for, ConfigStore, MAX_INSTRUCTION_FILE_BYTES};
use crate::git::{GitRunner, RealGitRunner};
use crate::pty_pool::{cleanup_orphans, PtyPool, PtySink};
use crate::session_metrics::{AiSessionDiscoveryCb, MetricsCb, MetricsRegistry, TurnCb};
use crate::types::{
    AppError, Error, InstructionSet, PartialAppConfig, Session, SessionCloseResult, SessionCreateArgs, SessionId, SessionInputArgs,
    SessionResizeArgs, SessionRestartArgs, SessionStatus, SessionView, Tool,
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
    /// Phase 7 closing-parent tombstone (see CONTEXT_MENU_PLAN.md). Holds
    /// the `SessionId`s of parent sessions whose `session_close` is
    /// currently mid-cascade. `subsession_create_impl` and the sub-session
    /// restore second pass consult this set so a child cannot be created
    /// or restored under a parent that's about to disappear. The lock is
    /// only ever held for the trivial "is X in the set?" check, so it
    /// never blocks for a meaningful duration.
    pub closing_parents: Arc<Mutex<HashSet<SessionId>>>,
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
    /// **Workspace-switch barrier — quiesce side** (Phase 7).
    ///
    /// Used in tandem with [`Self::switch_pending`]. Together the pair
    /// answers two questions a workspace-mutating handler must resolve
    /// before touching `ctx.store()`:
    ///
    /// 1. *Is a switch about to swap the scope out from under me?*
    ///    → consult `switch_pending`.
    /// 2. *Once I have decided to proceed, can the switch wait for me
    ///    to finish before it swaps?*
    ///    → hold a `try_read()` guard on `switch_lock` for my full body.
    ///
    /// Why both are needed:
    ///
    /// * `switch_lock` (this field) — `tokio::sync::RwLock<()>`. The
    ///   switch acquires `write().await` and holds it for the entire
    ///   pipeline. The `write().await` waits for every reader to drop
    ///   before it proceeds, which **quiesces** in-flight handlers /
    ///   restore / resize-deferred-spawn before the swap, the
    ///   `metrics.stop_all_and_join` drain, and the
    ///   `pending_spawn.clear` run. Concurrent switches queue serially
    ///   on the write side via tokio's FIFO waker queue, so we get
    ///   serialisation for free.
    /// * [`Self::switch_pending`] — `AtomicUsize` counter, incremented
    ///   immediately *before* the switch awaits the write lock and
    ///   decremented when its RAII guard drops. We need a counter
    ///   separate from the lock itself because tokio's
    ///   `RwLock::try_read` is permit-based and does NOT honour
    ///   writer-preferring fairness for non-awaiting callers — a
    ///   queued writer behind active readers does not bump out new
    ///   `try_read` calls. Without the counter, a handler issued
    ///   between "switch task spawned" and "switch's write().await
    ///   resolved" could acquire `try_read` successfully, complete a
    ///   mutation against the soon-to-be-discarded old store, and
    ///   silently lose the user's write. The counter lets handlers
    ///   detect the queued switch and bail out.
    ///
    /// Per-handler gating policy:
    ///
    /// * **Workspace-scoped lifecycle handlers** (`session_create`,
    ///   `session_close`, `session_restart`, `session_focus`,
    ///   `config_set`) call [`acquire_switch_read`] at impl entry —
    ///   it takes the read guard, then checks `switch_pending`. If the
    ///   counter is non-zero (or the read guard cannot be acquired),
    ///   return [`AppError::WorkspaceSwitchInProgress`] so the user
    ///   sees the failure and can retry. Otherwise hold the guard for
    ///   the **full impl body** (including across `.await` for async
    ///   impls — tokio guards are `Send` for `T: Send + Sync`) so the
    ///   switch cannot start swapping mid-handler.
    /// * **`session_resize` and `frontend_ready`** apply the same
    ///   take-then-check pattern but return `Ok(())` silently on a
    ///   negative outcome. There is no useful error to surface — the
    ///   next `ResizeObserver` fire (resize) re-issues the call
    ///   against the new scope, and `frontend_ready` is fire-once at
    ///   app boot (post-PR5 the workspace switch handles its own
    ///   inline restore + `restored=true` latch, so a defensive
    ///   `frontend_ready` re-fire after a switch is itself a no-op
    ///   CAS). A "switch in progress" toast for either of these
    ///   automatically-issued background commands would be noise.
    /// * **`restore_all_sessions`** (called from `frontend_ready`'s
    ///   wrapper inside `spawn_blocking`) inherits an
    ///   `OwnedRwLockReadGuard` moved into the task, so the entire
    ///   restore loop is bounded by the same barrier — a switch
    ///   cannot start swapping while restore is mid-iteration.
    /// * **`session_input`** is intentionally ungated — it operates by
    ///   id on the PTY pool (workspace-agnostic), and writes to a PTY
    ///   about to be parked are benign (the bytes go to a soon-killed
    ///   child). Gating would impose lock contention on every
    ///   keystroke for no correctness benefit.
    /// * **Read-only commands** (`session_list`, `config_get`,
    ///   `instructions_list`) are ungated — they see a consistent
    ///   `ctx.store()` snapshot for the duration of one call, which is
    ///   all the consistency they owe their caller.
    ///
    /// **Lock ordering / no deadlock cycles.** `switch_lock` is the
    /// outermost lock taken by every gated handler. Inner locks
    /// (`workspace`, `pending_spawn`, per-store `write_lock`) are
    /// taken briefly inside handler bodies and never re-acquire
    /// `switch_lock`. Concurrent switches queue serially on the write
    /// side; readers and writers never form cycles.
    ///
    /// **Background callbacks** (PTY status, AI-session discovery)
    /// fire from worker threads outside the request handler chain and
    /// remain outside this barrier — gating them would require a lock
    /// acquisition on every PTY byte. Instead they re-resolve the
    /// current store on every invocation and tolerate `NotFound` from
    /// a swapped-out store (see `commands::mod::build_production_sink`
    /// and `build_production_ai_session_discover`). The switch drains
    /// them deterministically before swapping (see steps 6 and 7 of
    /// `workspace_switch_impl_inner`) so the only callbacks that can
    /// fire post-swap are post-emit Tauri event deliveries to the JS
    /// side, which the `NotFound` tolerance handles cleanly.
    pub switch_lock: Arc<tokio::sync::RwLock<()>>,
    /// **Workspace-switch barrier — rejection counter** (Phase 7).
    /// See [`Self::switch_lock`] for why a counter separate from the
    /// lock itself is necessary.
    ///
    /// Incremented **before** `workspace_switch_impl_inner` awaits the
    /// write lock; decremented by [`SwitchPendingGuard::drop`] on any
    /// exit (normal return, early return, panic). Handlers load this
    /// counter under their read guard to detect a queued switch and
    /// reject (or silently `Ok` for resize/frontend_ready).
    pub switch_pending: Arc<AtomicUsize>,
}

/// RAII counter for [`AppContext::switch_pending`]. Increments on
/// `new`, decrements on drop. Held by `workspace_switch_impl_inner`
/// for the entire pipeline so any concurrent handler that loads the
/// counter sees a non-zero value and rejects. Drop on panic
/// guarantees the counter cannot get stuck above zero and lock the
/// app out of all workspace-mutating commands.
pub(crate) struct SwitchPendingGuard(Arc<AtomicUsize>);

impl SwitchPendingGuard {
    fn new(counter: Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self(counter)
    }
}

impl Drop for SwitchPendingGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
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
        Self::with_workspace_internal(pool, workspace, sink, git_runner, metrics_emit, ai_session_discover, turn_emit)
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
            closing_parents: Arc::new(Mutex::new(HashSet::new())),
            pending_spawn: Arc::new(Mutex::new(HashMap::new())),
            switch_lock: Arc::new(tokio::sync::RwLock::new(())),
            switch_pending: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// True iff `session_close` is currently mid-cascade for `id`.
    /// Used by `subsession_create_impl` and the sub-session restore
    /// second pass to refuse new children under a closing parent.
    ///
    /// Fails *closed* on a poisoned mutex: a panic while holding
    /// `closing_parents` could otherwise let a sub-session be spawned
    /// under a parent mid-close (the cascade would then race to kill
    /// it). Returning `true` on poisoning preserves the tombstone
    /// invariant — the caller will refuse the operation, which is
    /// the safe failure mode.
    #[must_use]
    pub fn is_parent_closing(&self, id: &SessionId) -> bool {
        match self.closing_parents.lock() {
            Ok(g) => g.contains(id),
            Err(_) => true,
        }
    }

    /// Mark a parent as mid-close. Returns a guard that removes the id
    /// on drop — guaranteed cleanup even if the close path panics.
    #[must_use]
    pub fn mark_parent_closing(&self, id: SessionId) -> ClosingParentGuard {
        if let Ok(mut g) = self.closing_parents.lock() {
            g.insert(id);
        }
        ClosingParentGuard {
            set: Arc::clone(&self.closing_parents),
            id,
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
    /// **Workspace-switch atomicity.** Workspace-mutating handlers
    /// must call `store()` once at the top of their impl and operate
    /// on the returned handle for the whole body. They must also be
    /// holding a `switch_lock` read guard (acquired via
    /// [`acquire_switch_read`]) for that same body, so the switch
    /// cannot start its scope swap mid-handler. With both rules
    /// followed, a handler's snapshot is guaranteed to still be the
    /// current scope's store at every persist call — there is no
    /// window where a handler can mutate a store that has just been
    /// released by the switch. See [`Self::switch_lock`] for the full
    /// barrier contract and the per-handler gating policy.
    ///
    /// Will `panic!` if the workspace lock is poisoned (which can only
    /// happen if a writer panicked mid-mutation; recovery is
    /// impossible because the swap is not idempotent).
    #[must_use]
    pub fn store(&self) -> ConfigStore {
        self.workspace.read().expect("workspace lock poisoned").store.clone()
    }
}

/// RAII guard returned by [`AppContext::mark_parent_closing`]. Removes
/// the id from the closing-parents set when dropped so the tombstone
/// never outlives the cascade — even on panic.
pub struct ClosingParentGuard {
    set: Arc<Mutex<HashSet<SessionId>>>,
    id: SessionId,
}

impl Drop for ClosingParentGuard {
    fn drop(&mut self) {
        if let Ok(mut g) = self.set.lock() {
            g.remove(&self.id);
        }
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
pub fn session_create_impl(ctx: &AppContext, args: SessionCreateArgs) -> Result<SessionView, AppError> {
    let _switch = acquire_switch_read(ctx)?;
    validate_pty_dims(args.cols, args.rows)?;

    // 1. Validate worktree (canonicalises; rejects relative/missing).
    let worktree = compose::validate_worktree(&args.worktree_path).map_err(AppError::from)?;

    // 2. Optionally resolve the instruction set & enforce tool match.
    //    Empty-string IDs from the frontend are treated as "no selection"
    //    so an over-eager wizard can't trigger a NotFound for a `none`
    //    sentinel.
    let cfg = ctx.store().load_config();
    let id_opt = args.instruction_set_id.as_ref().filter(|id| !id.as_str().is_empty());
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
    let existing_labels: Vec<&str> = existing_sessions.values().map(|s| s.label.as_str()).collect();
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
    // 7. Build the persisted record. `tab_index` puts the new session at
    //    the end of the current order.
    //
    //    For Copilot we *pre-allocate* the conversation id (a fresh
    //    `--resume <uuid>` causes Copilot to create the session at that
    //    exact uuid — verified). This guarantees `Session.ai_session_id`
    //    is populated from create-time onward, so restore-on-launch can
    //    always splice `--resume` and Copilot never starts a fresh
    //    conversation just because the user hadn't typed anything before
    //    the previous shutdown. (Original symptom: "only the active tab
    //    fully resumes".)
    //
    //    The persisted `composed_command` stays bare (DESIGN §5.4 — no
    //    `--resume` baked into the immutable record); the splice happens
    //    at every spawn site below, mirroring `restore_all_sessions`.
    //
    //    Claude has no equivalent flag to pre-allocate against; its
    //    `ai_session_id` continues to be discovered from the transcript
    //    after the first user prompt.
    let tab_index = ctx.store().load_config().tab_order.len().min(usize::MAX - 1);
    let preallocated_ai_id = match args.tool {
        Tool::Copilot => Some(uuid::Uuid::new_v4().to_string()),
        Tool::Claude => None,
    };
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
        ai_session_id: preallocated_ai_id.clone(),
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
    //     For Copilot we splice the pre-allocated `--resume <uuid>` here
    //     so Copilot creates the session-state directory at the known
    //     deterministic path from spawn time. The persisted
    //     `composed_command` stays bare (see step 7).
    let session_to_spawn = if let Some(aid) = preallocated_ai_id.as_deref() {
        let mut s = session.clone();
        s.composed_command = compose::with_resume(&session.composed_command, args.tool, aid);
        s
    } else {
        session.clone()
    };
    let pid = ctx
        .pool
        .spawn(
            &session_to_spawn,
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
    //     For Copilot the pre-allocated ai_session_id (step 7) lets the
    //     events.jsonl tailer attach immediately on the deterministic
    //     `~/.copilot/session-state/<aid>/events.jsonl` path.
    ctx.metrics.start(
        session.id,
        session.tool,
        session.worktree_path.clone(),
        SystemTime::now(),
        Arc::clone(&ctx.metrics_emit),
        Arc::clone(&ctx.turn_emit),
        Arc::clone(&ctx.ai_session_discover),
        Arc::clone(&ctx.sink.activity),
        preallocated_ai_id.clone(),
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

pub async fn session_close_impl(ctx: &AppContext, id: SessionId, delete_worktree: bool) -> Result<SessionCloseResult, AppError> {
    // Reject if a workspace switch is queued/active. Held for the full
    // lifetime of this call (including across `pool.kill().await`) so
    // the switch's `write().await` cannot proceed until our teardown
    // completes against the old store. See [`AppContext::switch_lock`].
    let _switch = acquire_switch_read(ctx)?;

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
                None => WorktreeDeleteIntent::Refused(format!("session {id} not found in store; cannot determine worktree to delete")),
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
    let new_last: Vec<SessionId> = cfg.last_open_sessions.iter().copied().filter(|s| s != &id).collect();
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

/// **Park** an old-workspace session in preparation for a workspace
/// switch. Tears down the live PTY (and the session's pending-spawn
/// registration and temp-dir on disk) but **preserves every persisted
/// record**: the entry in `sessions.json` stays put,
/// `last_open_sessions` / `tab_order` / `active_session_id` are not
/// touched. When the user switches back to this workspace,
/// [`restore_all_sessions`] re-spawns the PTY from the unchanged
/// `Session` record using `composed_command` verbatim — Claude /
/// Copilot `--resume` splicing (see [`compose::with_resume`]) keeps
/// the AI conversation context alive across the round-trip.
///
/// Best-effort by design: `pool.kill` may fail (rare; e.g. the child
/// has already exited and the wait thread is mid-cleanup). We log
/// and continue rather than abort the workspace switch — park
/// performs zero irreversible store mutations, so a partial-park
/// state is benign and self-heals on the next switch-back. This is
/// the key behavioural difference vs. `session_close_impl`, which
/// destroys persisted state and therefore historically had to
/// hard-fail the switch on partial close.
///
/// **Metrics watchers are NOT touched here — the caller has already
/// stopped them.** By the time this runs, the switch has acquired
/// [`AppContext::switch_lock`] for write and called
/// [`MetricsRegistry::stop_all_and_join`] (step 6 of the pipeline,
/// see `workspace_switch_impl_inner`). The write guard prevents any
/// new watchers from being armed (every lifecycle handler / resize
/// deferred-spawn has either drained their read guard before the
/// switch acquired write, or is rejected outright by `try_read`), so
/// a per-session `metrics.stop` here would be unconditionally a no-op
/// and is intentionally omitted.
async fn park_session_for_switch_impl(ctx: &AppContext, id: SessionId) {
    // 1. Drop any deferred-spawn registration so a stale resize for
    //    this id can't trigger a phantom spawn against the new
    //    (post-swap) workspace. (Belt-and-braces: the switch's
    //    write-guard step already cleared `pending_spawn`; this is
    //    defense-in-depth against future code paths that might insert
    //    after the drain.)
    if let Ok(mut g) = ctx.pending_spawn.lock() {
        g.remove(&id);
    }

    // 2. Best-effort kill. Temp dir is removed by pool.kill itself
    //    (step 6 inside pool::kill); restore re-materialises temp
    //    files from the persisted `Session.temp_files` so this is
    //    safe.
    //
    //    `pool.kill` reports a `KillOutcome` so we can distinguish
    //    "OS confirmed the child was reaped within KILL_GRACE" from
    //    "kill issued but reap not observed in time". For an
    //    `Unconfirmed` outcome we log loudly with the PID so a human
    //    can find and clean up the orphan if one actually leaked
    //    (the kill primitive itself was still issued — see step 3 of
    //    `pool.kill` — so this is genuinely a rare edge case, not the
    //    hot path). We still continue with the swap because rolling
    //    back the workspace switch on a single park's reap timeout
    //    would block the user on a problem they can't see; the swap
    //    contract is "park is best-effort" (DESIGN §5.5c step 7).
    if ctx.pool.contains(&id) {
        match ctx.pool.kill(&id).await {
            Ok(crate::pty_pool::KillOutcome::Reaped) => {}
            Ok(crate::pty_pool::KillOutcome::Unconfirmed { pid }) => {
                warn!(
                    session_id = %id,
                    pid,
                    "park: pool.kill issued but reap unconfirmed within grace period; \
                     a CLI process may still be alive at this PID. Workspace switch \
                     proceeding (record preserved for restore); manual cleanup may be needed."
                );
            }
            Err(e) => {
                warn!(
                    session_id = %id,
                    error = ?e,
                    "park: pool.kill failed during workspace switch; continuing without aborting swap (record preserved for restore)"
                );
            }
        }
    }
    // **Intentionally absent**: store.remove_session, save_config,
    // worktree-delete. Those are the irreversible side-effects of
    // session_close_impl that we explicitly *don't* perform on park.
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
fn delete_worktree_after_close(ctx: &AppContext, id: &SessionId, worktree_path: &Path, workspace_root: &Option<PathBuf>) -> Result<(), AppError> {
    // Require an explicit workspace root. Without it we have neither a
    // safe `-C` directory to invoke git from (running git inside the
    // worktree we're about to delete fails on Windows because the OS
    // locks a process's CWD) nor a basis for the containment check below.
    let root = workspace_root
        .as_ref()
        .ok_or_else(|| AppError::from(Error::Internal("cannot delete worktree without a configured workspace root".to_owned())))?;
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
    let canon_root = dunce::canonicalize(root)
        .map_err(|e| AppError::from(Error::Internal(format!("cannot canonicalize workspace root {}: {e}", root.display()))))?;

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
    let still_in_use = sessions.values().any(|s| match dunce::canonicalize(&s.worktree_path) {
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

    ctx.git_runner.remove_worktree(&repo_root, worktree_path).map_err(AppError::from)?;
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
    // Reject if a workspace switch is queued/active. Without this, a
    // stale focus event from the frontend (e.g. user clicked a tab a
    // moment before triggering a switch) could write
    // `active_session_id` for a not-yet-torn-down old-workspace
    // session into a snapshot of the *old* store that races the swap.
    let _switch = acquire_switch_read(ctx)?;
    let sessions = ctx.store().load_sessions();
    if !sessions.contains_key(&id) {
        return Err(AppError::from(Error::NotFound(format!("session {id} not found"))));
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
    let SessionResizeArgs { session_id, cols, rows } = args;

    // Skip silently if a workspace switch is queued or active. The next
    // `ResizeObserver` event after the switch completes will re-fire
    // this resize against the new workspace's PTY (or a no-op if the
    // session was parked); no error is propagated to the UI. Held for
    // the full body so the deferred-spawn arm cannot interleave with
    // the switch's `pending_spawn` drain or `metrics.stop_all_and_join`.
    //
    // **Pre-check**: cheap atomic load avoids the `try_read` permit
    // dance during a switch (resize is hot-path; ResizeObservers can
    // fire dozens of times in a switch window). **Post-check** (after
    // taking the guard) closes the take-then-set race the same way
    // [`acquire_switch_read`] does. See [`AppContext::switch_lock`].
    if ctx.switch_pending.load(Ordering::SeqCst) > 0 {
        return Ok(());
    }
    let _switch = match ctx.switch_lock.try_read() {
        Ok(g) => g,
        Err(_) => return Ok(()),
    };
    if ctx.switch_pending.load(Ordering::SeqCst) > 0 {
        return Ok(());
    }

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
                // the inline spawn. For Copilot the persisted
                // `ai_session_id` (set at create / restart) drives the
                // events.jsonl tailer to the correct conversation's
                // path; for Claude it's typically `None` and the
                // watcher discovers the transcript post-spawn.
                ctx.metrics.start(
                    session.id,
                    session.tool,
                    session.worktree_path.clone(),
                    SystemTime::now(),
                    Arc::clone(&ctx.metrics_emit),
                    Arc::clone(&ctx.turn_emit),
                    Arc::clone(&ctx.ai_session_discover),
                    Arc::clone(&ctx.sink.activity),
                    session.ai_session_id.clone(),
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
                let _ = ctx.store().update_session_status(&session.id, SessionStatus::Error, None);
                (ctx.sink.status)(&session.id, SessionStatus::Error, None, Some(msg));
                return Err(AppError::from(e));
            }
        }
    }

    ctx.pool.resize(&session_id, cols, rows).map_err(AppError::from)
}

pub fn session_input_impl(ctx: &AppContext, args: SessionInputArgs) -> Result<(), AppError> {
    ctx.pool.write(&args.session_id, args.data.as_bytes()).map_err(AppError::from)
}

// ---------------------------------------------------------------------------
// session_restart
// ---------------------------------------------------------------------------

pub fn session_restart_impl(ctx: &AppContext, args: SessionRestartArgs) -> Result<(), AppError> {
    let _switch = acquire_switch_read(ctx)?;
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
        let _ = ctx.store().update_session_status(&id, SessionStatus::Error, None);
        (ctx.sink.status)(&id, SessionStatus::Error, None, Some(msg));
        return Err(AppError::from(Error::WorktreeMissing(session.worktree_path.clone())));
    }

    // Re-materialise temp files in case they were deleted (e.g. by a prior
    // close path that ran while the session was still open in another
    // window — defensive). Composed command is reused verbatim per
    // DESIGN §5.4 — *never* recompose at restart time.
    if let Err(e) = materialise_temp_files(&session.temp_files) {
        let msg = format!("Failed to prepare session temp files: {e}");
        let _ = ctx.store().update_session_status(&id, SessionStatus::Error, None);
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
    // not be writing to.
    //
    // For Copilot we *re-allocate* a fresh uuid and pre-bind the new
    // conversation to it via `--resume <new-uuid>` (Copilot will create a
    // brand-new session at that uuid). This keeps the events.jsonl path
    // deterministic across restart (same property as the create path) so
    // restore-on-launch can resume the post-restart conversation.
    //
    // For Claude we keep today's behavior: clear ai_session_id and let
    // the watcher re-discover the new transcript after the user prompts.
    //
    // Order matters: stop the OLD watcher first, *then* mutate
    // ai_session_id. We need `stop_and_join` (not just `stop`) because
    // the worker only re-checks its `running` flag at the top of each
    // poll iteration — a fire-and-forget stop would let the in-flight
    // iteration call `discover()` one more time and persist the stale id
    // back, undoing the mutation. After join returns, the worker thread
    // has fully exited; the new watcher started below by `metrics.start`
    // will repopulate the field if/when the CLI rotates the conversation
    // (e.g. user-typed `/clear` or `/resume <other-id>`).
    //
    // Persist order:
    //   - Claude: clear *eagerly* (before respawn). This preserves the
    //     pre-Phase-2 semantics — a crash between here and the new
    //     watcher's first discovery could otherwise let the next restore
    //     `--resume` the pre-restart conversation. Cost: a failed Claude
    //     restart loses the prior id from the persisted record (same as
    //     today).
    //   - Copilot: defer until *after* `respawn_existing` succeeds. The
    //     pre-allocated uuid is ours and unique; if respawn fails, we
    //     keep the old conversation id resumable rather than rotating to
    //     a uuid with no Copilot session-state directory (which would
    //     irrevocably orphan the prior conversation on a transient
    //     respawn failure).
    ctx.metrics.stop_and_join(&id);
    let restart_ai_id: Option<String> = match session.tool {
        Tool::Copilot => Some(uuid::Uuid::new_v4().to_string()),
        Tool::Claude => None,
    };
    if matches!(session.tool, Tool::Claude) {
        if let Err(e) = ctx.store().update_session_ai_session_id(&id, None) {
            warn!(session_id = %id, error = ?e, "restart: failed to clear ai_session_id");
        }
    }

    let session_to_spawn = if let Some(aid) = restart_ai_id.as_deref() {
        let mut s = session.clone();
        s.composed_command = compose::with_resume(&session.composed_command, session.tool, aid);
        s
    } else {
        session.clone()
    };

    if let Err(e) = ctx.pool.respawn_existing(
        &session_to_spawn,
        ctx.sink.clone(),
        PtySize {
            cols: args.cols,
            rows: args.rows,
            pixel_width: 0,
            pixel_height: 0,
        },
    ) {
        let msg = format!("Failed to restart session: {e}");
        let _ = ctx.store().update_session_status(&id, SessionStatus::Error, None);
        (ctx.sink.status)(&id, SessionStatus::Error, None, Some(msg));
        return Err(AppError::from(e));
    }

    // Spawn succeeded — *now* persist the rotated Copilot uuid. Doing
    // this after respawn means a failed restart (above) leaves the prior
    // ai_session_id intact and resumable.
    if matches!(session.tool, Tool::Copilot) {
        if let Err(e) = ctx.store().update_session_ai_session_id(&id, restart_ai_id.clone()) {
            warn!(session_id = %id, error = ?e, "restart: failed to persist rotated ai_session_id");
        }
    }
    // Issue #3: restart the metrics watcher with a fresh spawn instant so
    // the freshness filter on Claude project JSONL files re-anchors. For
    // Copilot, the freshly-allocated ai_session_id (above) drives the
    // events.jsonl tailer to the new conversation's path.
    ctx.metrics.start(
        session.id,
        session.tool,
        session.worktree_path.clone(),
        SystemTime::now(),
        Arc::clone(&ctx.metrics_emit),
        Arc::clone(&ctx.turn_emit),
        Arc::clone(&ctx.ai_session_discover),
        Arc::clone(&ctx.sink.activity),
        restart_ai_id.clone(),
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// frontend_ready / restore_all_sessions
// ---------------------------------------------------------------------------

/// Idempotent: returns `true` if this call won the CAS and triggered the
/// restore path, `false` if restore had already been kicked off.
///
/// **Workspace-switch coordination** is handled by the Tauri wrapper
/// (`commands::frontend_ready`), not here: the wrapper takes an
/// `OwnedRwLockReadGuard` on [`AppContext::switch_lock`] and moves it
/// into the `spawn_blocking` task that runs [`restore_all_sessions`],
/// so the entire restore loop is bounded by the same barrier as every
/// other workspace-mutating handler.
pub fn frontend_ready_impl(ctx: &AppContext) -> bool {
    ctx.restored.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_ok()
}

/// Phase 7 helper — acquire a read guard on [`AppContext::switch_lock`]
/// for handlers that must REJECT (with `WorkspaceSwitchInProgress`)
/// when a workspace switch is queued or active. Used by
/// `session_create`, `session_close`, `session_restart`,
/// `session_focus`, and `config_set` impls.
///
/// The returned guard MUST be held for the full lifetime of any
/// store-mutating, PTY-spawning, or metrics-arming work the handler
/// performs — the switch's `write().await` waits for the guard to drop
/// before commencing. Holding it past `.await`s is safe; tokio's
/// `RwLockReadGuard<()>` is `Send + Sync`.
///
/// **Take-then-check ordering** (load-bearing). We take the read
/// guard *before* checking `switch_pending`. Reasoning:
///
/// 1. If the switch hasn't yet incremented `switch_pending`, we
///    acquire the guard and the switch's later `write().await`
///    waits for our drop — no race.
/// 2. If the switch already incremented `switch_pending` but hasn't
///    yet acquired write (or we beat it to `try_read`), we take the
///    guard, observe the non-zero counter, drop the guard, and reject.
///    The brief held-then-dropped guard means the switch's
///    `write().await` waits an extra moment for our drop, which is
///    fine.
///
/// The reverse order (check-then-take) would have a race window
/// between the load and the `try_read` where the switch could
/// acquire write and the handler still see a stale "no pending"
/// reading.
pub(crate) fn acquire_switch_read(ctx: &AppContext) -> Result<tokio::sync::RwLockReadGuard<'_, ()>, AppError> {
    let guard = ctx
        .switch_lock
        .try_read()
        .map_err(|_| AppError::new("WorkspaceSwitchInProgress", "A workspace switch is in progress; retry once it completes."))?;
    if ctx.switch_pending.load(Ordering::SeqCst) > 0 {
        drop(guard);
        return Err(AppError::new(
            "WorkspaceSwitchInProgress",
            "A workspace switch is in progress; retry once it completes.",
        ));
    }
    Ok(guard)
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
fn ai_session_transcript_exists(tool: Tool, worktree_path: &std::path::Path, ai_session_id: &str) -> bool {
    let Some(home) = crate::session_metrics::home_dir() else {
        return true;
    };
    let path = match tool {
        Tool::Claude => home
            .join(".claude")
            .join("projects")
            .join(crate::session_metrics::encode_cwd(worktree_path))
            .join(format!("{ai_session_id}.jsonl")),
        Tool::Copilot => home.join(".copilot").join("session-state").join(ai_session_id),
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

/// Defense-in-depth helper for [`restore_all_sessions`]: trim
/// `last_open_sessions` / `tab_order` / `active_session_id` of any
/// IDs that have no corresponding entry in `sessions.json`. Such
/// orphan IDs can be left over when a previous build seeded
/// `config.json` from a legacy/canonical source without seeding
/// `sessions.json` (the bug that motivated this helper). The
/// `seed.rs` strip prevents NEW instances; this helper cleans up
/// existing ones on first restore after the upgrade.
///
/// No-op when nothing needs trimming, so the common path doesn't
/// rewrite `config.json` on every launch.
fn trim_unknown_session_refs_with_store(store: &ConfigStore, known: &std::collections::HashSet<SessionId>) -> Result<(), Error> {
    let cfg = store.load_config();
    let mut patch = PartialAppConfig::default();
    let mut dirty = false;

    let trimmed_last: Vec<SessionId> = cfg.last_open_sessions.iter().copied().filter(|s| known.contains(s)).collect();
    if trimmed_last.len() != cfg.last_open_sessions.len() {
        patch.last_open_sessions = Some(trimmed_last);
        dirty = true;
    }

    let trimmed_order: Vec<SessionId> = cfg.tab_order.iter().copied().filter(|s| known.contains(s)).collect();
    if trimmed_order.len() != cfg.tab_order.len() {
        patch.tab_order = Some(trimmed_order);
        dirty = true;
    }

    if let Some(active) = cfg.active_session_id {
        if !known.contains(&active) {
            patch.active_session_id = Some(None);
            dirty = true;
        }
    }

    if dirty {
        store.save_config(patch)?;
    }
    Ok(())
}

/// Re-spawn every persisted session. Called once after the frontend signals
/// readiness. Failures on individual sessions are logged but do not abort
/// the rest — a single broken session must not strand the whole app.
///
/// Idempotent on a per-session basis: any session already live in the
/// PTY pool *or* already registered in `pending_spawn` is skipped. This
/// matters for the Phase 7 in-app workspace switch, which calls this
/// function inline (under the switch's exclusive `switch_lock.write()`
/// guard) after the scope swap and then latches `ctx.restored = true`.
/// The `restored` atomic is **never reset to `false`** — once a binding
/// has had its restore fired (either by `frontend_ready` at boot or by
/// the inline restore inside `workspace_switch_impl_inner`) any
/// subsequent `frontend_ready` becomes a no-op CAS. Idempotency here
/// guards against the user racing a manual `session_create` against
/// the inline restore: restore must not double-spawn or overwrite the
/// live record.
///
/// **Workspace-binding stability.** The store is snapshotted ONCE at
/// the top of this function and re-used for every per-session read /
/// write. Calling `ctx.store()` per iteration would re-read the
/// `WorkspaceScope` on each call — and a workspace switch that lands
/// mid-loop would silently re-target subsequent writes to the new
/// (post-swap) store, with the OLD session ids of the workspace we
/// were restoring. The pinned snapshot keeps every write in this
/// invocation aimed at the workspace whose `sessions` we loaded.
/// **Workspace-switch coordination** is handled by the caller: the
/// `frontend_ready` Tauri wrapper holds an `OwnedRwLockReadGuard` on
/// [`AppContext::switch_lock`] for the full duration of this function,
/// so a switch cannot start its scope swap until restore returns —
/// the loop body itself does not need to consult any switch state.
pub fn restore_all_sessions(ctx: &AppContext) {
    // Pin the store for the lifetime of this invocation — see fn doc
    // comment on workspace-binding stability.
    let store = ctx.store();
    let sessions = store.load_sessions();
    let ids: Vec<SessionId> = sessions.keys().copied().collect();

    // Sweep stale temp dirs whose UUIDs no longer correspond to any
    // persisted session. Stale dirs whose UUID *is* still persisted are
    // intentionally kept (DESIGN §5.6 / Phase 6 spec).
    if let Err(e) = cleanup_orphans(&ids) {
        warn!(error = %e, "cleanup_orphans failed during restore");
    }

    // Defense-in-depth: trim IDs that appear in config's
    // `last_open_sessions` / `tab_order` / `active_session_id` but
    // have NO corresponding record in `sessions.json`. This catches
    // pre-fix-state stores where a branch build seeded `config.json`
    // from a legacy/canonical source without seeding `sessions.json`,
    // leaving the seeded config carrying phantom IDs that the per-
    // session worktree-missing trim below never visits (it iterates
    // over actual records, not config refs). The seed-fix in
    // `seed.rs` prevents new instances of this; this trim cleans up
    // existing ones on first restore after the upgrade.
    let known: std::collections::HashSet<SessionId> = ids.iter().copied().collect();
    if let Err(e) = trim_unknown_session_refs_with_store(&store, &known) {
        warn!(error = ?e, "restore: trim_unknown_session_refs failed");
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
    let pending_ids: std::collections::HashSet<SessionId> = ctx.pending_spawn.lock().map(|g| g.keys().copied().collect()).unwrap_or_default();

    for (id, session) in sessions {
        if ctx.pool.contains(&id) || pending_ids.contains(&id) {
            debug!(session_id = %id, "restore: skipping already-live or already-pending session");
            continue;
        }

        // Worktree path validation — Roadmap §4.3 / Phase 7 in-app
        // workspace switch. If the worktree directory is gone (e.g.
        // user ran `git worktree remove` while the session was parked
        // across a workspace switch, or deleted the directory by hand
        // between launches), spawning would fail with an opaque OS
        // error. Drop the persisted record entirely (and trim it from
        // last_open_sessions / tab_order / active_session_id) so the
        // user doesn't get a permanent ghost tab they have to close
        // manually. The session is irrecoverable at this point — its
        // working directory is gone, and `composed_command` references
        // a path that no longer exists.
        if !session.worktree_path.is_dir() {
            warn!(
                session_id = %id,
                path = %session.worktree_path.display(),
                "restore: worktree missing; dropping persisted session record"
            );
            if let Err(e) = store.remove_session(&id) {
                warn!(session_id = %id, error = ?e, "restore: remove_session failed for stale-worktree drop");
                continue;
            }
            let cfg = store.load_config();
            let new_last: Vec<SessionId> = cfg.last_open_sessions.iter().copied().filter(|s| s != &id).collect();
            let new_order: Vec<SessionId> = cfg.tab_order.iter().copied().filter(|s| s != &id).collect();
            let active_patch: Option<Option<SessionId>> = match cfg.active_session_id {
                Some(active) if active == id => Some(new_order.first().copied()),
                _ => None,
            };
            if let Err(e) = store.save_config(PartialAppConfig {
                last_open_sessions: Some(new_last),
                tab_order: Some(new_order),
                active_session_id: active_patch,
                ..Default::default()
            }) {
                warn!(session_id = %id, error = ?e, "restore: save_config failed for stale-worktree drop");
            }
            continue;
        }

        // Re-materialise temp files in case they were swept by an OS-level
        // tmp clean. `respawn_existing` reuses `composed_command` verbatim.
        if let Err(e) = materialise_temp_files(&session.temp_files) {
            warn!(session_id = %id, error = ?e, "restore: temp-file materialise failed");
            let msg = format!("Failed to restore session temp files: {e}");
            let _ = store.update_session_status(&id, SessionStatus::Error, None);
            (ctx.sink.status)(&id, SessionStatus::Error, None, Some(msg));
            continue;
        }

        if let Err(e) = store.update_session_status(&id, SessionStatus::Starting, None) {
            warn!(session_id = %id, error = ?e, "restore: status update failed");
            continue;
        }
        (ctx.sink.status)(&id, SessionStatus::Starting, None, None);

        // AI-session resume — DESIGN §5.5. Augment composed_command with
        // `--resume <ai_session_id>` so the underlying CLI continues the
        // prior conversation. We *augment*, never *recompose from inputs*
        // (DESIGN §5.4 still holds for the persisted record). We only
        // resume on app-restart restore — user-initiated `session_restart`
        // intentionally allocates a fresh AI-side conversation (Copilot
        // gets a freshly-allocated uuid; Claude clears the field).
        //
        // For **Copilot** we splice unconditionally when `ai_session_id`
        // is set — we don't preflight against the on-disk session-state
        // directory because a `--resume <unknown-uuid>` is safe (Copilot
        // creates a fresh session at that uuid). The pre-allocated
        // create-time id may legitimately have no directory yet if the
        // app crashed before Copilot's first `session.start` flush;
        // splicing anyway gives Copilot a chance to materialize the
        // session at the persisted id rather than allocating a different
        // one and losing the link.
        //
        // For **Claude** we keep the preflight: a stale id with no
        // transcript would have Claude error out before the user sees
        // anything useful, so we drop the splice and start fresh.
        //
        // Known limitation (ROADMAP §4.5): for Claude, the
        // `ai_session_id` is discovered heuristically from the newest
        // JSONL in the project dir post-spawn. If two Arborist sessions
        // share the same worktree (same `<encoded-cwd>`), the watchers
        // can converge on the same file and persist the same id for
        // both. On restart, both sessions would then try to `--resume`
        // the same Claude conversation; only one resumes faithfully and
        // the other will see Claude's own "no such session" /
        // "conversation in use" error in its terminal. The fix is a
        // hook-driven session-id source (tracked in #4); the
        // single-session-per-worktree case (the common one) is
        // unaffected. Copilot is not affected — its conversation id is
        // pre-allocated by Arborist at create/restart time.
        let mut session_to_spawn = session.clone();
        if let Some(aid) = session.ai_session_id.as_deref() {
            let should_splice = match session.tool {
                Tool::Copilot => true,
                Tool::Claude => {
                    let exists = ai_session_transcript_exists(session.tool, &session.worktree_path, aid);
                    if !exists {
                        warn!(
                            session_id = %id,
                            ai_session_id = %aid,
                            "restore: Claude transcript missing on disk; starting fresh conversation",
                        );
                        let _ = store.update_session_ai_session_id(&id, None);
                    }
                    exists
                }
            };
            if should_splice {
                session_to_spawn.composed_command = compose::with_resume(&session.composed_command, session.tool, aid);
            }
        }

        match ctx.pending_spawn.lock().map(|mut g| g.insert(id, session_to_spawn)) {
            Ok(_) => {
                info!(
                    session_id = %id,
                    "restore: registered for deferred spawn (will fire on first session_resize)",
                );
            }
            Err(_) => {
                warn!(session_id = %id, "restore: pending_spawn mutex poisoned; skipping deferred registration");
                let _ = store.update_session_status(&id, SessionStatus::Error, None);
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
pub fn worktrees_list_impl(ctx: &AppContext, repo_root: &std::path::Path) -> Result<Vec<crate::types::WorktreeInfo>, AppError> {
    if !repo_root.is_dir() {
        debug!(
            code = "GitUnavailable",
            repo_root = %repo_root.display(),
            "worktrees_list: repo_root not a directory; returning empty list"
        );
        return Ok(Vec::new());
    }
    ctx.git_runner.list_worktrees(repo_root).map_err(AppError::from)
}

// ---------------------------------------------------------------------------
// workspace_validate / worktree_create (Roadmap §1, §2)
// ---------------------------------------------------------------------------

/// Validate a candidate workspace root for the first-boot picker. Never
/// returns an `AppError` for the "invalid" case — the picker shows inline
/// feedback. Real `AppError`s are reserved for unexpected backend failures.
///
/// "Valid" means: an absolute, existing directory whose
/// `git rev-parse --show-toplevel` equals itself **AND** which has
/// `<canon>/.git` as a *directory* (i.e. a primary clone, not a linked
/// worktree). Linked worktrees and submodule working trees have `.git`
/// as a *file* containing `gitdir: <path-into-primary>`; both are
/// rejected because Arborist's session model spawns child worktrees
/// from a primary repo root and a linked worktree cannot host its own
/// worktrees. This rejection is mirrored in
/// [`crate::boot::validate_repo_root`] for the boot-time resolution
/// chain (CLI / hint / legacy / native picker) — keep the two in sync.
///
/// `app_data_dir` + `branch` enable the optional Phase 8 advisory
/// lock-contention probe: if both are provided, after the path passes
/// repo-root validation we try a non-blocking acquire of the
/// per-(branch, workspace) `.lock` and report the result as
/// `already_open_in_another_instance`. Pass `None` for `app_data_dir`
/// in tests (or any caller that doesn't need the advisory signal) and
/// the field is left as `None` in the result.
pub fn workspace_validate_impl(
    ctx: &AppContext,
    path: &std::path::Path,
    app_data_dir: Option<&std::path::Path>,
    branch: &str,
) -> Result<crate::types::WorkspaceValidateResult, AppError> {
    use crate::types::WorkspaceValidateResult;

    let invalid = |msg: &str| WorkspaceValidateResult {
        valid: false,
        error: Some(msg.to_owned()),
        already_open_in_another_instance: None,
    };

    if path.as_os_str().is_empty() {
        return Ok(invalid("path is empty"));
    }
    if path.is_relative() {
        return Ok(invalid("path must be absolute"));
    }
    let canon = match crate::store_layout::CanonicalPath::canonicalise(path) {
        Ok(c) => c,
        Err(e) => return Ok(invalid(&format!("path could not be resolved: {e}"))),
    };
    if !canon.as_path().is_dir() {
        return Ok(invalid("path is not a directory"));
    }
    let toplevel = ctx.git_runner.git_toplevel(canon.as_path()).map_err(AppError::from)?;
    let Some(toplevel) = toplevel else {
        return Ok(invalid("path is not a git repository"));
    };
    if toplevel != *canon.as_path() {
        return Ok(invalid(&format!("path must be the repository root ({})", toplevel.display())));
    }
    // Reject linked git worktrees (and submodule working trees): they
    // have `.git` as a *file* (containing `gitdir: <path-into-primary>`),
    // whereas a primary clone has `.git` as a *directory*. Arborist's
    // model is "spawn child worktrees from a primary repo root" — a
    // linked worktree cannot host its own worktrees, so binding one as
    // a workspace would make every session-creation flow break. Mirrors
    // the parallel check in [`crate::boot::validate_repo_root`]; keep
    // the two in sync.
    if !canon.as_path().join(".git").is_dir() {
        return Ok(invalid(
            "path is a linked git worktree, not a primary repository root \
             (Arborist cannot spawn worktrees from inside another worktree; \
             pick the primary clone instead)",
        ));
    }

    // Phase 8 — advisory contention probe. Only meaningful for callers
    // that supplied `app_data_dir`; tests typically don't.
    let already_open = app_data_dir.and_then(|root| {
        let layout = crate::store_layout::StoreRoot::new(root, branch).for_workspace(&canon);
        match crate::workspace_lock::WorkspaceLockGuard::probe(layout.lock_path()) {
            Ok(free) => Some(!free),
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    "workspace_validate: lock probe I/O error; advisory signal unavailable"
                );
                None
            }
        }
    });

    Ok(WorkspaceValidateResult {
        valid: true,
        error: None,
        already_open_in_another_instance: already_open,
    })
}

// ---------------------------------------------------------------------------
// workspace_switch (Phase 7)
// ---------------------------------------------------------------------------

// In-app workspace switch — transactional swap of the active
// [`WorkspaceScope`] without restarting the process.
//
// Concurrency primitives used by this pipeline:
// * [`AppContext::switch_pending`] (`AtomicUsize`) — bumped before
//   the write lock is awaited; decremented when the
//   `SwitchPendingGuard` drops at function return. This is what makes
//   handlers' `try_read` rejections deterministic; see the rustdoc on
//   [`AppContext::switch_lock`] for why tokio's writer-preferring
//   fairness alone is insufficient.
// * [`AppContext::switch_lock`] (`tokio::sync::RwLock<()>`) —
//   acquired for write, held for the entire body. Quiesces in-flight
//   handlers (their read guards must drop before our `write().await`
//   resolves) and serialises concurrent switches.
//
// Pipeline:
// 1. Bump `switch_pending` and acquire `switch_lock.write().await`.
//    The counter is set BEFORE awaiting the lock so handlers issued
//    after this point reject (with `WorkspaceSwitchInProgress`) or
//    silently `Ok` (resize/frontend_ready) — see take-then-check in
//    [`acquire_switch_read`] and the rustdoc on
//    [`AppContext::switch_lock`] for why the counter is necessary in
//    addition to the lock. The `write().await` then waits for any
//    in-flight read guards (handlers whose work began before our
//    bump) to drop before resolving — this is what makes the rest of
//    the pipeline safe to run against an unobstructed scope. Both
//    guards are held for the **entire** function body and drop at
//    function return (success or error); concurrent switches queue
//    serially on the write side via tokio's FIFO waker queue.
// 2. Validate the new path is a real git-repository directory and
//    canonicalise it. Reject early on any failure — no state mutated.
// 3. No-op fast path: if the canonical new path equals the current
//    workspace, return `{ no_op: true }` immediately. The existing
//    binding stays valid; no event is emitted.
// 4. Acquire the OS lock + open `ConfigStore` for the new workspace
//    via [`crate::boot::bind_workspace`]. While step 4 runs we hold
//    BOTH the old lock (inside the current `WorkspaceScope`) and the
//    new lock (inside `WorkspaceBinding`). On
//    [`crate::boot::BootError::Contention`] we abort the switch with
//    `WorkspaceLocked` — the user stays on the current workspace.
// 5. Persist `workspace_root` into the *new* store's `config.json`
//    BEFORE the scope swap. If this write fails, abort cleanly:
//    drop `binding` (releases the new OS lock), the write guard's
//    Drop releases `switch_lock`, and the old workspace remains
//    bound. Doing this BEFORE the swap (rather than after) prevents
//    the failure mode where the swap commits but the new store
//    lacks `workspace_root`, which would make the post-switch
//    rehydrate read `workspaceRoot: null` and pop the first-boot
//    picker on top of an already-bound workspace.
// 6. **Drain the AI-discovery callback channel.**
//    `pending_spawn.clear()` drops any deferred-spawn entry queued
//    by the old workspace's restore, then
//    [`MetricsRegistry::stop_all_and_join`] stops AND joins every
//    metrics watcher thread. After this returns, no AI-session-
//    discovery callback for an old session can fire again. Under
//    our write guard no resize-deferred-spawn / restore can be in
//    flight (their read guards have all dropped before our
//    `write().await` resolved), so the join is deterministic — and
//    no new watchers can be armed until we drop the write guard at
//    function exit.
// 7. **Park** old workspace sessions — and, as a side effect, drain
//    the PTY-status callback channel. We `pool.kill().await` per
//    session: the kill sets `killed=true` (so the wait thread skips
//    its final status emit, see `pty_pool::pty_wait_loop`), awaits
//    the bounded drain task that pumps PTY output, and joins the
//    wait thread before returning. After step 7 finishes, no PTY
//    output / status callback for an old session can fire again. We
//    **preserve** every session record (sessions.json,
//    lastOpenSessions, tabOrder, activeSessionId untouched). When
//    the user switches back to this workspace,
//    [`restore_all_sessions`] re-spawns the PTYs from the persisted
//    records — Claude/Copilot `--resume` splicing
//    ([`compose::with_resume`]) keeps the AI conversation context
//    alive across the round-trip. Park is *best-effort*: a failed
//    `pool.kill` (rare; e.g. PTY already dead) is logged and
//    ignored. There is no abort path because park performs zero
//    irreversible store mutations.
// 8. Swap [`AppContext::workspace`] under `RwLock` write — the old
//    `WorkspaceLockGuard` inside the old scope is dropped here, in
//    one atomic moment, releasing the OS lock on the old workspace.
//    Steps 6 and 7 ensured the only callbacks that *could* still
//    fire are post-emit Tauri event deliveries to the JS side
//    (handled by the `NotFound`-tolerant store re-resolution in
//    `commands::mod::build_production_*`). The `restored` atomic is
//    **not** reset here (PR4's flow reset it before the swap so the
//    frontend's follow-up `frontend_ready` could trigger restore;
//    PR5 owns the restore inline at step 10, so resetting would be
//    wrong — it would let a defensive `frontend_ready` re-fire
//    restore against a workspace that was just restored).
// 9. Best-effort: update the per-branch `last-workspace.json` hint
//    so the next launch resumes the new workspace by default.
//    `workspace_root` was already persisted at step 5, so the
//    post-swap frontend rehydrate is correct regardless of whether
//    this succeeds.
// 10. Run [`restore_all_sessions`] for the new workspace inline. We
//     are still under our exclusive `switch_lock.write()` guard, so no
//     other workspace-mutating handler can interleave (lifecycle
//     handlers reject; `frontend_ready` / `session_resize` silently
//     no-op). Restore is dispatched onto a `spawn_blocking` thread
//     because it does store IO + temp-file materialise + cleanup_orphans
//     — same rationale as the existing `commands::frontend_ready`
//     wrapper. Awaiting the join ensures sessions are in their
//     post-restore state (`Starting` / `Error`) before we build the
//     response. After the join, **latch `ctx.restored` to `true`**
//     so any subsequent `frontend_ready` (defensive re-issue from the
//     frontend, or a future code path that calls it after a switch)
//     becomes a no-op CAS — restore for this binding has already
//     fired exactly once, here.
// 11. Build [`WorkspaceSwitchResult`] from the new store
//     (`load_config` + `session_list_impl`). The frontend adopts the
//     full state in one render — no follow-up `frontend_ready`
//     round-trip and no `workspace://changed` event is needed.
//     Returning from the function drops both guards (write lock +
//     `switch_pending` decrement), allowing queued lifecycle
//     handlers to proceed against the new scope.

/// Tauri-shaped wrapper around the inner switch implementation —
/// converts the [`AppHandle`] into the testable seams the inner
/// function needs (just an `app_data_dir` path).
///
/// Production callers go through this; tests can call
/// [`workspace_switch_impl_inner`] directly with a tempdir to avoid
/// having to build a real Tauri app.
pub async fn workspace_switch_impl(
    ctx: &Arc<AppContext>,
    app_handle: &tauri::AppHandle,
    new_path: &Path,
) -> Result<crate::types::WorkspaceSwitchResult, AppError> {
    use tauri::Manager;

    let app_data_dir = app_handle
        .path()
        .app_data_dir()
        .map_err(|e| AppError::new("Io", format!("app_data_dir: {e}")))?;
    workspace_switch_impl_inner(ctx, &app_data_dir, crate::BUILD_BRANCH, new_path).await
}

/// Testable inner of [`workspace_switch_impl`]. See module-level docs
/// on the public wrapper for the full pipeline. Split out so unit tests
/// can drive the swap with a tempdir-backed `app_data_dir` without
/// standing up a real Tauri app.
pub async fn workspace_switch_impl_inner(
    ctx: &Arc<AppContext>,
    app_data_dir: &Path,
    branch: &str,
    new_path: &Path,
) -> Result<crate::types::WorkspaceSwitchResult, AppError> {
    use crate::types::WorkspaceSwitchResult;

    // Step 1 — bump `switch_pending` and acquire the workspace switch
    // barrier for write. Held for the entire function body. The
    // counter MUST be incremented *before* the `write().await` so
    // concurrent `try_read`s in workspace-mutating handlers see a
    // queued switch and reject (tokio's `RwLock::try_read` is
    // permit-based and does NOT honour writer-preferring fairness for
    // non-awaiting callers). The `SwitchPendingGuard` decrements on
    // drop (normal return, early return, panic). Concurrent switches
    // queue serially on the same `write().await`. See
    // [`AppContext::switch_lock`].
    let _pending_guard = SwitchPendingGuard::new(Arc::clone(&ctx.switch_pending));
    let _switch = ctx.switch_lock.write().await;

    // Step 2 — validate + canonicalise. We re-use workspace_validate_impl
    // for parity with what the frontend already showed the user. Empty /
    // relative / non-dir / non-repo all turn into a clean error here.
    // Pass `None` for `app_data_dir` to skip the advisory contention
    // probe — the authoritative lock acquire happens in step 4 anyway.
    let validate = workspace_validate_impl(ctx, new_path, None, branch)?;
    if !validate.valid {
        return Err(AppError::new(
            "InvalidPath",
            validate.error.unwrap_or_else(|| "workspace validation failed".to_owned()),
        ));
    }
    let canonical = dunce::canonicalize(new_path).map_err(|e| AppError::new("InvalidPath", format!("could not canonicalise workspace path: {e}")))?;

    // Step 3 — no-op fast path. We populate `config` and `sessions`
    // from the *current* (unchanged) store so the wire payload is
    // non-nullable; the frontend short-circuits adoption on the
    // `noOp` flag.
    let current_root = ctx.workspace.read().expect("workspace lock poisoned").workspace_root.clone();
    if current_root.as_ref() == Some(&canonical) {
        let config = ctx.store().load_config();
        let sessions = session_list_impl(ctx)?;
        return Ok(WorkspaceSwitchResult {
            workspace_root: canonical,
            no_op: true,
            config,
            sessions,
        });
    }

    // Step 4 — acquire OS lock + ConfigStore for the new workspace.
    if let Err(e) = std::fs::create_dir_all(app_data_dir) {
        return Err(AppError::new("Io", format!("create_dir_all({}): {e}", app_data_dir.display())));
    }
    let binding = match crate::boot::bind_workspace(&canonical, app_data_dir, branch, ctx.git_runner.as_ref(), crate::boot::BootSource::Picker) {
        Ok(b) => b,
        Err(crate::boot::BootError::Contention { branch, workspace }) => {
            return Err(AppError::new(
                "WorkspaceLocked",
                format!(
                    "Workspace is already open in another Arborist window (branch: {}, path: {}).",
                    if branch.trim().is_empty() { "main" } else { &branch },
                    workspace.display(),
                ),
            ));
        }
        Err(crate::boot::BootError::NotARepository {
            workspace,
            reason,
            origin: _,
        }) => {
            // Defensively unreachable: step 2 above already ran
            // `workspace_validate_impl` which performs the same
            // git-toplevel check. Surface as InvalidPath so the
            // frontend treats it like the validate-failure shape
            // rather than a generic internal error if it ever
            // does fire (e.g. the repo was deleted between steps).
            return Err(AppError::new(
                "InvalidPath",
                format!("workspace path is not a git repository root ({}): {reason}", workspace.display()),
            ));
        }
        Err(other) => {
            return Err(AppError::new("Internal", format!("workspace bind failed: {other}")));
        }
    };

    // Step 5 — persist `workspace_root` into the NEW workspace's
    // `config.json` BEFORE we commit the scope swap. If this write
    // fails, the post-switch frontend rehydrate would otherwise read
    // `workspaceRoot: null` from the new store and fall back to the
    // first-boot picker even though the backend had already swapped
    // — a self-contradictory state that's hard for the user to
    // recover from. By writing first, we can abort cleanly:
    //
    // * Drop `binding` on the early-return path → releases the new
    //   OS lock, reverting any state we may have started to
    //   materialise on the new workspace.
    // * Old `WorkspaceScope` is still bound (we have not yet
    //   modified `ctx.workspace`).
    // * `_switch`'s Drop releases the write guard so subsequent
    //   commands resume against the still-bound old workspace.
    //
    // This is the asymmetric counterpart to `boot_select_workspace`,
    // which tolerates the same failure (boot is one-shot; the user
    // can restart). See `ensure_workspace_root_in_config` docs.
    if let Err(e) = crate::boot::ensure_workspace_root_in_config(&binding.store, &canonical) {
        return Err(AppError::new(
            "Internal",
            format!("failed to persist workspace_root into new workspace's config.json before switch commit: {e}"),
        ));
    }

    // Step 6 — drain pending_spawn and join all metrics watchers.
    // Under our write guard, no resize-deferred-spawn / restore /
    // lifecycle handler can be in flight (every one of them either
    // dropped its read guard before our `write().await` resolved, or
    // is rejected outright by `try_read` while we hold this guard).
    // So `stop_all_and_join` deterministically joins every armed
    // worker, and no new watchers can be armed until we drop the
    // write guard at function exit.
    if let Ok(mut g) = ctx.pending_spawn.lock() {
        g.clear();
    }
    ctx.metrics.stop_all_and_join();

    // Step 7 — **park** old workspace sessions. We kill the PTYs but
    // **preserve** every session record (sessions.json, lastOpenSessions,
    // tabOrder, activeSessionId untouched). When the user switches back
    // to this workspace, restore_all_sessions will re-spawn the PTYs
    // from the persisted records — Claude/Copilot `--resume` splicing
    // (compose::with_resume) keeps the AI conversation context alive
    // across the round-trip.
    //
    // Park is *best-effort*: a failed `pool.kill` (rare; e.g. PTY
    // already dead) is logged and ignored. There is no abort path
    // because park performs zero irreversible store mutations — at
    // worst we leak a still-running child PTY whose record will be
    // re-found on the next switch-back. The previous "close + recovery
    // loop" complexity was driven by `session_close_impl`'s
    // `store.remove_session` + `save_config` being permanent; with
    // park, neither happens, so neither does the recovery.
    //
    // Enumerate from the *current* store (still the old one until the
    // swap in step 9). park_session_for_switch_impl uses ctx.store()
    // which clones from the old scope; safe.
    let old_session_ids: Vec<SessionId> = ctx.store().load_sessions().keys().copied().collect();
    for id in old_session_ids {
        park_session_for_switch_impl(ctx, id).await;
    }

    // Step 8 — swap WorkspaceScope. The OLD WorkspaceLockGuard inside
    // the old scope is dropped at this assignment, releasing the OS
    // lock on the old workspace. We swap **before** running restore
    // because `restore_all_sessions` calls `ctx.store()` which
    // delegates to `ctx.workspace.read()` — restore must read from
    // the NEW workspace's store.
    let new_scope = crate::boot::into_scope(binding);
    {
        let mut w = ctx.workspace.write().expect("workspace lock poisoned");
        *w = new_scope;
    }

    // Step 9 — best-effort hint. `workspace_root` was already
    // persisted at step 5, so the frontend rehydrate is correct
    // regardless of whether this succeeds; the hint is only used at
    // the *next* process boot to skip the picker.
    if let Err(e) = crate::boot::write_hint(app_data_dir, branch, &canonical) {
        warn!(error = %e, "failed to persist last-workspace hint after switch; non-fatal");
    }

    // Step 10 — run [`restore_all_sessions`] for the new workspace
    // INLINE while we still hold the write guard. PR5 collapsed the
    // previous "reset gate → return → frontend re-issues
    // frontend_ready → backend kicks off restore" round-trip into
    // this single call so the response can carry the post-restore
    // state and the frontend adopts everything in one render.
    //
    // We dispatch onto `spawn_blocking` because restore does store
    // IO + temp-file materialise + cleanup_orphans (same rationale
    // as the existing `commands::frontend_ready` wrapper).
    //
    // Holding the write guard across the await is correct: tokio's
    // `RwLockWriteGuard` is `Send`, and no `acquire_switch_read`
    // caller can interleave (they're rejected for the duration of
    // our guard). We do NOT reset `restored` to `false` — a future
    // `frontend_ready` from the frontend is a no-op CAS, which is
    // correct because restore for this binding has already fired
    // exactly once (here).
    //
    // Errors from `restore_all_sessions` are best-effort logged
    // inside the function; the only thing that *can* fail at this
    // boundary is the `spawn_blocking` JoinHandle, which we surface
    // as Internal so the caller sees a clean failure rather than a
    // half-restored state.
    let ctx_for_restore = Arc::clone(ctx);
    let restore_join = tauri::async_runtime::spawn_blocking(move || {
        restore_all_sessions(&ctx_for_restore);
    });
    if let Err(join_err) = restore_join.await {
        return Err(AppError::new(
            "Internal",
            format!("restore_all_sessions task panicked during workspace switch: {join_err}"),
        ));
    }
    // Latch the `restored` gate to true so that any subsequent
    // `frontend_ready` (e.g. defensive re-issue from the frontend, or a
    // future code path that calls it after a switch) becomes a no-op
    // CAS. PR4's flow reset this to `false` BEFORE the swap so the
    // frontend's follow-up `frontend_ready` could trigger restore; PR5
    // owns the restore inline, so we must explicitly mark it done to
    // prevent a double-spawn.
    ctx.restored.store(true, Ordering::SeqCst);

    // Step 11 — assemble the result. The write guard `_switch` is
    // dropped at function return, allowing queued lifecycle handlers
    // to proceed against the new scope. Frontend adopts the full
    // payload in one `setState`.
    let config = ctx.store().load_config();
    let sessions = session_list_impl(ctx)?;
    info!(workspace = %canonical.display(), "workspace switch complete");
    Ok(WorkspaceSwitchResult {
        workspace_root: canonical,
        no_op: false,
        config,
        sessions,
    })
}

/// Create a new linked worktree at `<workspaceRoot>/.worktrees/<name>` on a
/// fresh branch named `<name>`.
pub fn worktree_create_impl(ctx: &AppContext, name: &str) -> Result<crate::types::WorktreeCreateResult, AppError> {
    use crate::types::WorktreeCreateResult;

    // Validate the name with the same rules the frontend used (defence in depth).
    let validated = compose::validate_worktree_name(name).map_err(|msg| AppError::from(Error::InvalidPath(msg)))?;

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
        return Err(AppError::from(Error::InvalidPath(format!("{} already exists", absolute.display()))));
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
        std::fs::create_dir_all(&worktrees_dir)
            .map_err(|e| AppError::from(Error::Internal(format!("could not create {}: {e}", worktrees_dir.display()))))?;
    }
    let canon_worktrees = dunce::canonicalize(&worktrees_dir)
        .map_err(|e| AppError::from(Error::Internal(format!("could not canonicalize {}: {e}", worktrees_dir.display()))))?;
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
fn lookup_instruction_set(cfg: &crate::types::AppConfig, id: &crate::types::InstructionSetId, tool: Tool) -> Result<InstructionSet, AppError> {
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
        return Err(AppError::from(Error::NotFound(format!("instruction set {id} not found"))));
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
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

// Re-export some common path types so call sites don't have to remember the
// import paths.
pub use std::path::Path as _SessionPath;
#[allow(dead_code)]
type _PathBufAlias = PathBuf;
