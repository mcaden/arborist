//! Git Status dashboard-widget backend module — scaffold only.
//!
//! Issue #95 lands only the directory + module declaration. Issue #98 moves the `worktree_git_status` command into this module and registers a
//! `GitStatusBackend` (`required_commands = &["worktree_git_status"]`) into the host's [`crate::plugins::PluginRegistry`].
//
// TODO(#98): move Git Status backend support here and implement `DashboardWidgetBackend`.
