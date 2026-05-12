//! Dashboard-widget backend trait.
//!
//! Each dashboard widget on the frontend (Git Status, AI Usage, …) has an optional backend descriptor on the Rust side declaring the commands /
//! events the widget depends on. The descriptor lets capability wiring be code-genned later (so a widget can declare its required commands without
//! every reviewer remembering to update `capabilities/main.json` by hand) and gives the host a place to hang backend-only widget plumbing.
//!
//! **Implementor contract** (v1):
//!
//! * [`Plugin::id`](crate::plugins::Plugin::id) returns a stable kebab-case widget identifier (`"git-status"`, `"ai-usage"`). The frontend widget
//!   plugin uses the **same** id; the parity test in #98 will assert the two sides stay in sync.
//! * [`Self::required_commands`] returns the Tauri commands this widget invokes. Defaulted to `&[]` so widgets with no backend dependency (e.g. AI
//!   Usage, which only reads the session-store) don't need to override it.

use crate::plugins::Plugin;

pub mod ai_usage;
pub mod git_status;

/// Dashboard-widget backend descriptor. See module-level docs for the full implementor contract.
pub trait DashboardWidgetBackend: Plugin {
    /// Tauri commands this widget invokes. Used in future capability-wiring code-gen; v1 is informational only.
    fn required_commands(&self) -> &'static [&'static str] {
        &[]
    }
}
