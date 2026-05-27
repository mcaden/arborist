use std::collections::BTreeMap;

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::commands::AppContext;
use crate::mcp::ipc::effective_config;
use crate::mcp::types::{
    AppConfigMcp, ConfirmationToken, McpAuditFilter, McpAuditPage, McpPendingAction, McpSessionMode, McpStatus, McpTrustRecord, PartialAppConfigMcp,
    PartialMcpSessionConfig,
};
use crate::mcp::McpContext;
use crate::types::{AppError, PartialAppConfig, SessionId};

fn workspace_bound(ctx: &AppContext) -> bool {
    let workspace = match ctx.workspace.read() {
        Ok(workspace) => workspace,
        Err(poisoned) => poisoned.into_inner(),
    };
    !workspace.is_unbound()
}

fn require_workspace(ctx: &AppContext) -> Result<(), AppError> {
    if workspace_bound(ctx) {
        Ok(())
    } else {
        Err(AppError::new("WorkspaceUnbound", "MCP settings require an open workspace"))
    }
}

fn tampered_logs(mcp: &McpContext) -> Vec<String> {
    mcp.audit.tampered_logs().into_iter().map(|path| path.display().to_string()).collect()
}

fn status_from_config(config: AppConfigMcp, mcp: &McpContext) -> McpStatus {
    McpStatus {
        config,
        tampered_logs: tampered_logs(mcp),
    }
}

fn current_status(ctx: &AppContext, mcp: &McpContext) -> McpStatus {
    if workspace_bound(ctx) {
        return status_from_config(ctx.store().load_config().mcp, mcp);
    }
    status_from_config(AppConfigMcp::default(), mcp)
}

fn now_rfc3339() -> Result<String, AppError> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|err| AppError::new("Internal", format!("format MCP timestamp: {err}")))
}

pub fn mcp_status_impl(ctx: &AppContext, mcp: &McpContext) -> McpStatus {
    current_status(ctx, mcp)
}

pub fn mcp_set_enabled_impl(ctx: &AppContext, mcp: &McpContext, enabled: bool) -> Result<McpStatus, AppError> {
    require_workspace(ctx)?;
    let patch = PartialAppConfig {
        mcp: Some(PartialAppConfigMcp {
            enabled: Some(enabled),
            disclosure_acknowledged_at: if enabled { Some(now_rfc3339()?) } else { None },
            ..Default::default()
        }),
        ..Default::default()
    };
    let merged = ctx.store().save_config(patch).map_err(AppError::from)?;
    Ok(status_from_config(merged.mcp, mcp))
}

pub fn mcp_set_session_mode_impl(ctx: &AppContext, mcp: &McpContext, session_id: SessionId, mode: McpSessionMode) -> Result<McpStatus, AppError> {
    require_workspace(ctx)?;
    let patch = PartialAppConfig {
        mcp: Some(PartialAppConfigMcp {
            per_session: BTreeMap::from([(session_id.to_string(), PartialMcpSessionConfig { mode: Some(mode) })]),
            ..Default::default()
        }),
        ..Default::default()
    };
    let merged = ctx.store().save_config(patch).map_err(AppError::from)?;
    Ok(status_from_config(merged.mcp, mcp))
}

pub fn mcp_get_effective_config_impl(ctx: &AppContext, session_id: SessionId) -> crate::mcp::types::McpEffectiveConfig {
    let config = if workspace_bound(ctx) {
        ctx.store().load_config().mcp
    } else {
        AppConfigMcp::default()
    };
    effective_config(&config, &session_id.to_string())
}

pub fn mcp_pending_actions_impl(mcp: &McpContext, session_id: Option<SessionId>) -> Vec<McpPendingAction> {
    match session_id {
        Some(session_id) => mcp.confirm.list_for_session_wire(&session_id.to_string()),
        None => mcp.confirm.list_all_wire(),
    }
}

pub fn mcp_approve_impl(mcp: &McpContext, action_id: &str) -> Result<ConfirmationToken, AppError> {
    mcp.confirm
        .approve(action_id)
        .ok_or_else(|| AppError::new("NotFound", format!("pending MCP action '{action_id}' was not found")))
}

pub fn mcp_deny_impl(mcp: &McpContext, action_id: &str) -> bool {
    mcp.confirm.deny(action_id)
}

pub fn mcp_trust_list_impl(mcp: &McpContext, session_id: SessionId) -> Vec<McpTrustRecord> {
    mcp.trust.list_for_session_wire(&session_id.to_string())
}

pub fn mcp_trust_revoke_impl(mcp: &McpContext, session_id: SessionId, id: &str) -> bool {
    mcp.trust.revoke(&session_id.to_string(), id)
}

pub fn mcp_audit_recent_impl(ctx: &AppContext, mcp: &McpContext, filter: McpAuditFilter) -> Result<McpAuditPage, AppError> {
    if workspace_bound(ctx) {
        mcp.audit
            .read_page(&filter)
            .map_err(|err| AppError::new("Internal", format!("read MCP audit log: {err}")))
    } else {
        Ok(McpAuditPage {
            records: Vec::new(),
            next_cursor: None,
        })
    }
}
