use serde::{Deserialize, Serialize};

// --- Server → Relay messages ---

#[derive(Debug, Deserialize)]
pub struct RelayRegister {
    pub server_id: String,
    pub server_name: String,
    pub version: String,
    pub bridge_token: String,
}

#[derive(Debug, Deserialize)]
pub struct RelayResponse {
    pub id: String,
    pub status: u16,
    #[serde(default)]
    pub headers: serde_json::Map<String, serde_json::Value>,
    pub body: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RelayEvent {
    pub client_id: String,
    pub event: serde_json::Value,
}

#[derive(Debug, Deserialize)]
pub struct RelayStreamStart {
    pub id: String,
    pub status: u16,
    #[serde(default)]
    pub headers: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub content_length: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct RelayStreamEnd {
    pub id: String,
}

// --- Relay → Server messages ---

#[derive(Debug, Serialize)]
pub struct RelayRegistered {
    #[serde(rename = "type")]
    pub msg_type: &'static str,
    pub ok: bool,
    pub server_id: String,
}

#[derive(Debug, Serialize)]
pub struct RelayRequest {
    #[serde(rename = "type")]
    pub msg_type: &'static str,
    pub id: String,
    pub method: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RelayClientConnected {
    #[serde(rename = "type")]
    pub msg_type: &'static str,
    pub client_id: String,
}

#[derive(Debug, Serialize)]
pub struct RelayClientDisconnected {
    #[serde(rename = "type")]
    pub msg_type: &'static str,
    pub client_id: String,
}

#[derive(Debug, Serialize)]
pub struct RelayStreamRequest {
    #[serde(rename = "type")]
    pub msg_type: &'static str,
    pub id: String,
    pub stream_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<String>,
}

pub fn parse_message_type(text: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    v.get("type")?.as_str().map(|s| s.to_string())
}
