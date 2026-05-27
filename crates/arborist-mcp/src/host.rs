use std::{
    collections::HashMap,
    io,
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use interprocess::local_socket::{tokio::prelude::*, tokio::Stream as LocalSocketStream, GenericFilePath};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::{
    io::split,
    sync::{mpsc, oneshot, Mutex},
};

use crate::protocol::{read_frame, write_frame};

const MAX_FRAME_LEN: usize = 1024 * 1024;

type PendingMap = Arc<Mutex<HashMap<String, oneshot::Sender<Result<Value, HostCallError>>>>>;

pub struct HostConnection {
    pub client: HostClient,
    pub activity_rx: mpsc::Receiver<Value>,
}

#[derive(Clone)]
pub struct HostClient {
    outbound_tx: mpsc::Sender<Vec<u8>>,
    pending: PendingMap,
    next_id: Arc<AtomicU64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpError {
    pub code: String,
    pub message: String,
    pub recoverable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_action: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_remaining: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audit_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_by: Option<DisabledBy>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DisabledBy {
    Global,
    Tool,
    Session,
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
    pub mcp: McpError,
}

#[derive(Debug, Clone)]
pub enum HostCallError {
    Host(HostErrorEnvelope),
    HostUnavailable(String),
    Protocol(String),
    Encode(String),
}

#[derive(Debug, Error)]
pub enum HostConnectError {
    #[error("invalid host socket name: {0}")]
    InvalidSocketName(io::Error),
    #[error("failed to connect to Arborist MCP host: {0}")]
    Connect(io::Error),
    #[error("failed to serialize host hello: {0}")]
    SerializeHello(serde_json::Error),
    #[error("failed to write host hello: {0}")]
    WriteHello(io::Error),
    #[error("failed to read host hello ack: {0}")]
    ReadHello(io::Error),
    #[error("failed to parse host hello ack: {0}")]
    ParseHello(serde_json::Error),
    #[error("{0}")]
    Rejected(String),
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct HelloFrame<'a> {
    protocol: &'a str,
    token: &'a str,
    session_id: &'a str,
    pid: u32,
}

#[derive(Debug, Deserialize)]
struct HelloAck {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum OutboundFrame {
    Request {
        id: String,
        method: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        params: Option<Value>,
    },
    Notification {
        method: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        params: Option<Value>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum InboundFrame {
    Response {
        id: String,
        ok: bool,
        #[serde(default)]
        result: Option<Value>,
        #[serde(default)]
        error: Option<HostErrorEnvelope>,
    },
    Activity {
        #[serde(default)]
        params: Option<Value>,
    },
}

pub async fn connect(socket: &str, token: &str, session_id: &str) -> Result<HostConnection, HostConnectError> {
    let socket_name = Path::new(socket)
        .to_fs_name::<GenericFilePath>()
        .map_err(HostConnectError::InvalidSocketName)?;
    let mut stream = LocalSocketStream::connect(socket_name).await.map_err(HostConnectError::Connect)?;

    let hello = HelloFrame {
        protocol: "arborist-mcp/1",
        token,
        session_id,
        pid: std::process::id(),
    };
    let hello_bytes = serde_json::to_vec(&hello).map_err(HostConnectError::SerializeHello)?;
    write_frame(&mut stream, &hello_bytes).await.map_err(HostConnectError::WriteHello)?;

    let ack_bytes = read_frame(&mut stream, MAX_FRAME_LEN).await.map_err(HostConnectError::ReadHello)?;
    let ack: HelloAck = serde_json::from_slice(&ack_bytes).map_err(HostConnectError::ParseHello)?;
    if !ack.ok {
        return Err(HostConnectError::Rejected(
            ack.error.unwrap_or_else(|| "host rejected sidecar handshake".to_owned()),
        ));
    }

    let (reader, writer) = split(stream);
    let (outbound_tx, outbound_rx) = mpsc::channel(64);
    let (activity_tx, activity_rx) = mpsc::channel(64);
    let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));

    tokio::spawn(writer_task(writer, outbound_rx, Arc::clone(&pending)));
    tokio::spawn(reader_task(reader, Arc::clone(&pending), activity_tx));

    Ok(HostConnection {
        client: HostClient {
            outbound_tx,
            pending,
            next_id: Arc::new(AtomicU64::new(1)),
        },
        activity_rx,
    })
}

impl HostClient {
    pub async fn request(&self, method: &str, params: Option<Value>) -> Result<Value, HostCallError> {
        let request_id = format!("req-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let payload = serde_json::to_vec(&OutboundFrame::Request {
            id: request_id.clone(),
            method: method.to_owned(),
            params,
        })
        .map_err(|error| HostCallError::Encode(format!("failed to encode host request: {error}")))?;

        let (response_tx, response_rx) = oneshot::channel();
        self.pending.lock().await.insert(request_id.clone(), response_tx);

        if let Err(error) = self.outbound_tx.send(payload).await {
            self.pending.lock().await.remove(&request_id);
            return Err(HostCallError::HostUnavailable(format!("failed to queue host request: {error}")));
        }

        match response_rx.await {
            Ok(result) => result,
            Err(_) => Err(HostCallError::HostUnavailable(
                "host connection closed while waiting for a response".to_owned(),
            )),
        }
    }

    pub async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), HostCallError> {
        let payload = serde_json::to_vec(&OutboundFrame::Notification {
            method: method.to_owned(),
            params,
        })
        .map_err(|error| HostCallError::Encode(format!("failed to encode host notification: {error}")))?;

        self.outbound_tx
            .send(payload)
            .await
            .map_err(|error| HostCallError::HostUnavailable(format!("failed to queue host notification: {error}")))
    }
}

impl HostCallError {
    #[must_use]
    pub fn json_rpc_code(&self) -> i64 {
        match self {
            Self::Host(error) => error.json_rpc_code(),
            Self::HostUnavailable(_) | Self::Protocol(_) | Self::Encode(_) => -32603,
        }
    }

    #[must_use]
    pub fn message(&self) -> String {
        self.as_mcp_error().message
    }

    #[must_use]
    pub fn data(&self) -> Option<Value> {
        serde_json::to_value(self.as_mcp_error()).ok()
    }

    fn as_mcp_error(&self) -> McpError {
        match self {
            Self::Host(error) => error.mcp.clone(),
            Self::HostUnavailable(message) => McpError::host_unavailable(message.clone()),
            Self::Protocol(message) | Self::Encode(message) => McpError::internal(message.clone()),
        }
    }
}

impl HostErrorEnvelope {
    #[must_use]
    pub fn json_rpc_code(&self) -> i64 {
        match self.kind {
            HostErrorKind::InvalidParams => -32602,
            HostErrorKind::Internal => -32603,
        }
    }
}

impl McpError {
    fn host_unavailable(message: String) -> Self {
        Self {
            code: "host-unavailable".to_owned(),
            message,
            recoverable: true,
            user_action: Some("Keep Arborist running and retry the MCP request".to_owned()),
            retry_after_ms: None,
            budget_remaining: None,
            audit_id: None,
            disabled_by: None,
        }
    }

    fn internal(message: String) -> Self {
        Self {
            code: "internal".to_owned(),
            message,
            recoverable: false,
            user_action: None,
            retry_after_ms: None,
            budget_remaining: None,
            audit_id: None,
            disabled_by: None,
        }
    }
}

async fn writer_task<W>(mut writer: W, mut outbound_rx: mpsc::Receiver<Vec<u8>>, pending: PendingMap)
where
    W: tokio::io::AsyncWrite + Unpin,
{
    while let Some(frame) = outbound_rx.recv().await {
        if let Err(error) = write_frame(&mut writer, &frame).await {
            fail_pending(&pending, HostCallError::HostUnavailable(format!("failed to write host frame: {error}"))).await;
            return;
        }
    }
}

async fn reader_task<R>(mut reader: R, pending: PendingMap, activity_tx: mpsc::Sender<Value>)
where
    R: tokio::io::AsyncRead + Unpin,
{
    loop {
        let frame = match read_frame(&mut reader, MAX_FRAME_LEN).await {
            Ok(frame) => frame,
            Err(error) => {
                fail_pending(&pending, HostCallError::HostUnavailable(format!("failed to read host frame: {error}"))).await;
                return;
            }
        };

        let inbound: InboundFrame = match serde_json::from_slice(&frame) {
            Ok(frame) => frame,
            Err(error) => {
                fail_pending(&pending, HostCallError::Protocol(format!("received malformed host frame: {error}"))).await;
                return;
            }
        };

        match inbound {
            InboundFrame::Response { id, ok, result, error } => {
                let responder = {
                    let mut pending_guard = pending.lock().await;
                    pending_guard.remove(&id)
                };
                let Some(responder) = responder else {
                    tracing::warn!(request_id = %id, "received host response for an unknown request");
                    continue;
                };

                let response = if ok {
                    Ok(result.unwrap_or(Value::Null))
                } else {
                    Err(match error {
                        Some(error) => HostCallError::Host(error),
                        None => HostCallError::Protocol(format!("host response {id} reported failure without an MCP error payload")),
                    })
                };
                let _ = responder.send(response);
            }
            InboundFrame::Activity { params } => {
                if activity_tx.send(params.unwrap_or(Value::Null)).await.is_err() {
                    tracing::debug!("activity receiver dropped; discarding host activity frame");
                }
            }
        }
    }
}

async fn fail_pending(pending: &PendingMap, error: HostCallError) {
    let responders = {
        let mut pending_guard = pending.lock().await;
        pending_guard.drain().map(|(_, responder)| responder).collect::<Vec<_>>()
    };

    for responder in responders {
        let _ = responder.send(Err(error.clone()));
    }
}
