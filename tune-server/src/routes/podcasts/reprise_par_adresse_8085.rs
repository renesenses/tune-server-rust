//! #8085 — un épisode relancé par son ADRESSE seule.
//!
//! La carte « Récemment écoutés » de l'accueil et l'Historique rejouent une
//! ligne d'historique par la route générique (`POST /zones/{id}/play`) avec
//! `{source: "podcast", source_id: <URL du média>}`. Cette route ne
//! connaissait pas les épisodes : `resolve_streaming_queue_meta` cherchait un
//! service « podcast » qui n'existe pas dans le registre, la piste partait
//! sous « Episode », sans pochette, à la position 0. Elle passe désormais par
//! le même chemin que l'écran Podcasts ([`play_episode`]) et que « Reprendre
//! l'écoute » : la reprise enregistrée de l'épisode donne titre, podcast,
//! pochette et position (moins le recul réglé, #7819) ; sans reprise, la
//! dernière écoute de l'historique donne les métadonnées.
use super::{PlayEpisodeRequest, play_episode};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use tracing::{info, warn};
use tune_core::db::history_repo::HistoryRepo;
use tune_core::db::listening_resume as reprises;
use tune_core::db::play_queue_repo::QueueInput;
use tune_core::db::settings_repo::SettingsRepo;

/// La demande qui reprend l'épisode d'une reprise enregistrée, à sa position
/// moins le recul réglé. `None` si la reprise ne porte aucun épisode.
pub(in crate::routes) fn demande_de_reprise(
    state: &AppState,
    r: &reprises::Resume,
) -> Option<PlayEpisodeRequest> {
    let Some(QueueInput::Streaming {
        source_id,
        title,
        artist,
        cover_url,
        duration_ms,
        ..
    }) = r.items.first()
    else {
        return None;
    };
    let secondes = reprises::effective_rewind(
        SettingsRepo::with_backend(state.backend.clone())
            .get(reprises::REWIND_KEY)
            .ok()
            .flatten()
            .as_deref(),
    );
    Some(PlayEpisodeRequest {
        audio_url: source_id.clone(),
        title: Some(title.clone()),
        podcast_name: Some(artist.clone()),
        podcast_id: Some(r.context_id.clone()),
        cover_url: cover_url.clone(),
        duration_ms: Some((*duration_ms).max(0) as u64),
        seek_ms: Some(reprises::rewind(r.position_ms, secondes)),
        // Sans type gardé, l'extension puis l'hébergeur le disent (#7894).
        mime_type: None,
    })
}

/// Joue l'épisode par [`play_episode`] et rend la zone, comme les autres
/// lancements ; un refus garde la réponse (statut et `code`) de l'épisode.
pub(in crate::routes) async fn jouer_et_rendre_la_zone(
    state: &AppState,
    zone_id: i64,
    demande: PlayEpisodeRequest,
) -> Response {
    let reponse = play_episode(State(state.clone()), Path(zone_id), Json(demande))
        .await
        .into_response();
    if reponse.status().is_success() {
        Json(crate::routes::playback::build_zone_json(state, zone_id).await).into_response()
    } else {
        reponse
    }
}

/// Ce que la route générique sait de l'épisode demandé : son adresse, et ce
/// que le client a bien voulu joindre.
pub(crate) struct EpisodeParAdresse<'a> {
    pub(crate) audio_url: &'a str,
    pub(crate) title: Option<&'a str>,
    pub(crate) artist_name: Option<&'a str>,
    pub(crate) album_title: Option<&'a str>,
    pub(crate) cover_path: Option<&'a str>,
    pub(crate) duration_ms: Option<i64>,
    pub(crate) seek_ms: Option<u64>,
    pub(crate) context_id: Option<&'a str>,
}

fn renseigne(v: Option<&str>) -> Option<String> {
    v.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Sans reprise (épisode fini, ou jamais repris sur ce serveur) : ce que le
/// client a joint, sinon la dernière écoute de l'épisode dans l'historique.
fn demande_sans_reprise(state: &AppState, e: &EpisodeParAdresse<'_>) -> PlayEpisodeRequest {
    let ecoute = HistoryRepo::with_backend(state.backend.clone())
        .derniere_ecoute_du_flux("podcast", e.audio_url)
        .unwrap_or_else(|erreur| {
            warn!(error = %erreur, "podcast_par_adresse_historique_illisible");
            None
        });
    let ecoute = ecoute.as_ref();
    PlayEpisodeRequest {
        audio_url: e.audio_url.to_string(),
        title: renseigne(e.title).or_else(|| ecoute.and_then(|h| renseigne(Some(&h.title)))),
        podcast_name: renseigne(e.artist_name)
            .or_else(|| renseigne(e.album_title))
            .or_else(|| {
                ecoute.and_then(|h| {
                    renseigne(h.artist_name.as_deref()).or(renseigne(h.album_title.as_deref()))
                })
            }),
        podcast_id: renseigne(e.context_id)
            .or_else(|| ecoute.and_then(|h| renseigne(h.context_id.as_deref()))),
        cover_url: renseigne(e.cover_path)
            .or_else(|| ecoute.and_then(|h| renseigne(h.cover_url.as_deref()))),
        duration_ms: e
            .duration_ms
            .filter(|d| *d > 0)
            .or_else(|| ecoute.map(|h| h.duration_ms).filter(|d| *d > 0))
            .map(|d| d as u64),
        seek_ms: None,
        mime_type: None,
    }
}

/// Lance l'épisode `e.audio_url` comme l'écran Podcasts : reprise
/// enregistrée d'abord, historique ensuite. Un `seek_ms` explicite du client
/// prime sur la position enregistrée.
pub(crate) async fn jouer_l_episode_par_son_adresse(
    state: &AppState,
    zone_id: i64,
    e: EpisodeParAdresse<'_>,
) -> Response {
    // Un nouveau geste : un seek récent ne doit pas survivre à la relance.
    state.playback.oublier_le_deplacement(zone_id).await;
    let reprise = reprises::podcast_de_l_adresse(&state.backend, zone_id, e.audio_url)
        .unwrap_or_else(|erreur| {
            warn!(zone_id, error = %erreur, "podcast_par_adresse_reprise_illisible");
            None
        });
    let depuis_la_reprise = reprise.as_ref().and_then(|r| demande_de_reprise(state, r));
    let avec_reprise = depuis_la_reprise.is_some();
    let mut demande = depuis_la_reprise.unwrap_or_else(|| demande_sans_reprise(state, &e));
    if e.seek_ms.is_some() {
        demande.seek_ms = e.seek_ms;
    }
    info!(
        zone_id,
        reprise = avec_reprise,
        seek_ms = ?demande.seek_ms,
        titre = demande.title.as_deref().unwrap_or(""),
        "podcast_episode_par_adresse"
    );
    jouer_et_rendre_la_zone(state, zone_id, demande).await
}
