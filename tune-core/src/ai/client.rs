use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{debug, warn};

const API_URL: &str = "https://api.anthropic.com/v1/messages";
const API_VERSION: &str = "2023-06-01";
const DEFAULT_MODEL: &str = "claude-sonnet-4-6";
const DEFAULT_MAX_TOKENS: u32 = 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: MessageContent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApiResponse {
    pub content: Vec<ContentBlock>,
    pub stop_reason: Option<String>,
    pub model: Option<String>,
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Usage {
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
struct ApiError {
    #[allow(dead_code)]
    #[serde(rename = "type")]
    error_type: Option<String>,
    error: Option<ApiErrorDetail>,
}

#[derive(Debug, Clone, Deserialize)]
struct ApiErrorDetail {
    message: Option<String>,
}

pub struct AnthropicClient {
    api_key: String,
    model: String,
    max_tokens: u32,
}

impl AnthropicClient {
    pub fn new(api_key: String, model: String) -> Self {
        Self {
            api_key,
            model: if model.is_empty() {
                DEFAULT_MODEL.to_string()
            } else {
                model
            },
            max_tokens: DEFAULT_MAX_TOKENS,
        }
    }

    pub async fn chat(
        &self,
        system: &str,
        messages: Vec<Message>,
        tools: &[Tool],
    ) -> Result<ApiResponse, String> {
        let client = crate::http::client::shared();

        let mut body = serde_json::json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "system": system,
            "messages": messages,
        });

        if !tools.is_empty() {
            body.as_object_mut()
                .unwrap()
                .insert("tools".into(), serde_json::to_value(tools).unwrap());
        }

        debug!(model = %self.model, messages = messages.len(), tools = tools.len(), "anthropic_request");

        let resp = client
            .post(API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .header("Content-Type", "application/json")
            .timeout(std::time::Duration::from_secs(60))
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("anthropic request failed: {e}"))?;

        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| format!("failed to read anthropic response: {e}"))?;

        if !status.is_success() {
            warn!(status = %status, body = %text, "anthropic_api_error");
            if let Ok(err) = serde_json::from_str::<ApiError>(&text) {
                let msg = err
                    .error
                    .and_then(|e| e.message)
                    .unwrap_or_else(|| format!("API error {status}"));
                return Err(msg);
            }
            return Err(format!("Anthropic API error {status}: {text}"));
        }

        let response: ApiResponse =
            serde_json::from_str(&text).map_err(|e| format!("failed to parse response: {e}"))?;

        debug!(
            stop_reason = ?response.stop_reason,
            content_blocks = response.content.len(),
            "anthropic_response"
        );

        Ok(response)
    }
}
