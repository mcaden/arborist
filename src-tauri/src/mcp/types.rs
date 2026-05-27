//! Host-internal MCP types — for runtime bookkeeping with `Instant` / `[u8; 32]` shapes that
//! do not cross the IPC boundary. All wire types (anything serialized over Tauri events,
//! Tauri commands, the MCP JSON-RPC channel, or persisted to `AppConfig`) MUST come from
//! `arborist_types::mcp`; the convention `// MIRROR:` in `src/types/arborist.ts` then locks
//! the TypeScript shape to match. Adding a new wire-format type here is a bug.
//!
//! What lives here:
//! * `McpToolName` / `McpToolKind` — strongly-typed tool dispatch (string id only crosses the
//!   wire; we want exhaustive Rust matches internally).
//! * `McpRequestContext` — held inside the host while a tool call is running; never sent out.
//! * `TrustRecordInternal` — paired with `arborist_types::mcp::McpTrustRecord` via `to_wire()`.
//!   The host needs `Instant` so it can sweep expired entries cheaply; the wire form uses
//!   hex / RFC3339 strings for cross-language portability.
//! * `McpContextConfig` — bundles the rate-limit + trust-TTL config used to construct
//!   `McpContext`. Composed from canonical `AppConfigMcp` at session start.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use arborist_types::mcp::{McpRateLimitsConfig, McpTrustRecord as McpTrustRecordWire};

pub use arborist_types::mcp::{
    AppConfigMcp, ConfirmationToken, MCPError, McpActivityEvent, McpActivityPhase, McpAuditDecision, McpAuditFilter, McpAuditPage, McpAuditRecord,
    McpBudgetRemaining, McpConfirmRequestPayload, McpConfirmationMode, McpDisabledBy, McpEffectiveConfig, McpEffectiveSource,
    McpEffectiveSourceEffect, McpEffectiveSourceLayer, McpEffectiveTool, McpErrorCode, McpPendingAction, McpRateLimits, McpRateScope,
    McpSessionConfig, McpSessionMode, McpStatus, McpToolCallParams, McpToolConfig, McpToolDescriptor, McpTrustRecord, PartialAppConfigMcp,
    PartialMcpRateLimits, PartialMcpRateLimitsConfig, PartialMcpSessionConfig, PartialMcpToolConfig,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpToolName {
    ListWorktrees,
    WorkspaceStatus,
    CreateWorktree,
    CleanupMergedWorktrees,
    MergeMainIntoWorktrees,
}

impl McpToolName {
    pub const ALL: &'static [Self] = &[
        Self::ListWorktrees,
        Self::WorkspaceStatus,
        Self::CreateWorktree,
        Self::CleanupMergedWorktrees,
        Self::MergeMainIntoWorktrees,
    ];

    /// Stable wire identifier — used as the JSON-RPC `name` field, as the key in
    /// `AppConfig.mcp.tools`, and as the value written into audit records. Changing one of
    /// these strings is a breaking change.
    #[must_use]
    pub const fn as_id(self) -> &'static str {
        match self {
            Self::ListWorktrees => "list_worktrees",
            Self::WorkspaceStatus => "workspace_status",
            Self::CreateWorktree => "create_worktree",
            Self::CleanupMergedWorktrees => "cleanup_merged_worktrees",
            Self::MergeMainIntoWorktrees => "merge_main_into_worktrees",
        }
    }

    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|tool| tool.as_id() == id)
    }
}

/// Coarse classification used by the rate limiter and audit log. Specifically NOT the same as
/// `McpRateKind` (which discriminates more finely for token-bucket dispatch); this is the
/// "read vs write" split used for the destructive-action audit-log routing decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum McpToolKind {
    Read,
    Destructive,
}

#[must_use]
pub const fn tool_kind(name: McpToolName) -> McpToolKind {
    match name {
        McpToolName::ListWorktrees | McpToolName::WorkspaceStatus => McpToolKind::Read,
        McpToolName::CreateWorktree | McpToolName::CleanupMergedWorktrees | McpToolName::MergeMainIntoWorktrees => McpToolKind::Destructive,
    }
}

/// Host-internal context for a single in-flight MCP request. Carries the `Instant` so we can
/// compute `duration_ms` for the audit log without storing it on the canonical wire types.
#[derive(Debug, Clone)]
pub struct McpRequestContext {
    pub session_id: String,
    pub workspace_root: PathBuf,
    pub request_id: String,
    pub started_at: Instant,
    pub tool: McpToolName,
}

/// Host-internal trust record. `created_at`/`expires_at` are `Instant`s for cheap sweep
/// comparisons, plus a captured wall-clock `created_at_wall` so we can emit a stable RFC3339
/// timestamp when converting to the wire form (the `Instant`s are not directly translatable to
/// wall-clock once the process restarts).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustRecordInternal {
    pub id: String,
    pub session_id: String,
    pub tool: McpToolName,
    pub fingerprint: [u8; 32],
    pub summary: String,
    pub created_at: Instant,
    pub created_at_wall: OffsetDateTime,
    pub expires_at: Instant,
    pub expires_at_wall: OffsetDateTime,
}

impl TrustRecordInternal {
    /// Convert to the canonical wire form. Hex-encodes the fingerprint and formats both
    /// timestamps as RFC3339 strings. Returns `None` if either wall-clock value fails to
    /// format (should never happen for `OffsetDateTime` produced by `OffsetDateTime::now_*`).
    #[must_use]
    pub fn to_wire(&self) -> Option<McpTrustRecordWire> {
        let created_at = self.created_at_wall.format(&Rfc3339).ok()?;
        let expires_at = self.expires_at_wall.format(&Rfc3339).ok()?;
        Some(McpTrustRecordWire {
            id: self.id.clone(),
            session_id: self.session_id.clone(),
            tool: self.tool.as_id().to_owned(),
            args_fingerprint_hex: hex::encode(self.fingerprint),
            created_at,
            expires_at,
            summary: self.summary.clone(),
        })
    }
}

/// Bundle of runtime knobs handed to `McpContext::new`. Derived from `AppConfigMcp`; tests
/// construct it directly. We keep this host-internal because it bundles `Duration` (rather
/// than the canonical scalar `_per_min` fields) which is more ergonomic for the rate-limit
/// internals.
#[derive(Debug, Clone)]
pub struct McpContextConfig {
    pub rate_limits: McpRateLimitsConfig,
    pub trust_ttl: Duration,
}

impl Default for McpContextConfig {
    fn default() -> Self {
        Self {
            rate_limits: McpRateLimitsConfig::default(),
            trust_ttl: Duration::from_secs(24 * 60 * 60),
        }
    }
}

impl From<&AppConfigMcp> for McpContextConfig {
    fn from(value: &AppConfigMcp) -> Self {
        Self {
            rate_limits: value.rate_limits.clone(),
            trust_ttl: Duration::from_secs(24 * 60 * 60),
        }
    }
}
