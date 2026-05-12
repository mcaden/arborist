//! VS Code custom-process plugin module — scaffold only.
//!
//! Issue #95 lands only the directory + module declaration. Issue #97 migrates `vscode_owner.rs` (and the `looks_like_vscode_command` sniff in
//! `commands/subsession.rs::owner_resolver_for`) here and registers a `VsCodePlugin` into the host's [`crate::plugins::PluginRegistry`].
//
// TODO(#97): move VS Code owner-resolver code here and implement `CustomProcessPlugin`.
