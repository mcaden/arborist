mod auth;
mod host;
mod json_rpc;
mod protocol;

use std::{
    process::ExitCode,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use anyhow::{Context, Result};
use arborist_types::SessionId;
use json_rpc::{IncomingMessage, JsonRpcError, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, JSONRPC_VERSION};
use serde::Serialize;
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter},
    sync::mpsc,
};
use tracing::{debug, warn};
use tracing_subscriber::EnvFilter;

#[derive(Debug)]
enum SidecarError {
    Usage(String),
    HashMismatch,
    Fatal(anyhow::Error),
}

#[derive(Debug)]
struct StartupEnv {
    socket: String,
    token_hex: String,
    session_id: String,
    host_hash_hex: String,
}

#[tokio::main]
async fn main() -> ExitCode {
    init_tracing();

    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(SidecarError::Usage(message)) => {
            eprintln!("{message}");
            ExitCode::from(2)
        }
        Err(SidecarError::HashMismatch) => {
            eprintln!("sidecar hash mismatch");
            ExitCode::from(3)
        }
        Err(SidecarError::Fatal(error)) => {
            eprintln!("{error}");
            ExitCode::from(1)
        }
    }
}

async fn run() -> Result<(), SidecarError> {
    let startup = StartupEnv::from_env()?;
    let allow_mismatch = cfg!(debug_assertions) || cfg!(feature = "dev-allow-mismatch");

    match auth::verify_against(&startup.host_hash_hex, allow_mismatch) {
        Ok(()) => {}
        Err(auth::AuthError::HashMismatch { .. }) => return Err(SidecarError::HashMismatch),
        Err(error) => return Err(SidecarError::usage(error.to_string())),
    }

    let host::HostConnection { client, activity_rx } = host::connect(&startup.socket, &startup.token_hex, &startup.session_id)
        .await
        .context("failed to establish Arborist MCP host connection")
        .map_err(SidecarError::Fatal)?;

    let activity_enabled = Arc::new(AtomicBool::new(false));
    let (output_tx, output_rx) = mpsc::channel::<String>(64);
    let writer_task = tokio::spawn(stdout_writer(output_rx));
    let activity_task = tokio::spawn(forward_activity(activity_rx, output_tx.clone(), Arc::clone(&activity_enabled)));

    let mut stdin_lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = stdin_lines
        .next_line()
        .await
        .context("failed to read stdin")
        .map_err(SidecarError::Fatal)?
    {
        if line.trim().is_empty() {
            continue;
        }

        handle_client_message(&line, &client, &output_tx, activity_enabled.as_ref())
            .await
            .map_err(SidecarError::Fatal)?;
    }

    activity_task.abort();
    let _ = activity_task.await;
    drop(output_tx);

    let writer_result = writer_task.await.context("stdout writer task panicked").map_err(SidecarError::Fatal)?;
    writer_result.map_err(SidecarError::Fatal)?;
    Ok(())
}

async fn handle_client_message(line: &str, client: &host::HostClient, output_tx: &mpsc::Sender<String>, activity_enabled: &AtomicBool) -> Result<()> {
    let incoming = match serde_json::from_str::<IncomingMessage>(line) {
        Ok(incoming) => incoming,
        Err(error) => {
            let response = JsonRpcResponse::failure(Value::Null, JsonRpcError::new(-32700, format!("parse error: {error}"), None));
            return emit_json(output_tx, &response).await;
        }
    };

    match incoming {
        IncomingMessage::Request(request) => handle_request(request, client, output_tx, activity_enabled).await,
        IncomingMessage::Notification(notification) => handle_notification(notification, client, activity_enabled).await,
    }
}

async fn handle_request(
    request: JsonRpcRequest,
    client: &host::HostClient,
    output_tx: &mpsc::Sender<String>,
    activity_enabled: &AtomicBool,
) -> Result<()> {
    let JsonRpcRequest { jsonrpc, id, method, params } = request;

    if jsonrpc != JSONRPC_VERSION {
        return send_error(output_tx, id, -32600, "invalid request", None).await;
    }

    match method.as_str() {
        "initialize" => {
            activity_enabled.store(true, Ordering::Relaxed);
            let response = JsonRpcResponse::success(id, initialize_result());
            emit_json(output_tx, &response).await
        }
        "tools/list" | "tools/call" => {
            if let Some(error) = invalid_object_params(&method, params.as_ref()) {
                let response = JsonRpcResponse::failure(id, error);
                return emit_json(output_tx, &response).await;
            }

            let response = match client.request(&method, params).await {
                Ok(result) => JsonRpcResponse::success(id, result),
                Err(error) => JsonRpcResponse::failure(id, JsonRpcError::new(error.json_rpc_code(), error.message(), error.data())),
            };
            emit_json(output_tx, &response).await
        }
        _ => send_error(output_tx, id, -32601, format!("method not found: {method}"), None).await,
    }
}

async fn handle_notification(notification: JsonRpcNotification, client: &host::HostClient, activity_enabled: &AtomicBool) -> Result<()> {
    if notification.jsonrpc != JSONRPC_VERSION {
        debug!("ignoring non-2.0 JSON-RPC notification");
        return Ok(());
    }

    match notification.method.as_str() {
        "initialized" => {
            activity_enabled.store(true, Ordering::Relaxed);
            Ok(())
        }
        "notifications/cancelled" => {
            if invalid_object_params(&notification.method, notification.params.as_ref()).is_some() {
                debug!("ignoring notifications/cancelled with non-object params");
                return Ok(());
            }

            if let Err(error) = client.notify(&notification.method, notification.params).await {
                warn!(message = %error.message(), "failed to forward cancellation notification to host");
            }
            Ok(())
        }
        _ => {
            debug!(method = %notification.method, "ignoring unsupported JSON-RPC notification");
            Ok(())
        }
    }
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": "2025-03-26",
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": "arborist-mcp",
            "version": "0.1.0"
        }
    })
}

fn invalid_object_params(method: &str, params: Option<&Value>) -> Option<JsonRpcError> {
    match params {
        None | Some(Value::Object(_)) => None,
        Some(_) => Some(JsonRpcError::new(-32602, format!("{method} params must be a JSON object"), None)),
    }
}

async fn stdout_writer(mut output_rx: mpsc::Receiver<String>) -> Result<()> {
    let mut stdout = BufWriter::new(tokio::io::stdout());
    while let Some(line) = output_rx.recv().await {
        stdout.write_all(line.as_bytes()).await?;
        stdout.write_all(b"\n").await?;
    }
    stdout.flush().await?;
    Ok(())
}

async fn forward_activity(mut activity_rx: mpsc::Receiver<Value>, output_tx: mpsc::Sender<String>, activity_enabled: Arc<AtomicBool>) -> Result<()> {
    while let Some(activity) = activity_rx.recv().await {
        if !activity_enabled.load(Ordering::Relaxed) {
            continue;
        }

        let notification = JsonRpcNotification::new("notifications/mcp/activity", Some(activity));
        let payload = serde_json::to_string(&notification)?;
        if output_tx.send(payload).await.is_err() {
            break;
        }
    }
    Ok(())
}

async fn emit_json<T: Serialize>(output_tx: &mpsc::Sender<String>, payload: &T) -> Result<()> {
    let line = serde_json::to_string(payload)?;
    output_tx.send(line).await.context("stdout channel closed")?;
    Ok(())
}

async fn send_error(output_tx: &mpsc::Sender<String>, id: Value, code: i64, message: impl Into<String>, data: Option<Value>) -> Result<()> {
    let response = JsonRpcResponse::failure(id, JsonRpcError::new(code, message, data));
    emit_json(output_tx, &response).await
}

impl StartupEnv {
    fn from_env() -> Result<Self, SidecarError> {
        let socket = required_env("ARBORIST_MCP_SOCKET")?;
        let token_hex = required_env("ARBORIST_MCP_SOCKET_TOKEN")?;
        let session_id = required_env("ARBORIST_MCP_SESSION_ID")?;
        let host_hash_hex = required_env("ARBORIST_MCP_HOST_HASH_HEX")?;

        let token_bytes = hex::decode(&token_hex).map_err(|_| SidecarError::usage("ARBORIST_MCP_SOCKET_TOKEN must be valid hex"))?;
        if token_bytes.len() != 32 {
            return Err(SidecarError::usage(
                "ARBORIST_MCP_SOCKET_TOKEN must be a 32-byte token encoded as 64 hex characters",
            ));
        }

        serde_json::from_value::<SessionId>(Value::String(session_id.clone()))
            .map_err(|_| SidecarError::usage("ARBORIST_MCP_SESSION_ID must be a UUID string"))?;

        Ok(Self {
            socket,
            token_hex,
            session_id,
            host_hash_hex,
        })
    }
}

impl SidecarError {
    fn usage(message: impl Into<String>) -> Self {
        Self::Usage(message.into())
    }
}

fn required_env(name: &str) -> Result<String, SidecarError> {
    match std::env::var(name) {
        Ok(value) if !value.trim().is_empty() => Ok(value),
        _ => Err(SidecarError::usage(format!("missing required env var {name}"))),
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).try_init();
}
