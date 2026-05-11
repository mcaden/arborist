//! Claude AI plugin module — scaffold only.
//!
//! Issue #95 lands only the directory + module declaration. Issue #96 migrates the per-tool Claude code (compose, metrics, activity, icon resolution
//! branches) into this module and registers a `ClaudePlugin` into the host's [`crate::plugins::PluginRegistry`].
//
// TODO(#96): move Claude-specific code here and implement `AiPlugin`.
