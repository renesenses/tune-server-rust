use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::warn;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/rooms", get(list_rooms))
        .route("/speakers", get(list_speakers))
        .route("/rooms/{id}/play", post(play_room))
        .route("/rooms/{id}/pause", post(pause_room))
        .route("/rooms/{id}/volume", post(set_room_volume))
        .route("/rooms/{id}/group", post(group_rooms))
}

/// « Cet appareil découvert est-il un Sonos ? »
///
/// Le seul critère dont dispose ce fichier : le fabricant ou le modèle
/// annoncés dans le descriptif UPnP. Il n'y a **aucune** découverte propre à
/// Sonos ici — ces routes filtrent le résultat de la découverte DLNA
/// générique, par laquelle une enceinte Sonos joue déjà.
///
/// Le prédicat était écrit deux fois, à l'identique, dans `list_rooms` et
/// `list_speakers` : deux routes qui doivent rendre le MÊME ensemble
/// d'appareils sous deux formes différentes. Deux copies, c'est deux chances
/// de diverger sans que rien ne le dise.
///
/// Ce que ce prédicat ne fait pas, et qu'il faudra : reconnaître un Sonos à
/// son identifiant `RINCON_…`. C'est le marqueur fiable — `outputs/dlna.rs`
/// s'en sert déjà (`device_id.contains("RINCON")`) pour router le volume vers
/// `GroupRenderingControl`. Ici on dépend encore d'un descriptif que la
/// découverte peut n'avoir jamais réussi à lire.
fn est_un_sonos(d: &tune_core::discovery::device::DiscoveredDevice) -> bool {
    let mfr = d.manufacturer.as_deref().unwrap_or("").to_lowercase();
    let model = d.model.as_deref().unwrap_or("").to_lowercase();
    mfr.contains("sonos") || model.contains("sonos")
}

/// Return DLNA devices whose manufacturer or model contains "Sonos".
async fn list_rooms(State(state): State<AppState>) -> Json<Value> {
    let scanner = &state.scanner;
    let devices = scanner.devices().await;

    let sonos: Vec<Value> = devices
        .iter()
        .filter(|d| est_un_sonos(d))
        .map(|d| {
            json!({
                "id": d.id,
                "name": d.name,
                "host": d.host,
                "port": d.port,
                "manufacturer": d.manufacturer,
                "model": d.model,
                "available": d.available,
            })
        })
        .collect();

    Json(json!(sonos))
}

/// GET /sonos/speakers
///
/// Les mêmes appareils que `/rooms`, sous la forme que la barre latérale
/// attend : `uid`, `name`, `ip`.
///
/// L'interface appelait cette route depuis toujours ; elle n'a jamais existé,
/// et la section multiroom restait donc vide sans rien dire (#2004). `/rooms`
/// rend `id`/`host`, pas `uid`/`ip` : renommer la route n'aurait pas suffi,
/// c'est la forme qui diffère.
///
/// `is_coordinator` et `group_uid`, déclarés par le type web, ne sont
/// délibérément PAS rendus : ils viennent du service UPnP ZoneGroupTopology,
/// que ce serveur n'interroge pas — il ne fait que de la découverte DLNA
/// générique. Les inventer à `false`/`null` ferait passer une absence
/// d'information pour un fait. Aucun composant ne les lit aujourd'hui.
async fn list_speakers(State(state): State<AppState>) -> Json<Value> {
    let devices = state.scanner.devices().await;

    let speakers: Vec<Value> = devices
        .iter()
        .filter(|d| est_un_sonos(d))
        .map(|d| {
            json!({
                "uid": d.id,
                "name": d.name,
                "ip": d.host,
                "available": d.available,
            })
        })
        .collect();

    Json(json!(speakers))
}

/// Send a Play (resume) command to the given Sonos device via its DLNA output.
async fn play_room(State(state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    let outputs = state.outputs.lock().await;
    let Some(output) = outputs.get(&id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "room not found"})),
        )
            .into_response();
    };
    let output = output.lock().await;
    match output.checked_resume().await {
        Ok(()) => Json(json!({"status": "playing"})).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

/// Send a Pause command to the given Sonos device via its DLNA output.
async fn pause_room(State(state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    let outputs = state.outputs.lock().await;
    let Some(output) = outputs.get(&id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "room not found"})),
        )
            .into_response();
    };
    let output = output.lock().await;
    match output.checked_pause().await {
        Ok(()) => Json(json!({"status": "paused"})).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

#[derive(Deserialize)]
struct VolumeBody {
    volume: f64,
    /// #6721 — hausse de plus de 10 dB confirmée par l'utilisateur.
    #[serde(default)]
    confirm_increase: bool,
}

/// Set volume on the given Sonos device via its DLNA output.
/// Volume is a float 0.0..1.0 matching the OutputTarget trait.
async fn set_room_volume(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<VolumeBody>,
) -> impl IntoResponse {
    let outputs = state.outputs.lock().await;
    let Some(output) = outputs.get(&id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "room not found"})),
        )
            .into_response();
    };
    let output = output.lock().await;
    // #6541 — la limite de sécurité de la zone qui porte cet appareil tient
    // aussi sur ce pilotage direct, qui ne passe pas par la zone.
    let volume = match tune_core::audio::volume_limites::securite_de_l_appareil(&state.backend, &id)
    {
        Some(s) => body.volume.min(s),
        None => body.volume,
    };
    // #6721 — ce pilotage direct ne passe pas par la zone : la garde contre
    // les montées brusques y est appliquée ici, depuis le volume que
    // l'appareil rapporte (illisible : depuis le silence, par prudence).
    let actuel = output.get_status().await.map(|s| s.volume).unwrap_or(0.0);
    let retenu = tune_core::audio::garde_de_hausse::retenir(
        actuel,
        volume,
        tune_core::audio::volume_scale::EchelleDeVolume::Lineaire,
        tune_core::audio::garde_de_hausse::Hausse::depuis_confirmation(body.confirm_increase),
    );
    let garde = tune_core::audio::garde_de_hausse::VolumeRetenu {
        demande: volume,
        retenu,
        hausse_plafonnee: retenu < volume,
    };
    let volume = retenu;
    match output.checked_set_volume(volume).await {
        Ok(()) => {
            let mut corps = serde_json::Map::new();
            corps.insert("volume".into(), json!(volume));
            garde.inserer(&mut corps);
            Json(serde_json::Value::Object(corps)).into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

#[derive(Deserialize)]
struct GroupBody {
    #[allow(dead_code)]
    room_ids: Vec<String>,
}

/// POST /sonos/rooms/{id}/group — **refuse**, faute de savoir grouper.
///
/// Cette route répondait **200 OK** en renvoyant `coordinator`, `members`, et
/// un champ `status` valant « grouping not yet implemented ». Un appelant qui
/// ne lit pas ce champ — c'est-à-dire tout appelant écrit contre le code HTTP,
/// ce qu'est `fetchJSON` côté client — voyait un **succès**, et un objet dont
/// la forme est exactement celle d'un groupe formé : un coordinateur, ses
/// membres. Rien n'avait été envoyé à la moindre enceinte.
///
/// Ce n'est pas de la cosmétique de code d'état : un 200 qui décrit un groupe
/// inexistant se propage. L'interface l'afficherait, une reprise après
/// redémarrage le persisterait, et le défaut se signalerait bien plus tard
/// sous la forme « le groupage Sonos ne marche pas », loin d'ici.
///
/// Ce qui manque réellement, et qui n'est pas cette route :
///
/// * grouper une enceinte Sonos, c'est appeler `SetAVTransportURI` sur le
///   service **AVTransport du MEMBRE** avec `CurrentURI = x-rincon:RINCON_…`
///   désignant le coordinateur — pas une action du service de topologie, qui
///   est en lecture seule ;
/// * le serveur ne sait pas aujourd'hui QUI est coordinateur : cela se lit
///   dans `ZoneGroupTopology`, service que rien n'interroge ici.
///
/// Tant que ces deux morceaux manquent, la seule réponse honnête est un refus.
/// 501 plutôt que 400 : la requête n'a rien de fautif, c'est le serveur qui ne
/// sait pas faire.
async fn group_rooms(Path(id): Path<String>, Json(_body): Json<GroupBody>) -> impl IntoResponse {
    warn!(coordinator = %id, "sonos_group_non_implemente");
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(crate::message_api::corps_refus(
            "sonos_grouping_not_implemented",
            "api.sonos.groupingNotImplemented",
            &[],
        )),
    )
}
