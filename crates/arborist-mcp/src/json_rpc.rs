use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const JSONRPC_VERSION: &str = "2.0";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum IncomingMessage {
    Request(JsonRpcRequest),
    Notification(JsonRpcNotification),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: Value,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl JsonRpcNotification {
    #[must_use]
    pub fn new(method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            method: method.into(),
            params,
        }
    }
}

impl JsonRpcResponse {
    #[must_use]
    pub fn success(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id,
            result: Some(result),
            error: None,
        }
    }

    #[must_use]
    pub fn failure(id: Value, error: JsonRpcError) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

impl JsonRpcError {
    #[must_use]
    pub fn new(code: i64, message: impl Into<String>, data: Option<Value>) -> Self {
        Self {
            code,
            message: message.into(),
            data,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{JsonRpcError, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, JSONRPC_VERSION};

    #[test]
    fn request_round_trips_through_serde() {
        let request = JsonRpcRequest {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id: json!(1),
            method: "tools/list".to_owned(),
            params: Some(json!({ "cursor": null })),
        };

        let encoded = serde_json::to_string(&request).expect("request should serialize");
        let decoded: JsonRpcRequest = serde_json::from_str(&encoded).expect("request should deserialize");
        assert_eq!(decoded, request);
    }

    #[test]
    fn response_round_trips_through_serde() {
        let response = JsonRpcResponse::success(json!("req-1"), json!({ "tools": [] }));

        let encoded = serde_json::to_string(&response).expect("response should serialize");
        let decoded: JsonRpcResponse = serde_json::from_str(&encoded).expect("response should deserialize");
        assert_eq!(decoded, response);
    }

    #[test]
    fn notification_round_trips_through_serde() {
        let notification = JsonRpcNotification::new("notifications/mcp/activity", Some(json!({ "phase": "running" })));

        let encoded = serde_json::to_string(&notification).expect("notification should serialize");
        let decoded: JsonRpcNotification = serde_json::from_str(&encoded).expect("notification should deserialize");
        assert_eq!(decoded, notification);
    }

    #[test]
    fn error_data_preserves_mcp_error_shape() {
        let mcp_error = json!({
            "code": "confirmation-required",
            "message": "Approve the request in Arborist",
            "recoverable": true,
            "userAction": "Approve the request in Arborist",
            "retryAfterMs": 60000,
            "budgetRemaining": 1,
            "auditId": "audit-123",
            "disabledBy": "tool"
        });
        let response = JsonRpcResponse::failure(
            json!(7),
            JsonRpcError::new(-32603, "Approve the request in Arborist", Some(mcp_error.clone())),
        );

        let encoded = serde_json::to_string(&response).expect("error response should serialize");
        let decoded: JsonRpcResponse = serde_json::from_str(&encoded).expect("error response should deserialize");
        let error = decoded.error.expect("error payload should be present");
        assert_eq!(error.data, Some(mcp_error));
    }
}
