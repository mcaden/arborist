//! Backend regression tests for the worktree-tab command surface (Issue #44, PR #65 review feedback).
//!
//! Coverage focuses on the bugs surfaced by the PR review:
//! * `worktree_tab_open` is idempotent on path AND must promote the existing tab to active so "open" doubles as "focus".
//! * `worktree_tab_reorder` rejects duplicate ids (`[A, A, B]` against `{A, B, C}`) instead of silently dropping `C`.
//! * `worktree_tab_reorder` rejects partial lists and leaves both `worktree_tab_order` and per-tab `tab_index` unchanged on validation failure.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use arborist_lib::commands::session::AppContext;
use arborist_lib::commands::worktree_tab::{worktree_tab_open_impl, worktree_tab_reorder_impl};
use arborist_lib::config_store::ConfigStore;
use arborist_lib::git::GitRunner;
use arborist_lib::pty_pool::{PortablePtySpawner, PtyPool, PtySink};
use arborist_lib::types::{Error, SessionId, SessionStatus, WorktreeInfo, WorktreeTabId, WorktreeTabOpenArgs};
use tempfile::TempDir;

struct NullGitRunner;

impl GitRunner for NullGitRunner {
    fn list_worktrees(&self, _: &Path) -> Result<Vec<WorktreeInfo>, Error> {
        Ok(vec![])
    }
    fn git_toplevel(&self, _: &Path) -> Result<Option<PathBuf>, Error> {
        Ok(None)
    }
    fn create_worktree(&self, _repo_root: &Path, _relative_path: &Path, _branch: &str) -> Result<PathBuf, Error> {
        Err(Error::Internal("create_worktree unused".into()))
    }
    fn remove_worktree(&self, _repo_root: &Path, _worktree_path: &Path) -> Result<(), Error> {
        Ok(())
    }
}

fn null_sink() -> PtySink {
    let output = Arc::new(|_: &SessionId, _: String| {});
    let status = Arc::new(|_: &SessionId, _: SessionStatus, _: Option<u32>, _: Option<String>| {});
    PtySink::new(output, status, Arc::new(|_id, _evt| {}))
}

struct Harness {
    ctx: Arc<AppContext>,
    _config_dir: TempDir,
    worktree_dirs: Mutex<Vec<TempDir>>,
}

fn build_harness() -> Harness {
    let config_dir = TempDir::new().unwrap();
    let store = ConfigStore::open(config_dir.path()).unwrap();
    let pool = Arc::new(PtyPool::new(Arc::new(PortablePtySpawner)));
    let ctx = Arc::new(AppContext::new(
        pool,
        store,
        null_sink(),
        Arc::new(NullGitRunner),
        Arc::new(|_| {}),
        Arc::new(|_, _| {}),
        Arc::new(|_, _| {}),
    ));
    Harness {
        ctx,
        _config_dir: config_dir,
        worktree_dirs: Mutex::new(Vec::new()),
    }
}

/// Creates a real on-disk directory and returns its absolute path. The TempDir handle is parked on the harness so it lives for the test's lifetime
/// (otherwise canonicalize-then-is_dir would fail mid-test).
fn fresh_worktree_dir(h: &Harness) -> PathBuf {
    let td = TempDir::new().unwrap();
    let path = td.path().to_path_buf();
    h.worktree_dirs.lock().unwrap().push(td);
    path
}

fn open(h: &Harness, path: &Path) -> WorktreeTabId {
    worktree_tab_open_impl(
        &h.ctx,
        WorktreeTabOpenArgs {
            path: path.to_string_lossy().into_owned(),
        },
    )
    .expect("open should succeed")
    .id
}

// ---------------------------------------------------------------------------
// open: idempotency + focus-existing
// ---------------------------------------------------------------------------

#[test]
fn open_returns_existing_tab_for_repeated_path_and_promotes_it_to_active() {
    let h = build_harness();
    let path_a = fresh_worktree_dir(&h);
    let path_b = fresh_worktree_dir(&h);

    let id_a = open(&h, &path_a);
    let id_b = open(&h, &path_b);
    // After opening B, B is active (more recent open).
    assert_eq!(h.ctx.store().load_config().active_worktree_tab_id, Some(id_b));

    // Re-opening A should return the same tab id (no duplicate created) AND promote A back to active.
    let id_a_again = open(&h, &path_a);
    assert_eq!(id_a_again, id_a, "open must be idempotent on canonical path");

    let cfg = h.ctx.store().load_config();
    assert_eq!(cfg.worktree_tabs.len(), 2, "no duplicate tab created");
    assert_eq!(cfg.worktree_tab_order.len(), 2);
    assert_eq!(
        cfg.active_worktree_tab_id,
        Some(id_a),
        "open must focus the existing tab so the active id moves to A"
    );
}

#[test]
fn open_existing_tab_when_already_active_is_a_no_op_at_storage_level() {
    let h = build_harness();
    let path = fresh_worktree_dir(&h);
    let id = open(&h, &path);
    let before = h.ctx.store().load_config();
    assert_eq!(before.active_worktree_tab_id, Some(id));

    let id_again = open(&h, &path);
    assert_eq!(id_again, id);

    let after = h.ctx.store().load_config();
    assert_eq!(after.worktree_tabs.len(), 1);
    assert_eq!(after.active_worktree_tab_id, Some(id));
}

// ---------------------------------------------------------------------------
// reorder: validation
// ---------------------------------------------------------------------------

#[test]
fn reorder_rejects_duplicate_ids_and_leaves_state_unchanged() {
    let h = build_harness();
    let id_a = open(&h, &fresh_worktree_dir(&h));
    let id_b = open(&h, &fresh_worktree_dir(&h));
    let id_c = open(&h, &fresh_worktree_dir(&h));
    let original_order = h.ctx.store().load_config().worktree_tab_order.clone();
    assert_eq!(original_order, vec![id_a, id_b, id_c]);

    // [A, A, B] passes the length check (3) AND every element is "known", but it duplicates A and drops C — must error out.
    let err = worktree_tab_reorder_impl(&h.ctx, vec![id_a, id_a, id_b]).expect_err("duplicate ids must be rejected");
    assert_eq!(err.code, "InvalidArgument", "expected InvalidArgument, got {err:?}");
    assert!(
        err.message.to_lowercase().contains("duplicate"),
        "error message should mention duplicates, got {:?}",
        err.message
    );

    // No mutation: order untouched, every tab_index still matches its position in the original order.
    let cfg = h.ctx.store().load_config();
    assert_eq!(cfg.worktree_tab_order, original_order);
    let by_id: std::collections::HashMap<_, _> = cfg.worktree_tabs.iter().map(|t| (t.id, t.tab_index)).collect();
    assert_eq!(by_id[&id_a], 0);
    assert_eq!(by_id[&id_b], 1);
    assert_eq!(by_id[&id_c], 2);
}

#[test]
fn reorder_rejects_wrong_length_and_leaves_state_unchanged() {
    let h = build_harness();
    let id_a = open(&h, &fresh_worktree_dir(&h));
    let id_b = open(&h, &fresh_worktree_dir(&h));
    let _id_c = open(&h, &fresh_worktree_dir(&h));
    let before = h.ctx.store().load_config();

    let err = worktree_tab_reorder_impl(&h.ctx, vec![id_b, id_a]).expect_err("wrong-length list must be rejected");
    assert_eq!(err.code, "InvalidArgument");

    let after = h.ctx.store().load_config();
    assert_eq!(after.worktree_tab_order, before.worktree_tab_order);
    assert_eq!(after.worktree_tabs.len(), before.worktree_tabs.len());
}

#[test]
fn reorder_applies_new_order_and_updates_tab_index() {
    let h = build_harness();
    let id_a = open(&h, &fresh_worktree_dir(&h));
    let id_b = open(&h, &fresh_worktree_dir(&h));
    let id_c = open(&h, &fresh_worktree_dir(&h));

    worktree_tab_reorder_impl(&h.ctx, vec![id_c, id_a, id_b]).expect("reorder ok");

    let cfg = h.ctx.store().load_config();
    assert_eq!(cfg.worktree_tab_order, vec![id_c, id_a, id_b]);
    let by_id: std::collections::HashMap<_, _> = cfg.worktree_tabs.iter().map(|t| (t.id, t.tab_index)).collect();
    assert_eq!(by_id[&id_c], 0);
    assert_eq!(by_id[&id_a], 1);
    assert_eq!(by_id[&id_b], 2);
}
