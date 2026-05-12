//! VS Code custom-process plugin.
//!
//! Owns VS Code command-shape matching and owner re-discovery wiring.

use std::path::Path;
use std::sync::Arc;

use crate::app_launcher::OwnerResolver;
use crate::plugins::custom_process::CustomProcessPlugin;
use crate::plugins::Plugin;
use crate::types::CustomProcessDef;

pub mod owner;

/// Built-in custom-process plugin for VS Code (`code`, `code.cmd`, `code.exe`, and Insiders variants).
pub struct VsCodePlugin;

impl Plugin for VsCodePlugin {
    fn id(&self) -> &'static str {
        "vscode"
    }

    fn display_name(&self) -> &'static str {
        "VS Code"
    }
}

impl CustomProcessPlugin for VsCodePlugin {
    fn matches(&self, def: &CustomProcessDef) -> bool {
        owner::looks_like_vscode_command(&def.command)
    }

    fn supported_on_platform(&self) -> bool {
        true
    }

    fn owner_resolver(&self, cwd: &Path) -> Option<Arc<dyn OwnerResolver>> {
        Some(Arc::new(owner::VsCodeOwnerResolver::new(cwd.to_path_buf())))
    }
}
