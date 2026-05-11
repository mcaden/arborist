//! Windows Explorer custom-process plugin module — scaffold only.
//!
//! Issue #95 lands only the directory + module declaration. Issue #97 migrates `explorer_owner.rs` (and the Explorer-specific killer wiring in
//! `commands/subsession.rs`) here and registers an `ExplorerPlugin` into the host's [`crate::plugins::PluginRegistry`].
//
// TODO(#97): move Explorer owner-resolver code here and implement `CustomProcessPlugin`.
