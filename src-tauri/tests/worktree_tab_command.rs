//! Backend regression tests for the worktree-tab command surface (Issue #44, PR #65 review feedback).
//!
//! Coverage focuses on the bugs surfaced by the PR review:
//! * `worktree_tab_open` is idempotent on path AND must promote the existing tab to active so "open" doubles as "focus".
//! * `worktree_tab_open` returns the stable `WorktreeMissing` / `InvalidPath` error codes (matching `session_create` via `compose::validate_worktree`)
//!   so the frontend can branch on a single shared set of codes regardless of which command surfaced the failure.
//! * `worktree_tab_reorder` rejects duplicate ids (`[A, A, B]` against `{A, B, C}`) instead of silently dropping `C`.
//! * `worktree_tab_reorder` rejects partial lists and leaves both `worktree_tab_order` and per-tab `tab_index` unchanged on validation failure.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use arborist_lib::commands::session::AppContext;
use arborist_lib::commands::worktree_tab::{worktree_tab_focus_impl, worktree_tab_open_impl, worktree_tab_reorder_impl};
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
    config_dir: TempDir,
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
        config_dir,
        worktree_dirs: Mutex::new(Vec::new()),
    }
}

/// Path to the on-disk `config.json` for this harness. Used by no-op-no-write regression tests that assert disk state is unchanged across calls.
fn config_path(h: &Harness) -> PathBuf {
    h.config_dir.path().join("config.json")
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
// open: input validation — error codes must stay aligned with the session-create path so the frontend can branch on a single shared set of
// codes regardless of which surface routed the request.
// ---------------------------------------------------------------------------

#[test]
fn open_rejects_relative_path_with_invalid_argument() {
    let h = build_harness();
    let err = worktree_tab_open_impl(&h.ctx, WorktreeTabOpenArgs { path: "relative/dir".into() }).expect_err("relative path must be rejected");
    assert_eq!(err.code, "InvalidArgument", "expected InvalidArgument, got {err:?}");
    assert!(
        err.message.to_lowercase().contains("absolute"),
        "error should mention absolute requirement, got {:?}",
        err.message
    );
}

#[test]
fn open_returns_worktree_missing_for_nonexistent_path() {
    // Regression for PR #65 review feedback — must return the stable `WorktreeMissing` code (same code `session_create` returns via
    // `compose::validate_worktree`) so the frontend can route both API surfaces through the same error-handling branch.
    let h = build_harness();
    let missing = std::env::temp_dir().join(format!("arborist-test-missing-{}", uuid::Uuid::new_v4()));
    assert!(!missing.exists(), "test setup invariant: path must not exist");

    let err = worktree_tab_open_impl(
        &h.ctx,
        WorktreeTabOpenArgs {
            path: missing.to_string_lossy().into_owned(),
        },
    )
    .expect_err("missing path must be rejected");
    assert_eq!(err.code, "WorktreeMissing", "expected WorktreeMissing, got {err:?}");
}

#[test]
fn open_returns_invalid_path_when_path_exists_but_is_not_a_directory() {
    let h = build_harness();
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("a-file.txt");
    std::fs::write(&file, b"not a directory").unwrap();

    let err = worktree_tab_open_impl(
        &h.ctx,
        WorktreeTabOpenArgs {
            path: file.to_string_lossy().into_owned(),
        },
    )
    .expect_err("non-directory path must be rejected");
    assert_eq!(err.code, "InvalidPath", "expected InvalidPath, got {err:?}");
    assert!(
        err.message.to_lowercase().contains("not a directory"),
        "error should mention not-a-directory, got {:?}",
        err.message
    );
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

// ---------------------------------------------------------------------------
// disk-churn: idempotent / NotFound calls must not rewrite config.json (PR #65 sixth/seventh-review-round feedback)
// ---------------------------------------------------------------------------

/// Read the file's bytes, then **roll its mtime back ~60s** and return both. Combined with a post-call comparison
/// (`mtime_after == mtime_before` for no-op tests), this makes the rewrite-detection independent of filesystem timestamp resolution: if anything
/// rewrites the file, `mtime_after` will jump to ~now (visibly different from "60s ago" even on coarse-resolution filesystems like FAT 2s/HFS+ 1s).
/// The previous "sleep 50ms; expect mtime to advance" approach was false-negative on those filesystems — a real rewrite within the same mtime tick
/// would not visibly change `modified()`. (PR #65 review-7 fix.)
fn snapshot_with_rolled_back_mtime(path: &Path) -> (Vec<u8>, std::time::SystemTime) {
    let bytes = std::fs::read(path).expect("read config.json snapshot");
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open config.json for set_modified (write(true) does not truncate)");
    f.set_modified(old)
        .expect("set_modified must succeed (Rust 1.75+); fail loudly on platforms without timestamp support");
    drop(f);
    let mtime = std::fs::metadata(path).expect("stat").modified().expect("mtime");
    (bytes, mtime)
}

#[test]
fn open_existing_active_tab_does_not_rewrite_config_json() {
    // Re-opening a worktree-tab path that already exists AND is already active is a true no-op. Without an early-return fast path, every such call
    // would re-enter `save_config_with`, which writes `config.json` unconditionally — producing pointless disk churn (and config-mtime changes the
    // frontend or filesystem watchers might react to) on every UI re-trigger.
    let h = build_harness();
    let path = fresh_worktree_dir(&h);
    let id = open(&h, &path);

    let cfg_path = config_path(&h);
    assert!(cfg_path.exists(), "config.json must be on disk after the first open");
    let (bytes_before, mtime_before) = snapshot_with_rolled_back_mtime(&cfg_path);

    let id_again = open(&h, &path);
    assert_eq!(id_again, id);

    let (bytes_after, mtime_after) = snapshot_file(&cfg_path);
    assert_eq!(bytes_before, bytes_after, "no-op open must leave config.json byte-identical");
    assert_eq!(
        mtime_before, mtime_after,
        "no-op open must NOT rewrite config.json (mtime advanced from rolled-back snapshot)"
    );
}

#[test]
fn focus_with_unknown_id_does_not_rewrite_config_json() {
    // `worktree_tab_focus` for a stale id (e.g. one racing a close) must surface `NotFound` without touching the disk. Without an early-return,
    // every such call re-entered `save_config_with` (which always writes), producing one unnecessary write per stale id from the UI.
    let h = build_harness();
    // Open one tab so config.json materialises on disk.
    let _id = open(&h, &fresh_worktree_dir(&h));

    let cfg_path = config_path(&h);
    let (bytes_before, mtime_before) = snapshot_with_rolled_back_mtime(&cfg_path);

    let bogus = WorktreeTabId::new();
    let err = worktree_tab_focus_impl(&h.ctx, bogus).expect_err("focus on unknown id must error");
    assert_eq!(err.code, "NotFound", "expected NotFound, got {err:?}");

    let (bytes_after, mtime_after) = snapshot_file(&cfg_path);
    assert_eq!(bytes_before, bytes_after, "NotFound focus must leave config.json byte-identical");
    assert_eq!(
        mtime_before, mtime_after,
        "NotFound focus must NOT rewrite config.json (mtime advanced from rolled-back snapshot)"
    );
}

#[test]
fn focus_existing_tab_persists_active_id_and_does_rewrite_config_json() {
    // Companion to the no-op tests above: when focus *does* change the active tab, it must persist that change — i.e. the early-return fast paths
    // must not over-eagerly skip writes that are real mutations. Open A, open B (B becomes active), focus A → expect A to become active AND
    // config.json to be rewritten.
    let h = build_harness();
    let id_a = open(&h, &fresh_worktree_dir(&h));
    let id_b = open(&h, &fresh_worktree_dir(&h));
    assert_eq!(h.ctx.store().load_config().active_worktree_tab_id, Some(id_b));

    let cfg_path = config_path(&h);
    let (bytes_before, mtime_before) = snapshot_with_rolled_back_mtime(&cfg_path);

    worktree_tab_focus_impl(&h.ctx, id_a).expect("focus A must succeed");

    assert_eq!(h.ctx.store().load_config().active_worktree_tab_id, Some(id_a));
    let (bytes_after, mtime_after) = snapshot_file(&cfg_path);
    assert_ne!(bytes_before, bytes_after, "focus that changes the active tab MUST rewrite config.json");
    // mtime_before was rolled back ~60s; a real rewrite stamps mtime to ~now, so the diff is enormous regardless of FS resolution.
    let advance = mtime_after.duration_since(mtime_before).expect("mtime moved forward");
    assert!(
        advance > std::time::Duration::from_secs(30),
        "rewrite must visibly advance mtime by tens of seconds (rolled-back snapshot → ~now); got {advance:?}",
    );
}

/// Helper retained for post-call snapshots (no rollback needed — we just want bytes + current mtime).
fn snapshot_file(path: &Path) -> (Vec<u8>, std::time::SystemTime) {
    let bytes = std::fs::read(path).expect("read config.json snapshot");
    let mtime = std::fs::metadata(path).expect("stat config.json").modified().expect("mtime");
    (bytes, mtime)
}

// ---------------------------------------------------------------------------
// open: icon assignment (Issue #45)
// ---------------------------------------------------------------------------

/// Each new tab in a fresh workspace must receive an `iconId` that's least-used among the existing tabs (lowest number wins on ties), so the first
/// 16 distinct worktrees walk 1..=16 in order. Catches a regression where `worktree_tab_open_impl` would forget to assign an icon and persist 0.
#[test]
fn open_assigns_distinct_icons_to_first_n_distinct_worktrees() {
    use arborist_lib::worktree_icon::WORKTREE_ICON_COUNT;

    let h = build_harness();
    let mut assigned: Vec<u32> = Vec::new();
    for i in 0..WORKTREE_ICON_COUNT {
        let path = fresh_worktree_dir(&h);
        let id = open(&h, &path);
        let cfg = h.ctx.store().load_config();
        let tab = cfg.worktree_tabs.iter().find(|t| t.id == id).expect("tab persisted");
        assert!(
            (1..=WORKTREE_ICON_COUNT).contains(&tab.icon_id),
            "tab #{i} must get a valid iconId, got {}",
            tab.icon_id
        );
        assigned.push(tab.icon_id);
    }
    let expected: Vec<u32> = (1..=WORKTREE_ICON_COUNT).collect();
    assert_eq!(
        assigned, expected,
        "first {WORKTREE_ICON_COUNT} distinct worktrees must walk icons 1..={WORKTREE_ICON_COUNT} in order",
    );
}

/// Tab N+1 (after the icon set is exhausted) must wrap back to icon 1 — every existing icon has count 1, so the lowest-numbered ties wins.
#[test]
fn open_wraps_icon_assignment_after_set_is_exhausted() {
    use arborist_lib::worktree_icon::WORKTREE_ICON_COUNT;

    let h = build_harness();
    for _ in 0..WORKTREE_ICON_COUNT {
        let _ = open(&h, &fresh_worktree_dir(&h));
    }
    let extra_id = open(&h, &fresh_worktree_dir(&h));
    let cfg = h.ctx.store().load_config();
    let extra = cfg.worktree_tabs.iter().find(|t| t.id == extra_id).expect("extra tab persisted");
    assert_eq!(
        extra.icon_id,
        1,
        "tab #{} must reuse icon 1 (lowest at min count = 1)",
        WORKTREE_ICON_COUNT + 1
    );
}

/// Re-opening an existing path returns the existing tab unchanged — including its `iconId`. Catches a regression where the idempotent path would
/// re-pick an icon and silently change the user's visual identifier across restarts.
#[test]
fn reopening_existing_path_preserves_icon_id() {
    let h = build_harness();
    let path = fresh_worktree_dir(&h);
    let id = open(&h, &path);
    let original_icon = h
        .ctx
        .store()
        .load_config()
        .worktree_tabs
        .iter()
        .find(|t| t.id == id)
        .expect("tab persisted")
        .icon_id;

    // Open another path so the in-memory cfg moves on, then re-open the original.
    let _other = open(&h, &fresh_worktree_dir(&h));
    let id_again = open(&h, &path);
    assert_eq!(id_again, id, "open must remain idempotent on canonical path");

    let cfg = h.ctx.store().load_config();
    let same_tab = cfg.worktree_tabs.iter().find(|t| t.id == id).expect("tab still present");
    assert_eq!(same_tab.icon_id, original_icon, "iconId must survive an idempotent re-open");
}
