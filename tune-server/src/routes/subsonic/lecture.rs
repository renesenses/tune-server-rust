//! Lecture (#7945, étape 2) : stream, download et getCoverArt.
//!
//! `stream` et `download` servent le fichier d'origine par la MÊME fonction
//! que `/api/v1/library/tracks/{id}/audio` (`Range`, 206, 416, choix
//! d'exemplaire, chemin du disque, images ISO). Aucun transcodage : `format`,
//! `maxBitRate` et `estimateContentLength` sont ignorés.
//!
//! `getCoverArt` sert l'image elle-même : la route de `/api/v1` redirige vers
//! le cache de pochettes, une adresse authentifiée qu'un client Subsonic ne
//! peut pas suivre.

use axum::Extension;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::{IntoResponse, Response};
use tune_core::db::artist_repo::ArtistRepo;

use super::identifiants::{Genre, decoder, decoder_un};
use super::navigation::{garde, piste_visible};
use super::reponse::{self, CodeErreur};
use super::{ClientSubsonic, Parametres, format_de, parametre};
use crate::routes::library::{self, PochetteAlbum};
use crate::state::AppState;

async fn servir(
    state: AppState,
    client: ClientSubsonic,
    params: Parametres,
    entetes: HeaderMap,
    telechargement: bool,
) -> Response {
    let format = format_de(&params);
    let Some(texte) = parametre(&params, "id") else {
        return reponse::erreur(
            format,
            CodeErreur::ParametreManquant,
            "Required parameter is missing: id",
        );
    };
    let garde = match garde(&state, &client, format) {
        Ok(g) => g,
        Err(r) => return r,
    };
    let Some(id) =
        decoder(Genre::Piste, texte).filter(|id| piste_visible(&state, &garde, *id).is_some())
    else {
        return reponse::erreur(format, CodeErreur::Introuvable, "Song not found");
    };
    let mut r = library::stream_track_audio(State(state), Path(id), entetes)
        .await
        .into_response();
    if telechargement {
        r.headers_mut().insert(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_static("attachment"),
        );
    }
    r
}

pub(super) async fn stream(
    State(state): State<AppState>,
    Extension(client): Extension<ClientSubsonic>,
    Query(params): Query<Parametres>,
    entetes: HeaderMap,
) -> Response {
    servir(state, client, params, entetes, false).await
}

pub(super) async fn download(
    State(state): State<AppState>,
    Extension(client): Extension<ClientSubsonic>,
    Query(params): Query<Parametres>,
    entetes: HeaderMap,
) -> Response {
    servir(state, client, params, entetes, true).await
}

pub(super) async fn get_cover_art(
    State(state): State<AppState>,
    Extension(client): Extension<ClientSubsonic>,
    Query(params): Query<Parametres>,
) -> Response {
    let format = format_de(&params);
    let Some(texte) = parametre(&params, "id") else {
        return reponse::erreur(
            format,
            CodeErreur::ParametreManquant,
            "Required parameter is missing: id",
        );
    };
    let garde = match garde(&state, &client, format) {
        Ok(g) => g,
        Err(r) => return r,
    };
    let taille = parametre(&params, "size").and_then(|s| s.trim().parse::<u32>().ok());
    let introuvable = || reponse::erreur(format, CodeErreur::Introuvable, "Cover art not found");

    let pochette = match decoder_un(texte) {
        Some((Genre::Album, id)) => {
            let repo = tune_core::db::album_repo::AlbumRepo::with_backend(state.backend.clone());
            match repo.get(id).ok().flatten() {
                Some(a) if !garde.album_bloque(&a) => library::pochette_de_l_album(&state, id),
                _ => None,
            }
        }
        Some((Genre::Piste, id)) => piste_visible(&state, &garde, id)
            .and_then(|t| t.album_id)
            .and_then(|al| library::pochette_de_l_album(&state, al)),
        Some((Genre::Artiste, id)) => ArtistRepo::with_backend(state.backend.clone())
            .get(id)
            .ok()
            .flatten()
            .filter(|a| !garde.artiste_bloque(id, &a.name))
            .and_then(|a| a.image_path)
            .map(|chemin| {
                if chemin.starts_with("http") {
                    PochetteAlbum::Distante(chemin)
                } else if library::artwork_is_hex_hash(&chemin) {
                    PochetteAlbum::EnCache(chemin)
                } else {
                    PochetteAlbum::EnCache(tune_core::library::artwork::artwork_hash(&chemin))
                }
            }),
        None => None,
    };
    match pochette {
        Some(PochetteAlbum::Distante(url)) => {
            axum::response::Redirect::temporary(&url).into_response()
        }
        Some(PochetteAlbum::EnCache(hash)) => {
            let r = library::serve_artwork_from(&library::artwork_cache_dir(), &hash, taille).await;
            if r.status().is_success() {
                r
            } else {
                introuvable()
            }
        }
        None => introuvable(),
    }
}
