//! Windows Explorer custom-process plugin.
//!
//! Owns Explorer command-shape matching, Windows platform gating, and owner re-discovery wiring.

use std::path::Path;
use std::sync::Arc;

use crate::app_launcher::OwnerResolver;
use crate::plugins::custom_process::CustomProcessPlugin;
use crate::plugins::Plugin;
use crate::types::CustomProcessDef;

pub mod owner;

/// Built-in custom-process plugin for Windows Explorer.
pub struct ExplorerPlugin;

impl Plugin for ExplorerPlugin {
    fn id(&self) -> &'static str {
        "explorer"
    }

    fn display_name(&self) -> &'static str {
        "Windows Explorer"
    }
}

impl CustomProcessPlugin for ExplorerPlugin {
    fn matches(&self, def: &CustomProcessDef) -> bool {
        owner::looks_like_explorer_command(&def.command)
    }

    fn supported_on_platform(&self) -> bool {
        cfg!(target_os = "windows")
    }

    fn owner_resolver(&self, cwd: &Path) -> Option<Arc<dyn OwnerResolver>> {
        if !self.supported_on_platform() {
            return None;
        }
        Some(Arc::new(owner::ExplorerOwnerResolver::new(cwd.to_path_buf())))
    }
}
