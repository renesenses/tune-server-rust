//! `POST /library/albums/{id}/reidentify` — refaire l'identification d'UN album.
//!
//! Le geste que réclamait le fil forum #1455 (#2128). Jusqu'ici, un album mal
//! identifié l'était pour de bon : toute l'écriture d'enrichissement est
//! `COALESCE`, donc le mauvais MBID en place empêchait à jamais une nouvelle
//! correspondance. Le seul contournement connu — dupliquer le dossier sous un
//! autre nom, rescanner, supprimer l'original — passait par le système de
//! fichiers et faisait perdre favoris, notes et historique.
//!
//! # Ce que fait la route
//!
//! 1. Relève et efface les trois clés d'identification de cet album
//!    ([`tune_core::metadata::reidentify::clear_album_identification`]).
//! 2. Interroge MusicBrainz **au grain de l'album** : une recherche de
//!    pressage, puis un détail avec sa liste de pistes. Deux requêtes en tout,
//!    quel que soit le nombre de pistes — et non deux par piste comme la passe
//!    de fond, ce qui rend l'opération tenable dans le temps d'une requête HTTP.
//! 3. Pose le résultat : les clés en remplacement, le descriptif en
//!    remplissage seul.
//! 4. Si rien n'est trouvé, **repose l'identification d'avant** et le dit.
//!
//! # Bornes
//!
//! L'effet ne sort pas de l'album demandé. Aucun scan n'est déclenché, aucune
//! passe de fond n'est lancée, aucune tâche d'arrière-plan n'est enregistrée :
//! tout se joue dans la requête, sur les lignes de cet album. Les seules
//! écritures sont des `UPDATE` portant `WHERE id = ?` sur `albums` et
//! `WHERE ... AND album_id = ?` sur `tracks` (voir le module `reidentify` de
//! `tune-core`). Aucune ligne n'est créée ni supprimée, donc aucun `id` ne
//! bouge, donc favoris, notes, historique, listes de lecture et collections —
//! qui s'y rattachent tous par `id` — sont intacts par construction.
//!
//! # Le retour
//!
//! Un verdict explicite, jamais un silence : `reidentified`, `unchanged`,
//! `not_found`, `no_tracks`. « Retomber sur le même pressage » est un résultat
//! à part entière, et c'est même l'information la plus utile — elle dit à
//! l'utilisateur que la source en ligne confirme, et donc que l'erreur est
//! ailleurs (souvent dans les balises de ses propres fichiers).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::json;
use tracing::{info, warn};

use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::track_repo::TrackRepo;
use tune_core::metadata::musicbrainz_release;
use tune_core::metadata::reidentify::{
    LocalTrack, apply_album_identification, clear_album_identification, map_recording_ids,
    restore_album_identification,
};

use crate::state::AppState;

/// Combien de pressages candidats on regarde. On n'en retient qu'un — le
/// mieux classé — mais en demander plusieurs laisse au classement de quoi
/// travailler.
const CANDIDATS: usize = 5;

pub(super) async fn reidentify_album(
    State(state): State<AppState>,
    Path(album_id): Path<i64>,
) -> impl IntoResponse {
    let album_repo = AlbumRepo::with_backend(state.backend.clone());
    let album = match album_repo.get(album_id) {
        Ok(Some(a)) => a,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "album introuvable"})),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };

    let track_repo = TrackRepo::with_backend(state.backend.clone());
    let tracks = track_repo.list_by_album(album_id).unwrap_or_default();
    if tracks.is_empty() {
        // Rien à ré-identifier, et surtout : ne rien effacer pour autant.
        return Json(json!({
            "album_id": album_id,
            "verdict": "no_tracks",
            "tracks_total": 0,
        }))
        .into_response();
    }

    // L'artiste à interroger : celui de l'album quand il est connu, sinon
    // celui de la première piste. Une compilation sans artiste d'album ne doit
    // pas partir avec une chaîne vide, qui rendrait la recherche inexploitable.
    let artist = album
        .artist_name
        .clone()
        .or_else(|| tracks.iter().find_map(|t| t.artist_name.clone()))
        .unwrap_or_default();

    // 1. Effacer, en gardant le calque de ce qu'on efface.
    let cleared = match clear_album_identification(&state.backend, album_id) {
        Ok(c) => c,
        Err(e) => {
            warn!(album_id, error = %e, "reidentify_clear_failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response();
        }
    };

    // 2. Chercher le pressage. Volontairement SANS le MBID d'avant : c'est lui
    //    qu'on soupçonne, et le scan a pu le lire dans des balises fausses
    //    (`scan_import.rs:443`). On repart du titre et de l'artiste.
    let candidats = musicbrainz_release::lookup_release_candidates(
        &album.title,
        &artist,
        Some(tracks.len() as u32),
        CANDIDATS,
    )
    .await;

    let Some(meilleur) = candidats.into_iter().next() else {
        // Rien trouvé : l'album doit se retrouver EXACTEMENT comme avant.
        if let Err(e) = restore_album_identification(&state.backend, album_id, &cleared) {
            warn!(album_id, error = %e, "reidentify_restore_failed");
        }
        info!(album_id, title = %album.title, "reidentify_not_found");
        return Json(json!({
            "album_id": album_id,
            "verdict": "not_found",
            "tracks_total": tracks.len(),
            "previous_identification_restored": cleared.was_identified(),
            "searched_title": album.title,
            "searched_artist": artist,
        }))
        .into_response();
    };

    musicbrainz_release::rate_limit_delay().await;
    let detail = musicbrainz_release::lookup_release_detail(&meilleur.release_id).await;

    // 3. Associer les pistes du pressage aux pistes locales.
    let locales: Vec<LocalTrack> = tracks
        .iter()
        .filter_map(|t| {
            Some(LocalTrack {
                id: t.id?,
                disc: t.disc_number,
                position: t.track_number,
                title: t.title.clone(),
            })
        })
        .collect();
    let recordings = match detail.as_ref() {
        Some(d) => map_recording_ids(&locales, &d.tracks),
        None => Vec::new(),
    };

    // 4. Poser.
    let applied = match apply_album_identification(
        &state.backend,
        album_id,
        &meilleur.release_id,
        meilleur.release_group_id.as_deref(),
        &recordings,
        locales.len(),
        detail.as_ref(),
    ) {
        Ok(a) => a,
        Err(e) => {
            warn!(album_id, error = %e, "reidentify_apply_failed");
            // Ne pas laisser l'album à moitié effacé.
            if let Err(e2) = restore_album_identification(&state.backend, album_id, &cleared) {
                warn!(album_id, error = %e2, "reidentify_restore_failed");
            }
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response();
        }
    };

    // 5. Le verdict. « Le même pressage qu'avant » n'est pas un échec, mais ce
    //    n'est pas non plus une correction : il faut le distinguer.
    let meme_pressage = cleared.release_id.as_deref() == Some(meilleur.release_id.as_str());
    let verdict = if meme_pressage {
        "unchanged"
    } else {
        "reidentified"
    };

    info!(
        album_id,
        verdict,
        release_id = %meilleur.release_id,
        matched = applied.tracks_matched,
        "reidentify_done"
    );

    Json(json!({
        "album_id": album_id,
        "verdict": verdict,
        "was_identified_before": cleared.was_identified(),
        "previous_release_id": cleared.release_id,
        "release_id": meilleur.release_id,
        "release_group_id": meilleur.release_group_id,
        "release_title": meilleur.title,
        "release_artist": meilleur.artist,
        "release_date": meilleur.date,
        "release_country": meilleur.country,
        "release_disambiguation": meilleur.disambiguation,
        "match_score": meilleur.score,
        "tracks_total": locales.len(),
        "tracks_matched": applied.tracks_matched,
        "tracks_unmatched": applied.tracks_unmatched,
        // Ce que Tune a refusé d'écraser, nommément. Sans cette liste,
        // l'utilisateur croirait la ré-identification incomplète.
        "fields_left_as_is": applied.fields_left_as_is,
    }))
    .into_response()
}
