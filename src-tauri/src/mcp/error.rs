//! Host-internal MCP error enum + boundary conversion to the canonical wire `MCPError`.
//!
//! Conventions:
//! * Internal call sites return `Result<T, McpInternalError>` — the variants carry typed
//!   context (`retry_after`, `budget_remaining`, `disabled_by`) so the host can make routing
//!   decisions without re-parsing strings.
//! * The IPC boundary converts to `arborist_types::mcp::MCPError` via `From`. That step is
//!   where we choose `recoverable`, fill in `user_action`, and lose typed fields not part of
//!   the wire contract.
//! * Pass-through conversions from `crate::types::Error`, `crate::git::GitError`, and
//!   `std::io::Error` live here so call sites can `?` their underlying errors directly. The
//!   compose/path-validation helpers return `String`s today; callers convert those manually
//!   to `InvalidArg` / `InvalidName` at the validation boundary.

use std::time::Duration;

use thiserror::Error;

use crate::mcp::types::{MCPError, McpDisabledBy, McpErrorCode};

#[derive(Debug, Clone, Error)]
pub enum McpInternalError {
    #[error("{message}")]
    WorkspaceUnbound { message: String },
    #[error("{message}")]
    ToolDisabled { message: String, disabled_by: Option<McpDisabledBy> },
    #[error("{message}")]
    ToolNotImplemented { message: String },
    #[error("{message}")]
    RateLimited {
        message: String,
        retry_after: Duration,
        budget_remaining: u32,
    },
    #[error("{message}")]
    HostUnavailable { message: String },
    #[error("{message}")]
    InvalidArg { message: String },
    #[error("{message}")]
    InvalidName { message: String },
    #[error("{message}")]
    InvalidPath { message: String },
    #[error("{message}")]
    NameInUse { message: String },
    #[error("{message}")]
    ConfirmationRequired { message: String },
    #[error("{message}")]
    ConfirmationExpired { message: String },
    #[error("{message}")]
    ConfirmationStale { message: String },
    #[error("{message}")]
    InvalidConfirmation { message: String },
    #[error("{message}")]
    RepoCommandTrustRequired { message: String },
    #[error("{message}")]
    SpawnLineageLimitExceeded { message: String },
    #[error("{message}")]
    WorktreeVanished { message: String },
    #[error("{message}")]
    StaleRemoteData { message: String },
    #[error("{message}")]
    OwnWorktreeRefused { message: String },
    #[error("{message}")]
    Busy { message: String },
    #[error("{message}")]
    WorktreeMissing { message: String },
    #[error("{message}")]
    DefaultBranchUnknown { message: String },
    #[error("{message}")]
    DryRunUnsupported { message: String },
    #[error("{message}")]
    TooManyPendingActions { message: String },
    #[error("{message}")]
    Unauthenticated { message: String },
    #[error("{message}")]
    SessionRevoked { message: String },
    #[error("{message}")]
    Internal { message: String },
}

impl McpInternalError {
    #[must_use]
    pub const fn code(&self) -> McpErrorCode {
        match self {
            Self::WorkspaceUnbound { .. } => McpErrorCode::WorkspaceUnbound,
            Self::ToolDisabled { .. } => McpErrorCode::ToolDisabled,
            Self::ToolNotImplemented { .. } => McpErrorCode::ToolNotImplemented,
            Self::RateLimited { .. } => McpErrorCode::RateLimited,
            Self::HostUnavailable { .. } => McpErrorCode::HostUnavailable,
            Self::InvalidArg { .. } => McpErrorCode::InvalidArg,
            Self::InvalidName { .. } => McpErrorCode::InvalidName,
            Self::InvalidPath { .. } => McpErrorCode::InvalidPath,
            Self::NameInUse { .. } => McpErrorCode::NameInUse,
            Self::ConfirmationRequired { .. } => McpErrorCode::ConfirmationRequired,
            Self::ConfirmationExpired { .. } => McpErrorCode::ConfirmationExpired,
            Self::ConfirmationStale { .. } => McpErrorCode::ConfirmationStale,
            Self::InvalidConfirmation { .. } => McpErrorCode::InvalidConfirmation,
            Self::RepoCommandTrustRequired { .. } => McpErrorCode::RepoCommandTrustRequired,
            Self::SpawnLineageLimitExceeded { .. } => McpErrorCode::SpawnLineageLimitExceeded,
            Self::WorktreeVanished { .. } => McpErrorCode::WorktreeVanished,
            Self::StaleRemoteData { .. } => McpErrorCode::StaleRemoteData,
            Self::OwnWorktreeRefused { .. } => McpErrorCode::OwnWorktreeRefused,
            Self::Busy { .. } => McpErrorCode::Busy,
            Self::WorktreeMissing { .. } => McpErrorCode::WorktreeMissing,
            Self::DefaultBranchUnknown { .. } => McpErrorCode::DefaultBranchUnknown,
            Self::DryRunUnsupported { .. } => McpErrorCode::DryRunUnsupported,
            Self::TooManyPendingActions { .. } => McpErrorCode::TooManyPendingActions,
            Self::Unauthenticated { .. } => McpErrorCode::Unauthenticated,
            Self::SessionRevoked { .. } => McpErrorCode::SessionRevoked,
            Self::Internal { .. } => McpErrorCode::Internal,
        }
    }

    #[must_use]
    pub fn disabled_by(&self) -> Option<McpDisabledBy> {
        match self {
            Self::ToolDisabled { disabled_by, .. } => *disabled_by,
            _ => None,
        }
    }

    /// Wall-clock retry hint in milliseconds, populated for rate-limited errors so clients can
    /// back off without polling. Returns `None` for non-rate-limited errors (the wire field is
    /// `Option<u64>` so this maps directly).
    #[must_use]
    pub fn retry_after_ms(&self) -> Option<u64> {
        match self {
            Self::RateLimited { retry_after, .. } => Some(u64::try_from(retry_after.as_millis()).unwrap_or(u64::MAX)),
            _ => None,
        }
    }

    #[must_use]
    pub fn budget_remaining(&self) -> Option<u32> {
        match self {
            Self::RateLimited { budget_remaining, .. } => Some(*budget_remaining),
            _ => None,
        }
    }
}

/// Construct an internal error from a code + message at call sites that don't have additional
/// typed context (e.g., where we're forwarding a parse/validation failure). For rate-limited /
/// tool-disabled / similar, construct the variant directly so the structured fields are not
/// silently zero-filled.
#[must_use]
pub fn error(code: McpErrorCode, message: impl Into<String>) -> McpInternalError {
    let message = message.into();
    match code {
        McpErrorCode::WorkspaceUnbound => McpInternalError::WorkspaceUnbound { message },
        McpErrorCode::ToolDisabled => McpInternalError::ToolDisabled { message, disabled_by: None },
        McpErrorCode::ToolNotImplemented => McpInternalError::ToolNotImplemented { message },
        McpErrorCode::RateLimited => McpInternalError::RateLimited {
            message,
            retry_after: Duration::ZERO,
            budget_remaining: 0,
        },
        McpErrorCode::HostUnavailable => McpInternalError::HostUnavailable { message },
        McpErrorCode::InvalidArg => McpInternalError::InvalidArg { message },
        McpErrorCode::InvalidName => McpInternalError::InvalidName { message },
        McpErrorCode::InvalidPath => McpInternalError::InvalidPath { message },
        McpErrorCode::InvalidFromBranch | McpErrorCode::InvalidSourceBranch | McpErrorCode::InvalidTargetBranch => {
            McpInternalError::InvalidArg { message }
        }
        McpErrorCode::NameInUse => McpInternalError::NameInUse { message },
        McpErrorCode::ToolNotConfigured | McpErrorCode::PrepNotConfigured | McpErrorCode::PrepFailed => McpInternalError::InvalidArg { message },
        McpErrorCode::ConfirmationRequired => McpInternalError::ConfirmationRequired { message },
        McpErrorCode::ConfirmationExpired => McpInternalError::ConfirmationExpired { message },
        McpErrorCode::ConfirmationStale => McpInternalError::ConfirmationStale { message },
        McpErrorCode::InvalidConfirmation => McpInternalError::InvalidConfirmation { message },
        McpErrorCode::RepoCommandTrustRequired => McpInternalError::RepoCommandTrustRequired { message },
        McpErrorCode::SpawnLineageLimitExceeded => McpInternalError::SpawnLineageLimitExceeded { message },
        McpErrorCode::WorktreeVanished => McpInternalError::WorktreeVanished { message },
        McpErrorCode::StaleRemoteData => McpInternalError::StaleRemoteData { message },
        McpErrorCode::OwnWorktreeRefused => McpInternalError::OwnWorktreeRefused { message },
        McpErrorCode::Busy => McpInternalError::Busy { message },
        McpErrorCode::WorktreeMissing => McpInternalError::WorktreeMissing { message },
        McpErrorCode::DefaultBranchUnknown => McpInternalError::DefaultBranchUnknown { message },
        McpErrorCode::DryRunUnsupported => McpInternalError::DryRunUnsupported { message },
        McpErrorCode::TooManyPendingActions => McpInternalError::TooManyPendingActions { message },
        McpErrorCode::Unauthenticated => McpInternalError::Unauthenticated { message },
        McpErrorCode::SessionRevoked => McpInternalError::SessionRevoked { message },
        McpErrorCode::Internal => McpInternalError::Internal { message },
    }
}

impl From<McpInternalError> for MCPError {
    fn from(err: McpInternalError) -> Self {
        let code = err.code();
        Self {
            code,
            message: err.to_string(),
            recoverable: code.is_recoverable(),
            user_action: code.default_user_action().map(str::to_owned),
            retry_after_ms: err.retry_after_ms(),
            budget_remaining: None,
            audit_id: None,
            disabled_by: err.disabled_by(),
        }
    }
}

impl From<crate::types::Error> for McpInternalError {
    fn from(err: crate::types::Error) -> Self {
        match err {
            crate::types::Error::InvalidPath(message) => Self::InvalidPath { message },
            crate::types::Error::WorktreeMissing(path) => Self::WorktreeMissing {
                message: path.display().to_string(),
            },
            other => Self::Internal { message: other.to_string() },
        }
    }
}

impl From<crate::git::GitError> for McpInternalError {
    fn from(err: crate::git::GitError) -> Self {
        // GitError variants are coarse: the git porcelain produces messages we forward more or
        // less verbatim. The host classifier picks a recoverable code for the well-known
        // failures (default branch unknown, ref not found) and falls back to `Internal` for
        // IO / process / timeout faults — which a client cannot work around without operator
        // intervention.
        match err {
            crate::git::GitError::DefaultBranchUnknown => Self::DefaultBranchUnknown {
                message: "default branch could not be determined".to_owned(),
            },
            crate::git::GitError::RefNotFound { ref_expr } => Self::InvalidArg {
                message: format!("ref not found: {ref_expr}"),
            },
            crate::git::GitError::TimedOut { context, timeout } => Self::Busy {
                message: format!("git {context} timed out after {timeout:?}"),
            },
            crate::git::GitError::CommandFailed { context, message } => Self::Internal {
                message: format!("git {context}: {message}"),
            },
            crate::git::GitError::Io { context, source } => Self::Internal {
                message: format!("git {context}: {source}"),
            },
        }
    }
}

impl From<std::io::Error> for McpInternalError {
    fn from(err: std::io::Error) -> Self {
        Self::Internal { message: err.to_string() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limited_carries_structured_fields_to_wire() {
        let internal = McpInternalError::RateLimited {
            message: "budget exhausted".to_owned(),
            retry_after: Duration::from_millis(1_500),
            budget_remaining: 4,
        };
        let wire: MCPError = internal.into();
        assert_eq!(wire.code, McpErrorCode::RateLimited);
        assert!(wire.recoverable);
        assert_eq!(wire.retry_after_ms, Some(1_500));
    }

    #[test]
    fn tool_disabled_carries_disabled_by_to_wire() {
        let internal = McpInternalError::ToolDisabled {
            message: "session revoked".to_owned(),
            disabled_by: Some(McpDisabledBy::Session),
        };
        let wire: MCPError = internal.into();
        assert_eq!(wire.disabled_by, Some(McpDisabledBy::Session));
    }

    #[test]
    fn git_error_default_branch_unknown_maps_to_dedicated_code() {
        let internal: McpInternalError = crate::git::GitError::DefaultBranchUnknown.into();
        assert_eq!(internal.code(), McpErrorCode::DefaultBranchUnknown);
    }

    #[test]
    fn git_error_ref_not_found_maps_to_invalid_arg() {
        let internal: McpInternalError = crate::git::GitError::RefNotFound {
            ref_expr: "origin/missing".to_owned(),
        }
        .into();
        assert_eq!(internal.code(), McpErrorCode::InvalidArg);
        assert!(internal.to_string().contains("origin/missing"));
    }

    #[test]
    fn git_error_timeout_maps_to_busy() {
        let internal: McpInternalError = crate::git::GitError::TimedOut {
            context: "fetch origin",
            timeout: Duration::from_secs(30),
        }
        .into();
        assert_eq!(internal.code(), McpErrorCode::Busy);
    }
}
