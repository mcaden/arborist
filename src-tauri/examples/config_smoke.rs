//! Manual smoke test for the Phase 4 quarantine flow.
//!
//! Writes a malformed `config.json` into a tempdir, asks `ConfigStore` to
//! load it, and prints the recovery report (defaults returned + quarantine
//! file present). Run with:
//!
//! ```text
//! cargo run -p arborist --example config_smoke
//! ```

use std::fs;

use arborist_lib::config_store::ConfigStore;
use arborist_lib::init_tracing;
use tempfile::TempDir;

fn main() {
    init_tracing();

    let td = TempDir::new().expect("tempdir");
    let dir = td.path().to_path_buf();
    println!("[smoke] using store dir: {}", dir.display());

    let cfg_path = dir.join("config.json");
    fs::write(&cfg_path, b"{ this is not json").expect("seed bad config");
    println!("[smoke] seeded malformed config.json");

    let store = ConfigStore::open(&dir).expect("open store");
    let cfg = store.load_config();
    println!(
        "[smoke] load_config returned defaults: configVersion={}",
        cfg.config_version
    );

    let bad: Vec<_> = fs::read_dir(&dir)
        .expect("readdir")
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("config.json.bad-"))
        .collect();
    assert_eq!(
        bad.len(),
        1,
        "expected exactly one quarantine file, got {bad:?}"
    );
    println!("[smoke] quarantine file present: {}", bad[0]);
    println!("[smoke] OK");
}
