//! Phase 7 happy-path integration test against the **real** PortablePtySpawner.
//!
//! We override the `claude` program token with the test-only env-var seam
//! (`GROVE_CLI_OVERRIDE_CLAUDE`) and point it at `grove-test-child`, the
//! deterministic child shipped alongside the PTY-pool tests. This proves the
//! end-to-end flow — compose → temp file → portable-pty spawn → output
//! drain → status persistence — works with no fakes anywhere except the CLI
//! itself.
//!
//! **Unix-only.** On Windows, `shell_quote_cmd` always wraps its input in
//! `"…"`. Combined with the temp-file path argument's own quoting, the
//! composed string we hand to `cmd.exe /c` contains four quote characters,
//! which trips cmd.exe's "old behavior" (strip first + last quote) and
//! produces an unspawnable command. Production `claude` is a bare token
//! (never quoted), so this only manifests under the test override seam.
//! The FakeSpawner suite (`session_lifecycle_fake.rs`) covers every Phase 7
//! code path on Windows, and `pty_pool.rs` exercises PortablePtySpawner
//! end-to-end with a hand-crafted composed string that sidesteps the issue.

#![cfg(unix)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use grove_lib::commands::session::{
    session_close_impl, session_create_impl, session_input_impl, AppContext,
};
use grove_lib::compose::CLAUDE_OVERRIDE_ENV;
use grove_lib::config_store::ConfigStore;
use grove_lib::pty_pool::{PortablePtySpawner, PtyPool, PtySink};
use grove_lib::types::{
    InstructionSetId, PartialAppConfig, PartialDefaultInstructionSets, SessionCreateArgs,
    SessionId, SessionInputArgs, SessionStatus, Tool,
};
use tempfile::TempDir;

const TEST_CHILD: &str = env!("CARGO_BIN_EXE_grove-test-child");

#[derive(Default)]
struct Captured {
    output: Mutex<String>,
    statuses: Mutex<Vec<SessionStatus>>,
}

fn build_sink(captured: Arc<Captured>, store: ConfigStore) -> PtySink {
    let out = Arc::clone(&captured);
    let output = Arc::new(move |_id: &SessionId, data: String| {
        out.output.lock().unwrap().push_str(&data);
    });
    let st = Arc::clone(&captured);
    let status = Arc::new(
        move |id: &SessionId, status: SessionStatus, pid: Option<u32>| {
            let _ = store.update_session_status(id, status, pid);
            st.statuses.lock().unwrap().push(status);
        },
    );
    PtySink::new(output, status)
}

fn wait_until<F: FnMut() -> bool>(mut f: F, dur: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < dur {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    f()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_spawner_drives_create_input_close_round_trip() {
    // Tests can race via process-global env vars, but `cargo test` runs
    // tests in this binary on the same process. We only have one test in
    // this file, and the env var stays set for its duration — fine.
    // SAFETY: setting an env var at the start of a single-test binary is
    // race-free because nothing else reads it concurrently.
    unsafe {
        std::env::set_var(CLAUDE_OVERRIDE_ENV, TEST_CHILD);
    }

    let config_dir = TempDir::new().unwrap();
    let instructions_dir = TempDir::new().unwrap();
    let worktree = TempDir::new().unwrap();

    let instruction_id = InstructionSetId("claude-default".into());
    std::fs::write(
        instructions_dir.path().join("claude-default.md"),
        "# real-spawner test instructions",
    )
    .unwrap();

    let store = ConfigStore::open(config_dir.path()).unwrap();
    store
        .save_config(PartialAppConfig {
            instruction_sets_dir: Some(instructions_dir.path().to_path_buf()),
            default_instruction_sets: Some(PartialDefaultInstructionSets {
                claude: Some(instruction_id.clone()),
                copilot: None,
            }),
            ..Default::default()
        })
        .unwrap();

    let pool = Arc::new(PtyPool::new(Arc::new(PortablePtySpawner)));
    let captured = Arc::new(Captured::default());
    let sink = build_sink(Arc::clone(&captured), store.clone());
    let ctx = Arc::new(AppContext::new(pool, store, sink));

    // Create — this materialises the temp file, composes the command using
    // the override (so the program is `<TEST_CHILD>` instead of `claude`),
    // and spawns through portable-pty.
    let view = session_create_impl(
        &ctx,
        SessionCreateArgs {
            tool: Tool::Claude,
            worktree_path: worktree.path().to_path_buf(),
            instruction_set_id: instruction_id,
        },
    )
    .expect("create");
    assert_eq!(view.status, SessionStatus::Running);

    // The test child prints a banner; wait for it to drain through the sink.
    let saw_banner = wait_until(
        || {
            captured
                .output
                .lock()
                .unwrap()
                .contains("GROVE-TEST-CHILD READY")
        },
        Duration::from_secs(5),
    );
    assert!(
        saw_banner,
        "expected banner in captured output; got {:?}",
        captured.output.lock().unwrap()
    );

    // Drive an echo so we know stdin is wired.
    session_input_impl(
        &ctx,
        SessionInputArgs {
            session_id: view.id,
            data: "hello\r\n".into(),
        },
    )
    .unwrap();
    let saw_echo = wait_until(
        || captured.output.lock().unwrap().contains("echo: hello"),
        Duration::from_secs(5),
    );
    assert!(
        saw_echo,
        "expected echo of input; got {:?}",
        captured.output.lock().unwrap()
    );

    // Close. The pool kills the child and removes the persisted record;
    // tearDown should be clean within a couple of seconds even on Windows.
    session_close_impl(&ctx, view.id).await.unwrap();
    assert!(!ctx.pool.contains(&view.id));

    // Restore parity for any later tests sharing this process. The test
    // binary will exit immediately, so this is just hygiene.
    unsafe {
        std::env::remove_var(CLAUDE_OVERRIDE_ENV);
    }
}
