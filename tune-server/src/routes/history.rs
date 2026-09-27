use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tune_http_types::panne_sql::OuDefautJournalise;

use tune_core::db::history_repo::HistoryRepo;
use tune_core::db::track_repo::TrackRepo;

use crate::state::AppState;

#[derive(Deserialize)]
struct HistoryParams {
    limit: Option<i64>,
    offset: Option<i64>,
    #[allow(dead_code)]
    period: Option<String>,
}

#[derive(Deserialize)]
struct DashboardParams {
    period: Option<String>,
    zone_id: Option<i64>,
    profile_id: Option<i64>,
    top_n: Option<i64>,
}

#[derive(Deserialize)]
struct SlotParams {
    period: Option<String>,
    weekday: i64,
    hour: i64,
    limit: Option<i64>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(recent_history).delete(clear_history))
        .route("/top-tracks", get(top_tracks))
        .route("/tracks/{id}/plays", get(track_plays))
        .route("/top-artists", get(top_artists))
        .route("/top-albums", get(top_albums))
        .route("/dashboard", get(dashboard))
        .route("/at", get(slot_tracks))
        .route("/export", get(export_csv))
}

/// GET /library/history/tracks/{id}/plays — how many times this track was
/// played (non-radio), so the UI can show a play count on a track (Progman,
/// feature #1056). Matched by title + artist, like the dashboard top tracks.
async fn track_plays(State(state): State<AppState>, Path(id): Path<i64>) -> Json<Value> {
    let track_repo = TrackRepo::with_backend(state.backend.clone());
    let (title, artist) = match track_repo.get(id) {
        Ok(Some(t)) => (t.title, t.artist_name),
        _ => return Json(json!({ "track_id": id, "plays": 0 })),
    };
    let plays = HistoryRepo::with_backend(state.backend.clone())
        .track_plays(&title, artist.as_deref())
        .unwrap_or(0);
    Json(json!({ "track_id": id, "plays": plays }))
}

/// Tracks listened during one weekday×hour heatmap cell (drill-down).
async fn slot_tracks(State(state): State<AppState>, Query(p): Query<SlotParams>) -> Json<Value> {
    let period = p.period.as_deref().unwrap_or("30d");
    let limit = p.limit.unwrap_or(50).clamp(1, 500);
    let repo = HistoryRepo::with_backend(state.backend.clone());
    match repo.history_at_slot(period, p.weekday, p.hour, limit) {
        Ok(items) => Json(json!({
            "weekday": p.weekday,
            "hour": p.hour,
            "period": period,
            "tracks": items,
        })),
        Err(e) => {
            tracing::warn!(error = %e, weekday = p.weekday, hour = p.hour, "history_slot_failed");
            Json(json!({ "weekday": p.weekday, "hour": p.hour, "period": period, "tracks": [] }))
        }
    }
}

async fn recent_history(
    State(state): State<AppState>,
    Query(p): Query<HistoryParams>,
) -> Json<Value> {
    let limit = p.limit.unwrap_or(50);
    let offset = p.offset.unwrap_or(0);
    let repo = HistoryRepo::with_backend(state.backend.clone());
    let (items, total) = repo.recent_paginated(limit, offset).unwrap_or_default();
    let ids: Vec<i64> = items.iter().filter_map(|item| item.id).collect();
    let names = repo.playlist_context_names(&ids).ou_defaut_journalise();
    let items: Vec<Value> = items
        .into_iter()
        .map(|item| {
            let name = item.id.and_then(|id| names.get(&id));
            let mut value = json!(item);
            value["context_name"] = json!(name);
            value
        })
        .collect();
    Json(json!({
        "items": items,
        "total": total,
        "limit": limit,
        "offset": offset,
    }))
}

async fn clear_history(State(state): State<AppState>) -> Json<Value> {
    let repo = HistoryRepo::with_backend(state.backend.clone());
    match repo.clear() {
        Ok(()) => Json(json!({ "status": "ok" })),
        Err(e) => {
            tracing::error!(error = %e, "clear_history_failed");
            Json(json!({ "status": "error", "detail": e }))
        }
    }
}

async fn top_albums(State(state): State<AppState>, Query(p): Query<HistoryParams>) -> Json<Value> {
    let limit = p.limit.unwrap_or(20);
    let repo = HistoryRepo::with_backend(state.backend.clone());
    let items: Vec<Value> = repo
        .top_albums(limit)
        .unwrap_or_default()
        .into_iter()
        .map(|(title, artist, plays)| {
            json!({ "album_title": title, "artist_name": artist, "plays": plays })
        })
        .collect();
    Json(json!(items))
}

async fn top_tracks(State(state): State<AppState>, Query(p): Query<HistoryParams>) -> Json<Value> {
    let limit = p.limit.unwrap_or(20);
    let repo = HistoryRepo::with_backend(state.backend.clone());
    let items = repo.top_tracks(limit).unwrap_or_default();
    Json(json!(items))
}

async fn top_artists(State(state): State<AppState>, Query(p): Query<HistoryParams>) -> Json<Value> {
    let limit = p.limit.unwrap_or(20);
    let engine = state.backend.engine();
    let p1 = if engine == tune_core::db::engine::Engine::Postgres {
        "$1"
    } else {
        "?"
    };
    // `ar.image_path` : l'image de l'artiste de BIBLIOTHÈQUE, quand la fiche
    // en a une. Elle dépend de `ar.id` seul, le regroupement ne change donc
    // pas (#5165).
    let sql = format!(
        "SELECT lh.artist_name, COUNT(*) as plays, ar.id as artist_id, ar.image_path \
         FROM listen_history lh \
         LEFT JOIN artists ar ON LOWER(lh.artist_name) = LOWER(ar.name) \
         WHERE lh.artist_name IS NOT NULL \
         GROUP BY lh.artist_name, ar.id, ar.image_path \
         ORDER BY plays DESC \
         LIMIT {p1}"
    );
    use tune_core::db::backend::ToSqlValue;
    let rows = state
        .backend
        .query_many(&sql, &[&limit as &dyn ToSqlValue])
        .ou_defaut_journalise();

    // 🔴 #5165 — un artiste écouté SEULEMENT sur un service (Qobuz…) n'a pas
    // de fiche dans `artists` : `artist_id` nul, et aucune image. Son image se
    // demande au service, par une écoute de lui qu'on a déjà.
    let sans_image: Vec<String> = rows
        .iter()
        .filter(|cols| {
            cols.get(3)
                .and_then(|v| v.as_string())
                .is_none_or(|i| i.trim().is_empty())
        })
        .filter_map(|cols| cols.first().and_then(|v| v.as_string()))
        .collect();
    let images_service = images_d_artistes_de_service::resoudre(&state, &sans_image).await;

    let items: Vec<Value> = rows
        .iter()
        .map(|cols| {
            let nom = cols.first().and_then(|v| v.as_string()).unwrap_or_default();
            let image_path = cols
                .get(3)
                .and_then(|v| v.as_string())
                .filter(|i| !i.trim().is_empty())
                .or_else(|| images_service.get(&nom.to_lowercase()).cloned());
            json!({
                "name": nom,
                "artist_name": nom,
                "plays": cols.get(1).and_then(|v| v.as_i64()).unwrap_or(0),
                "artist_id": cols.get(2).and_then(|v| v.as_i64()),
                "id": cols.get(2).and_then(|v| v.as_i64()),
                "image_path": image_path,
            })
        })
        .collect();
    Json(json!(items))
}

/// L'image d'un artiste chez le service qui l'a fait écouter (#5165).
///
/// Alex Campbell, 26/09/2026 : sur l'accueil de l'app iPad, AUCUN des huit
/// « Top Artistes » n'avait de photo, dont plusieurs écoutés sur Qobuz
/// seulement. `top_artists` ne rendait pas d'`image_path`, et un artiste
/// absent de la table `artists` n'y a ni fiche ni image.
///
/// L'historique garde, pour chaque écoute de service, `source` et
/// `source_id` (l'identifiant de la PISTE). La piste donne l'identifiant de
/// l'artiste chez ce service (`StreamTrack::artist_id`), l'artiste donne son
/// image (`StreamArtist::image_path`, la forme que rendent déjà la recherche
/// et les fiches artiste de service). Rien n'est deviné par le nom : une
/// piste dont l'artiste ne porte pas le nom affiché ne donne aucune image.
///
/// Deux appels réseau par artiste : le résultat est mémorisé (un jour s'il
/// a été trouvé, une heure sinon), et la route n'attend pas plus de
/// [`BUDGET`] — ce qui n'est pas prêt arrive à l'appel suivant.
mod images_d_artistes_de_service {
    use std::collections::HashMap;
    use std::sync::{Arc, LazyLock, Mutex};
    use std::time::{Duration, Instant};

    use futures_util::StreamExt;
    use tune_core::db::backend::{DbBackend, ToSqlValue};
    use tune_core::streaming::ServiceRegistry;

    use crate::state::AppState;

    /// Ce que la route accepte d'attendre du réseau.
    pub(super) const BUDGET: Duration = Duration::from_secs(3);
    const DUREE_TROUVEE: Duration = Duration::from_secs(24 * 3600);
    const DUREE_ABSENTE: Duration = Duration::from_secs(3600);
    const EN_PARALLELE: usize = 6;

    /// Le moment du constat, et l'image — ou son absence constatée.
    type Constat = (Instant, Option<String>);

    /// Nom d'artiste en minuscules → constat.
    static MEMOIRE: LazyLock<Mutex<HashMap<String, Constat>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));

    fn memorisee(cle: &str) -> Option<Option<String>> {
        let memoire = MEMOIRE.lock().ok()?;
        let (quand, image) = memoire.get(cle)?;
        let duree = if image.is_some() {
            DUREE_TROUVEE
        } else {
            DUREE_ABSENTE
        };
        (quand.elapsed() < duree).then(|| image.clone())
    }

    fn memoriser(cle: &str, image: Option<String>) {
        if let Ok(mut memoire) = MEMOIRE.lock() {
            memoire.insert(cle.to_string(), (Instant::now(), image));
        }
    }

    /// Pour chaque nom (en minuscules), les écoutes de service qui peuvent
    /// le rattacher à une fiche : `(service, identifiant de piste)`.
    fn ancres(db: &Arc<dyn DbBackend>, cles: &[String]) -> HashMap<String, Vec<(String, String)>> {
        let mut out: HashMap<String, Vec<(String, String)>> = HashMap::new();
        if cles.is_empty() {
            return out;
        }
        let params: Vec<Box<dyn ToSqlValue>> = cles
            .iter()
            .map(|n| Box::new(n.clone()) as Box<dyn ToSqlValue>)
            .collect();
        let refs: Vec<&dyn ToSqlValue> = params.iter().map(|p| p.as_ref()).collect();
        let dans = vec!["?"; cles.len()].join(", ");
        let sql = format!(
            "SELECT LOWER(artist_name), source, MAX(source_id) FROM listen_history \
             WHERE source_id IS NOT NULL AND source NOT IN ('local', 'radio', 'upnp') \
             AND LOWER(artist_name) IN ({dans}) \
             GROUP BY LOWER(artist_name), source"
        );
        match db.query_many(&sql, &refs) {
            Ok(rows) => {
                for r in rows {
                    if let (Some(n), Some(src), Some(id)) = (
                        r.first().and_then(|v| v.as_string()),
                        r.get(1).and_then(|v| v.as_string()),
                        r.get(2).and_then(|v| v.as_string()),
                    ) {
                        out.entry(n).or_default().push((src, id));
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "top_artistes_ancres_de_service_illisibles");
            }
        }
        out
    }

    /// L'image de l'artiste `cle` chez `service`, par la piste `piste`.
    async fn chez_le_service(
        services: &tokio::sync::Mutex<ServiceRegistry>,
        cle: &str,
        service: &str,
        piste: &str,
    ) -> Option<String> {
        let svc = services.lock().await.get(service)?;
        let svc = svc.read().await;
        if !svc.utilisable().await {
            return None;
        }
        let t = match svc.get_track(piste).await {
            Ok(t) => t,
            Err(e) => {
                tracing::debug!(service, piste, error = %e, "top_artistes_piste_de_service_injoignable");
                return None;
            }
        };
        // L'identifiant ne vaut que pour l'artiste que la piste NOMME.
        if t.artist.trim().to_lowercase() != cle.trim() {
            return None;
        }
        let artiste = t.artist_id.filter(|a| !a.trim().is_empty())?;
        match svc.get_artist(&artiste).await {
            Ok(a) => a.image_path.filter(|i| !i.trim().is_empty()),
            Err(e) => {
                tracing::debug!(service, artiste, error = %e, "top_artistes_artiste_de_service_injoignable");
                None
            }
        }
    }

    /// Les images trouvées pour `noms`, par nom en minuscules.
    pub(super) async fn resoudre(state: &AppState, noms: &[String]) -> HashMap<String, String> {
        let mut out = HashMap::new();
        let mut a_resoudre: Vec<String> = Vec::new();
        for nom in noms {
            let cle = nom.to_lowercase();
            match memorisee(&cle) {
                Some(Some(image)) => {
                    out.insert(cle, image);
                }
                Some(None) => {}
                None if !a_resoudre.contains(&cle) => a_resoudre.push(cle),
                None => {}
            }
        }
        if a_resoudre.is_empty() {
            return out;
        }
        let ancres = ancres(&state.backend, &a_resoudre);
        if ancres.is_empty() {
            return out;
        }

        // Tâche détachée : ce qui dépasse le budget continue et remplit la
        // mémoire pour l'appel suivant, au lieu d'être jeté.
        let services = state.services.clone();
        let tache = tokio::spawn(async move {
            futures_util::stream::iter(ancres)
                .for_each_concurrent(EN_PARALLELE, |(cle, ecoutes)| {
                    let services = services.clone();
                    async move {
                        let mut image = None;
                        for (service, piste) in &ecoutes {
                            image = chez_le_service(&services, &cle, service, piste).await;
                            if image.is_some() {
                                break;
                            }
                        }
                        memoriser(&cle, image);
                    }
                })
                .await;
        });
        if tokio::time::timeout(BUDGET, tache).await.is_err() {
            tracing::info!(
                artistes = a_resoudre.len(),
                "top_artistes_images_de_service_hors_budget — la suite servira l'appel suivant"
            );
        }
        for cle in a_resoudre {
            if let Some(Some(image)) = memorisee(&cle) {
                out.insert(cle, image);
            }
        }
        out
    }
}

// View scope, NOT action identity: this handler reads the profile from the
// explicit `?profile_id=` query param and deliberately does NOT use the
// `ActiveProfile` extractor. A present param means "this profile's stats"
// (strict, NULL rows excluded in full_dashboard); an absent param means "the
// household total" (all rows, NULL included). Adding a header fallback here
// would flip that default to per-profile the moment api.ts starts sending
// X-Profile-Id on every request. See ActiveProfile's convention doc.
async fn dashboard(State(state): State<AppState>, Query(p): Query<DashboardParams>) -> Json<Value> {
    let period = p.period.as_deref().unwrap_or("30d");
    let top_n = p.top_n.unwrap_or(10);
    let repo = HistoryRepo::with_backend(state.backend.clone());
    match repo.full_dashboard(period, p.zone_id, p.profile_id, top_n) {
        Ok(data) => Json(json!(data)),
        Err(e) => {
            tracing::error!(error = %e, period, "full_dashboard_failed");
            Json(json!({
                "period": period,
                "range": { "from": null, "to": "" },
                "totals": { "plays": 0, "listening_ms": 0, "unique_tracks": 0, "unique_artists": 0 },
                "top_artists": [],
                "top_albums": [],
                "top_tracks": [],
                "trend": [],
                "hourly": [],
                "by_zone": [],
                "by_source": [],
                "completion": { "completed": 0, "skipped": 0, "avg_listened_ms": 0, "avg_track_duration_ms": 0 }
            }))
        }
    }
}

async fn export_csv(
    State(state): State<AppState>,
    Query(p): Query<HistoryParams>,
) -> impl axum::response::IntoResponse {
    let limit = p.limit.unwrap_or(10000);
    let repo = HistoryRepo::with_backend(state.backend.clone());
    let (items, _) = repo.recent_paginated(limit, 0).unwrap_or_default();

    let mut csv = String::from("title,artist,album,source,duration_ms,listened_at,zone_id\n");
    for item in &items {
        let title = item.title.replace(',', ";");
        let artist = item.artist_name.as_deref().unwrap_or("").replace(',', ";");
        let album = item.album_title.as_deref().unwrap_or("").replace(',', ";");
        let source = &item.source;
        let dur = item.duration_ms;
        let listened = item.listened_at.as_deref().unwrap_or("");
        let zone = item.zone_id.unwrap_or(0);
        csv.push_str(&format!(
            "{title},{artist},{album},{source},{dur},{listened},{zone}\n"
        ));
    }

    (
        axum::http::StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, "text/csv; charset=utf-8"),
            (
                axum::http::header::CONTENT_DISPOSITION,
                "attachment; filename=\"tune-history.csv\"",
            ),
        ],
        csv,
    )
}

#[cfg(test)]
#[path = "top_artistes_image_i5165_tests.rs"]
mod top_artistes_image_i5165_tests;

#[cfg(test)]
mod historique_4041_tests {
    use super::*;

    #[tokio::test]
    async fn la_route_historique_sert_la_pochette_persistee() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        state.backend.execute(
            "INSERT INTO listen_history (title, source, source_id, duration_ms, cover_url, context_type, context_id, context_position) VALUES ('Service', 'qobuz', '123', 90000, 'https://static.qobuz.com/cover.jpg', 'album', 'album-qobuz', 2)",
            &[],
        ).unwrap();
        let Json(body) = recent_history(
            State(state),
            Query(HistoryParams {
                limit: Some(20),
                offset: Some(0),
                period: None,
            }),
        )
        .await;
        assert_eq!(body["total"], 1);
        assert_eq!(
            body["items"][0]["cover_url"], "https://static.qobuz.com/cover.jpg",
            "GET /library/history a perdu la pochette déjà stockée (#4041)"
        );
        assert_eq!(body["items"][0]["album_id"], Value::Null);
        assert_eq!(body["items"][0]["context_id"], "album-qobuz");
        assert_eq!(body["items"][0]["context_position"], 2);
    }
}
