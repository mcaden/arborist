//! MCP tool implementations.
//!
//! Each tool lives in its own module with two public entry points:
//!
//! * `pub fn descriptor() -> McpToolDescriptor` — the metadata returned to the agent via
//!   `tools/list`. Owns the JSON schema (string-typed; we do not generate it from Rust types
//!   yet because keeping the wire schema near the dispatcher makes review easier).
//! * `pub async fn invoke(registry: &McpSessionRegistry, session_id: &str, args: Value) ->
//!   Result<Value, McpInternalError>` — the handler. Receives args already validated as JSON
//!   but should re-validate against the schema (defence in depth).
//!
//! `ipc.rs::handle_tool_call` does the cross-cutting work (tool-enabled check, rate limit
//! consume, audit log) and then dispatches into the per-tool `invoke()`. This keeps each tool
//! file focused on its domain logic and its own tests.

pub mod cleanup_merged_worktrees;
pub mod create_worktree;
pub mod list_worktrees;
pub mod merge_main_into_worktrees;
pub mod workspace_status;
