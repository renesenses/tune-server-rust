//! #6001 — liens sortants vers ProgArchives, Metal Archives et AllMusic, et
//! arbre des genres tiré de données libres (MusicBrainz + Wikidata, CC0).
//!
//! Voir `tune_core::metadata::liens_externes` et
//! `tune_core::metadata::arbre_genres` pour les règles et les sources.
//!
//! * `GET /library/artists/{id}/external-links`
//! * `GET /library/albums/{id}/external-links`
//!
//!   `{statut, mbid, liens: [{site, nom, url}]}`. `statut` : `ok`,
//!   `non_identifie` (pas d'identifiant MusicBrainz), `inconnu` (MusicBrainz
//!   ne le connaît pas), `indisponible` (MusicBrainz n'a pas répondu). Seule
//!   une réponse `ok` est gardée (24 h, `api_cache`).
//!
//! * `GET /library/genre-tree/musicbrainz`
//!
//!   `202 {statut: "en_preparation"}` tant que la liste des genres
//!   MusicBrainz n'a pas été lue — sa lecture part alors en tâche de fond,
//!   une seule à la fois. Ensuite `200 {statut: ok|partiel, source, licence,
//!   noeuds: [{genre, mbid, nom_musicbrainz, parents, sous_genres}],
//!   non_reconnus}`. L'arbre réglé par l'auditeur (`/genre-tree`) n'est pas
//!   touché.

use std::sync::atomic::{AtomicBool, Ordering};

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::metadata::arbre_genres;
use tune_core::metadata::liens_externes::{self, Entite, Issue};
use tune_core::metadata::wikidata;

use super::{api_cache_get, api_cache_set};
use crate::state::AppState;

fn cle_cache(entite: Entite, mbid: &str) -> String {
    let nom = match entite {
        Entite::Artiste => "artist",
        Entite::GroupeDeParutions => "release-group",
    };
    format!("cache:liens-externes:{nom}:{mbid}")
}

async fn reponse_liens(state: &AppState, entite: Entite, mbid: Option<String>) -> Response {
    let Some(mbid) = mbid.map(|m| m.trim().to_string()).filter(|m| !m.is_empty()) else {
        return Json(json!({"statut": "non_identifie", "mbid": null, "liens": []})).into_response();
    };
    let cle = cle_cache(entite, &mbid);
    if let Some(liens) = api_cache_get(&state.backend, &cle) {
        return Json(json!({"statut": "ok", "mbid": mbid, "liens": liens})).into_response();
    }
    let issue = liens_externes::liens_par(
        entite,
        &mbid,
        &liens_externes::lire_sur_musicbrainz_avec_creneau,
    )
    .await;
    let corps = match issue {
        Issue::Liens(liens) => {
            let liens = json!(liens);
            api_cache_set(&state.backend, &cle, &liens);
            json!({"statut": "ok", "mbid": mbid, "liens": liens})
        }
        Issue::Inconnu => json!({"statut": "inconnu", "mbid": mbid, "liens": []}),
        Issue::Indisponible => json!({"statut": "indisponible", "mbid": mbid, "liens": []}),
    };
    Json(corps).into_response()
}

/// `GET /library/artists/{id}/external-links`
pub(super) async fn liens_artiste(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    match ArtistRepo::with_backend(state.backend.clone()).get(id) {
        Ok(Some(a)) => reponse_liens(&state, Entite::Artiste, a.musicbrainz_id).await,
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

/// `GET /library/albums/{id}/external-links` — par le groupe de parutions :
/// c'est à ce niveau que MusicBrainz range ProgArchives et AllMusic pour un
/// album.
pub(super) async fn liens_album(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    match AlbumRepo::with_backend(state.backend.clone()).get(id) {
        Ok(Some(a)) => {
            reponse_liens(
                &state,
                Entite::GroupeDeParutions,
                a.musicbrainz_release_group_id,
            )
            .await
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Une seule lecture de la liste des genres à la fois, pour tout le serveur.
static LECTURE_INDEX_EN_COURS: AtomicBool = AtomicBool::new(false);

fn maintenant() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Remet l'indicateur à faux quand elle tombe : fin normale de la tâche,
/// panique (le futur est lâché pendant le déroulement de la pile) ou tâche
/// annulée à l'arrêt du runtime. Remis à la main en fin de tâche, il restait
/// levé après une panique, et plus aucune lecture ne partait jusqu'au
/// redémarrage : `/genre-tree/musicbrainz` répondait 202 pour toujours.
struct BaisserALaFin(&'static AtomicBool);

impl Drop for BaisserALaFin {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Lance `tache` si `indicateur` est baissé, et le lève pendant toute sa
/// vie. Rend `None` si une tâche tient déjà l'indicateur.
fn lancer_seule<F>(indicateur: &'static AtomicBool, tache: F) -> Option<tokio::task::JoinHandle<()>>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    if indicateur.swap(true, Ordering::SeqCst) {
        return None;
    }
    let garde = BaisserALaFin(indicateur);
    Some(tokio::spawn(async move {
        let _garde = garde;
        tache.await;
    }))
}

/// Lance la lecture de la liste des genres MusicBrainz si aucune n'est en
/// cours. Rend `true` si elle vient d'être lancée.
fn lancer_lecture_index(state: &AppState) -> bool {
    let backend = state.backend.clone();
    lancer_seule(&LECTURE_INDEX_EN_COURS, async move {
        match arbre_genres::lire_index_par(&arbre_genres::lire_page_de_genres).await {
            Ok(index) if !index.is_empty() => {
                tracing::info!(genres = index.len(), "genres_musicbrainz_liste_lue_6001");
                arbre_genres::garder_index(&backend, &index, maintenant());
            }
            Ok(_) => tracing::warn!("genres_musicbrainz_liste_vide_6001"),
            Err(()) => tracing::warn!("genres_musicbrainz_liste_illisible_6001"),
        }
    })
    .is_some()
}

/// `GET /library/genre-tree/musicbrainz`
pub(super) async fn arbre_musicbrainz(State(state): State<AppState>) -> Response {
    let t = maintenant();
    let Some(index) = arbre_genres::index_garde(&state.backend, t) else {
        lancer_lecture_index(&state);
        return (
            StatusCode::ACCEPTED,
            Json(json!({"statut": "en_preparation"})),
        )
            .into_response();
    };
    let locaux = super::genres::genres_de_la_bibliotheque(&state);
    let (trouves, non_reconnus) = arbre_genres::rapprocher(&locaux, &index);
    let client = state.http_client.clone();
    let lire = move |url: String| {
        let c = client.clone();
        async move { wikidata::lire_json(&c, url).await }
    };
    let (voisins, complet) = arbre_genres::voisins_par(&state.backend, &trouves, &lire, t).await;
    let noeuds = arbre_genres::assembler(&trouves, &index, &voisins);
    let corps: Value = json!({
        "statut": if complet { "ok" } else { "partiel" },
        "source": "MusicBrainz (genres), Wikidata (P8052, P279)",
        "licence": "CC0",
        "noeuds": noeuds,
        "non_reconnus": non_reconnus,
    });
    Json(corps).into_response()
}

#[cfg(test)]
#[path = "liens_et_genres_mb_tests.rs"]
mod tests;
