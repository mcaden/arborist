//! Custom-process plugin trait.
//!
//! Generalises the special-case wiring that the runtime currently applies to VS Code and Windows Explorer
//! (`plugins/custom_process/*/owner.rs`, `subsession::owner_resolver_for`). The generic [`crate::types::CustomProcessDef`] runtime keeps working
//! unchanged for user-defined processes; a built-in plugin is matched **by command-shape sniffing** ([`Self::matches`]) and can provide a per-target
//! owner resolver.
//!
//! **Implementor contract** (v1):
//!
//! * [`Plugin::id`](crate::plugins::Plugin::id) returns a stable kebab-case identifier (`"vscode"`, `"explorer"`).
//! * [`Self::matches`] returns `true` exactly when this plugin should claim a given `CustomProcessDef`. Implementations sniff `def.command` (and
//!   `def.id` if a built-in user has been seeded). The first plugin to return `true` wins; issue #97 enforces a single-match invariant in registry
//!   tests.
//! * [`Self::supported_on_platform`] gates the plugin on the current OS (e.g. the Explorer plugin returns `false` on macOS / Linux).
//! * [`Self::owner_resolver`] returns an [`crate::app_launcher::OwnerResolver`] for the supplied working directory when this plugin needs one (VS Code,
//!   Explorer), or `None` when launcher PID ownership is sufficient.

use std::path::Path;
use std::sync::Arc;

use crate::app_launcher::OwnerResolver;
use crate::plugins::Plugin;
use crate::types::CustomProcessDef;

pub mod explorer;
pub mod vscode;

/// Custom-process plugin trait. See module-level docs for the full implementor contract.
pub trait CustomProcessPlugin: Plugin {
    /// Returns `true` if this plugin should be applied to the supplied [`CustomProcessDef`]. Implementations typically check `def.command` against a
    /// small set of allowed program-name shapes; the host calls this method in registration order via
    /// [`crate::plugins::PluginRegistry::custom_process_for_def`].
    fn matches(&self, def: &CustomProcessDef) -> bool;

    /// Returns `true` if the current platform supports this plugin. The host uses this to hide the plugin from the UI on unsupported OSes (e.g. the
    /// Explorer plugin is Windows-only).
    fn supported_on_platform(&self) -> bool;

    /// Returns an owner resolver for `cwd` when this plugin needs owner re-discovery. Default is `None` so plugins that don't need re-targeting don't
    /// have to implement this method.
    fn owner_resolver(&self, _cwd: &Path) -> Option<Arc<dyn OwnerResolver>> {
        None
    }
}
