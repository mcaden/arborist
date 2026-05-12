//! Copilot AI plugin metadata.
//!
//! Issue #96 migrates Copilot-specific behavior behind plugin dispatch. This
//! file defines the stable plugin identity and core metadata consumed across
//! composition, settings, and instruction discovery paths.

use crate::plugins::ai::AiPlugin;
use crate::plugins::Plugin;
use crate::types::Tool;

/// Stable singleton instance for dispatch sites that need a `'static`
/// [`AiPlugin`] reference without allocating.
pub static PLUGIN: CopilotPlugin = CopilotPlugin;

pub struct CopilotPlugin;

impl Plugin for CopilotPlugin {
    fn id(&self) -> &'static str {
        Tool::Copilot.as_id()
    }

    fn display_name(&self) -> &'static str {
        "GitHub Copilot"
    }
}

impl AiPlugin for CopilotPlugin {
    fn default_program(&self) -> &'static str {
        "copilot"
    }

    fn default_instruction_set_path(&self) -> &'static str {
        "copilot-default.md"
    }
}

/// Filename-stem prefix for Copilot instruction sets (`copilot-*.md`).
pub const INSTRUCTION_STEM_PREFIX: &str = "copilot-";
