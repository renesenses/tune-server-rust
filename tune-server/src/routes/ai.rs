use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{info, warn};

use tune_core::ai::client::{AnthropicClient, ContentBlock, Message, MessageContent};
use tune_core::ai::executor::ToolExecutor;
use tune_core::ai::tools::all_tools;
use tune_core::db::settings_repo::SettingsRepo;

use crate::error::AppError;
use crate::state::AppState;

const SYSTEM_PROMPT: &str = "\
Tu es l'assistant musical de Tune Server, un serveur de musique audiophile. \
Tu controles la lecture de musique, la recherche dans la bibliotheque locale, \
la gestion de la file d'attente et des zones de lecture. \
Reponds toujours en francais sauf si l'utilisateur parle dans une autre langue. \
Sois concis et naturel. Quand tu lances de la musique, confirme ce que tu joues. \
Si tu ne trouves pas ce que l'utilisateur demande, propose des alternatives \
basees sur les resultats de recherche.";

/// Maximum number of tool-calling round-trips before we stop.
const MAX_TOOL_ROUNDS: usize = 5;

#[derive(Deserialize)]
struct AiQuery {
    message: String,
    zone_id: Option<i64>,
}

pub fn router() -> Router<AppState> {
    Router::new().route("/query", post(ai_query))
}

async fn ai_query(
    State(state): State<AppState>,
    Json(body): Json<AiQuery>,
) -> Result<Json<Value>, AppError> {
    let settings = SettingsRepo::with_backend(state.backend.clone());

    let api_key = settings
        .get("anthropic_api_key")
        .ok()
        .flatten()
        .unwrap_or_default();
    if api_key.is_empty() {
        return Err(AppError::bad_request(
            "AI assistant not configured: set anthropic_api_key in settings",
        ));
    }

    let model = settings
        .get("anthropic_model")
        .ok()
        .flatten()
        .unwrap_or_default();

    let zone_id = body.zone_id.unwrap_or(1);

    info!(zone_id, message = %body.message, "ai_query");

    let client = AnthropicClient::new(api_key, model);
    let tools = all_tools();
    let mut executor = ToolExecutor::new(
        state.db.clone(),
        state.orchestrator.clone(),
        state.playback.clone(),
        zone_id,
    );

    // Build conversation with the user's message
    let mut messages = vec![Message {
        role: "user".into(),
        content: MessageContent::Text(body.message.clone()),
    }];

    let mut actions: Vec<Value> = Vec::new();

    // Tool-calling loop: Claude may request multiple tools before giving a final answer
    for round in 0..MAX_TOOL_ROUNDS {
        let response = client
            .chat(SYSTEM_PROMPT, messages.clone(), &tools)
            .await
            .map_err(|e| AppError::internal(format!("Claude API error: {e}")))?;

        // Check if there are any tool_use blocks
        let tool_uses: Vec<_> = response
            .content
            .iter()
            .filter(|b| matches!(b, ContentBlock::ToolUse { .. }))
            .collect();

        if tool_uses.is_empty() {
            // No tool calls — extract the text response and return
            let reply = extract_text(&response.content);
            info!(
                round,
                reply_len = reply.len(),
                actions = actions.len(),
                "ai_done"
            );
            return Ok(Json(json!({
                "reply": reply,
                "actions": actions,
                "zone_id": executor.zone_id(),
            })));
        }

        // There are tool calls — we need to execute them and loop
        // First, add Claude's response (with tool_use blocks) to the conversation
        let assistant_blocks: Vec<ContentBlock> = response.content.clone();
        messages.push(Message {
            role: "assistant".into(),
            content: MessageContent::Blocks(assistant_blocks),
        });

        // Execute each tool and build tool_result blocks
        let mut tool_results = Vec::new();
        for block in &response.content {
            if let ContentBlock::ToolUse { id, name, input } = block {
                let result = executor.execute(name, input.clone()).await;
                actions.push(json!({
                    "tool": name,
                    "input": input,
                    "result": result,
                }));
                tool_results.push(ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content: result.to_string(),
                });
            }
        }

        // Add tool results as a user message
        messages.push(Message {
            role: "user".into(),
            content: MessageContent::Blocks(tool_results),
        });

        // If this was the last round, we'll break and give whatever we have
        if round == MAX_TOOL_ROUNDS - 1 {
            warn!("ai_max_rounds_reached");
        }
    }

    // Fell through — do one last call without tools to get a text summary
    let final_response = client
        .chat(SYSTEM_PROMPT, messages, &[])
        .await
        .map_err(|e| AppError::internal(format!("Claude API error: {e}")))?;

    let reply = extract_text(&final_response.content);
    Ok(Json(json!({
        "reply": reply,
        "actions": actions,
        "zone_id": executor.zone_id(),
    })))
}

fn extract_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}
