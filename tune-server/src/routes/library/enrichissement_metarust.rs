//! Lot 1 de l'enrichissement d'après MetaRust, de Xavier Joly (#4805) : les
//! routes de la fiche d'album.
//!
//! | route | effet |
//! |---|---|
//! | `GET  /library/albums/{id}/completeness/musicbrainz` | pistes manquantes / en double, hors réseau |
//! | `GET  /library/albums/incomplete` | les albums à qui il manque des pistes (filtre) |
//! | `POST /library/albums/{id}/instruments` | instruments Wikidata des musiciens du disque |
//! | `GET  /library/albums/{id}/discogs` | propositions Discogs gardées |
//! | `POST /library/albums/{id}/discogs/search` | cherche par code-barres / n° de catalogue |
//! | `POST /library/albums/{id}/discogs/{discogs_id}/accept` | remplit le VIDE seulement |
//! | `POST /library/albums/{id}/discogs/{discogs_id}/reject` | |
//! | `GET  /library/albums/{id}/bpm` | BPM par piste et leur source |
//! | `POST /library/albums/{id}/bpm/estimate` | Wikidata, puis le signal |
//!
//! Aucune n'écrit dans un fichier ; aucune ne remplace une donnée de
//! MusicBrainz ou une correction faite à la main.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::audio::tempo;
use tune_core::metadata::completude::{self, Etat};
use tune_core::metadata::discogs_propositions::{self as discogs, Empechement};
use tune_core::metadata::wikidata;

use crate::state::AppState;

/// Plafond d'artistes interrogés par un geste sur UN album.
const ARTISTES_PAR_ALBUM: usize = 60;

// ── Complétude ──────────────────────────────────────────────────────────────

pub(super) fn completude_json(etat: Etat) -> Value {
    match etat {
        Etat::NonIdentifie => json!({ "status": "not_identified" }),
        Etat::ReleaseNonGardee { release_mbid } => {
            json!({ "status": "release_not_cached", "release_mbid": release_mbid })
        }
        Etat::Calculee {
            release_mbid,
            completude,
        } => json!({
            "status": if completude.incomplet() { "incomplete" } else if completude.a_des_ecarts() { "duplicates" } else { "complete" },
            "release_mbid": release_mbid,
            "expected": completude.attendues,
            "present": completude.presentes,
            "missing": completude.manquantes,
            "duplicates": completude.doublons,
            "outside_release": completude.hors_release,
        }),
    }
}

/// `GET /library/albums/{id}/completeness/musicbrainz` (la route `…/completeness`
/// historique compte les balises ; celle-ci compare à l'édition MusicBrainz)
pub(super) async fn album_completeness(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Json<Value> {
    let backend = state.backend.clone();
    let etat = tokio::task::spawn_blocking(move || completude::pour_album(&backend, id))
        .await
        .unwrap_or(Etat::NonIdentifie);
    Json(completude_json(etat))
}

/// `GET /library/albums/incomplete`
pub(super) async fn albums_incomplete(State(state): State<AppState>) -> Json<Value> {
    let backend = state.backend.clone();
    let ids = tokio::task::spawn_blocking(move || completude::albums_incomplets(&backend))
        .await
        .unwrap_or_default();
    Json(json!({ "total": ids.len(), "ids": ids }))
}

// ── Instruments (Wikidata) ──────────────────────────────────────────────────

/// `POST /library/albums/{id}/instruments`
pub(super) async fn album_instruments(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Json<Value> {
    let client = state.http_client.clone();
    let bilan = wikidata::completer_instruments_par(
        &state.backend,
        Some(id),
        ARTISTES_PAR_ALBUM,
        move |url| {
            let c = client.clone();
            async move { wikidata::lire_json(&c, url).await }
        },
    )
    .await;
    Json(json!({ "album_id": id, "result": bilan }))
}

// ── Discogs ─────────────────────────────────────────────────────────────────

/// Le transport Discogs du serveur : client HTTP partagé, limiteur partagé,
/// jeton PERSONNEL d'abord. `Err` = panne (le jeton n'y figure jamais).
pub(super) async fn lire_discogs(
    client: reqwest::Client,
    jeton: Option<String>,
    url: String,
) -> Result<Option<Value>, String> {
    use tune_core::http::fetch::{CLE_DISCOGS, DISCOGS};
    DISCOGS.acquire(CLE_DISCOGS).await;
    let mut req = client
        .get(&url)
        .header(
            reqwest::header::USER_AGENT,
            tune_core::metadata::musicbrainz_release::MB_UA,
        )
        .timeout(std::time::Duration::from_secs(20));
    if let Some(t) = jeton {
        req = req.header(reqwest::header::AUTHORIZATION, format!("Discogs token={t}"));
    }
    let r = req.send().await.map_err(|_| "transport".to_string())?;
    DISCOGS.constater_reponse(CLE_DISCOGS, &r);
    match r.status().as_u16() {
        404 => Ok(None),
        s if (200..300).contains(&s) => r.json().await.map(Some).map_err(|_| "body".into()),
        s => Err(format!("status_{s}")),
    }
}

/// `GET /library/albums/{id}/discogs`
pub(super) async fn album_discogs(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Json<Value> {
    let backend = state.backend.clone();
    let liste = tokio::task::spawn_blocking(move || discogs::lister(&backend, id))
        .await
        .unwrap_or_default();
    Json(json!({ "album_id": id, "proposals": liste }))
}

/// `POST /library/albums/{id}/discogs/search`
pub(super) async fn album_discogs_search(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Response {
    let st = state.clone();
    let jeton = tokio::task::spawn_blocking(move || crate::routes::discogs::discogs_token(&st))
        .await
        .ok()
        .flatten();
    // La recherche par code-barres exige un jeton (API Discogs).
    if jeton.is_none() {
        return (
            StatusCode::PRECONDITION_FAILED,
            Json(json!({"error": "discogs_token not configured"})),
        )
            .into_response();
    }
    let client = state.http_client.clone();
    let resultat = discogs::chercher_par(&state.backend, id, move |url| {
        lire_discogs(client.clone(), jeton.clone(), url)
    })
    .await;
    match resultat {
        Ok(p) => Json(json!({ "album_id": id, "proposal": p })).into_response(),
        Err(e) => {
            let (statut, code) = match e {
                Empechement::AlbumInconnu => (StatusCode::NOT_FOUND, "unknown_album"),
                Empechement::DejaIdentifie => (StatusCode::CONFLICT, "already_identified"),
                Empechement::SansCode => (StatusCode::UNPROCESSABLE_ENTITY, "no_barcode_or_catno"),
                Empechement::Panne => (StatusCode::BAD_GATEWAY, "discogs_unavailable"),
            };
            (statut, Json(json!({ "error": code }))).into_response()
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct CorpsAcceptation {
    /// Poser aussi les crédits Discogs sur les pistes qui n'en ont aucun.
    #[serde(default)]
    credits: bool,
}

/// `POST /library/albums/{id}/discogs/{discogs_id}/accept`
pub(super) async fn album_discogs_accept(
    State(state): State<AppState>,
    Path((id, discogs_id)): Path<(i64, u64)>,
    corps: Option<Json<CorpsAcceptation>>,
) -> Response {
    let avec_credits = corps.map(|Json(c)| c.credits).unwrap_or(false);
    let backend = state.backend.clone();
    match tokio::task::spawn_blocking(move || {
        discogs::accepter(&backend, id, discogs_id, avec_credits)
    })
    .await
    {
        Ok(Ok(bilan)) => Json(json!({ "accepted": true, "result": bilan })).into_response(),
        Ok(Err(e)) => (StatusCode::CONFLICT, Json(json!({ "error": e }))).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// `POST /library/albums/{id}/discogs/{discogs_id}/reject`
pub(super) async fn album_discogs_reject(
    State(state): State<AppState>,
    Path((id, discogs_id)): Path<(i64, u64)>,
) -> Response {
    let backend = state.backend.clone();
    match tokio::task::spawn_blocking(move || discogs::rejeter(&backend, id, discogs_id)).await {
        Ok(Ok(())) => Json(json!({ "rejected": true })).into_response(),
        Ok(Err(e)) => (StatusCode::CONFLICT, Json(json!({ "error": e }))).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

// ── BPM ─────────────────────────────────────────────────────────────────────

/// `GET /library/albums/{id}/bpm`
pub(super) async fn album_bpm(State(state): State<AppState>, Path(id): Path<i64>) -> Json<Value> {
    let backend = state.backend.clone();
    let pistes = tokio::task::spawn_blocking(move || tempo::bpm_de_l_album(&backend, id))
        .await
        .unwrap_or_default();
    Json(json!({ "album_id": id, "tracks": pistes }))
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct CorpsEstimation {
    /// Remesurer aussi les pistes déjà estimées.
    #[serde(default)]
    redo: bool,
}

/// Estime les BPM d'un album : Wikidata (P1725), puis le signal décodé par
/// Tune (Rust natif). Le chemin UNIQUE du geste sur la fiche et de la passe de
/// lot (`POST /library/bpm-all`), pour qu'ils ne puissent pas diverger.
///
/// `ceder` : avant chaque décodage, attendre son tour (pause de
/// l'enrichissement, puis cession à la lecture). La passe de lot seulement.
pub(super) async fn estimer_les_bpm(
    state: &AppState,
    id: i64,
    refaire: bool,
    ceder: bool,
) -> tempo::BilanBpm {
    let client = state.http_client.clone();
    tempo::estimer_l_album_par(
        &state.backend,
        id,
        refaire,
        move |rid, works| {
            let c = client.clone();
            async move {
                wikidata::bpm_de_l_enregistrement_par(&rid, &works, &|url| {
                    let c = c.clone();
                    async move { wikidata::lire_json(&c, url).await }
                })
                .await
            }
        },
        move |chemin| async move {
            if ceder {
                tune_core::taches_de_fond::attendre_son_tour(
                    tune_core::taches_de_fond::Tache::Enrichissement,
                )
                .await;
            }
            tempo::mesurer_bpm_du_fichier(&chemin).await
        },
    )
    .await
}

/// `POST /library/albums/{id}/bpm/estimate`
pub(super) async fn album_bpm_estimate(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    corps: Option<Json<CorpsEstimation>>,
) -> Json<Value> {
    let refaire = corps.map(|Json(c)| c.redo).unwrap_or(false);
    // Le geste sur UNE fiche ne cède pas à la lecture : l'utilisateur l'a
    // demandé maintenant. Seule la passe de lot cède.
    let bilan = estimer_les_bpm(&state, id, refaire, false).await;
    let backend = state.backend.clone();
    let pistes = tokio::task::spawn_blocking(move || tempo::bpm_de_l_album(&backend, id))
        .await
        .unwrap_or_default();
    Json(json!({ "album_id": id, "result": bilan, "tracks": pistes }))
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use serde_json::Value;
    use tower::ServiceExt;
    use tune_core::db::backend::ToSqlValue;

    use crate::state::AppState;

    async fn get(etat: &AppState, chemin: &str) -> (StatusCode, Value) {
        let r = super::super::router()
            .with_state(etat.clone())
            .oneshot(Request::get(chemin).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let s = r.status();
        let o = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        (s, serde_json::from_slice(&o).unwrap_or(Value::Null))
    }

    /// La fiche dit « il manque la piste 2 du disque 2 » sans aucune requête :
    /// la release gardée en base suffit, et le filtre la retrouve.
    #[tokio::test]
    async fn la_fiche_et_le_filtre_disent_ce_qui_manque() {
        let e = AppState::new(":memory:", 0, Default::default()).unwrap();
        let b = &e.backend;
        b.execute(
            "INSERT INTO albums (id, title, musicbrainz_release_id) VALUES (3, 'Double', 'rel-3')",
            &[],
        )
        .unwrap();
        for (id, d, n) in [(31_i64, 1_i64, 1_i64), (32, 2, 1)] {
            b.execute(
                "INSERT INTO tracks (id, title, album_id, disc_number, track_number, file_path) \
                 VALUES (?, ?, 3, ?, ?, ?)",
                &[
                    &id as &dyn ToSqlValue,
                    &format!("T{d}-{n}") as &dyn ToSqlValue,
                    &d as &dyn ToSqlValue,
                    &n as &dyn ToSqlValue,
                    &format!("/m/{id}.flac") as &dyn ToSqlValue,
                ],
            )
            .unwrap();
        }
        tune_core::metadata::musicbrainz_release_cache::ecrire(
            b,
            "rel-3",
            tune_core::metadata::musicbrainz_release::INC_RELEASE_COMPLET,
            &serde_json::json!({"id": "rel-3", "title": "Double", "media": [
                {"position": 1, "tracks": [{"position": 1, "title": "T1-1"}]},
                {"position": 2, "tracks": [
                    {"position": 1, "title": "T2-1"}, {"position": 2, "title": "T2-2"}]}
            ]}),
            chrono::Utc::now(),
        );
        let (s, v) = get(&e, "/albums/3/completeness/musicbrainz").await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["status"], "incomplete");
        assert_eq!(v["missing"][0]["disque"], 2);
        assert_eq!(v["missing"][0]["position"], 2);
        let (_, f) = get(&e, "/albums/incomplete").await;
        assert_eq!(f["ids"], serde_json::json!([3]));
    }
}
