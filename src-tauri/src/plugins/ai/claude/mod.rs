//! Claude AI plugin metadata.
//!
//! Issue #96 migrates Claude-specific behavior behind plugin dispatch. This file
//! defines the stable plugin identity and core metadata consumed across
//! composition, settings, and instruction discovery paths.

use crate::plugins::ai::AiPlugin;
use crate::plugins::Plugin;
use crate::types::Tool;

/// Stable singleton instance for dispatch sites that need a `'static`
/// [`AiPlugin`] reference without allocating.
pub static PLUGIN: ClaudePlugin = ClaudePlugin;

pub struct ClaudePlugin;

impl Plugin for ClaudePlugin {
    fn id(&self) -> &'static str {
        Tool::Claude.as_id()
    }

    fn display_name(&self) -> &'static str {
        "Claude"
    }
}

impl AiPlugin for ClaudePlugin {
    fn default_program(&self) -> &'static str {
        "claude"
    }

    fn default_instruction_set_path(&self) -> &'static str {
        "claude-default.md"
    }
}

/// Filename-stem prefix for Claude instruction sets (`claude-*.md`).
pub const INSTRUCTION_STEM_PREFIX: &str = "claude-";
