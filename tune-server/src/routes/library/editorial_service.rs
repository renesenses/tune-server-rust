//! #8022 — la bio d'un artiste et la note d'un album LOCAUX prises chez Qobuz,
//! à la demande, pour un compte Qobuz connecté.
//!
//! Place dans l'ordre des routes `/bio` (#7027, #7930) : bio stockée dans la
//! langue, cache, site, PUIS ce texte, PUIS un repli dans une autre langue.
//! Les routes n'appellent ces fonctions que si le site n'a rien rendu dans la
//! langue demandée.
//!
//! - Aucun appel si Qobuz n'est pas activé ET connecté (`utilisable`).
//! - Correspondance sûre ou rien : voir
//!   [`tune_core::streaming::editorial_bibliotheque`].
//! - Le résultat de la recherche (texte retenu, ou « rien ») est gardé 24 h
//!   dans le cache d'API : une fiche rouverte ne relance pas Qobuz.
//! - Le texte n'est rangé dans la fiche locale (`bio`, `bio_source = qobuz`,
//!   `bio_lang`, lien vers la page Qobuz) que si elle n'a AUCUNE bio : une bio
//!   d'une autre source n'est jamais écrasée. `cloud::bio_sync` ne l'envoie
//!   jamais au fonds communautaire (droits des textes éditoriaux).

use std::time::Duration;

use serde_json::{Value, json};
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::models::{Album, Artist};
use tune_core::db::track_repo::TrackRepo;
use tune_core::streaming::editorial_bibliotheque::{
    self, ALBUMS_EXAMINES, CodesAlbum, EditorialTrouve,
};

use super::now_iso_utc;
use crate::state::AppState;

/// Le service interrogé. TIDAL suivra (issue séparée) par la même voie.
pub(super) const SERVICE: &str = "qobuz";

/// ISRC relevés au plus par album local.
const ISRC_PAR_ALBUM: usize = 3;

/// Délai au-delà duquel la fiche n'attend plus Qobuz.
const DELAI: Duration = Duration::from_secs(8);

/// Les codes d'un album local : son code-barres, et quelques ISRC de ses
/// titres.
pub(super) fn codes_de_l_album(state: &AppState, album: &Album) -> CodesAlbum {
    let upc = album
        .barcode
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .map(str::to_string);
    let mut isrcs: Vec<String> = Vec::new();
    if let Some(id) = album.id {
        let pistes = TrackRepo::with_backend(state.backend.clone())
            .list_by_album(id)
            .unwrap_or_default();
        for p in pistes {
            if let Some(i) = p.isrc.as_deref().map(str::trim).filter(|i| !i.is_empty())
                && !isrcs.iter().any(|x| x.eq_ignore_ascii_case(i))
            {
                isrcs.push(i.to_string());
                if isrcs.len() >= ISRC_PAR_ALBUM {
                    break;
                }
            }
        }
    }
    CodesAlbum { upc, isrcs }
}

/// Le corps rendu par une route `/bio` pour un texte Qobuz : même forme que
/// celle d'une bio stockée, `source` à plat compris, et `bio_provenance`
/// (`source = "qobuz"`, lien vers la page Qobuz, langue annoncée, aucune
/// licence libre).
pub(super) fn corps(cle: &str, nom: &str, e: &EditorialTrouve) -> Value {
    json!({
        cle: nom,
        "bio": e.texte,
        "source": e.source,
        "bio_provenance": {
            "source": e.source,
            "source_url": e.source_url,
            "license": Value::Null,
            "lang": e.langue,
            "fetched_at": now_iso_utc(),
        },
    })
}

fn depuis_le_cache(v: &Value) -> Option<Option<EditorialTrouve>> {
    if v.get("rien").and_then(Value::as_bool) == Some(true) {
        return Some(None);
    }
    Some(Some(EditorialTrouve {
        texte: v.get("texte")?.as_str()?.to_string(),
        langue: v.get("langue")?.as_str()?.to_string(),
        source: v.get("source")?.as_str()?.to_string(),
        source_url: v
            .get("source_url")
            .and_then(Value::as_str)
            .map(str::to_string),
    }))
}

fn vers_le_cache(e: Option<&EditorialTrouve>) -> Value {
    match e {
        None => json!({ "rien": true }),
        Some(e) => json!({
            "texte": e.texte,
            "langue": e.langue,
            "source": e.source,
            "source_url": e.source_url,
        }),
    }
}

enum Cible<'a> {
    Artiste(&'a Artist),
    Album(&'a Album),
}

async fn chercher(state: &AppState, cible: Cible<'_>) -> Option<EditorialTrouve> {
    let (cle_cache, id) = match &cible {
        Cible::Artiste(a) => ("artist", a.id?),
        Cible::Album(a) => ("album", a.id?),
    };
    // Règle 2 : rien sans compte connecté — pas même une lecture du cache,
    // qui rendrait encore le texte d'un compte déconnecté.
    let svc = {
        let registre = state.services.lock().await;
        registre.get(SERVICE)?
    };
    if !svc.read().await.utilisable().await {
        return None;
    }
    let cle = format!("cache:editorial:{SERVICE}:{cle_cache}:{id}");
    if let Some(v) = super::api_cache_get(&state.backend, &cle)
        && let Some(e) = depuis_le_cache(&v)
    {
        return e;
    }
    let recherche = async {
        let svc = svc.read().await;
        match &cible {
            Cible::Album(album) => {
                let codes = codes_de_l_album(state, album);
                editorial_bibliotheque::note_d_album(&**svc, &codes).await
            }
            Cible::Artiste(artist) => {
                let albums: Vec<CodesAlbum> = AlbumRepo::with_backend(state.backend.clone())
                    .list_by_artist(id)
                    .unwrap_or_default()
                    .iter()
                    .map(|a| codes_de_l_album(state, a))
                    .filter(|c| !c.est_vide())
                    .take(ALBUMS_EXAMINES)
                    .collect();
                editorial_bibliotheque::bio_d_artiste(&**svc, &artist.name, &albums).await
            }
        }
    };
    match tokio::time::timeout(DELAI, recherche).await {
        Ok(Ok(trouve)) => {
            super::api_cache_set(&state.backend, &cle, &vers_le_cache(trouve.as_ref()));
            tracing::debug!(
                id,
                cible = cle_cache,
                trouve = trouve.is_some(),
                "editorial_service_recherche"
            );
            trouve
        }
        // Une panne n'est pas une absence : rien n'est mis en cache, la
        // prochaine ouverture réessaie. Le message ne porte ni jeton ni texte.
        Ok(Err(e)) => {
            tracing::info!(id, cible = cle_cache, error = %e, "editorial_service_echec");
            None
        }
        Err(_) => {
            tracing::info!(id, cible = cle_cache, "editorial_service_delai_depasse");
            None
        }
    }
}

/// La bio Qobuz d'un artiste local, rangée dans sa fiche si elle n'en a
/// aucune.
pub(super) async fn bio_d_artiste(state: &AppState, artist: &Artist) -> Option<EditorialTrouve> {
    let e = chercher(state, Cible::Artiste(artist)).await?;
    if artist.bio.as_deref().is_none_or(|b| b.trim().is_empty())
        && let Some(id) = artist.id
        && let Err(err) =
            tune_core::db::artist_repo::ArtistRepo::with_backend(state.backend.clone())
                .update_bio_full(id, &e.texte, &e.source, e.source_url.clone(), "", &e.langue)
    {
        tracing::warn!(id, error = %err, "editorial_service_ecriture_artiste");
    }
    Some(e)
}

/// La note Qobuz d'un album local, rangée dans sa fiche s'il n'en a aucune.
pub(super) async fn note_d_album(state: &AppState, album: &Album) -> Option<EditorialTrouve> {
    let e = chercher(state, Cible::Album(album)).await?;
    if album.bio.as_deref().is_none_or(|b| b.trim().is_empty())
        && let Some(id) = album.id
        && let Err(err) = AlbumRepo::with_backend(state.backend.clone()).update_bio_full(
            id,
            &e.texte,
            &e.source,
            e.source_url.clone(),
            "",
            &e.langue,
        )
    {
        tracing::warn!(id, error = %err, "editorial_service_ecriture_album");
    }
    Some(e)
}
