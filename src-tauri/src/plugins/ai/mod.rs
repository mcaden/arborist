//! AI plugin trait.
//!
//! An AI plugin owns everything currently keyed off `crate::types::Tool::{Claude, Copilot}`: launch composition, metrics watching, activity / event
//! parsing, icon resolution, and per-tool default instruction-set discovery. Issue #96 migrates the existing Claude / Copilot code into per-plugin
//! modules (`plugins/ai/claude/`, `plugins/ai/copilot/`); this issue (#95) only lands the trait shape.
//!
//! **Implementor contract** (v1):
//!
//! * [`Plugin::id`](crate::plugins::Plugin::id) returns the **serde discriminator** for `crate::types::Tool` — `"claude"`, `"copilot"`. Future AI
//!   plugins choose a stable lower-kebab-case id. The registry indexes on this string.
//! * [`Self::default_program`] returns the bare program token used in the composed launch command (`"claude"`, `"copilot"`). The user can override
//!   per-plugin via `AppConfig.ai_launch_commands`; that override layering remains the host's responsibility.
//! * [`Self::default_instruction_set_path`] is the filename of the built-in instruction set under `instructions/` (e.g. `"claude-default.md"`).

use crate::plugins::Plugin;

pub mod claude;
pub mod copilot;

/// AI plugin trait. See module-level docs for the full implementor contract. Sub-issue #96 will expand this with `compose(...)`, `env(...)`, and
/// `spawn_metrics_watcher(...)` methods as the existing per-tool code is migrated; the v1 shape keeps only the fields needed for the registry to
/// surface an AI plugin and resolve its default program / instruction set.
pub trait AiPlugin: Plugin {
    /// Bare program token used when composing the launch command. The user may override this via `AppConfig.ai_launch_commands` (per-plugin map);
    /// callers that want the **effective** program string must consult the config first.
    fn default_program(&self) -> &'static str;

    /// Filename of the built-in instruction-set markdown under the `instructions/` directory (e.g. `"claude-default.md"`). Used by the host to seed
    /// `AppConfig.default_instruction_sets` when the user has not selected anything.
    fn default_instruction_set_path(&self) -> &'static str;
}
