//! `POST /zones/{id}/radio/artist` — la radio artiste à la demande (#5395).
//!
//! Le bouton « Radio de l'artiste » d'une fiche l'appelle : la file de la zone
//! est remplacée par un premier lot (l'artiste de départ, environ 20 %, et des
//! artistes proches, en aléatoire), la lecture part du premier titre, et
//! l'auto-lecture de fin de file recharge la radio sans fin
//! (`poller/fin_de_piste.rs`, `continuer_la_radio_artiste`). Toute la
//! composition vit dans `tune_core::playback::radio_artiste`.
//!
//! Gratuite (décision 4) : aucune garde Premium, à la différence de
//! `/ai/smart-radio`.
//!
//! Corps : `{"artist": "Nom", "service": "qobuz" | null, "artist_id": "…" | null}`
//! — `service` est celui de la fiche (absent ou `local` : fiche de
//! bibliothèque), `artist_id` l'identifiant de l'artiste sur ce service.
//!
//! Réponses : 200 et la zone (comme `POST /play`), avec `radio` ; 400 sans
//! nom d'artiste ; 404 zone inconnue, ou aucun titre trouvé
//! (`radio_artiste_vide`) — la file n'est alors pas touchée.

use super::*;
use tune_core::playback::radio_artiste;

#[derive(Debug, Deserialize)]
pub(super) struct RadioArtisteRequest {
    artist: String,
    #[serde(default)]
    service: Option<String>,
    #[serde(default)]
    artist_id: Option<String>,
}

pub(super) async fn lancer_radio_artiste(
    State(state): State<AppState>,
    profile: ActiveProfile,
    Path(zone_id): Path<i64>,
    headers: axum::http::HeaderMap,
    Json(body): Json<RadioArtisteRequest>,
) -> axum::response::Response {
    let lang = crate::i18n::lang_from_header(&headers);
    let artiste = body.artist.trim().to_string();
    if artiste.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "artist_required", "message": "artist is required"})),
        )
            .into_response();
    }
    match tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone()).get(zone_id) {
        Ok(Some(_)) => {}
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "zone_not_found", "zone_id": zone_id})),
            )
                .into_response();
        }
        Err(e) => return lecture_base_echouee("radio_artiste_zone", zone_id, e),
    }
    let lot = radio_artiste::demarrer(
        &state.backend,
        &state.orchestrator.services,
        zone_id,
        &artiste,
        body.service.as_deref(),
        body.artist_id.as_deref(),
    )
    .await;
    if lot.candidats.is_empty() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": "radio_artiste_vide",
                "artist": artiste,
                "message": "no playable track found for this artist radio",
            })),
        )
            .into_response();
    }
    // Comme `play` : la session de la zone appartient à qui lance la radio,
    // et les rechargements de l'auto-lecture en héritent.
    state
        .playback
        .set_session_profile(zone_id, Some(profile.id()))
        .await;
    let items: Vec<QueueInput> = lot
        .candidats
        .iter()
        .map(radio_artiste::Candidat::en_entree_de_file)
        .collect();
    let queue_repo = PlayQueueRepo::with_backend(state.backend.clone());
    if let Err(e) = queue_repo.clear(zone_id) {
        return lecture_base_echouee("radio_artiste_vider_file", zone_id, e);
    }
    if let Err(e) = queue_repo.append(zone_id, &items) {
        warn!(zone_id, error = %e, "radio_artiste_file_non_ecrite");
        let _ = queue_repo.clear(zone_id);
        radio_artiste::effacer_contexte(&state.backend, zone_id);
        return lecture_base_echouee("radio_artiste_ecrire_file", zone_id, e);
    }
    let longueur = match queue_repo.count_all(zone_id) {
        Ok(n) => n,
        Err(e) => return lecture_base_echouee("radio_artiste_longueur_file", zone_id, e),
    };
    info!(
        zone_id,
        artiste = %artiste,
        service = ?body.service,
        en_file = longueur,
        titres_graine = lot.titres_graine,
        "radio_artiste_lancee"
    );
    state.playback.update_queue_info(zone_id, 0, longueur).await;
    let radio = json!({
        "artist": artiste,
        "count": lot.candidats.len(),
        "seed_tracks": lot.titres_graine,
        "similar_sources": lot
            .voisins_par_source
            .iter()
            .map(|(source, n)| json!({"source": source, "artists": n}))
            .collect::<Vec<_>>(),
        "tracks": lot
            .candidats
            .iter()
            .map(radio_artiste::Candidat::en_json)
            .collect::<Vec<_>>(),
    });
    match state.orchestrator.play_from_queue(zone_id, 0).await {
        Ok(result) => {
            persist_queue_async(&state, zone_id);
            let mut zone = build_zone_json_with_result(&state, zone_id, &result).await;
            if let Some(obj) = zone.as_object_mut() {
                obj.insert("radio".into(), radio);
            }
            Json(zone).into_response()
        }
        Err(e) => {
            persist_queue_async(&state, zone_id);
            warn!(zone_id, error = %e, "radio_artiste_lecture_echouee");
            play_error_response(e, &lang)
        }
    }
}
