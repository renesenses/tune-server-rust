//! #6288 — MetaRust, « Albums manquants » : la discographie MusicBrainz d'un
//! artiste identifié, moins la bibliothèque. Règles et choix prudents :
//! `tune_core::metadata::albums_manquants`.
//!
//! `GET /library/artists/{id}/missing-albums[?secondaires=1]` →
//! `{statut, mbid, manquants: [{mbid, titre, type_primaire,
//! types_secondaires, premiere_sortie}], possedes, retenus, tronquee}`.
//! `statut` : `ok`, `non_identifie` (l'artiste n'a pas d'identifiant
//! MusicBrainz), `inconnu` (MusicBrainz ne le connaît pas), `indisponible`
//! (MusicBrainz n'a pas répondu — rien n'est gardé), `sans_album` (aucun
//! album de l'artiste dans la bibliothèque : rien n'est comparé).
//!
//! Une bibliothèque illisible n'est PAS un statut : `500 {code:
//! "bibliotheque_illisible", error}`, sans liste.
//!
//! La bibliothèque comparée : les albums dont l'artiste est l'ARTISTE DE
//! L'ALBUM (`albums.artist_id`), masqués exclus — la même liste que sa fiche.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::metadata::albums_manquants::{self as am, AlbumPossede, Lecture};

use crate::state::AppState;

/// Le `code` d'une erreur de lecture de la bibliothèque : stable, traduit par
/// l'écran. Le texte de `error` est réservé au journal et aux outils.
pub(super) const CODE_BIBLIOTHEQUE_ILLISIBLE: &str = "bibliotheque_illisible";

#[derive(Deserialize)]
pub(super) struct ManquantsQuery {
    /// `1` / `true` : live, compilations, etc. compris.
    secondaires: Option<String>,
}

/// `GET /library/artists/{id}/missing-albums`
pub(super) async fn albums_manquants(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<ManquantsQuery>,
) -> Response {
    let Ok(Some(artiste)) = ArtistRepo::with_backend(state.backend.clone()).get(id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(mbid) = artiste
        .musicbrainz_id
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
    else {
        return Json(json!({"statut": "non_identifie", "mbid": null, "manquants": []}))
            .into_response();
    };
    let avec_secondaires = matches!(q.secondaires.as_deref(), Some("1" | "true"));
    let maintenant = chrono::Utc::now().timestamp();
    let discographie = match am::discographie_par(
        &state.backend,
        &mbid,
        &am::lire_page_de_discographie,
        maintenant,
    )
    .await
    {
        Lecture::Lue(d) => d,
        Lecture::Inconnu => {
            return Json(json!({"statut": "inconnu", "mbid": mbid, "manquants": []}))
                .into_response();
        }
        Lecture::Indisponible => {
            return Json(json!({"statut": "indisponible", "mbid": mbid, "manquants": []}))
                .into_response();
        }
    };
    // Une bibliothèque illisible n'est pas une bibliothèque vide : la prendre
    // pour vide annoncerait toute la discographie « manquante » (relecture
    // de #6636). L'erreur remonte comme une ERREUR — 500, avec un `code`
    // stable que l'écran traduit — et sans aucune liste.
    let albums = match AlbumRepo::with_backend(state.backend.clone()).list_by_artist(id) {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(artist_id = id, erreur = %e, "albums_manquants_bibliotheque_illisible_6288");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(crate::message_api::corps_refus(
                    CODE_BIBLIOTHEQUE_ILLISIBLE,
                    "api.library.readFailedNoComparison",
                    &[],
                )),
            )
                .into_response();
        }
    };
    // Aucun album de l'artiste dans la bibliothèque (il n'y figure que par des
    // pistes d'autres albums) : comparer annoncerait toute sa discographie.
    // On ne compare pas, et on le dit.
    if albums.is_empty() {
        return Json(json!({"statut": "sans_album", "mbid": mbid, "manquants": []}))
            .into_response();
    }
    let bibliotheque: Vec<AlbumPossede> = albums
        .into_iter()
        .map(|a| AlbumPossede {
            titre: a.title,
            groupe_mbid: a.musicbrainz_release_group_id,
        })
        .collect();
    let bilan = am::comparer(&discographie, &bibliotheque, avec_secondaires);
    Json(json!({
        "statut": "ok",
        "mbid": mbid,
        "manquants": bilan.manquants,
        "possedes": bilan.possedes,
        "retenus": bilan.retenus,
        "tronquee": bilan.tronquee,
    }))
    .into_response()
}

#[cfg(test)]
#[path = "albums_manquants_mb_tests.rs"]
mod tests;
