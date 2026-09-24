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

/// Ce qu'une identification d'album a donné, indépendamment du transport.
///
/// 🔴 Sortie de la route POUR ÊTRE APPELÉE DEUX FOIS (#4805). Le pilote de lot
/// de `identification_lot.rs` doit faire *exactement* ce que fait le bouton
/// « Ré-identifier » d'un album — même recherche, même appariement, mêmes
/// écritures, mêmes garde-fous. Recopier la chaîne aurait fabriqué deux
/// identifications qui divergent au premier correctif ; la route est
/// désormais une mise en forme JSON, et rien d'autre.
pub(super) struct Identification {
    pub verdict: &'static str,
    pub tracks_total: usize,
    pub was_identified_before: bool,
    pub previous_release_id: Option<String>,
    pub searched_title: String,
    pub searched_artist: String,
    /// Le pressage retenu. `None` sur `not_found` / `no_tracks`.
    pub meilleur: Option<musicbrainz_release::MBReleaseMatch>,
    /// Ce qui a été écrit. `None` sur `not_found` / `no_tracks`.
    pub applied: Option<tune_core::metadata::reidentify::AppliedIdentification>,
}

/// Pourquoi une identification n'a même pas pu être tentée. À distinguer d'un
/// `not_found`, qui est un résultat : ici, rien n'a été interrogé.
pub(super) enum EchecIdentification {
    AlbumIntrouvable,
    Base(String),
}

/// La chaîne complète pour UN album : recherche, détail, appariement, écriture.
///
/// Deux requêtes MusicBrainz, séparées par [`musicbrainz_release::rate_limit_delay`].
/// L'appelant qui enchaîne des albums doit ajouter SON propre délai entre deux
/// appels — celui d'ici ne couvre que l'intervalle interne.
pub(super) async fn identifier_album(
    state: &AppState,
    album_id: i64,
) -> Result<Identification, EchecIdentification> {
    let album_repo = AlbumRepo::with_backend(state.backend.clone());
    let album = match album_repo.get(album_id) {
        Ok(Some(a)) => a,
        Ok(None) => return Err(EchecIdentification::AlbumIntrouvable),
        Err(e) => return Err(EchecIdentification::Base(e.to_string())),
    };

    let track_repo = TrackRepo::with_backend(state.backend.clone());
    let tracks = track_repo.list_by_album(album_id).unwrap_or_default();
    if tracks.is_empty() {
        // Rien à ré-identifier, et surtout : ne rien effacer pour autant.
        return Ok(Identification {
            verdict: "no_tracks",
            tracks_total: 0,
            was_identified_before: false,
            previous_release_id: None,
            searched_title: album.title.clone(),
            searched_artist: String::new(),
            meilleur: None,
            applied: None,
        });
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
            return Err(EchecIdentification::Base(e));
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
        return Ok(Identification {
            verdict: "not_found",
            tracks_total: tracks.len(),
            was_identified_before: cleared.was_identified(),
            previous_release_id: cleared.release_id.clone(),
            searched_title: album.title.clone(),
            searched_artist: artist,
            meilleur: None,
            applied: None,
        });
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
            return Err(EchecIdentification::Base(e));
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

    Ok(Identification {
        verdict,
        tracks_total: locales.len(),
        was_identified_before: cleared.was_identified(),
        previous_release_id: cleared.release_id.clone(),
        searched_title: album.title.clone(),
        searched_artist: artist,
        meilleur: Some(meilleur),
        applied: Some(applied),
    })
}

pub(super) async fn reidentify_album(
    State(state): State<AppState>,
    Path(album_id): Path<i64>,
) -> impl IntoResponse {
    let issue = match identifier_album(&state, album_id).await {
        Ok(i) => i,
        Err(EchecIdentification::AlbumIntrouvable) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "album introuvable"})),
            )
                .into_response();
        }
        Err(EchecIdentification::Base(e)) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response();
        }
    };

    match (issue.verdict, &issue.meilleur, &issue.applied) {
        ("no_tracks", _, _) => Json(json!({
            "album_id": album_id,
            "verdict": "no_tracks",
            "tracks_total": 0,
        }))
        .into_response(),
        ("not_found", _, _) => Json(json!({
            "album_id": album_id,
            "verdict": "not_found",
            "tracks_total": issue.tracks_total,
            "previous_identification_restored": issue.was_identified_before,
            "searched_title": issue.searched_title,
            "searched_artist": issue.searched_artist,
        }))
        .into_response(),
        (verdict, Some(meilleur), Some(applied)) => Json(json!({
            "album_id": album_id,
            "verdict": verdict,
            "was_identified_before": issue.was_identified_before,
            "previous_release_id": issue.previous_release_id,
            "release_id": meilleur.release_id,
            "release_group_id": meilleur.release_group_id,
            "release_title": meilleur.title,
            "release_artist": meilleur.artist,
            "release_date": meilleur.date,
            "release_country": meilleur.country,
            "release_disambiguation": meilleur.disambiguation,
            "match_score": meilleur.score,
            "tracks_total": issue.tracks_total,
            "tracks_matched": applied.tracks_matched,
            "tracks_unmatched": applied.tracks_unmatched,
            // Ce que Tune a refusé d'écraser, nommément. Sans cette liste,
            // l'utilisateur croirait la ré-identification incomplète.
            "fields_left_as_is": applied.fields_left_as_is,
        }))
        .into_response(),
        // Inatteignable : un verdict posé sans pressage. On le dit au lieu de
        // rendre un corps muet.
        (verdict, _, _) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "verdict sans pressage", "verdict": verdict})),
        )
            .into_response(),
    }
}
