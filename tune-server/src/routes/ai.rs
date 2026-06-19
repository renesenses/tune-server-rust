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
    let zone_id = body.zone_id.unwrap_or(1);

    info!(zone_id, message = %body.message, "ai_query");

    let api_key = settings
        .get("anthropic_api_key")
        .ok()
        .flatten()
        .unwrap_or_default();

    if api_key.is_empty() {
        return local_ai_query(state, &body.message, zone_id).await;
    }

    let model = settings
        .get("anthropic_model")
        .ok()
        .flatten()
        .unwrap_or_default();

    let client = AnthropicClient::new(api_key, model);
    let tools = all_tools();
    let mut executor = ToolExecutor::with_backend(
        state.backend.clone(),
        state.orchestrator.clone(),
        state.playback.clone(),
        zone_id,
    );

    let mut messages = vec![Message {
        role: "user".into(),
        content: MessageContent::Text(body.message.clone()),
    }];

    let mut actions: Vec<Value> = Vec::new();

    for round in 0..MAX_TOOL_ROUNDS {
        let response = client
            .chat(SYSTEM_PROMPT, messages.clone(), &tools)
            .await
            .map_err(|e| AppError::internal(format!("Claude API error: {e}")))?;

        let tool_uses: Vec<_> = response
            .content
            .iter()
            .filter(|b| matches!(b, ContentBlock::ToolUse { .. }))
            .collect();

        if tool_uses.is_empty() {
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

        let assistant_blocks: Vec<ContentBlock> = response.content.clone();
        messages.push(Message {
            role: "assistant".into(),
            content: MessageContent::Blocks(assistant_blocks),
        });

        let mut tool_results = Vec::new();
        for block in &response.content {
            if let ContentBlock::ToolUse { id, name, input } = block {
                let result = executor.execute(name, input.clone()).await;
                actions.push(json!({ "tool": name, "input": input, "result": result }));
                tool_results.push(ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content: result.to_string(),
                });
            }
        }

        messages.push(Message {
            role: "user".into(),
            content: MessageContent::Blocks(tool_results),
        });

        if round == MAX_TOOL_ROUNDS - 1 {
            warn!("ai_max_rounds_reached");
        }
    }

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

// ---------------------------------------------------------------------------
// Local AI — intent parser, no API key needed
// ---------------------------------------------------------------------------

async fn local_ai_query(
    state: AppState,
    message: &str,
    zone_id: i64,
) -> Result<Json<Value>, AppError> {
    let mut executor = ToolExecutor::with_backend(
        state.backend.clone(),
        state.orchestrator.clone(),
        state.playback.clone(),
        zone_id,
    );

    let msg = message.to_lowercase();
    let mut actions: Vec<Value> = Vec::new();

    // --- Transport controls (en/fr/de/es/it/zh/ko/ja) ---
    if matches_any(
        &msg,
        &[
            "pause",
            "stop",
            "arrête",
            "arrete",
            "stoppe", // fr
            "anhalten",
            "stopp", // de
            "parar",
            "detener",
            "para ", // es
            "ferma",
            "fermati", // it
            "暂停",
            "停止", // zh
            "일시정지",
            "멈춰",
            "정지", // ko
            "一時停止",
            "止めて",
            "ストップ", // ja
        ],
    ) {
        let result = executor.execute("pause", json!({})).await;
        actions.push(json!({ "tool": "pause", "result": result }));
        return Ok(Json(json!({
            "reply": "Lecture en pause.",
            "actions": actions,
            "zone_id": executor.zone_id(),
        })));
    }
    let resume_words = [
        "resume",
        "reprend",
        "play",
        "lecture",
        "continue",
        "fortsetzen",
        "weiter", // de
        "reanudar",
        "continuar", // es
        "riprendi",
        "continua", // it
        "继续",
        "播放", // zh
        "재생",
        "계속", // ko
        "再生",
        "再開", // ja
    ];
    if matches_any(&msg, &resume_words) && !msg.contains(' ')
        || msg == "play"
        || msg == "lecture"
        || msg == "播放"
        || msg == "再生"
        || msg == "재생"
    {
        let result = executor.execute("resume", json!({})).await;
        actions.push(json!({ "tool": "resume", "result": result }));
        return Ok(Json(json!({
            "reply": "Lecture reprise.",
            "actions": actions,
            "zone_id": executor.zone_id(),
        })));
    }
    if matches_any(
        &msg,
        &[
            "next",
            "suivant",
            "suivante",
            "piste suivante",
            "skip",
            "nächster",
            "nächstes",
            "weiter",
            "überspringen", // de
            "siguiente",
            "saltar", // es
            "prossimo",
            "prossima",
            "salta", // it
            "下一首",
            "跳过", // zh
            "다음",
            "다음곡",
            "건너뛰기", // ko
            "次の曲",
            "スキップ",
            "次へ", // ja
        ],
    ) {
        let result = executor.execute("next_track", json!({})).await;
        actions.push(json!({ "tool": "next_track", "result": result }));
        let title = result["track"].as_str().unwrap_or("piste suivante");
        return Ok(Json(json!({
            "reply": format!("Piste suivante : {title}"),
            "actions": actions,
            "zone_id": executor.zone_id(),
        })));
    }

    // --- Volume ---
    if let Some(vol) = parse_volume(&msg) {
        let result = executor
            .execute("set_volume", json!({ "volume": vol }))
            .await;
        actions.push(json!({ "tool": "set_volume", "result": result }));
        return Ok(Json(json!({
            "reply": format!("Volume réglé à {}%.", (vol * 100.0) as i32),
            "actions": actions,
            "zone_id": executor.zone_id(),
        })));
    }

    // --- Now playing ---
    if matches_any(
        &msg,
        &[
            "qu'est-ce qui joue",
            "qu'est ce qui joue",
            "c'est quoi", // fr
            "en cours",
            "quel morceau",
            "quelle chanson",
            "quoi joue",
            "what's playing",
            "now playing",
            "what is playing", // en
            "was läuft",
            "was spielt",
            "welcher song", // de
            "qué suena",
            "qué está sonando",
            "qué canción", // es
            "cosa suona",
            "cosa sta suonando",
            "che canzone", // it
            "现在播放",
            "在放什么",
            "什么歌", // zh
            "지금 뭐 재생",
            "무슨 노래",
            "지금 곡", // ko
            "今何を再生",
            "何の曲",
            "今流れてる", // ja
        ],
    ) {
        let result = executor.execute("now_playing", json!({})).await;
        actions.push(json!({ "tool": "now_playing", "result": result }));
        let reply = if let Some(track) = result["track"].as_str() {
            let artist = result["artist"].as_str().unwrap_or("Inconnu");
            format!("En cours : {track} — {artist}")
        } else {
            "Rien ne joue actuellement.".into()
        };
        return Ok(Json(json!({
            "reply": reply,
            "actions": actions,
            "zone_id": executor.zone_id(),
        })));
    }

    // --- Play intent: "joue/mets/lance/play [query]" ---
    let play_query = extract_play_query(&msg);
    if let Some(query) = play_query {
        // Search library first
        let search_result = executor
            .execute("search_library", json!({ "query": query }))
            .await;

        let albums = search_result["albums"].as_array();
        let tracks = search_result["tracks"].as_array();

        // Prefer album match
        if let Some(albums) = albums {
            if let Some(first) = albums.first() {
                if let Some(title) = first["title"].as_str() {
                    let result = executor
                        .execute("play_album", json!({ "album_name": title }))
                        .await;
                    actions.push(json!({ "tool": "play_album", "input": { "album_name": title }, "result": result }));
                    let artist = result["artist"].as_str().unwrap_or("");
                    let count = result["track_count"].as_i64().unwrap_or(0);
                    return Ok(Json(json!({
                        "reply": format!("▶ {title} — {artist} ({count} pistes)"),
                        "actions": actions,
                        "zone_id": executor.zone_id(),
                    })));
                }
            }
        }

        // Fall back to track match
        if let Some(tracks) = tracks {
            if let Some(first) = tracks.first() {
                if let Some(title) = first["title"].as_str() {
                    let result = executor
                        .execute("play_track", json!({ "track_name": title }))
                        .await;
                    actions.push(json!({ "tool": "play_track", "input": { "track_name": title }, "result": result }));
                    let artist = result["artist"].as_str().unwrap_or("");
                    return Ok(Json(json!({
                        "reply": format!("▶ {title} — {artist}"),
                        "actions": actions,
                        "zone_id": executor.zone_id(),
                    })));
                }
            }
        }

        return Ok(Json(json!({
            "reply": format!("Je n'ai rien trouvé pour « {query} » dans la bibliothèque."),
            "actions": actions,
            "zone_id": executor.zone_id(),
        })));
    }

    // --- Search intent ---
    let search_query = extract_search_query(&msg);
    if let Some(query) = search_query {
        let result = executor
            .execute("search_library", json!({ "query": query }))
            .await;
        actions.push(
            json!({ "tool": "search_library", "input": { "query": query }, "result": result }),
        );

        let n_artists = result["artists"].as_array().map(|a| a.len()).unwrap_or(0);
        let n_albums = result["albums"].as_array().map(|a| a.len()).unwrap_or(0);
        let n_tracks = result["tracks"].as_array().map(|a| a.len()).unwrap_or(0);

        let reply = if n_artists + n_albums + n_tracks == 0 {
            format!("Aucun résultat pour « {query} ».")
        } else {
            let mut parts = Vec::new();
            if n_artists > 0 {
                parts.push(format!("{n_artists} artiste(s)"));
            }
            if n_albums > 0 {
                parts.push(format!("{n_albums} album(s)"));
            }
            if n_tracks > 0 {
                parts.push(format!("{n_tracks} piste(s)"));
            }
            format!("Résultats pour « {query} » : {}", parts.join(", "))
        };

        return Ok(Json(json!({
            "reply": reply,
            "actions": actions,
            "zone_id": executor.zone_id(),
        })));
    }

    // --- Zone selection ---
    if matches_any(
        &msg,
        &[
            "zone",
            "zones",
            "list zones",
            "les zones",
            "zonen",  // de
            "zonas",  // es
            "ゾーン", // ja
            "영역",   // ko
            "区域",   // zh
        ],
    ) {
        let result = executor.execute("list_zones", json!({})).await;
        actions.push(json!({ "tool": "list_zones", "result": result }));
        let zones = result["zones"].as_array();
        let reply = match zones {
            Some(z) => {
                let names: Vec<&str> = z.iter().filter_map(|v| v["name"].as_str()).collect();
                format!("Zones disponibles : {}", names.join(", "))
            }
            None => "Aucune zone trouvée.".into(),
        };
        return Ok(Json(json!({
            "reply": reply,
            "actions": actions,
            "zone_id": executor.zone_id(),
        })));
    }

    // --- Fallback: treat the whole message as a search, then play ---
    let result = executor
        .execute("search_library", json!({ "query": message }))
        .await;
    let albums = result["albums"].as_array();
    let tracks = result["tracks"].as_array();

    if let Some(albums) = albums {
        if let Some(first) = albums.first() {
            if let Some(title) = first["title"].as_str() {
                let play = executor
                    .execute("play_album", json!({ "album_name": title }))
                    .await;
                actions.push(json!({ "tool": "play_album", "input": { "album_name": title }, "result": play }));
                let artist = play["artist"].as_str().unwrap_or("");
                return Ok(Json(json!({
                    "reply": format!("▶ {title} — {artist}"),
                    "actions": actions,
                    "zone_id": executor.zone_id(),
                })));
            }
        }
    }
    if let Some(tracks) = tracks {
        if let Some(first) = tracks.first() {
            if let Some(title) = first["title"].as_str() {
                let play = executor
                    .execute("play_track", json!({ "track_name": title }))
                    .await;
                actions.push(json!({ "tool": "play_track", "input": { "track_name": title }, "result": play }));
                let artist = play["artist"].as_str().unwrap_or("");
                return Ok(Json(json!({
                    "reply": format!("▶ {title} — {artist}"),
                    "actions": actions,
                    "zone_id": executor.zone_id(),
                })));
            }
        }
    }

    Ok(Json(json!({
        "reply": format!("Désolé, je n'ai pas compris « {} ». Essayez « joue [artiste/album] » ou « cherche [terme] ».", message),
        "actions": actions,
        "zone_id": executor.zone_id(),
    })))
}

// ---------------------------------------------------------------------------
// Intent parsing helpers
// ---------------------------------------------------------------------------

fn matches_any(msg: &str, patterns: &[&str]) -> bool {
    patterns.iter().any(|p| msg.contains(p))
}

fn extract_play_query(msg: &str) -> Option<&str> {
    let prefixes = [
        // en
        "play ",
        "listen to ",
        // fr
        "joue ",
        "jouer ",
        "joue-moi ",
        "joue moi ",
        "mets ",
        "mets-moi ",
        "mets moi ",
        "mettre ",
        "lance ",
        "lancer ",
        "écouter ",
        "ecouter ",
        "fais tourner ",
        "balance ",
        // de
        "spiele ",
        "spiel ",
        "höre ",
        "hör ",
        "abspielen ",
        "starte ",
        // es
        "pon ",
        "reproduce ",
        "escucha ",
        "ponme ",
        // it
        "riproduci ",
        "ascolta ",
        "metti ",
        "suona ",
        // zh
        "播放 ",
        "听 ",
        "放 ",
        // ko
        "재생 ",
        "틀어 ",
        "들려줘 ",
        // ja
        "再生 ",
        "かけて ",
        "聴かせて ",
        "流して ",
    ];
    for prefix in &prefixes {
        if let Some(rest) = msg.strip_prefix(prefix) {
            let rest = rest.trim();
            if !rest.is_empty() {
                // Strip trailing service hints: "sur qobuz", "on tidal", etc.
                let cleaned = strip_service_suffix(rest);
                return Some(cleaned);
            }
        }
    }
    None
}

fn extract_search_query(msg: &str) -> Option<&str> {
    let prefixes = [
        // en
        "search ",
        "find ",
        "look for ",
        "search for ",
        // fr
        "cherche ",
        "recherche ",
        "trouve ",
        "trouver ",
        "ai-je ",
        "est-ce que j'ai ",
        "quels ",
        "quelles ",
        "combien ",
        // de
        "suche ",
        "finde ",
        "such ",
        // es
        "busca ",
        "buscar ",
        "encuentra ",
        // it
        "cerca ",
        "cercare ",
        "trova ",
        // zh
        "搜索 ",
        "查找 ",
        "找 ",
        // ko
        "검색 ",
        "찾아 ",
        // ja
        "検索 ",
        "探して ",
        "調べて ",
    ];
    for prefix in &prefixes {
        if let Some(rest) = msg.strip_prefix(prefix) {
            let rest = rest.trim();
            if !rest.is_empty() {
                return Some(rest);
            }
        }
    }
    None
}

fn strip_service_suffix(s: &str) -> &str {
    let suffixes = [
        // fr
        " sur qobuz",
        " sur tidal",
        " sur deezer",
        " sur spotify",
        " sur youtube",
        " de qobuz",
        " de tidal",
        // en
        " on qobuz",
        " on tidal",
        " on deezer",
        " on spotify",
        " on youtube",
        " from qobuz",
        " from tidal",
        // de
        " auf qobuz",
        " auf tidal",
        " auf deezer",
        " auf spotify",
        " auf youtube",
        " von qobuz",
        " von tidal",
        // es
        " en qobuz",
        " en tidal",
        " en deezer",
        " en spotify",
        " en youtube",
        // it
        " su qobuz",
        " su tidal",
        " su deezer",
        " su spotify",
        " su youtube",
        // zh/ko/ja — service names stay latin
        " qobuz",
        " tidal",
    ];
    for suffix in &suffixes {
        if let Some(stripped) = s.strip_suffix(suffix) {
            return stripped.trim();
        }
    }
    s
}

fn parse_volume(msg: &str) -> Option<f64> {
    if !msg.contains("volume") && !msg.contains("vol ")
        && !msg.contains("lautstärke") && !msg.contains("lautstarke")   // de
        && !msg.contains("volumen")                                      // es
        && !msg.contains("音量")                                          // zh/ja
        && !msg.contains("볼륨")
    // ko
    {
        return None;
    }
    for word in msg.split(|c: char| !c.is_ascii_digit() && c != '.') {
        if let Ok(v) = word.parse::<f64>() {
            if v >= 0.0 && v <= 100.0 {
                return Some(v / 100.0);
            }
        }
    }
    None
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
