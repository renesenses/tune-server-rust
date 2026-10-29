use axum::extract::{Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{info, warn};

#[cfg(not(feature = "assistant-ia"))]
use tune_core::ai::client::AnthropicClient;
use tune_core::ai::client::{ContentBlock, Message, MessageContent};
use tune_core::ai::executor::ToolExecutor;
use tune_core::ai::recommendations;
use tune_core::ai::tools::all_tools;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::license::Feature;

use crate::error::AppError;
use crate::state::AppState;

const SYSTEM_PROMPT: &str = "\
Tu es l'assistant musical de Tune Server, un serveur de musique audiophile. \
Tu controles la lecture de musique, la recherche dans la bibliotheque locale, \
la gestion de la file d'attente et des zones de lecture. \
Sois concis et naturel. Quand tu lances de la musique, confirme ce que tu joues. \
Si tu ne trouves pas ce que l'utilisateur demande, propose des alternatives \
basees sur les resultats de recherche.";

/// Language directive appended to the system prompt so the assistant answers in
/// the UI language (the `Accept-Language`-derived `lang`), not always French
/// (#1143). Naming the target language in English keeps the instruction reliable
/// across models; the "unless…" clause preserves the original nuance of letting
/// the user switch language mid-conversation.
fn language_directive(lang: &str) -> &'static str {
    match lang {
        "en" => "Reply to the user in English unless they clearly write in another language.",
        "de" => "Reply to the user in German unless they clearly write in another language.",
        "es" => "Reply to the user in Spanish unless they clearly write in another language.",
        "it" => "Reply to the user in Italian unless they clearly write in another language.",
        "zh" => "Reply to the user in Chinese unless they clearly write in another language.",
        "ja" => "Reply to the user in Japanese unless they clearly write in another language.",
        "ko" => "Reply to the user in Korean unless they clearly write in another language.",
        "ro" => "Reply to the user in Romanian unless they clearly write in another language.",
        "sv" => "Reply to the user in Swedish unless they clearly write in another language.",
        "hu" => "Reply to the user in Hungarian unless they clearly write in another language.",
        "vi" => "Reply to the user in Vietnamese unless they clearly write in another language.",
        "nb" => {
            "Reply to the user in Norwegian (Bokmål) unless they clearly write in another language."
        }
        "pl" => "Reply to the user in Polish unless they clearly write in another language.",
        "el" => "Reply to the user in Greek unless they clearly write in another language.",
        _ => "Reply to the user in French unless they clearly write in another language.",
    }
}

const MAX_TOOL_ROUNDS: usize = 5;

/// Ce que la bibliothèque envoie au modèle, borné (étape 3 du chantier
/// « Assistant IA ») : c'est ce qui fixe le coût d'une question.
///
/// - un résultat d'outil part tronqué à 6 000 caractères (≈ 1 500 jetons) ;
///   `search_library` rend déjà au plus 10 artistes, 10 albums et 10 pistes ;
/// - une question n'envoie pas plus de 24 000 caractères de résultats en tout
///   (≈ 6 000 jetons) : au-delà, l'outil s'exécute mais le modèle ne reçoit
///   qu'un avis « limite atteinte » ;
/// - 5 tours d'outils au plus, plus le tour final.
pub(crate) const MAX_CARACTERES_PAR_RESULTAT: usize = 6_000;
pub(crate) const MAX_CARACTERES_PAR_QUESTION: usize = 24_000;

/// Le résultat d'un outil tel que le modèle le reçoit, borné. `envoyes` :
/// les caractères de résultats déjà envoyés pour cette question.
pub(crate) fn resultat_borne(resultat: &str, envoyes: &mut usize) -> String {
    const AVIS_TRONQUE: &str = " …[résultat tronqué]";
    const AVIS_LIMITE: &str = "{\"error\":\"limite_de_bibliotheque\",\"message\":\"Limite de données de la bibliothèque atteinte pour cette question : réponds avec ce que tu sais déjà.\"}";
    if *envoyes >= MAX_CARACTERES_PAR_QUESTION {
        return AVIS_LIMITE.to_string();
    }
    let place = MAX_CARACTERES_PAR_RESULTAT.min(MAX_CARACTERES_PAR_QUESTION - *envoyes);
    let n = resultat.chars().count();
    let sortie = if n <= place {
        resultat.to_string()
    } else {
        let mut t: String = resultat.chars().take(place).collect();
        t.push_str(AVIS_TRONQUE);
        t
    };
    *envoyes += n.min(place);
    sortie
}

#[derive(Deserialize)]
struct AiQuery {
    message: String,
    zone_id: Option<i64>,
}

// ---------------------------------------------------------------------------
// Query params for recommendations
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RecommendationsParams {
    limit: Option<i64>,
}

#[derive(Deserialize)]
struct SmartRadioBody {
    seed_track_id: Option<i64>,
    seed_artist: Option<String>,
    seed_genre: Option<String>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/query", post(ai_query))
        .route("/recommendations", get(ai_recommendations))
        .route("/daily-mixes", get(ai_daily_mixes))
        .route("/smart-radio", post(ai_smart_radio))
}

/// Celui à qui l'ancien assistant parle.
///
/// Étape 2 du chantier « Assistant IA » : un serveur qui porte le greffon
/// (`assistant-ia`, dans le `default`) ne lit plus la clé EN CLAIR
/// `anthropic_api_key` — elle est migrée dans le coffre du greffon — et chaque
/// tour est compté au tarif de l'assistant et refusé plafond atteint. Le
/// pilotage par outils est écrit pour l'API Messages : il faut que le
/// fournisseur de l'assistant soit Anthropic ; sinon, repli local, comme sans
/// clé. Le chemin historique ne reste que pour un binaire compilé sans le
/// greffon.
///
/// Étape 3 (« extension de l'existant », Bertrand, 13/10/2026) : le dialogue
/// part chez le fournisseur de l'assistant QUEL QU'IL SOIT (Anthropic,
/// compatible OpenAI, Gemini, Ollama) — la passerelle traduit les outils et
/// les tours. Toujours sur la bibliothèque locale seule (les outils de
/// `tune_core::ai::tools`), et bornée : voir [`MAX_CARACTERES_PAR_RESULTAT`].
enum Interlocuteur {
    #[cfg(not(feature = "assistant-ia"))]
    Historique(AnthropicClient),
    #[cfg(feature = "assistant-ia")]
    Assistant(tune_assistant_ia::Passerelle),
}

/// Pourquoi un tour n'a pas eu lieu.
enum Refus {
    /// Une erreur du client historique : 500, comme avant.
    #[cfg(not(feature = "assistant-ia"))]
    Erreur(String),
    /// Un refus de l'assistant (plafond atteint, clé refusée…) : rendu en
    /// réponse lisible, jamais en 401.
    #[cfg(feature = "assistant-ia")]
    Assistant { code: &'static str, message: String },
}

/// Jetons de sortie d'un tour par l'assistant (le défaut du client historique).
#[cfg(feature = "assistant-ia")]
const MAX_JETONS_PAR_TOUR: u32 = 1024;

impl Interlocuteur {
    /// Étape 3 : l'état du plafond de l'assistant (`plafond`, avec l'alerte à
    /// 80 %) joint à la réponse, comme pour les autres usages, et le coût
    /// EXACT de la question (`cout_eur`) : la somme de ce qui a été compté à
    /// chacun de ses tours, au tarif daté de l'assistant. Le chemin
    /// historique ne compte rien et ne porte aucun des deux.
    fn avec_plafond(&self, mut v: Value, cout_micro_eur: u64) -> Value {
        match self {
            #[cfg(not(feature = "assistant-ia"))]
            Self::Historique(_) => {
                let _ = cout_micro_eur;
            }
            #[cfg(feature = "assistant-ia")]
            Self::Assistant(p) => {
                v["plafond"] = p.etat_plafond();
                v["cout_eur"] = json!(tune_assistant_ia::couts::micro_en_euros(cout_micro_eur));
            }
        }
        v
    }

    fn choisir(state: &AppState, settings: &SettingsRepo) -> Option<Self> {
        #[cfg(feature = "assistant-ia")]
        {
            let _ = settings;
            let p = crate::plugins::passerelle_assistant(&state.backend);
            p.migrer();
            p.dialogue_pret().then_some(Self::Assistant(p))
        }
        #[cfg(not(feature = "assistant-ia"))]
        {
            let _ = state;
            let api_key = settings
                .get("anthropic_api_key")
                .ok()
                .flatten()
                .unwrap_or_default();
            if api_key.is_empty() {
                return None;
            }
            let model = settings
                .get("anthropic_model")
                .ok()
                .flatten()
                .unwrap_or_default();
            Some(Self::Historique(AnthropicClient::new(api_key, model)))
        }
    }

    async fn chat(
        &self,
        system: &str,
        messages: Vec<Message>,
        tools: &[tune_core::ai::client::Tool],
        cout_micro_eur: &mut u64,
    ) -> Result<tune_core::ai::client::ApiResponse, Refus> {
        match self {
            #[cfg(not(feature = "assistant-ia"))]
            Self::Historique(client) => {
                let _ = cout_micro_eur;
                client
                    .chat(system, messages, tools)
                    .await
                    .map_err(Refus::Erreur)
            }
            #[cfg(feature = "assistant-ia")]
            Self::Assistant(p) => {
                let illisible = || Refus::Assistant {
                    code: "erreur_fournisseur",
                    message: "Réponse illisible du fournisseur.".into(),
                };
                let messages = serde_json::to_value(&messages).map_err(|_| illisible())?;
                let outils = serde_json::to_value(tools).map_err(|_| illisible())?;
                let (v, cout) = p
                    .discuter_compte(system, &messages, &outils, MAX_JETONS_PAR_TOUR)
                    .await
                    .map_err(|e| Refus::Assistant {
                        code: e.code(),
                        message: e.message(),
                    })?;
                // Compté AVANT la lecture : un tour payé dont la réponse est
                // illisible reste dans le coût de la question.
                *cout_micro_eur += cout;
                serde_json::from_value(v).map_err(|_| illisible())
            }
        }
    }
}

/// La réponse d'un refus, ou l'erreur HTTP du chemin historique.
fn sortie_de_refus(
    client: &Interlocuteur,
    r: Refus,
    actions: &[Value],
    zone_id: i64,
    cout_micro_eur: u64,
) -> Result<Json<Value>, AppError> {
    match r {
        #[cfg(not(feature = "assistant-ia"))]
        Refus::Erreur(e) => {
            let _ = (client, actions, zone_id, cout_micro_eur);
            Err(AppError::internal(format!("Claude API error: {e}")))
        }
        #[cfg(feature = "assistant-ia")]
        Refus::Assistant { code, message } => {
            info!(code, "ai_query_refusee_par_l_assistant");
            // Les tours déjà payés de la question restent dans son coût.
            Ok(Json(client.avec_plafond(
                json!({
                    "reply": message,
                    "error": code,
                    "actions": actions,
                    "zone_id": zone_id,
                }),
                cout_micro_eur,
            )))
        }
    }
}

async fn ai_query(
    headers: axum::http::HeaderMap,
    State(state): State<AppState>,
    Json(body): Json<AiQuery>,
) -> Result<Json<Value>, AppError> {
    let lang = crate::i18n::lang_from_header(&headers);
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let zone_id = body.zone_id.unwrap_or(1);

    info!(zone_id, message = %body.message, "ai_query");

    let Some(client) = Interlocuteur::choisir(&state, &settings) else {
        return local_ai_query(state, &body.message, zone_id, &lang).await;
    };

    // Answer in the UI language, not always French (#1143).
    let system = format!("{SYSTEM_PROMPT} {}", language_directive(&lang));
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
    let mut envoyes = 0usize;
    let mut cout_micro_eur = 0u64;

    for round in 0..MAX_TOOL_ROUNDS {
        let response = match client
            .chat(&system, messages.clone(), &tools, &mut cout_micro_eur)
            .await
        {
            Ok(r) => r,
            Err(r) => {
                return sortie_de_refus(&client, r, &actions, executor.zone_id(), cout_micro_eur);
            }
        };

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
            return Ok(Json(client.avec_plafond(
                json!({
                    "reply": reply,
                    "actions": actions,
                    "zone_id": executor.zone_id(),
                }),
                cout_micro_eur,
            )));
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
                    content: resultat_borne(&result.to_string(), &mut envoyes),
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

    let final_response = match client
        .chat(&system, messages, &[], &mut cout_micro_eur)
        .await
    {
        Ok(r) => r,
        Err(r) => return sortie_de_refus(&client, r, &actions, executor.zone_id(), cout_micro_eur),
    };

    let reply = extract_text(&final_response.content);
    Ok(Json(client.avec_plafond(
        json!({
            "reply": reply,
            "actions": actions,
            "zone_id": executor.zone_id(),
        }),
        cout_micro_eur,
    )))
}

// ---------------------------------------------------------------------------
// GET /ai/recommendations — personalized track recommendations
// Free: 3 tracks max. Premium: full list.
// ---------------------------------------------------------------------------

async fn ai_recommendations(
    State(state): State<AppState>,
    Query(p): Query<RecommendationsParams>,
) -> Result<Json<Value>, AppError> {
    let is_premium = state
        .license
        .check_feature(Feature::AiRecommendations)
        .await;
    let requested_limit = p.limit.unwrap_or(20);
    let limit = if is_premium {
        requested_limit.min(100)
    } else {
        requested_limit.min(3)
    };

    let tracks = recommendations::get_recommendations(&state.backend, None, limit);

    Ok(Json(json!({
        "tracks": tracks,
        "count": tracks.len(),
        "premium": is_premium,
        "limited": !is_premium,
    })))
}

// ---------------------------------------------------------------------------
// GET /ai/daily-mixes — Premium only. 3-5 thematic playlists.
// ---------------------------------------------------------------------------

async fn ai_daily_mixes(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let is_premium = state
        .license
        .check_feature(Feature::AiRecommendations)
        .await;

    if !is_premium {
        return Ok(Json(json!({
            "error": "premium_required",
            "message": "Daily mixes require a Premium license",
            "mixes": [],
        })));
    }

    // Try cached mixes first
    let mixes = recommendations::get_cached_daily_mixes(&state.backend)
        .unwrap_or_else(|| recommendations::generate_daily_mixes(&state.backend));

    Ok(Json(json!({
        "mixes": mixes,
        "count": mixes.len(),
    })))
}

// ---------------------------------------------------------------------------
// POST /ai/smart-radio — Premium only. Seed-based radio.
// Body: { seed_track_id } or { seed_artist } or { seed_genre }
// ---------------------------------------------------------------------------

async fn ai_smart_radio(
    State(state): State<AppState>,
    Json(body): Json<SmartRadioBody>,
) -> Result<Json<Value>, AppError> {
    let is_premium = state
        .license
        .check_feature(Feature::AiRecommendations)
        .await;

    if !is_premium {
        return Ok(Json(json!({
            "error": "premium_required",
            "message": "Smart radio requires a Premium license",
            "tracks": [],
        })));
    }

    if body.seed_track_id.is_none() && body.seed_artist.is_none() && body.seed_genre.is_none() {
        return Err(AppError::bad_request(
            "at least one of seed_track_id, seed_artist, or seed_genre is required",
        ));
    }

    let tracks = recommendations::smart_radio(
        &state.backend,
        body.seed_track_id,
        body.seed_artist.as_deref(),
        body.seed_genre.as_deref(),
        30,
    );

    Ok(Json(json!({
        "tracks": tracks,
        "count": tracks.len(),
        "seed": {
            "track_id": body.seed_track_id,
            "artist": body.seed_artist,
            "genre": body.seed_genre,
        },
    })))
}

// ---------------------------------------------------------------------------
// Local AI — intent parser, no API key needed
// ---------------------------------------------------------------------------

async fn local_ai_query(
    state: AppState,
    message: &str,
    zone_id: i64,
    lang: &str,
) -> Result<Json<Value>, AppError> {
    let tr = |k: &str| crate::i18n::t(lang, k);
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
            "reply": tr("ai.nextTrack").replace("{title}", title),
            "actions": actions,
            "zone_id": executor.zone_id(),
        })));
    }

    // --- Volume ---
    if let Some(vol) = parse_volume(&msg) {
        let result = executor
            .execute("set_volume", json!({ "volume": vol }))
            .await;
        // #6836 — la réponse dit ce qui a été FAIT : une hausse trop grande
        // est refusée par l'outil, et la phrase ne doit pas prétendre le
        // contraire.
        let reply = match result["error"].as_str() {
            None => tr("ai.volumeSet").replace(
                "{value}",
                &((result["volume"].as_f64().unwrap_or(vol) * 100.0).round() as i32).to_string(),
            ),
            Some("volume_raise_too_large") => tr("ai.volumeRaiseLimited").replace(
                "{value}",
                &((result["max_volume_allowed"].as_f64().unwrap_or(0.0) * 100.0).floor() as i32)
                    .to_string(),
            ),
            Some(_) => tr("ai.volumeRefused"),
        };
        actions.push(json!({ "tool": "set_volume", "result": result }));
        return Ok(Json(json!({
            "reply": reply,
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
            let artist = result["artist"].as_str().unwrap_or("");
            tr("ai.nowPlaying")
                .replace("{track}", track)
                .replace("{artist}", artist)
        } else {
            tr("ai.nothingPlaying")
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
        if let Some(albums) = albums
            && let Some(first) = albums.first()
            && let Some(title) = first["title"].as_str()
        {
            let result = executor
                .execute("play_album", json!({ "album_name": title }))
                .await;
            actions.push(
                json!({ "tool": "play_album", "input": { "album_name": title }, "result": result }),
            );
            let artist = result["artist"].as_str().unwrap_or("");
            let count = result["track_count"].as_i64().unwrap_or(0);
            return Ok(Json(json!({
                "reply": tr("ai.playingCount").replace("{title}", title).replace("{artist}", artist).replace("{count}", &count.to_string()),
                "actions": actions,
                "zone_id": executor.zone_id(),
            })));
        }

        // Fall back to track match
        if let Some(tracks) = tracks
            && let Some(first) = tracks.first()
            && let Some(title) = first["title"].as_str()
        {
            let result = executor
                .execute("play_track", json!({ "track_name": title }))
                .await;
            actions.push(
                json!({ "tool": "play_track", "input": { "track_name": title }, "result": result }),
            );
            let artist = result["artist"].as_str().unwrap_or("");
            return Ok(Json(json!({
                "reply": tr("ai.playing").replace("{title}", title).replace("{artist}", artist),
                "actions": actions,
                "zone_id": executor.zone_id(),
            })));
        }

        return Ok(Json(json!({
            "reply": tr("ai.notFoundLibrary").replace("{query}", query),
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
            tr("ai.noResults").replace("{query}", query)
        } else {
            let mut parts = Vec::new();
            if n_artists > 0 {
                parts.push(tr("ai.nArtists").replace("{n}", &n_artists.to_string()));
            }
            if n_albums > 0 {
                parts.push(tr("ai.nAlbums").replace("{n}", &n_albums.to_string()));
            }
            if n_tracks > 0 {
                parts.push(tr("ai.nTracks").replace("{n}", &n_tracks.to_string()));
            }
            tr("ai.resultsFor")
                .replace("{query}", query)
                .replace("{parts}", &parts.join(", "))
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
                tr("ai.zonesAvailable").replace("{zones}", &names.join(", "))
            }
            None => tr("ai.noZone"),
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

    if let Some(albums) = albums
        && let Some(first) = albums.first()
        && let Some(title) = first["title"].as_str()
    {
        let play = executor
            .execute("play_album", json!({ "album_name": title }))
            .await;
        actions.push(
            json!({ "tool": "play_album", "input": { "album_name": title }, "result": play }),
        );
        let artist = play["artist"].as_str().unwrap_or("");
        return Ok(Json(json!({
            "reply": tr("ai.playing").replace("{title}", title).replace("{artist}", artist),
            "actions": actions,
            "zone_id": executor.zone_id(),
        })));
    }
    if let Some(tracks) = tracks
        && let Some(first) = tracks.first()
        && let Some(title) = first["title"].as_str()
    {
        let play = executor
            .execute("play_track", json!({ "track_name": title }))
            .await;
        actions.push(
            json!({ "tool": "play_track", "input": { "track_name": title }, "result": play }),
        );
        let artist = play["artist"].as_str().unwrap_or("");
        return Ok(Json(json!({
            "reply": tr("ai.playing").replace("{title}", title).replace("{artist}", artist),
            "actions": actions,
            "zone_id": executor.zone_id(),
        })));
    }

    Ok(Json(json!({
        "reply": tr("ai.notUnderstood").replace("{msg}", message),
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
        if let Ok(v) = word.parse::<f64>()
            && (0.0..=100.0).contains(&v)
        {
            return Some(v / 100.0);
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

#[cfg(test)]
mod tests_bornes {
    use super::*;

    #[test]
    fn un_resultat_court_part_tel_quel_et_compte() {
        let mut envoyes = 0;
        assert_eq!(
            resultat_borne("{\"ok\":true}", &mut envoyes),
            "{\"ok\":true}"
        );
        assert_eq!(envoyes, 11);
    }

    #[test]
    fn un_resultat_long_est_tronque_a_la_limite_par_resultat() {
        let mut envoyes = 0;
        // Un caractère de deux octets, absent de l'avis de troncature.
        let long = "ä".repeat(MAX_CARACTERES_PAR_RESULTAT + 500);
        let r = resultat_borne(&long, &mut envoyes);
        assert!(r.ends_with("[résultat tronqué]"));
        assert_eq!(
            r.chars().filter(|c| *c == 'ä').count(),
            MAX_CARACTERES_PAR_RESULTAT,
            "tronqué sur un caractère entier, pas un octet"
        );
        assert_eq!(envoyes, MAX_CARACTERES_PAR_RESULTAT);
    }

    #[test]
    fn une_question_n_envoie_pas_plus_que_sa_limite() {
        let mut envoyes = 0;
        let bloc = "x".repeat(MAX_CARACTERES_PAR_RESULTAT);
        let mut total = 0;
        for _ in 0..10 {
            let r = resultat_borne(&bloc, &mut envoyes);
            if !r.contains("limite_de_bibliotheque") {
                total += r.chars().count();
            }
        }
        assert_eq!(total, MAX_CARACTERES_PAR_QUESTION);
        assert_eq!(envoyes, MAX_CARACTERES_PAR_QUESTION);
        let r = resultat_borne("{}", &mut envoyes);
        let v: Value = serde_json::from_str(&r).expect("l'avis est du JSON");
        assert_eq!(v["error"], "limite_de_bibliotheque");
    }
}

#[cfg(test)]
mod language_directive_tests {
    use super::language_directive;

    /// Chaque langue de l'interface nomme SA langue à l'assistant. Le suédois,
    /// puis le hongrois, le vietnamien, le norvégien, le polonais et le grec, tombaient dans le `_` : l'assistant
    /// répondait en français à un utilisateur réglé en suédois.
    ///
    /// Contre-épreuve : retirer le bras `"vi"` fait rougir ce test sur «vi».
    #[test]
    fn chaque_langue_de_l_interface_a_sa_consigne() {
        let francais = language_directive("fr");
        for lang in crate::i18n::SUPPORTED {
            if *lang == "fr" {
                continue;
            }
            assert_ne!(
                language_directive(lang),
                francais,
                "«{lang}» reçoit la consigne française"
            );
        }
        assert!(language_directive("hu").contains("Hungarian"));
        assert!(language_directive("vi").contains("Vietnamese"));
        assert!(language_directive("nb").contains("Norwegian"));
        assert!(language_directive("pl").contains("Polish"));
        assert!(language_directive("el").contains("Greek"));
    }
}

#[cfg(all(test, not(feature = "assistant-ia")))]
#[path = "ai/query_volume_6836_tests.rs"]
mod query_volume_6836_tests;
