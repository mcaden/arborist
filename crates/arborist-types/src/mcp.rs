use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

fn default_true() -> bool {
    true
}

fn default_mcp_tools() -> BTreeMap<String, McpToolConfig> {
    BTreeMap::from([
        (
            "cleanup_merged_worktrees".to_owned(),
            McpToolConfig {
                enabled: true,
                requires_confirmation: McpConfirmationMode::Always,
            },
        ),
        (
            "create_worktree".to_owned(),
            McpToolConfig {
                enabled: true,
                requires_confirmation: McpConfirmationMode::FirstUse,
            },
        ),
        (
            "list_worktrees".to_owned(),
            McpToolConfig {
                enabled: true,
                requires_confirmation: McpConfirmationMode::Never,
            },
        ),
        (
            "merge_main_into_worktrees".to_owned(),
            McpToolConfig {
                enabled: true,
                requires_confirmation: McpConfirmationMode::Always,
            },
        ),
        (
            "workspace_status".to_owned(),
            McpToolConfig {
                enabled: true,
                requires_confirmation: McpConfirmationMode::Never,
            },
        ),
    ])
}

#[allow(clippy::upper_case_acronyms)]
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MCPError {
    pub code: McpErrorCode,
    pub message: String,
    pub recoverable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_remaining: Option<McpBudgetRemaining>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_by: Option<McpDisabledBy>,
}

/// Distinguishes the layer that disabled an MCP tool when a `tool-disabled` error is returned.
///
/// Wire form is lowercase (`"global"`, `"tool"`, `"session"`) so the UI can render a precise
/// remediation hint without having to parse the human-readable `message`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum McpDisabledBy {
    /// `AppConfig.mcp.enabled` is `false` — the whole MCP surface is off.
    Global,
    /// `AppConfig.mcp.tools[<tool>].enabled` is `false` — only this tool is off.
    Tool,
    /// The session was revoked at runtime via the dashboard / UI.
    Session,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum McpErrorCode {
    WorkspaceUnbound,
    InvalidName,
    InvalidArg,
    InvalidFromBranch,
    InvalidSourceBranch,
    InvalidTargetBranch,
    InvalidPath,
    InvalidConfirmation,
    NameInUse,
    ToolNotConfigured,
    PrepNotConfigured,
    PrepFailed,
    ConfirmationRequired,
    ConfirmationExpired,
    ConfirmationStale,
    RepoCommandTrustRequired,
    SpawnLineageLimitExceeded,
    WorktreeVanished,
    WorktreeMissing,
    OwnWorktreeRefused,
    StaleRemoteData,
    DefaultBranchUnknown,
    RateLimited,
    Busy,
    ToolDisabled,
    ToolNotImplemented,
    HostUnavailable,
    DryRunUnsupported,
    TooManyPendingActions,
    Unauthenticated,
    SessionRevoked,
    Internal,
}

impl McpErrorCode {
    /// Errors a client SHOULD be able to recover from automatically (retry after backoff,
    /// re-request confirmation, refresh data) versus terminal errors that require user action
    /// or a code change. Used by the host to populate `MCPError.recoverable`.
    #[must_use]
    pub const fn is_recoverable(self) -> bool {
        match self {
            Self::RateLimited
            | Self::ConfirmationRequired
            | Self::ConfirmationExpired
            | Self::ConfirmationStale
            | Self::RepoCommandTrustRequired
            | Self::StaleRemoteData
            | Self::Busy
            | Self::TooManyPendingActions => true,
            Self::WorkspaceUnbound
            | Self::InvalidName
            | Self::InvalidArg
            | Self::InvalidFromBranch
            | Self::InvalidSourceBranch
            | Self::InvalidTargetBranch
            | Self::InvalidPath
            | Self::InvalidConfirmation
            | Self::NameInUse
            | Self::ToolNotConfigured
            | Self::PrepNotConfigured
            | Self::PrepFailed
            | Self::SpawnLineageLimitExceeded
            | Self::WorktreeVanished
            | Self::WorktreeMissing
            | Self::OwnWorktreeRefused
            | Self::DefaultBranchUnknown
            | Self::ToolDisabled
            | Self::ToolNotImplemented
            | Self::HostUnavailable
            | Self::DryRunUnsupported
            | Self::Unauthenticated
            | Self::SessionRevoked
            | Self::Internal => false,
        }
    }

    /// Default localized hint for the user when an error surfaces. The host MAY override per
    /// call site for better context; if it returns `None`, the UI falls back to the raw message.
    #[must_use]
    pub const fn default_user_action(self) -> Option<&'static str> {
        match self {
            Self::WorkspaceUnbound => Some("Open a workspace in Arborist before retrying"),
            Self::ToolDisabled => Some("Enable the requested MCP tool in Arborist Settings → MCP"),
            Self::HostUnavailable => Some("Restart this Arborist session to reconnect"),
            Self::ConfirmationRequired => Some("Approve the request in Arborist"),
            Self::ConfirmationExpired => Some("Re-request the action so the user can approve it again"),
            Self::ConfirmationStale => Some("Re-issue the request because arguments or workspace state changed"),
            Self::InvalidConfirmation => Some("Request a fresh confirmation token"),
            Self::RepoCommandTrustRequired => Some("Approve the repo-provided command in Arborist Settings → Repo commands"),
            Self::SpawnLineageLimitExceeded => Some("Close some MCP-spawned sessions or raise the limit in Settings → MCP"),
            Self::StaleRemoteData => Some("Come back online or re-confirm with stale data acknowledged"),
            Self::Busy => Some("Wait a few seconds and retry"),
            Self::TooManyPendingActions => Some("Resolve or deny older MCP confirmation requests first"),
            _ => None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub enum McpRateScope {
    PerSession,
    PerWorkspace,
    PerHost,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct McpBudgetRemaining {
    pub scope: McpRateScope,
    pub remaining: u32,
    pub window_ms: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AppConfigMcp {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_mcp_tools")]
    pub tools: BTreeMap<String, McpToolConfig>,
    #[serde(default)]
    pub rate_limits: McpRateLimitsConfig,
    #[serde(default = "default_true")]
    pub allow_remote_fetch: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disclosure_acknowledged_at: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub per_session: BTreeMap<String, McpSessionConfig>,
}

impl Default for AppConfigMcp {
    fn default() -> Self {
        Self {
            enabled: false,
            tools: default_mcp_tools(),
            rate_limits: McpRateLimitsConfig::default(),
            allow_remote_fetch: true,
            disclosure_acknowledged_at: None,
            per_session: BTreeMap::new(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct McpToolConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub requires_confirmation: McpConfirmationMode,
}

impl Default for McpToolConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            requires_confirmation: McpConfirmationMode::Never,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub enum McpConfirmationMode {
    Always,
    FirstUse,
    #[default]
    Never,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub enum McpSessionMode {
    #[default]
    Full,
    ReadOnly,
    Off,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct McpSessionConfig {
    #[serde(default)]
    pub mode: McpSessionMode,
}

impl Default for McpSessionConfig {
    fn default() -> Self {
        Self { mode: McpSessionMode::Full }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct McpRateLimitsConfig {
    #[serde(default = "McpRateLimits::per_session_defaults")]
    pub per_session: McpRateLimits,
    #[serde(default = "McpRateLimits::per_workspace_defaults")]
    pub per_workspace: McpRateLimits,
    #[serde(default = "McpRateLimits::per_host_defaults")]
    pub per_host: McpRateLimits,
}

impl Default for McpRateLimitsConfig {
    fn default() -> Self {
        Self {
            per_session: McpRateLimits::per_session_defaults(),
            per_workspace: McpRateLimits::per_workspace_defaults(),
            per_host: McpRateLimits::per_host_defaults(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct McpRateLimits {
    pub structural_read_per_min: u32,
    pub expensive_read_per_min: u32,
    pub destructive_per_min: u32,
    pub total_per_min: u32,
    pub create_worktree_per_hour: u32,
    pub fetch_per_60s: u32,
}

impl McpRateLimits {
    #[must_use]
    pub const fn new(
        structural_read_per_min: u32,
        expensive_read_per_min: u32,
        destructive_per_min: u32,
        total_per_min: u32,
        create_worktree_per_hour: u32,
        fetch_per_60s: u32,
    ) -> Self {
        Self {
            structural_read_per_min,
            expensive_read_per_min,
            destructive_per_min,
            total_per_min,
            create_worktree_per_hour,
            fetch_per_60s,
        }
    }

    #[must_use]
    pub const fn per_session_defaults() -> Self {
        Self::new(30, 30, 5, 30, 10, 0)
    }

    #[must_use]
    pub const fn per_workspace_defaults() -> Self {
        Self::new(100, 6, 15, 100, 30, 1)
    }

    #[must_use]
    pub const fn per_host_defaults() -> Self {
        Self::new(500, 500, 500, 500, 0, 0)
    }
}

impl Default for McpRateLimits {
    fn default() -> Self {
        Self::per_session_defaults()
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct PartialAppConfigMcp {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tools: BTreeMap<String, PartialMcpToolConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limits: Option<PartialMcpRateLimitsConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_remote_fetch: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disclosure_acknowledged_at: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub per_session: BTreeMap<String, PartialMcpSessionConfig>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct PartialMcpToolConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_confirmation: Option<McpConfirmationMode>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct PartialMcpRateLimitsConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_session: Option<PartialMcpRateLimits>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_workspace: Option<PartialMcpRateLimits>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_host: Option<PartialMcpRateLimits>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct PartialMcpRateLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structural_read_per_min: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expensive_read_per_min: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destructive_per_min: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_per_min: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create_worktree_per_hour: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetch_per_60s: Option<u32>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct PartialMcpSessionConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<McpSessionMode>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct McpToolDescriptor {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct McpToolCallParams {
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub enum McpEffectiveSourceLayer {
    Global,
    Session,
    Repo,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub enum McpEffectiveSourceEffect {
    Enabled,
    Disabled,
    RequiresConfirmation,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct McpEffectiveSource {
    pub layer: McpEffectiveSourceLayer,
    pub effect: McpEffectiveSourceEffect,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct McpEffectiveTool {
    pub id: String,
    pub enabled: bool,
    pub requires_confirmation: bool,
    pub sources: Vec<McpEffectiveSource>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct McpEffectiveConfig {
    pub tools: Vec<McpEffectiveTool>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum McpActivityPhase {
    Requested,
    AwaitingConfirmation,
    Approved,
    Denied,
    AutoApproved,
    Running,
    Completed,
    Failed,
    RateLimited,
    HostUnavailable,
    Expired,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct McpActivityEvent {
    pub id: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    pub tool: String,
    pub phase: McpActivityPhase,
    pub started_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<MCPError>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct McpConfirmRequestPayload {
    pub id: String,
    pub session_id: String,
    pub tool: String,
    pub summary: String,
    pub args_preview: serde_json::Value,
    pub scope_hints: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct McpPendingAction {
    pub id: String,
    pub session_id: String,
    pub tool: String,
    /// Short one-line summary (≤200 chars). For long arg payloads the full detail goes in `details`.
    pub summary: String,
    /// Optional full args dump for the confirmation UI's "View full request" panel. Arbitrary
    /// JSON; the frontend renders it as a key/value tree. None if `summary` already conveys the
    /// full request (typical for short tools like `list_worktrees` confirmation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
    pub args_fingerprint_hex: String,
    pub created_at: String,
    pub expires_at: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct McpTrustRecord {
    pub id: String,
    pub session_id: String,
    pub tool: String,
    pub args_fingerprint_hex: String,
    pub created_at: String,
    pub expires_at: String,
    pub summary: String,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub enum McpAuditDecision {
    NotRequired,
    Pending,
    Approved,
    AutoApproved,
    Denied,
    Expired,
    Stale,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct McpAuditRecord {
    pub seq: u64,
    pub prev_hash_hex: String,
    pub ts: String,
    pub session_id: String,
    pub session_label: String,
    pub tool: String,
    pub decision: McpAuditDecision,
    pub args_summary: String,
    pub result: serde_json::Value,
    pub duration_ms: u64,
    pub request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmation_token_sha256: Option<String>,
    pub audit_id: String,
}

fn default_mcp_audit_limit() -> u32 {
    50
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct McpAuditFilter {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<McpAuditDecision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
    #[serde(default = "default_mcp_audit_limit")]
    pub limit: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct McpAuditPage {
    pub records: Vec<McpAuditRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

impl Default for McpAuditFilter {
    fn default() -> Self {
        Self {
            session_id: None,
            tool: None,
            decision: None,
            since: None,
            until: None,
            limit: default_mcp_audit_limit(),
            cursor: None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ConfirmationToken {
    pub token: String,
    pub expires_at: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct McpStatus {
    pub config: AppConfigMcp,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tampered_logs: Vec<String>,
}

#[cfg(test)]
mod tests {
    use crate::{AppConfig, CONFIG_VERSION_CURRENT};

    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::{json, Value};

    fn assert_roundtrip<T>(value: &T, fixture: Value)
    where
        T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
    {
        let serialized: Value = serde_json::to_value(value).expect("serialize");
        assert_eq!(serialized, fixture, "serialized form drifted from fixture");

        let deserialized: T = serde_json::from_value(fixture).expect("deserialize");
        assert_eq!(&deserialized, value, "deserialized value drifted");
    }

    fn budget_remaining_fixture() -> (McpBudgetRemaining, Value) {
        let value = McpBudgetRemaining {
            scope: McpRateScope::PerWorkspace,
            remaining: 9,
            window_ms: 60_000,
        };
        let fixture = json!({
            "scope": "perWorkspace",
            "remaining": 9,
            "windowMs": 60_000
        });
        (value, fixture)
    }

    fn tool_config_fixture() -> (McpToolConfig, Value) {
        let value = McpToolConfig {
            enabled: false,
            requires_confirmation: McpConfirmationMode::FirstUse,
        };
        let fixture = json!({
            "enabled": false,
            "requiresConfirmation": "firstUse"
        });
        (value, fixture)
    }

    fn rate_limits_fixture() -> (McpRateLimits, Value) {
        let value = McpRateLimits::new(12, 3, 2, 20, 8, 1);
        let fixture = json!({
            "structuralReadPerMin": 12,
            "expensiveReadPerMin": 3,
            "destructivePerMin": 2,
            "totalPerMin": 20,
            "createWorktreePerHour": 8,
            "fetchPer60s": 1
        });
        (value, fixture)
    }

    fn rate_limits_config_fixture() -> (McpRateLimitsConfig, Value) {
        let value = McpRateLimitsConfig {
            per_session: McpRateLimits::new(30, 10, 5, 30, 10, 0),
            per_workspace: McpRateLimits::new(100, 6, 15, 100, 30, 1),
            per_host: McpRateLimits::new(500, 50, 25, 500, 0, 0),
        };
        let fixture = json!({
            "perSession": {
                "structuralReadPerMin": 30,
                "expensiveReadPerMin": 10,
                "destructivePerMin": 5,
                "totalPerMin": 30,
                "createWorktreePerHour": 10,
                "fetchPer60s": 0
            },
            "perWorkspace": {
                "structuralReadPerMin": 100,
                "expensiveReadPerMin": 6,
                "destructivePerMin": 15,
                "totalPerMin": 100,
                "createWorktreePerHour": 30,
                "fetchPer60s": 1
            },
            "perHost": {
                "structuralReadPerMin": 500,
                "expensiveReadPerMin": 50,
                "destructivePerMin": 25,
                "totalPerMin": 500,
                "createWorktreePerHour": 0,
                "fetchPer60s": 0
            }
        });
        (value, fixture)
    }

    fn mcp_error_fixture() -> (MCPError, Value) {
        let (budget_remaining, budget_fixture) = budget_remaining_fixture();
        let value = MCPError {
            code: McpErrorCode::RateLimited,
            message: "Too many MCP calls".to_owned(),
            recoverable: true,
            user_action: Some("Retry after the bucket resets".to_owned()),
            retry_after_ms: Some(1_500),
            budget_remaining: Some(budget_remaining),
            audit_id: Some("audit-123".to_owned()),
            disabled_by: Some(McpDisabledBy::Session),
        };
        let fixture = json!({
            "code": "rate-limited",
            "message": "Too many MCP calls",
            "recoverable": true,
            "userAction": "Retry after the bucket resets",
            "retryAfterMs": 1_500,
            "budgetRemaining": budget_fixture,
            "auditId": "audit-123",
            "disabledBy": "session"
        });
        (value, fixture)
    }

    fn app_config_mcp_fixture() -> (AppConfigMcp, Value) {
        let (tool_config, tool_fixture) = tool_config_fixture();
        let (rate_limits, rate_limits_fixture) = rate_limits_config_fixture();
        let value = AppConfigMcp {
            enabled: true,
            tools: BTreeMap::from([("create_worktree".to_owned(), tool_config)]),
            rate_limits,
            allow_remote_fetch: false,
            disclosure_acknowledged_at: Some("2025-01-02T03:04:05Z".to_owned()),
            per_session: BTreeMap::from([(
                "session-123".to_owned(),
                McpSessionConfig {
                    mode: McpSessionMode::ReadOnly,
                },
            )]),
        };
        let fixture = json!({
            "enabled": true,
            "tools": {
                "create_worktree": tool_fixture
            },
            "rateLimits": rate_limits_fixture,
            "allowRemoteFetch": false,
            "disclosureAcknowledgedAt": "2025-01-02T03:04:05Z",
            "perSession": {
                "session-123": {
                    "mode": "readOnly"
                }
            }
        });
        (value, fixture)
    }

    fn tool_descriptor_fixture() -> (McpToolDescriptor, Value) {
        let value = McpToolDescriptor {
            name: "list_worktrees".to_owned(),
            description: "List worktrees".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "includeStatus": { "type": "boolean" }
                }
            }),
        };
        let fixture = json!({
            "name": "list_worktrees",
            "description": "List worktrees",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "includeStatus": { "type": "boolean" }
                }
            }
        });
        (value, fixture)
    }

    fn tool_call_params_fixture() -> (McpToolCallParams, Value) {
        let value = McpToolCallParams {
            name: "workspace_status".to_owned(),
            arguments: json!({ "includeStatus": true }),
        };
        let fixture = json!({
            "name": "workspace_status",
            "arguments": { "includeStatus": true }
        });
        (value, fixture)
    }

    fn effective_config_fixture() -> (McpEffectiveConfig, Value) {
        let value = McpEffectiveConfig {
            tools: vec![McpEffectiveTool {
                id: "workspace_status".to_owned(),
                enabled: true,
                requires_confirmation: false,
                sources: vec![
                    McpEffectiveSource {
                        layer: McpEffectiveSourceLayer::Global,
                        effect: McpEffectiveSourceEffect::Enabled,
                    },
                    McpEffectiveSource {
                        layer: McpEffectiveSourceLayer::Session,
                        effect: McpEffectiveSourceEffect::Enabled,
                    },
                ],
            }],
        };
        let fixture = json!({
            "tools": [
                {
                    "id": "workspace_status",
                    "enabled": true,
                    "requiresConfirmation": false,
                    "sources": [
                        { "layer": "global", "effect": "enabled" },
                        { "layer": "session", "effect": "enabled" }
                    ]
                }
            ]
        });
        (value, fixture)
    }

    fn activity_event_fixture() -> (McpActivityEvent, Value) {
        let (error, error_fixture) = mcp_error_fixture();
        let value = McpActivityEvent {
            id: "req-123".to_owned(),
            session_id: "session-123".to_owned(),
            workspace_id: Some("workspace-123".to_owned()),
            tool: "create_worktree".to_owned(),
            phase: McpActivityPhase::Failed,
            started_at: "2025-01-02T03:04:05Z".to_owned(),
            updated_at: "2025-01-02T03:04:08Z".to_owned(),
            summary: Some("Create worktree failed".to_owned()),
            error: Some(error),
        };
        let fixture = json!({
            "id": "req-123",
            "sessionId": "session-123",
            "workspaceId": "workspace-123",
            "tool": "create_worktree",
            "phase": "failed",
            "startedAt": "2025-01-02T03:04:05Z",
            "updatedAt": "2025-01-02T03:04:08Z",
            "summary": "Create worktree failed",
            "error": error_fixture
        });
        (value, fixture)
    }

    fn confirm_request_payload_fixture() -> (McpConfirmRequestPayload, Value) {
        let value = McpConfirmRequestPayload {
            id: "confirm-123".to_owned(),
            session_id: "session-123".to_owned(),
            tool: "cleanup_merged_worktrees".to_owned(),
            summary: "Remove 2 merged worktrees".to_owned(),
            args_preview: json!({ "dryRun": false, "paths": ["feature-a", "feature-b"] }),
            scope_hints: vec!["workspace".to_owned(), "destructive".to_owned()],
        };
        let fixture = json!({
            "id": "confirm-123",
            "sessionId": "session-123",
            "tool": "cleanup_merged_worktrees",
            "summary": "Remove 2 merged worktrees",
            "argsPreview": { "dryRun": false, "paths": ["feature-a", "feature-b"] },
            "scopeHints": ["workspace", "destructive"]
        });
        (value, fixture)
    }

    fn pending_action_fixture() -> (McpPendingAction, Value) {
        let value = McpPendingAction {
            id: "pending-123".to_owned(),
            session_id: "session-123".to_owned(),
            tool: "merge_main_into_worktrees".to_owned(),
            summary: "Merge main into 3 worktrees".to_owned(),
            details: None,
            args_fingerprint_hex: "deadbeef".to_owned(),
            created_at: "2025-01-02T03:04:05Z".to_owned(),
            expires_at: "2025-01-02T03:05:05Z".to_owned(),
        };
        let fixture = json!({
            "id": "pending-123",
            "sessionId": "session-123",
            "tool": "merge_main_into_worktrees",
            "summary": "Merge main into 3 worktrees",
            "argsFingerprintHex": "deadbeef",
            "createdAt": "2025-01-02T03:04:05Z",
            "expiresAt": "2025-01-02T03:05:05Z"
        });
        (value, fixture)
    }

    fn trust_record_fixture() -> (McpTrustRecord, Value) {
        let value = McpTrustRecord {
            id: "trust-123".to_owned(),
            session_id: "session-123".to_owned(),
            tool: "create_worktree".to_owned(),
            args_fingerprint_hex: "cafebabe".to_owned(),
            created_at: "2025-01-02T03:04:05Z".to_owned(),
            expires_at: "2025-01-03T03:04:05Z".to_owned(),
            summary: "Create feature/foo from origin/main".to_owned(),
        };
        let fixture = json!({
            "id": "trust-123",
            "sessionId": "session-123",
            "tool": "create_worktree",
            "argsFingerprintHex": "cafebabe",
            "createdAt": "2025-01-02T03:04:05Z",
            "expiresAt": "2025-01-03T03:04:05Z",
            "summary": "Create feature/foo from origin/main"
        });
        (value, fixture)
    }

    fn audit_record_fixture() -> (McpAuditRecord, Value) {
        let value = McpAuditRecord {
            seq: 7,
            prev_hash_hex: "abc123".to_owned(),
            ts: "2025-01-02T03:04:05Z".to_owned(),
            session_id: "session-123".to_owned(),
            session_label: "feature-x".to_owned(),
            tool: "create_worktree".to_owned(),
            decision: McpAuditDecision::AutoApproved,
            args_summary: "{\"name\":\"feature/foo\"}".to_owned(),
            result: json!({
                "status": "completed",
                "relativePath": ".arborist/.worktrees/feature-foo"
            }),
            duration_ms: 245,
            request_id: "req-123".to_owned(),
            confirmation_token_sha256: Some("deadbeefcafebabe".to_owned()),
            audit_id: "audit-123".to_owned(),
        };
        let fixture = json!({
            "seq": 7,
            "prevHashHex": "abc123",
            "ts": "2025-01-02T03:04:05Z",
            "sessionId": "session-123",
            "sessionLabel": "feature-x",
            "tool": "create_worktree",
            "decision": "autoApproved",
            "argsSummary": "{\"name\":\"feature/foo\"}",
            "result": {
                "status": "completed",
                "relativePath": ".arborist/.worktrees/feature-foo"
            },
            "durationMs": 245,
            "requestId": "req-123",
            "confirmationTokenSha256": "deadbeefcafebabe",
            "auditId": "audit-123"
        });
        (value, fixture)
    }

    fn audit_filter_fixture() -> (McpAuditFilter, Value) {
        let value = McpAuditFilter {
            session_id: Some("session-123".to_owned()),
            tool: Some("workspace_status".to_owned()),
            decision: Some(McpAuditDecision::Approved),
            since: Some("2025-01-01T00:00:00Z".to_owned()),
            until: Some("2025-01-02T00:00:00Z".to_owned()),
            limit: 25,
            cursor: Some("destructive:7".to_owned()),
        };
        let fixture = json!({
            "sessionId": "session-123",
            "tool": "workspace_status",
            "decision": "approved",
            "since": "2025-01-01T00:00:00Z",
            "until": "2025-01-02T00:00:00Z",
            "limit": 25,
            "cursor": "destructive:7"
        });
        (value, fixture)
    }

    fn audit_page_fixture() -> (McpAuditPage, Value) {
        let (record, record_fixture) = audit_record_fixture();
        let value = McpAuditPage {
            records: vec![record],
            next_cursor: Some("destructive:8".to_owned()),
        };
        let fixture = json!({
            "records": [record_fixture],
            "nextCursor": "destructive:8"
        });
        (value, fixture)
    }

    fn confirmation_token_fixture() -> (ConfirmationToken, Value) {
        let value = ConfirmationToken {
            token: "deadbeef".to_owned(),
            expires_at: "2025-01-02T03:05:05Z".to_owned(),
        };
        let fixture = json!({
            "token": "deadbeef",
            "expiresAt": "2025-01-02T03:05:05Z"
        });
        (value, fixture)
    }

    fn mcp_status_fixture() -> (McpStatus, Value) {
        let (config, config_fixture) = app_config_mcp_fixture();
        let value = McpStatus {
            config,
            tampered_logs: vec!["C:\\repo\\.arborist\\mcp-audit.jsonl".to_owned()],
        };
        let fixture = json!({
            "config": config_fixture,
            "tamperedLogs": ["C:\\repo\\.arborist\\mcp-audit.jsonl"]
        });
        (value, fixture)
    }

    #[test]
    fn mcp_budget_remaining_roundtrip() {
        let (value, fixture) = budget_remaining_fixture();
        assert_roundtrip(&value, fixture);
    }

    #[test]
    fn mcp_tool_config_roundtrip() {
        let (value, fixture) = tool_config_fixture();
        assert_roundtrip(&value, fixture);
    }

    #[test]
    fn mcp_rate_limits_roundtrip() {
        let (value, fixture) = rate_limits_fixture();
        assert_roundtrip(&value, fixture);
    }

    #[test]
    fn mcp_rate_limits_config_roundtrip() {
        let (value, fixture) = rate_limits_config_fixture();
        assert_roundtrip(&value, fixture);
    }

    #[test]
    fn mcp_error_roundtrip() {
        let (value, fixture) = mcp_error_fixture();
        assert_roundtrip(&value, fixture);
    }

    #[test]
    fn app_config_mcp_roundtrip() {
        let (value, fixture) = app_config_mcp_fixture();
        assert_roundtrip(&value, fixture);
    }

    #[test]
    fn mcp_tool_descriptor_roundtrip() {
        let (value, fixture) = tool_descriptor_fixture();
        assert_roundtrip(&value, fixture);
    }

    #[test]
    fn mcp_tool_call_params_roundtrip() {
        let (value, fixture) = tool_call_params_fixture();
        assert_roundtrip(&value, fixture);
    }

    #[test]
    fn mcp_effective_config_roundtrip() {
        let (value, fixture) = effective_config_fixture();
        assert_roundtrip(&value, fixture);
    }

    #[test]
    fn mcp_activity_event_roundtrip() {
        let (value, fixture) = activity_event_fixture();
        assert_roundtrip(&value, fixture);
    }

    #[test]
    fn mcp_confirm_request_payload_roundtrip() {
        let (value, fixture) = confirm_request_payload_fixture();
        assert_roundtrip(&value, fixture);
    }

    #[test]
    fn mcp_pending_action_roundtrip() {
        let (value, fixture) = pending_action_fixture();
        assert_roundtrip(&value, fixture);
    }

    #[test]
    fn mcp_trust_record_roundtrip() {
        let (value, fixture) = trust_record_fixture();
        assert_roundtrip(&value, fixture);
    }

    #[test]
    fn mcp_audit_record_roundtrip() {
        let (value, fixture) = audit_record_fixture();
        assert_roundtrip(&value, fixture);
    }

    #[test]
    fn mcp_audit_filter_roundtrip() {
        let (value, fixture) = audit_filter_fixture();
        assert_roundtrip(&value, fixture);
    }

    #[test]
    fn mcp_audit_page_roundtrip() {
        let (value, fixture) = audit_page_fixture();
        assert_roundtrip(&value, fixture);
    }

    #[test]
    fn confirmation_token_roundtrip() {
        let (value, fixture) = confirmation_token_fixture();
        assert_roundtrip(&value, fixture);
    }

    #[test]
    fn mcp_status_roundtrip() {
        let (value, fixture) = mcp_status_fixture();
        assert_roundtrip(&value, fixture);
    }

    #[test]
    fn mcp_error_code_serializes_kebab_case() {
        assert_eq!(
            serde_json::to_value(McpErrorCode::WorkspaceUnbound).expect("serialize"),
            json!("workspace-unbound")
        );
        assert_eq!(
            serde_json::to_value(McpErrorCode::ConfirmationRequired).expect("serialize"),
            json!("confirmation-required")
        );
        assert_eq!(
            serde_json::to_value(McpErrorCode::HostUnavailable).expect("serialize"),
            json!("host-unavailable")
        );
        assert_eq!(
            serde_json::to_value(McpErrorCode::ToolNotImplemented).expect("serialize"),
            json!("tool-not-implemented")
        );
    }

    #[test]
    fn mcp_activity_phase_serializes_kebab_case() {
        assert_eq!(
            serde_json::to_value(McpActivityPhase::AwaitingConfirmation).expect("serialize"),
            json!("awaiting-confirmation")
        );
        assert_eq!(
            serde_json::to_value(McpActivityPhase::AutoApproved).expect("serialize"),
            json!("auto-approved")
        );
        assert_eq!(
            serde_json::to_value(McpActivityPhase::HostUnavailable).expect("serialize"),
            json!("host-unavailable")
        );
    }

    #[test]
    fn mcp_other_enum_wire_formats_are_stable() {
        assert_eq!(serde_json::to_value(McpRateScope::PerSession).expect("serialize"), json!("perSession"));
        assert_eq!(serde_json::to_value(McpConfirmationMode::FirstUse).expect("serialize"), json!("firstUse"));
        assert_eq!(serde_json::to_value(McpSessionMode::ReadOnly).expect("serialize"), json!("readOnly"));
        assert_eq!(
            serde_json::to_value(McpAuditDecision::NotRequired).expect("serialize"),
            json!("notRequired")
        );
    }

    #[test]
    fn app_config_mcp_default_flags_match_planning_defaults() {
        let default_config = AppConfigMcp::default();

        assert!(!default_config.enabled);
        assert!(default_config.allow_remote_fetch);
        assert!(default_config.per_session.is_empty());
        assert_eq!(default_config.tools.len(), 5);
        assert_eq!(
            default_config.tools.get("create_worktree").map(|tool| tool.requires_confirmation),
            Some(McpConfirmationMode::FirstUse)
        );
        assert_eq!(
            default_config
                .tools
                .get("cleanup_merged_worktrees")
                .map(|tool| tool.requires_confirmation),
            Some(McpConfirmationMode::Always)
        );
    }

    #[test]
    fn mcp_rate_limits_default_matches_overview_caps() {
        let default_limits = McpRateLimits::default();

        assert_eq!(default_limits.structural_read_per_min, 30);
        assert_eq!(default_limits.destructive_per_min, 5);
        assert_eq!(default_limits.total_per_min, 30);
    }

    #[test]
    fn mcp_rate_limits_config_default_matches_planning_defaults() {
        let default_limits = McpRateLimitsConfig::default();

        assert_eq!(default_limits.per_session.total_per_min, 30);
        assert_eq!(default_limits.per_session.destructive_per_min, 5);
        assert_eq!(default_limits.per_workspace.total_per_min, 100);
        assert_eq!(default_limits.per_workspace.destructive_per_min, 15);
        assert_eq!(default_limits.per_workspace.create_worktree_per_hour, 30);
        assert_eq!(default_limits.per_workspace.fetch_per_60s, 1);
        assert_eq!(default_limits.per_host.total_per_min, 500);
    }

    #[test]
    fn legacy_app_config_without_mcp_deserializes_with_default_mcp_settings() {
        let fixture = json!({
            "configVersion": 11,
            "worktreeRoots": [],
            "lastOpenSessions": [],
            "tabOrder": []
        });

        let deserialized: AppConfig = serde_json::from_value(fixture).expect("deserialize legacy config");
        assert_eq!(deserialized.config_version, 11);
        assert_eq!(deserialized.mcp, AppConfigMcp::default());
    }

    #[test]
    fn config_version_current_bumps_for_mcp_schema() {
        assert_eq!(CONFIG_VERSION_CURRENT, 12);
    }
}
