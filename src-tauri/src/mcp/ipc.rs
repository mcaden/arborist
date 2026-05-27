use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use interprocess::local_socket::{
    tokio::prelude::*, tokio::Listener as LocalSocketListener, tokio::Stream as LocalSocketStream, GenericFilePath, ListenerOptions,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Digest;
use tauri::Emitter;
use tokio::io::{split, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};
use uuid::Uuid;

#[cfg(unix)]
use interprocess::os::unix::local_socket::ListenerOptionsExt;

use crate::mcp::error::McpInternalError;
use crate::mcp::rate_limit::McpRateKind;
use crate::mcp::types::{
    AppConfigMcp, MCPError, McpActivityEvent, McpActivityPhase, McpConfirmationMode, McpDisabledBy, McpEffectiveConfig, McpEffectiveSource,
    McpEffectiveSourceEffect, McpEffectiveSourceLayer, McpEffectiveTool, McpErrorCode, McpSessionMode, McpToolCallParams, McpToolDescriptor,
};
use crate::mcp::{McpContext, McpToolName};
use crate::types::SessionId;

const MAX_FRAME_LEN: usize = 1024 * 1024;
const MCP_PROTOCOL: &str = "arborist-mcp/1";
const FAILED_AUTH_WINDOW: Duration = Duration::from_secs(60);
const FAILED_AUTH_LIMIT: usize = 5;
#[cfg(windows)]
const WINDOWS_PIPE_PREFIX: &str = r"\\.\pipe\arborist-mcp";

type ActivityEmitter = Arc<dyn Fn(McpActivityEvent) + Send + Sync>;

#[derive(Debug, Clone)]
pub struct RegisteredSession {
    pub socket_path: PathBuf,
    pub token_hex: String,
    pub host_hash_hex: String,
}

struct McpSessionState {
    token: [u8; 32],
    socket_path: PathBuf,
    peer_pid: Option<u32>,
    listener_task: JoinHandle<()>,
    shutdown_tx: oneshot::Sender<()>,
}

pub struct McpSessionRegistry {
    context: Arc<McpContext>,
    sessions: Mutex<HashMap<String, McpSessionState>>,
    failed_auth: Mutex<VecDeque<Instant>>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HelloFrame {
    protocol: String,
    token: String,
    session_id: String,
    pid: u32,
}

#[derive(Debug, Serialize, Deserialize)]
struct HelloAck {
    ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum OutboundFrame {
    Request {
        id: String,
        method: String,
        #[serde(default)]
        params: Option<Value>,
    },
    Notification {
        method: String,
        #[serde(default)]
        params: Option<Value>,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum InboundFrame {
    Response {
        id: String,
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<HostErrorEnvelope>,
    },
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HostErrorKind {
    InvalidParams,
    #[default]
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostErrorEnvelope {
    #[serde(default)]
    pub kind: HostErrorKind,
    #[serde(flatten)]
    pub mcp: MCPError,
}

impl McpSessionRegistry {
    #[must_use]
    pub fn new(context: Arc<McpContext>) -> Self {
        Self {
            context,
            sessions: Mutex::new(HashMap::new()),
            failed_auth: Mutex::new(VecDeque::new()),
        }
    }

    /// Safe accessor for the host-side MCP context. Tool implementations under
    /// `crate::mcp::tools::*` need workspace state, the pending-action registry, the audit log,
    /// the trust store, and the rate limiter — all of which live on `McpContext`. We hand back a
    /// cloned `Arc` (not a borrow) so tools can move it into `tokio::spawn_blocking` closures
    /// without entangling the borrow checker with the registry guard.
    ///
    /// This accessor replaces an unsafe pointer-cast extraction that earlier agents wrote when
    /// no public seam existed; field reordering on `McpSessionRegistry` would have been
    /// undefined behaviour. Always use this accessor.
    #[must_use]
    pub fn context(&self) -> Arc<McpContext> {
        Arc::clone(&self.context)
    }

    pub fn register(self: &Arc<Self>, session_id: String, app_handle: tauri::AppHandle) -> Result<RegisteredSession, McpInternalError> {
        let emitter: ActivityEmitter = Arc::new(move |event| {
            if let Err(err) = app_handle.emit("mcp://activity", event) {
                warn!(%err, "failed to emit mcp://activity event");
            }
        });
        self.register_with_emitter(session_id, emitter)
    }

    pub(crate) fn register_with_emitter(
        self: &Arc<Self>,
        session_id: String,
        emitter: ActivityEmitter,
    ) -> Result<RegisteredSession, McpInternalError> {
        let token = mint_token();
        let token_hex = hex::encode(token);
        let socket_path = self.socket_path_for(&session_id, &token_hex);
        let listener = bind_listener(&socket_path)?;
        let host_hash_hex = hex::encode(self.context.sidecar_hash);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let registry = Arc::clone(self);
        let task_session_id = session_id.clone();
        let task_socket_path = socket_path.clone();
        let listener_task = tokio::spawn(async move {
            listener_loop(registry, listener, task_session_id, token, task_socket_path, shutdown_rx, emitter).await;
        });

        let mut guard = match self.sessions.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(previous) = guard.remove(&session_id) {
            debug!(session_id, path = %previous.socket_path.display(), "replacing prior MCP sidecar listener");
            let _ = previous.shutdown_tx.send(());
            previous.listener_task.abort();
            #[cfg(not(windows))]
            {
                let _ = std::fs::remove_file(&previous.socket_path);
            }
        }
        guard.insert(
            session_id,
            McpSessionState {
                token,
                socket_path: socket_path.clone(),
                peer_pid: None,
                listener_task,
                shutdown_tx,
            },
        );

        Ok(RegisteredSession {
            socket_path,
            token_hex,
            host_hash_hex,
        })
    }

    pub fn revoke(&self, session_id: &str) {
        let state = {
            let mut guard = match self.sessions.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.remove(session_id)
        };
        if let Some(state) = state {
            debug!(session_id, path = %state.socket_path.display(), "revoking MCP sidecar listener");
            let _ = state.shutdown_tx.send(());
            state.listener_task.abort();
            #[cfg(not(windows))]
            {
                let _ = std::fs::remove_file(&state.socket_path);
            }
        }
        self.context.rate.clear_session(session_id);
        self.context.confirm.clear_session(session_id);
        self.context.trust.clear_session(session_id);
    }

    fn current_config(&self) -> AppConfigMcp {
        let workspace = match self.context.app.workspace.read() {
            Ok(workspace) => workspace,
            Err(poisoned) => poisoned.into_inner(),
        };
        let Some(store) = workspace.store.clone() else {
            return AppConfigMcp::default();
        };
        drop(workspace);
        store.load_config().mcp
    }

    fn current_workspace_id(&self) -> Option<String> {
        let workspace = match self.context.app.workspace.read() {
            Ok(workspace) => workspace,
            Err(poisoned) => poisoned.into_inner(),
        };
        workspace.workspace_root.as_ref().map(|path| path.display().to_string())
    }

    fn socket_path_for(&self, session_id: &str, token_hex: &str) -> PathBuf {
        #[cfg(windows)]
        {
            PathBuf::from(format!("{WINDOWS_PIPE_PREFIX}-{session_id}-{}", &token_hex[..12]))
        }
        #[cfg(not(windows))]
        {
            let dir = self.context.workspace_state_dir.join("mcp-sockets");
            let _ = std::fs::create_dir_all(&dir);
            dir.join(format!("{session_id}-{}.sock", &token_hex[..12]))
        }
    }

    fn mark_peer_pid(&self, session_id: &str, peer_pid: Option<u32>) {
        let mut guard = match self.sessions.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(state) = guard.get_mut(session_id) {
            state.peer_pid = peer_pid;
        }
    }

    fn current_peer_pid(&self, session_id: &str) -> Option<u32> {
        let guard = match self.sessions.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.get(session_id).and_then(|state| state.peer_pid)
    }

    fn session_active(&self, session_id: &str, expected_token: &[u8; 32]) -> bool {
        let guard = match self.sessions.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.get(session_id).map(|state| &state.token == expected_token).unwrap_or(false)
    }

    fn record_failed_auth(&self) {
        let now = Instant::now();
        let mut guard = match self.failed_auth.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        while let Some(front) = guard.front().copied() {
            if now.duration_since(front) > FAILED_AUTH_WINDOW {
                let _ = guard.pop_front();
            } else {
                break;
            }
        }
        guard.push_back(now);
    }

    fn auth_backoff_active(&self) -> bool {
        let now = Instant::now();
        let mut guard = match self.failed_auth.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        while let Some(front) = guard.front().copied() {
            if now.duration_since(front) > FAILED_AUTH_WINDOW {
                let _ = guard.pop_front();
            } else {
                break;
            }
        }
        guard.len() > FAILED_AUTH_LIMIT
    }

    fn validate_lineage(&self, session_id: &str, peer_pid: Option<u32>) -> bool {
        let Some(peer_pid) = peer_pid else {
            warn!(
                session_id,
                "peer PID unavailable; accepting MCP connection based on token + session id only"
            );
            return true;
        };
        if self.current_peer_pid(session_id).is_some_and(|expected| expected == peer_pid) {
            return true;
        }
        let Some(session_pid) = session_root_pid(&self.context, session_id) else {
            warn!(
                session_id,
                "session PTY PID unavailable; accepting MCP connection based on token + session id only"
            );
            return true;
        };
        if peer_pid == session_pid {
            return true;
        }
        match is_descendant_of(peer_pid, session_pid) {
            Some(result) => result,
            None => {
                warn!(
                    session_id,
                    peer_pid, session_pid, "peer lineage validation unavailable on this platform; accepting MCP connection"
                );
                true
            }
        }
    }
}

async fn listener_loop(
    registry: Arc<McpSessionRegistry>,
    listener: LocalSocketListener,
    session_id: String,
    token: [u8; 32],
    socket_path: PathBuf,
    mut shutdown_rx: oneshot::Receiver<()>,
    emitter: ActivityEmitter,
) {
    loop {
        tokio::select! {
            _ = &mut shutdown_rx => break,
            accepted = listener.accept() => {
                match accepted {
                    Ok(stream) => {
                        let registry = Arc::clone(&registry);
                        let session_id = session_id.clone();
                        let emitter = Arc::clone(&emitter);
                        tokio::spawn(async move {
                            if let Err(err) = handle_connection(registry, session_id, token, stream, emitter).await {
                                debug!(%err, "MCP sidecar connection closed");
                            }
                        });
                    }
                    Err(err) => {
                        warn!(session_id, path = %socket_path.display(), %err, "MCP listener accept failed");
                        break;
                    }
                }
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = std::fs::remove_file(&socket_path);
    }
}

async fn handle_connection(
    registry: Arc<McpSessionRegistry>,
    session_id: String,
    token: [u8; 32],
    mut stream: LocalSocketStream,
    emitter: ActivityEmitter,
) -> io::Result<()> {
    let hello_bytes = read_frame(&mut stream, MAX_FRAME_LEN).await?;
    let hello: HelloFrame = match serde_json::from_slice(&hello_bytes) {
        Ok(hello) => hello,
        Err(err) => {
            registry.record_failed_auth();
            warn!(session_id, %err, "invalid MCP hello frame");
            write_hello_ack(&mut stream, false, Some("MCP authentication failed")).await?;
            return Ok(());
        }
    };

    if registry.auth_backoff_active() {
        write_hello_ack(
            &mut stream,
            false,
            Some("Too many failed MCP authentication attempts; wait a minute and retry."),
        )
        .await?;
        return Ok(());
    }

    let presented_token = match decode_token(&hello.token) {
        Some(token) => token,
        None => {
            registry.record_failed_auth();
            write_hello_ack(&mut stream, false, Some("MCP authentication failed")).await?;
            return Ok(());
        }
    };
    let peer_pid = peer_pid_of(&stream);
    if hello.protocol != MCP_PROTOCOL || hello.session_id != session_id || presented_token != token {
        registry.record_failed_auth();
        write_hello_ack(&mut stream, false, Some("MCP authentication failed")).await?;
        return Ok(());
    }
    if let Some(peer_pid) = peer_pid {
        if hello.pid != peer_pid {
            registry.record_failed_auth();
            write_hello_ack(&mut stream, false, Some("MCP authentication failed")).await?;
            return Ok(());
        }
    }
    if !registry.validate_lineage(&session_id, peer_pid) {
        registry.record_failed_auth();
        write_hello_ack(&mut stream, false, Some("MCP authentication failed")).await?;
        return Ok(());
    }
    registry.mark_peer_pid(&session_id, peer_pid);
    info!(session_id, peer_pid, "accepted MCP sidecar connection");
    write_hello_ack(&mut stream, true, None).await?;

    let (mut reader, mut writer) = split(stream);
    loop {
        let frame = match read_frame(&mut reader, MAX_FRAME_LEN).await {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(err) => return Err(err),
        };
        let outbound: OutboundFrame = match serde_json::from_slice(&frame) {
            Ok(outbound) => outbound,
            Err(err) => {
                warn!(session_id, %err, "failed to decode MCP outbound frame");
                continue;
            }
        };

        match outbound {
            OutboundFrame::Request { id, method, params } => {
                let response = handle_request(&registry, &session_id, &token, &id, &method, params, &emitter).await;
                write_frame(&mut writer, &serde_json::to_vec(&response).map_err(to_io_error)?).await?;
            }
            OutboundFrame::Notification { method, params } => {
                handle_notification(&session_id, &method, params);
            }
        }
    }
}

async fn handle_request(
    registry: &McpSessionRegistry,
    session_id: &str,
    token: &[u8; 32],
    request_id: &str,
    method: &str,
    params: Option<Value>,
    emitter: &ActivityEmitter,
) -> InboundFrame {
    if !registry.session_active(session_id, token) {
        return error_response(
            request_id,
            HostErrorEnvelope {
                kind: HostErrorKind::Internal,
                mcp: MCPError::from(McpInternalError::SessionRevoked {
                    message: format!("MCP session '{session_id}' has been revoked"),
                }),
            },
        );
    }

    match method {
        "tools/list" => {
            let started_at = now_rfc3339();
            emit_activity(
                emitter,
                registry,
                session_id,
                request_id,
                "tools/list",
                McpActivityPhase::Requested,
                Some(started_at.clone()),
                None,
                None,
            );
            emit_activity(
                emitter,
                registry,
                session_id,
                request_id,
                "tools/list",
                McpActivityPhase::Running,
                Some(started_at.clone()),
                Some("Listing available MCP tools".to_owned()),
                None,
            );
            let descriptors = phase2_descriptors(&registry.current_config(), session_id);
            emit_activity(
                emitter,
                registry,
                session_id,
                request_id,
                "tools/list",
                McpActivityPhase::Completed,
                Some(started_at),
                Some(format!("Listed {} MCP tools", descriptors.len())),
                None,
            );
            success_response(request_id, json!({ "tools": descriptors }))
        }
        "tools/call" => {
            let call_params = match params {
                Some(value) => match serde_json::from_value::<McpToolCallParams>(value) {
                    Ok(params) => params,
                    Err(err) => {
                        return error_response(
                            request_id,
                            HostErrorEnvelope {
                                kind: HostErrorKind::InvalidParams,
                                mcp: MCPError::from(McpInternalError::InvalidArg {
                                    message: format!("tools/call params must match McpToolCallParams: {err}"),
                                }),
                            },
                        )
                    }
                },
                None => {
                    return error_response(
                        request_id,
                        HostErrorEnvelope {
                            kind: HostErrorKind::InvalidParams,
                            mcp: MCPError::from(McpInternalError::InvalidArg {
                                message: "tools/call requires params".to_owned(),
                            }),
                        },
                    )
                }
            };
            handle_tool_call(registry, session_id, request_id, call_params, emitter).await
        }
        other => error_response(
            request_id,
            HostErrorEnvelope {
                kind: HostErrorKind::Internal,
                mcp: MCPError::from(McpInternalError::InvalidArg {
                    message: format!("unsupported MCP host method: {other}"),
                }),
            },
        ),
    }
}

async fn handle_tool_call(
    registry: &McpSessionRegistry,
    session_id: &str,
    request_id: &str,
    params: McpToolCallParams,
    emitter: &ActivityEmitter,
) -> InboundFrame {
    let started_at = now_rfc3339();
    emit_activity(
        emitter,
        registry,
        session_id,
        request_id,
        &params.name,
        McpActivityPhase::Requested,
        Some(started_at.clone()),
        None,
        None,
    );

    let Some(tool) = McpToolName::from_id(&params.name) else {
        let error = MCPError::from(McpInternalError::InvalidArg {
            message: format!("unknown MCP tool '{}'", params.name),
        });
        emit_activity(
            emitter,
            registry,
            session_id,
            request_id,
            &params.name,
            McpActivityPhase::Failed,
            Some(started_at),
            None,
            Some(error.clone()),
        );
        return error_response(
            request_id,
            HostErrorEnvelope {
                kind: HostErrorKind::Internal,
                mcp: error,
            },
        );
    };

    let config = registry.current_config();
    if let Err(err) = ensure_tool_available(&config, session_id, tool) {
        let error: MCPError = err.into();
        emit_activity(
            emitter,
            registry,
            session_id,
            request_id,
            tool.as_id(),
            McpActivityPhase::Failed,
            Some(started_at),
            None,
            Some(error.clone()),
        );
        return error_response(
            request_id,
            HostErrorEnvelope {
                kind: HostErrorKind::Internal,
                mcp: error,
            },
        );
    }

    let rate_kind = tool_rate_kind(tool);
    if let Err(err) = consume_rate_limits(registry, session_id, rate_kind) {
        let error: MCPError = err.into();
        emit_activity(
            emitter,
            registry,
            session_id,
            request_id,
            tool.as_id(),
            McpActivityPhase::RateLimited,
            Some(started_at),
            None,
            Some(error.clone()),
        );
        return error_response(
            request_id,
            HostErrorEnvelope {
                kind: HostErrorKind::Internal,
                mcp: error,
            },
        );
    }

    emit_activity(
        emitter,
        registry,
        session_id,
        request_id,
        tool.as_id(),
        McpActivityPhase::Running,
        Some(started_at.clone()),
        Some(format!("Invoking {}", tool.as_id())),
        None,
    );

    let invoke_result = match tool {
        McpToolName::ListWorktrees => crate::mcp::tools::list_worktrees::invoke(registry, session_id, params.arguments.clone()).await,
        McpToolName::WorkspaceStatus => crate::mcp::tools::workspace_status::invoke(registry, session_id, params.arguments.clone()).await,
        McpToolName::CreateWorktree => crate::mcp::tools::create_worktree::invoke(registry, session_id, params.arguments.clone()).await,
        McpToolName::CleanupMergedWorktrees => {
            crate::mcp::tools::cleanup_merged_worktrees::invoke(registry, session_id, params.arguments.clone()).await
        }
        McpToolName::MergeMainIntoWorktrees => {
            crate::mcp::tools::merge_main_into_worktrees::invoke(registry, session_id, params.arguments.clone()).await
        }
    };

    match invoke_result {
        Ok(result) => {
            emit_activity(
                emitter,
                registry,
                session_id,
                request_id,
                tool.as_id(),
                McpActivityPhase::Completed,
                Some(started_at),
                Some(format!("{} completed", tool.as_id())),
                None,
            );
            success_response(request_id, result)
        }
        Err(err) => {
            let error: MCPError = err.into();
            // why: Surface confirmation-required and rate-limited as their own phases so the UI
            // can distinguish "agent asked, user must approve" from a hard failure. All other
            // McpInternalError variants are surfaced as Failed.
            let phase = match error.code {
                McpErrorCode::ConfirmationRequired => McpActivityPhase::AwaitingConfirmation,
                McpErrorCode::RateLimited => McpActivityPhase::RateLimited,
                _ => McpActivityPhase::Failed,
            };
            emit_activity(
                emitter,
                registry,
                session_id,
                request_id,
                tool.as_id(),
                phase,
                Some(started_at),
                None,
                Some(error.clone()),
            );
            error_response(
                request_id,
                HostErrorEnvelope {
                    kind: HostErrorKind::Internal,
                    mcp: error,
                },
            )
        }
    }
}

fn handle_notification(session_id: &str, method: &str, params: Option<Value>) {
    if method == "notifications/cancelled" {
        debug!(session_id, ?params, "received MCP cancellation notification");
        return;
    }
    debug!(session_id, method, ?params, "ignoring unsupported MCP notification");
}

fn success_response(request_id: &str, result: Value) -> InboundFrame {
    InboundFrame::Response {
        id: request_id.to_owned(),
        ok: true,
        result: Some(result),
        error: None,
    }
}

fn error_response(request_id: &str, error: HostErrorEnvelope) -> InboundFrame {
    InboundFrame::Response {
        id: request_id.to_owned(),
        ok: false,
        result: None,
        error: Some(error),
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_activity(
    emitter: &ActivityEmitter,
    registry: &McpSessionRegistry,
    session_id: &str,
    request_id: &str,
    tool: &str,
    phase: McpActivityPhase,
    started_at: Option<String>,
    summary: Option<String>,
    error: Option<MCPError>,
) {
    let started_at = started_at.unwrap_or_else(now_rfc3339);
    let event = McpActivityEvent {
        id: request_id.to_owned(),
        session_id: session_id.to_owned(),
        workspace_id: registry.current_workspace_id(),
        tool: tool.to_owned(),
        phase,
        started_at,
        updated_at: now_rfc3339(),
        summary,
        error,
    };
    emitter(event);
}

fn now_rfc3339() -> String {
    match time::OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339) {
        Ok(value) => value,
        Err(_) => "1970-01-01T00:00:00Z".to_owned(),
    }
}

fn ensure_tool_available(config: &AppConfigMcp, session_id: &str, tool: McpToolName) -> Result<(), McpInternalError> {
    if !config.enabled {
        return Err(McpInternalError::ToolDisabled {
            message: "MCP is disabled for this workspace".to_owned(),
            disabled_by: Some(McpDisabledBy::Global),
        });
    }
    if !config.tools.get(tool.as_id()).map(|tool| tool.enabled).unwrap_or(true) {
        return Err(McpInternalError::ToolDisabled {
            message: format!("MCP tool '{}' is disabled", tool.as_id()),
            disabled_by: Some(McpDisabledBy::Tool),
        });
    }
    if !tool_enabled(config, session_id, tool) {
        return Err(McpInternalError::ToolDisabled {
            message: format!("MCP tool '{}' is disabled for session '{session_id}'", tool.as_id()),
            disabled_by: Some(McpDisabledBy::Session),
        });
    }
    Ok(())
}

pub(crate) fn tool_enabled(config: &AppConfigMcp, session_id: &str, tool: McpToolName) -> bool {
    if !config.enabled {
        return false;
    }
    if !config.tools.get(tool.as_id()).map(|tool| tool.enabled).unwrap_or(true) {
        return false;
    }
    match session_mode(config, session_id) {
        McpSessionMode::Full => true,
        McpSessionMode::ReadOnly => matches!(tool, McpToolName::ListWorktrees | McpToolName::WorkspaceStatus),
        McpSessionMode::Off => false,
    }
}

pub(crate) fn session_mode(config: &AppConfigMcp, session_id: &str) -> McpSessionMode {
    config
        .per_session
        .get(session_id)
        .map(|session| session.mode)
        .unwrap_or(McpSessionMode::Full)
}

pub(crate) fn effective_config(config: &AppConfigMcp, session_id: &str) -> McpEffectiveConfig {
    let session_mode = session_mode(config, session_id);
    let mut tools = Vec::new();
    for tool in McpToolName::ALL {
        let mut sources = Vec::new();
        if config.enabled {
            sources.push(McpEffectiveSource {
                layer: McpEffectiveSourceLayer::Global,
                effect: McpEffectiveSourceEffect::Enabled,
            });
        } else {
            sources.push(McpEffectiveSource {
                layer: McpEffectiveSourceLayer::Global,
                effect: McpEffectiveSourceEffect::Disabled,
            });
        }
        let tool_config = config.tools.get(tool.as_id()).cloned().unwrap_or_default();
        if tool_config.enabled {
            sources.push(McpEffectiveSource {
                layer: McpEffectiveSourceLayer::Global,
                effect: McpEffectiveSourceEffect::Enabled,
            });
        } else {
            sources.push(McpEffectiveSource {
                layer: McpEffectiveSourceLayer::Global,
                effect: McpEffectiveSourceEffect::Disabled,
            });
        }
        if tool_config.requires_confirmation != McpConfirmationMode::Never {
            sources.push(McpEffectiveSource {
                layer: McpEffectiveSourceLayer::Global,
                effect: McpEffectiveSourceEffect::RequiresConfirmation,
            });
        }
        let session_effect = match session_mode {
            McpSessionMode::Full => McpEffectiveSourceEffect::Enabled,
            McpSessionMode::ReadOnly if matches!(tool, McpToolName::ListWorktrees | McpToolName::WorkspaceStatus) => {
                McpEffectiveSourceEffect::Enabled
            }
            McpSessionMode::ReadOnly | McpSessionMode::Off => McpEffectiveSourceEffect::Disabled,
        };
        sources.push(McpEffectiveSource {
            layer: McpEffectiveSourceLayer::Session,
            effect: session_effect,
        });

        tools.push(McpEffectiveTool {
            id: tool.as_id().to_owned(),
            enabled: tool_enabled(config, session_id, *tool),
            requires_confirmation: tool_config.requires_confirmation != McpConfirmationMode::Never,
            sources,
        });
    }
    McpEffectiveConfig { tools }
}

pub(crate) fn phase2_descriptors(config: &AppConfigMcp, session_id: &str) -> Vec<McpToolDescriptor> {
    [McpToolName::ListWorktrees, McpToolName::WorkspaceStatus]
        .into_iter()
        .filter(|tool| tool_enabled(config, session_id, *tool))
        .map(tool_descriptor)
        .collect()
}

fn tool_descriptor(tool: McpToolName) -> McpToolDescriptor {
    // why: Each tool owns its own schema + description in `mcp/tools/<name>.rs`. Routing through
    // those module-level `descriptor()` fns means a Phase 3 sub-agent can update a single tool's
    // schema without touching the ipc dispatcher.
    match tool {
        McpToolName::ListWorktrees => crate::mcp::tools::list_worktrees::descriptor(),
        McpToolName::WorkspaceStatus => crate::mcp::tools::workspace_status::descriptor(),
        McpToolName::CreateWorktree => crate::mcp::tools::create_worktree::descriptor(),
        McpToolName::CleanupMergedWorktrees => crate::mcp::tools::cleanup_merged_worktrees::descriptor(),
        McpToolName::MergeMainIntoWorktrees => crate::mcp::tools::merge_main_into_worktrees::descriptor(),
    }
}

fn tool_rate_kind(tool: McpToolName) -> McpRateKind {
    match tool {
        McpToolName::ListWorktrees | McpToolName::WorkspaceStatus => McpRateKind::StructuralRead,
        McpToolName::CreateWorktree => McpRateKind::CreateWorktree,
        McpToolName::CleanupMergedWorktrees | McpToolName::MergeMainIntoWorktrees => McpRateKind::Destructive,
    }
}

fn consume_rate_limits(registry: &McpSessionRegistry, session_id: &str, kind: McpRateKind) -> Result<(), McpInternalError> {
    let now = Instant::now();
    registry
        .context
        .rate
        .check_and_consume(crate::mcp::types::McpRateScope::PerSession, session_id, kind, now)?;
    registry
        .context
        .rate
        .check_and_consume(crate::mcp::types::McpRateScope::PerWorkspace, session_id, kind, now)?;
    registry
        .context
        .rate
        .check_and_consume(crate::mcp::types::McpRateScope::PerHost, session_id, kind, now)?;
    Ok(())
}

async fn write_hello_ack(stream: &mut LocalSocketStream, ok: bool, error: Option<&str>) -> io::Result<()> {
    let payload = serde_json::to_vec(&HelloAck {
        ok,
        error: error.map(str::to_owned),
    })
    .map_err(to_io_error)?;
    write_frame(stream, &payload).await
}

fn decode_token(value: &str) -> Option<[u8; 32]> {
    let mut bytes = [0_u8; 32];
    hex::decode_to_slice(value, &mut bytes).ok()?;
    Some(bytes)
}

fn mint_token() -> [u8; 32] {
    let mut hasher = sha2::Sha256::new();
    hasher.update(Uuid::new_v4().as_bytes());
    hasher.update(Uuid::new_v4().as_bytes());
    hasher.finalize().into()
}

fn bind_listener(socket_path: &Path) -> Result<LocalSocketListener, McpInternalError> {
    let socket_name = socket_path
        .to_fs_name::<GenericFilePath>()
        .map_err(|err| McpInternalError::HostUnavailable {
            message: format!("invalid MCP socket path '{}': {err}", socket_path.display()),
        })?;
    let options = ListenerOptions::new().name(socket_name);
    #[cfg(unix)]
    let options = options.mode(0o600);
    options.create_tokio().map_err(|err| McpInternalError::HostUnavailable {
        message: format!("failed to bind MCP socket '{}': {err}", socket_path.display()),
    })
}

fn session_root_pid(context: &McpContext, session_id: &str) -> Option<u32> {
    let session_id = SessionId(Uuid::parse_str(session_id).ok()?);
    context.app.pool.pid_of(&session_id)
}

fn peer_pid_of(stream: &LocalSocketStream) -> Option<u32> {
    match stream.peer_creds() {
        #[cfg(windows)]
        Ok(creds) => creds.pid(),
        #[cfg(not(windows))]
        Ok(creds) => creds.pid().and_then(|pid| u32::try_from(pid).ok()),
        Err(err) => {
            warn!(%err, "failed to read MCP peer credentials");
            None
        }
    }
}

fn to_io_error(err: impl std::fmt::Display) -> io::Error {
    io::Error::other(err.to_string())
}

pub async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, bytes: &[u8]) -> io::Result<()> {
    let len = u32::try_from(bytes.len()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame exceeds u32 length limit"))?;
    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(bytes).await?;
    writer.flush().await
}

pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R, max_len: usize) -> io::Result<Vec<u8>> {
    let mut len_bytes = [0_u8; 4];
    reader.read_exact(&mut len_bytes).await?;
    let len = u32::from_be_bytes(len_bytes) as usize;
    if len > max_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame length {len} exceeds maximum {max_len}"),
        ));
    }
    let mut bytes = vec![0_u8; len];
    reader.read_exact(&mut bytes).await?;
    Ok(bytes)
}

fn is_descendant_of(peer_pid: u32, root_pid: u32) -> Option<bool> {
    #[cfg(windows)]
    {
        windows_descends_from(peer_pid, root_pid)
    }
    #[cfg(all(unix, any(target_os = "linux", target_os = "android")))]
    {
        linux_descends_from(peer_pid, root_pid)
    }
    #[cfg(not(any(windows, all(unix, any(target_os = "linux", target_os = "android")))))]
    {
        let _ = (peer_pid, root_pid);
        None
    }
}

#[cfg(all(unix, any(target_os = "linux", target_os = "android")))]
fn linux_descends_from(peer_pid: u32, root_pid: u32) -> Option<bool> {
    let mut current = peer_pid;
    loop {
        let stat_path = PathBuf::from(format!("/proc/{current}/stat"));
        let contents = std::fs::read_to_string(stat_path).ok()?;
        let end = contents.rfind(')')?;
        let rest = contents.get(end + 2..)?;
        let mut parts = rest.split_whitespace();
        let _state = parts.next()?;
        let parent_pid = parts.next()?.parse::<u32>().ok()?;
        if parent_pid == root_pid {
            return Some(true);
        }
        if parent_pid == 0 || parent_pid == current {
            return Some(false);
        }
        current = parent_pid;
    }
}

#[cfg(windows)]
fn windows_descends_from(peer_pid: u32, root_pid: u32) -> Option<bool> {
    let parents: HashMap<u32, u32> = crate::pty_pool::windows_process_tree::parent_pid_snapshot().ok()?.into_iter().collect();
    let mut current = peer_pid;
    loop {
        let parent = *parents.get(&current)?;
        if parent == root_pid {
            return Some(true);
        }
        if parent == 0 || parent == current {
            return Some(false);
        }
        current = parent;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn frame_roundtrip() {
        let payload = br#"{\"ok\":true}"#.to_vec();
        let (mut writer, mut reader) = duplex(128);

        let writer_task = tokio::spawn(async move { write_frame(&mut writer, &payload).await });
        let received = read_frame(&mut reader, 1024).await.expect("frame should round-trip");

        writer_task.await.expect("writer task should finish").expect("writer should succeed");
        assert_eq!(received, br#"{\"ok\":true}"#);
    }

    #[test]
    fn effective_config_respects_session_mode() {
        let mut config = AppConfigMcp {
            enabled: true,
            ..Default::default()
        };
        config.per_session.insert(
            "session-1".to_owned(),
            crate::mcp::types::McpSessionConfig {
                mode: McpSessionMode::ReadOnly,
            },
        );

        let effective = effective_config(&config, "session-1");
        let create = effective
            .tools
            .iter()
            .find(|tool| tool.id == McpToolName::CreateWorktree.as_id())
            .expect("tool present");
        let status = effective
            .tools
            .iter()
            .find(|tool| tool.id == McpToolName::WorkspaceStatus.as_id())
            .expect("tool present");

        assert!(!create.enabled);
        assert!(status.enabled);
    }
}
