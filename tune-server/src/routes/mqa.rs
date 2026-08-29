use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::settings_repo::SettingsRepo;

use crate::error::AppError;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/status", get(mqa_status))
        .route("/detect/{track_id}", get(detect_mqa))
        .route("/config", get(mqa_config).post(set_mqa_config))
}

/// MQA subsystem status.
async fn mqa_status(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let enabled = settings
        .get("mqa_passthrough")
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false);
    let renderer = settings
        .get("mqa_renderer")
        .ok()
        .flatten()
        .unwrap_or_else(|| "none".into());

    Json(json!({
        "available": true,
        "passthrough_enabled": enabled,
        "renderer": renderer,
        "info": "MQA (Master Quality Authenticated) detection and passthrough. Note: MQA Ltd entered administration in 2023.",
    }))
}

/// Detect if a track contains MQA signaling.
///
/// MQA embeds data in the least significant bits of a FLAC/WAV file.
/// Detection looks for specific bit patterns in the audio stream.
async fn detect_mqa(
    State(state): State<AppState>,
    Path(track_id): Path<i64>,
) -> Result<impl IntoResponse, AppError> {
    let track = state
        .backend
        .query_one(
            // La colonne s'appelle file_path — « path » faisait échouer la
            // requête et la route répondait 500 systématiquement : elle n'a
            // jamais fonctionné depuis son introduction.
            "SELECT file_path, format, sample_rate, bit_depth FROM tracks WHERE id = ?",
            &[&track_id as &dyn tune_core::db::backend::ToSqlValue],
        )
        .map_err(AppError::internal)?;

    let (path, format, sample_rate, bit_depth) = match track {
        Some(row) => (
            row.first().and_then(|v| v.as_string()),
            row.get(1).and_then(|v| v.as_string()),
            row.get(2).and_then(|v| v.as_i64()),
            row.get(3).and_then(|v| v.as_i64()),
        ),
        None => {
            return Ok((
                StatusCode::NOT_FOUND,
                Json(json!({"error": "track not found"})),
            )
                .into_response());
        }
    };

    let path = match path {
        Some(p) => p,
        None => {
            return Ok(Json(json!({
                "track_id": track_id,
                "mqa_detected": false,
                "error": "no file path for this track",
            }))
            .into_response());
        }
    };

    // MQA detection heuristics:
    // 1. Must be FLAC or WAV (MQA encodes in PCM)
    // 2. Typically 44.1kHz or 48kHz base rate (unfolds to higher rates)
    // 3. ≥16-bit : le MQA « CD » existe en 16 bits (le mot de synchro se
    //    trouve alors dans les tout derniers bits), ne pas exiger 24.
    let format_str = format.as_deref().unwrap_or("").to_lowercase();
    let is_candidate =
        (format_str.contains("flac") || format_str.contains("wav")) && bit_depth.unwrap_or(0) >= 16;

    if !is_candidate {
        return Ok(Json(json!({
            "track_id": track_id,
            "path": path,
            "format": format,
            "sample_rate": sample_rate,
            "bit_depth": bit_depth,
            "mqa_detected": false,
            "reason": "Not a candidate — MQA requires 16/24-bit FLAC/WAV",
        }))
        .into_response());
    }

    // Attempt to read the file and check for MQA magic bytes.
    // MQA signaling is embedded in the least significant bits of audio samples.
    // A proper implementation would decode a block of audio and look for the
    // MQA sync word pattern. For now, we do a best-effort check.
    let mqa_result = check_mqa_signaling(&path).await;

    Ok(Json(json!({
        "track_id": track_id,
        "path": path,
        "format": format,
        "sample_rate": sample_rate,
        "bit_depth": bit_depth,
        "mqa_detected": mqa_result.detected,
        "mqa_original_sample_rate": mqa_result.original_rate,
        "mqa_studio": mqa_result.is_studio,
        "analysis": mqa_result.analysis,
    }))
    .into_response())
}

struct MqaResult {
    detected: bool,
    original_rate: Option<u32>,
    is_studio: bool,
    analysis: String,
}

/// Mot de synchronisation MQA : 36 bits, embarqués en continu dans le XOR des
/// deux canaux à une position de bit fixe parmi les LSB. Constante et méthode
/// publiques et éprouvées (projets `mqa_identifier` — détection identique à
/// celle des lecteurs qui affichent le badge MQA).
const MQA_SYNC_WORD: u64 = 0xbe0498c88;
const MQA_SYNC_MASK: u64 = 0xF_FFFF_FFFF; // 36 bits

/// Cherche le mot de synchro MQA dans un bloc d'échantillons entrelacés
/// (i32 cadrés à droite, sortie de decode_to_pcm). Retourne la position de
/// bit (0..8, depuis le LSB) où le motif a été trouvé.
fn detect_mqa_sync(samples: &[i32], channels: u32) -> Option<u8> {
    if channels < 2 {
        return None; // le signal MQA vit dans le XOR L/R — rien à chercher en mono
    }
    let ch = channels as usize;
    let mut buffers = [0u64; 8];
    for frame in samples.chunks_exact(ch) {
        let x = (frame[0] ^ frame[1]) as u32;
        for (p, buf) in buffers.iter_mut().enumerate() {
            *buf = ((*buf << 1) | u64::from((x >> p) & 1)) & MQA_SYNC_MASK;
            if *buf == MQA_SYNC_WORD {
                return Some(p as u8);
            }
        }
    }
    None
}

async fn check_mqa_signaling(path: &str) -> MqaResult {
    // Décodage bloquant → spawn_blocking. Les 10 premières secondes suffisent :
    // le flux MQA signale en continu, pas seulement en tête de fichier.
    // (L'ancien stub lisait le fichier ENTIER en mémoire pour ne vérifier que
    // les 4 octets de magie et répondait toujours detected:false.)
    let p = path.to_string();
    let decoded = tokio::task::spawn_blocking(move || {
        tune_core::audio::decode::decode_to_pcm(&p, None, None, 0.0, 10.0)
    })
    .await;

    match decoded {
        Ok(Ok(audio)) => {
            if audio.channels < 2 {
                return MqaResult {
                    detected: false,
                    original_rate: None,
                    is_studio: false,
                    analysis: "Mono file — MQA signaling lives in the L/R XOR".into(),
                };
            }
            match detect_mqa_sync(&audio.samples_i32, audio.channels) {
                Some(pos) => MqaResult {
                    detected: true,
                    // Le décodage du champ « original sample rate » qui suit le
                    // mot de synchro n'est pas encore implémenté — ne rien
                    // inventer, les tags ORIGINALSAMPLERATE restent la source.
                    original_rate: None,
                    is_studio: false,
                    analysis: format!("MQA sync word found (XOR bit position {pos})"),
                },
                None => MqaResult {
                    detected: false,
                    original_rate: None,
                    is_studio: false,
                    analysis: "No MQA sync word in the first 10 seconds".into(),
                },
            }
        }
        Ok(Err(e)) => MqaResult {
            detected: false,
            original_rate: None,
            is_studio: false,
            analysis: format!("Could not decode file: {e}"),
        },
        Err(e) => MqaResult {
            detected: false,
            original_rate: None,
            is_studio: false,
            analysis: format!("Decode task failed: {e}"),
        },
    }
}

#[cfg(test)]
mod mqa_sync_tests {
    use super::*;

    #[test]
    fn trouve_le_mot_de_synchro_a_la_position_zero() {
        // Encode le mot 36 bits dans le LSB du XOR L/R : R = 0, L = bit.
        let mut samples: Vec<i32> = Vec::new();
        for i in (0..36).rev() {
            let bit = ((MQA_SYNC_WORD >> i) & 1) as i32;
            samples.push(bit); // L
            samples.push(0); // R
        }
        assert_eq!(detect_mqa_sync(&samples, 2), Some(0));
    }

    #[test]
    fn trouve_le_mot_a_une_position_haute() {
        // Même encodage, décalé au bit 5 (fichier 24 bits typique).
        let mut samples: Vec<i32> = Vec::new();
        for i in (0..36).rev() {
            let bit = (((MQA_SYNC_WORD >> i) & 1) as i32) << 5;
            samples.push(bit);
            samples.push(0);
        }
        assert_eq!(detect_mqa_sync(&samples, 2), Some(5));
    }

    #[test]
    fn silence_et_mono_ne_matchent_pas() {
        assert_eq!(detect_mqa_sync(&[0; 4096], 2), None);
        assert_eq!(detect_mqa_sync(&[1; 4096], 1), None);
        // Bruit déterministe : XOR alternant, aucun motif de 36 bits ne colle.
        let noise: Vec<i32> = (0..4096).map(|i| if i % 3 == 0 { 1 } else { 0 }).collect();
        assert_eq!(detect_mqa_sync(&noise, 2), None);
    }
}

/// Get MQA configuration.
async fn mqa_config(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let passthrough = settings
        .get("mqa_passthrough")
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false);
    let renderer = settings
        .get("mqa_renderer")
        .ok()
        .flatten()
        .unwrap_or_else(|| "none".into());
    let decode_first_unfold = settings
        .get("mqa_decode_first_unfold")
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false);

    Json(json!({
        "passthrough_enabled": passthrough,
        "renderer": renderer,
        "decode_first_unfold": decode_first_unfold,
        "options": {
            "renderer_values": ["none", "decoder", "renderer"],
            "description": {
                "none": "No MQA processing — pass bitstream as-is",
                "decoder": "Full MQA decode (software)",
                "renderer": "First unfold only — let DAC do final rendering",
            },
        },
    }))
}

#[derive(Deserialize)]
struct MqaConfigBody {
    passthrough_enabled: Option<bool>,
    renderer: Option<String>,
    decode_first_unfold: Option<bool>,
}

/// Update MQA configuration.
async fn set_mqa_config(
    State(state): State<AppState>,
    Json(body): Json<MqaConfigBody>,
) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());

    if let Some(v) = body.passthrough_enabled {
        settings
            .set("mqa_passthrough", if v { "true" } else { "false" })
            .ok();
    }
    if let Some(r) = &body.renderer {
        settings.set("mqa_renderer", r).ok();
    }
    if let Some(v) = body.decode_first_unfold {
        settings
            .set("mqa_decode_first_unfold", if v { "true" } else { "false" })
            .ok();
    }

    Json(json!({"saved": true}))
}
