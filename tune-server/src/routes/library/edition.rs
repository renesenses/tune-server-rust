//! Le mode « Modifier » de la fiche album — GO de Bertrand du 25/09/2026,
//! chantier « édition et modification des albums, compilations et coffrets ».
//!
//! - `GET  /library/albums/{id}/edition` — la vue d'édition ;
//! - `PUT  /library/albums/{id}/edition` — tout modifier en une transaction ;
//! - `POST /library/albums/{id}/discs/attach` — `{ album_id }` devient le
//!   disque suivant ;
//! - `POST /library/albums/{id}/discs/{number}/detach` — le disque redevient
//!   un album séparé.
//!
//! La logique, et ce qui la rend durable face aux analyses, vit dans
//! [`tune_core::db::edition_album`] ; ces routes ne font que traduire.
//! Rien n'est écrit dans les FICHIERS par ces routes : c'est
//! `POST /library/albums/{id}/edition/write-tags` (tranche 4,
//! [`super::edition_balises`]) qui reporte l'édition dans les balises.
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use tune_core::db::edition_album::{self, Modification, RefusEdition};

use super::refus;
use crate::state::AppState;

fn refuser(e: RefusEdition) -> Response {
    match e {
        RefusEdition::AlbumInconnu(id) => refus(
            StatusCode::NOT_FOUND,
            "album_inconnu",
            format!("l'album {id} n'existe pas"),
        ),
        RefusEdition::Invalide { code, message } => {
            refus(StatusCode::UNPROCESSABLE_ENTITY, code, message)
        }
        RefusEdition::Base(m) => {
            tracing::warn!(erreur = %m, "edition_album_echec");
            refus(StatusCode::INTERNAL_SERVER_ERROR, "erreur_base", m)
        }
    }
}

/// La vue d'édition, ou 404.
fn vue(state: &AppState, id: i64) -> Response {
    match edition_album::lire_vue(&state.backend, id) {
        Ok(Some(v)) => Json(v).into_response(),
        Ok(None) => refuser(RefusEdition::AlbumInconnu(id)),
        Err(e) => refuser(RefusEdition::Base(e.to_string())),
    }
}

fn annoncer(state: &AppState, source: &str, id: i64) {
    state.event_bus.emit(
        tune_core::event_types::EventType::LibraryUpdated.as_str(),
        json!({ "source": source, "album_id": id }),
    );
}

/// `GET /library/albums/{id}/edition`.
pub(super) async fn lire(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    vue(&state, id)
}

/// `PUT /library/albums/{id}/edition` — rend la vue après modification.
pub(super) async fn modifier(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(corps): Json<Modification>,
) -> Response {
    if let Err(e) = edition_album::appliquer(&state.backend, id, &corps) {
        return refuser(e);
    }
    annoncer(&state, "edition_album", id);
    vue(&state, id)
}

#[derive(Deserialize)]
pub(super) struct Attache {
    album_id: i64,
}

/// `POST /library/albums/{id}/discs/attach` — rend la vue du coffret.
pub(super) async fn attacher(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(corps): Json<Attache>,
) -> Response {
    if let Err(e) = edition_album::attacher(&state.backend, id, corps.album_id) {
        return refuser(e);
    }
    annoncer(&state, "disque_attache", id);
    vue(&state, id)
}

/// `POST /library/albums/{id}/discs/{number}/detach` — rend la vue du
/// coffret, augmentée de `detached_album_id` (l'album recréé).
pub(super) async fn detacher(
    State(state): State<AppState>,
    Path((id, numero)): Path<(i64, i32)>,
) -> Response {
    let nouveau = match edition_album::detacher(&state.backend, id, numero) {
        Ok(n) => n,
        Err(e) => return refuser(e),
    };
    annoncer(&state, "disque_detache", id);
    match edition_album::lire_vue(&state.backend, id) {
        Ok(Some(v)) => {
            let mut rendu = serde_json::to_value(v).unwrap_or_default();
            rendu["detached_album_id"] = json!(nouveau);
            Json(rendu).into_response()
        }
        Ok(None) => refuser(RefusEdition::AlbumInconnu(id)),
        Err(e) => refuser(RefusEdition::Base(e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use tune_core::db::backend::DbBackend;

    async fn corps(r: Response) -> (StatusCode, Value) {
        let statut = r.status();
        let octets = axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            statut,
            serde_json::from_slice(&octets).unwrap_or(Value::Null),
        )
    }

    fn banc() -> AppState {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let b = &state.backend;
        b.execute(
            "INSERT INTO artists (id, name) VALUES (1, 'Keith Jarrett')",
            &[],
        )
        .unwrap();
        b.execute(
            "INSERT INTO albums (id, title, artist_id) VALUES (1, 'Köln', 1), (2, 'Bonus', 1)",
            &[],
        )
        .unwrap();
        b.execute(
            "INSERT INTO tracks (id, title, album_id, artist_id, disc_number, track_number, \
             duration_ms, file_path) VALUES \
             (11, 'a', 1, 1, 1, 1, 1000, '/k/1/01.flac'), \
             (12, 'b', 1, 1, 1, 2, 1000, '/k/1/02.flac'), \
             (21, 'c', 2, 1, 1, 1, 1000, '/k/2/01.flac')",
            &[],
        )
        .unwrap();
        state
    }

    /// Le contrat de l'écran, clé par clé : ce que le client web code en
    /// parallèle (web#1599).
    #[tokio::test]
    async fn get_put_attach_detach_rendent_la_forme_du_contrat() {
        let state = banc();
        let (s, v) = corps(lire(State(state.clone()), Path(1)).await).await;
        assert_eq!(s, StatusCode::OK);
        for cle in [
            "id",
            "title",
            "album_artist",
            "year",
            "label",
            "genre",
            "release_type",
            "cover_path",
            "compilation_mode",
            "compilation_effective",
            "coffret",
            "champs_edites",
        ] {
            assert!(v["album"].get(cle).is_some(), "album.{cle} manquant : {v}");
        }
        assert_eq!(
            v["discs"][0],
            json!({ "number": 1, "title": null, "cover_path": null, "track_count": 2 })
        );
        assert_eq!(
            v["tracks"][0],
            json!({ "id": 11, "disc_number": 1, "track_number": 1, "title": "a", "artist_name": "Keith Jarrett", "duration_ms": 1000 })
        );

        let (s, _) = corps(lire(State(state.clone()), Path(999)).await).await;
        assert_eq!(s, StatusCode::NOT_FOUND);

        // 422 explicite : une piste manque.
        let m: Modification =
            serde_json::from_value(json!({ "discs": [ { "number": 1, "track_ids": [11] } ] }))
                .unwrap();
        let (s, v) = corps(modifier(State(state.clone()), Path(1), Json(m)).await).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(v["error"], "piste_manquante");

        let m: Modification = serde_json::from_value(json!({
            "title": "Köln 1975",
            "compilation_mode": "non",
            "discs": [ { "number": 1, "title": "Concert", "track_ids": [12, 11] } ]
        }))
        .unwrap();
        let (s, v) = corps(modifier(State(state.clone()), Path(1), Json(m)).await).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["album"]["title"], "Köln 1975");
        assert_eq!(v["album"]["compilation_mode"], "non");
        assert_eq!(v["discs"][0]["title"], "Concert");
        assert_eq!(v["tracks"][0]["id"], 12);

        let (s, v) =
            corps(attacher(State(state.clone()), Path(1), Json(Attache { album_id: 2 })).await)
                .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["album"]["coffret"], "manuel");
        assert_eq!(v["discs"][1]["number"], 2);
        assert_eq!(v["tracks"][2]["id"], 21);

        let (s, v) = corps(detacher(State(state.clone()), Path((1, 2))).await).await;
        assert_eq!(s, StatusCode::OK);
        let nouveau = v["detached_album_id"].as_i64().expect("detached_album_id");
        assert_eq!(v["album"]["coffret"], Value::Null);
        let (_, d) = corps(lire(State(state.clone()), Path(nouveau)).await).await;
        assert_eq!(d["tracks"][0]["id"], 21, "la piste garde son identifiant");
        assert_eq!(d["album"]["title"], "Köln 1975");

        let (s, v) = corps(detacher(State(state.clone()), Path((1, 1))).await).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(v["error"], "un_seul_disque");
    }
}
