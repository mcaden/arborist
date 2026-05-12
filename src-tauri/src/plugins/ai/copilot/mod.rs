//! Copilot AI plugin module — scaffold only.
//!
//! Issue #95 lands only the directory + module declaration. Issue #96 migrates the per-tool Copilot code (compose, metrics, OTel event parsing, icon
//! resolution branches) into this module and registers a `CopilotPlugin` into the host's [`crate::plugins::PluginRegistry`].
//
// TODO(#96): move Copilot-specific code here and implement `AiPlugin`.
