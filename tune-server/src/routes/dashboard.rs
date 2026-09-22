use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tune_http_types::panne_sql::OuDefautJournalise;

use tune_core::db::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use tune_core::db::history_repo::HistoryRepo;

use crate::error::AppError;
use crate::state::AppState;

#[derive(Deserialize)]
struct DashParams {
    limit: Option<i64>,
    days: Option<i64>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/stats", get(dashboard_stats))
        .route("/top-artists", get(top_artists))
        .route("/top-albums", get(top_albums))
        .route("/top-tracks", get(top_tracks))
        .route("/genre-breakdown", get(genre_breakdown))
        .route("/listening-history", get(listening_history))
        .route("/wrapped", get(wrapped))
}

/// Les albums de la bibliothèque qui ont été ÉCOUTÉS, avec leurs colonnes de
/// genre — une ligne par album distinct.
///
/// `listen_history` ne porte AUCUNE colonne genre : on passe par l'album de
/// l'écoute, `listen_history.album_id` s'il est connu, sinon l'album de la
/// piste écoutée. Une écoute de service (Qobuz…) sans album local n'a pas de
/// genre connu : elle ne compte pas — limite assumée, dite dans #4527.
///
/// `genres` est `TEXT` sous SQLite comme sous PostgreSQL : le `DISTINCT` sur
/// les deux colonnes est portable.
const ALBUMS_ECOUTES: &str = "SELECT DISTINCT al.genre, al.genres \
     FROM listen_history lh \
     LEFT JOIN tracks t ON t.id = lh.track_id \
     JOIN albums al ON al.id = COALESCE(lh.album_id, t.album_id)";

/// Le nombre de GENRES ÉCOUTÉS — #4527.
///
/// 🔴 Même notion de genre que `GET /library/genres`, par la MÊME fonction
/// (`genres_de_l_album`) : les deux chiffres voisinent sur l'accueil et
/// doivent se comparer.
///
/// `None` si la requête échoue — et le champ est alors ABSENT de la réponse,
/// pas mis à zéro. Un « 0 » serait un mensonge que l'écran afficherait tel
/// quel ; un champ absent, l'écran sait le taire.
fn genres_ecoutes(state: &AppState) -> Option<i64> {
    let lignes = match state.backend.query_many(ALBUMS_ECOUTES, &[]) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(error = %e, "dashboard_genres_ecoutes_error");
            return None;
        }
    };
    let mut cles: std::collections::HashSet<String> = std::collections::HashSet::new();
    for l in &lignes {
        let genre = l.first().and_then(|v| v.as_string());
        let genres = l.get(1).and_then(|v| v.as_string());
        for (cle, _) in
            crate::routes::library::genres_de_l_album(genre.as_deref(), genres.as_deref())
        {
            cles.insert(cle);
        }
    }
    Some(cles.len() as i64)
}

async fn dashboard_stats(State(state): State<AppState>) -> Json<Value> {
    let repo = HistoryRepo::with_backend(state.backend.clone());
    match repo.dashboard() {
        Ok(s) => {
            let mut v = json!(s);
            if let Some(n) = genres_ecoutes(&state) {
                v["unique_genres"] = json!(n);
            }
            Json(v)
        }
        Err(e) => {
            tracing::warn!(error = %e, "dashboard_stats_error");
            Json(json!({
                "total_listens": 0,
                "total_duration_ms": 0,
                "unique_tracks": 0,
                "unique_artists": 0,
            }))
        }
    }
}

async fn top_artists(State(state): State<AppState>, Query(p): Query<DashParams>) -> Json<Value> {
    let limit = p.limit.unwrap_or(20);
    let repo = HistoryRepo::with_backend(state.backend.clone());
    let items: Vec<Value> = repo
        .top_artists(limit)
        .unwrap_or_default()
        .into_iter()
        .map(|(name, plays)| json!({ "name": name, "plays": plays }))
        .collect();
    Json(json!(items))
}

async fn top_tracks(State(state): State<AppState>, Query(p): Query<DashParams>) -> Json<Value> {
    let limit = p.limit.unwrap_or(20);
    let repo = HistoryRepo::with_backend(state.backend.clone());
    let items = repo.top_tracks(limit).unwrap_or_default();
    Json(json!(items))
}

async fn top_albums(State(state): State<AppState>, Query(p): Query<DashParams>) -> Json<Value> {
    let limit = p.limit.unwrap_or(20);
    let repo = HistoryRepo::with_backend(state.backend.clone());
    let items: Vec<Value> = repo
        .top_albums(limit)
        .unwrap_or_default()
        .into_iter()
        .map(|(title, artist, plays)| json!({ "album_title": title, "artist_name": artist, "plays": plays }))
        .collect();
    Json(json!(items))
}

async fn listening_history(
    State(state): State<AppState>,
    Query(p): Query<DashParams>,
) -> Json<Value> {
    let days = p.days.unwrap_or(30);
    let repo = HistoryRepo::with_backend(state.backend.clone());
    let items: Vec<Value> = repo
        .listening_history(days)
        .unwrap_or_default()
        .into_iter()
        .map(|(day, play_count, total_ms)| {
            json!({
                "day": day,
                "play_count": play_count,
                "total_listened_ms": total_ms,
                "hours": (total_ms as f64 / 3_600_000.0 * 100.0).round() / 100.0,
            })
        })
        .collect();
    Json(json!(items))
}

async fn genre_breakdown(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let rows = state
        .backend
        .query_many(
            "SELECT genre, genres FROM tracks WHERE (genre IS NOT NULL AND genre != '') OR (genres IS NOT NULL AND genres != '')",
            &[],
        )
        .map_err(|e| AppError::internal(e))?;

    let mut counts: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    for cols in &rows {
        let genre_col = cols.first().and_then(|v| v.as_string());
        let genres_col = cols.get(1).and_then(|v| v.as_string());

        let mut genres_for_track: Vec<String> = Vec::new();
        if let Some(json_str) = &genres_col {
            if let Ok(arr) = serde_json::from_str::<Vec<String>>(json_str) {
                genres_for_track = arr
                    .into_iter()
                    .map(|g| g.trim().to_string())
                    .filter(|g| !g.is_empty())
                    .collect();
            }
        }
        if genres_for_track.is_empty() {
            if let Some(raw_genre) = &genre_col {
                genres_for_track = tune_core::metadata::split_genre_tag(raw_genre);
            }
        }
        for g in genres_for_track {
            *counts.entry(g).or_insert(0) += 1;
        }
    }

    let mut sorted: Vec<(String, i64)> = counts.into_iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1));
    sorted.truncate(30);

    let items: Vec<Value> = sorted
        .iter()
        .map(|(genre, count)| json!({ "genre": genre, "count": count }))
        .collect();

    Ok(Json(json!(items)))
}

fn is_consecutive_days(a: &str, b: &str) -> bool {
    fn to_days(s: &str) -> Option<i64> {
        let parts: Vec<&str> = s.split('-').collect();
        if parts.len() != 3 {
            return None;
        }
        let y: i64 = parts[0].parse().ok()?;
        let m: i64 = parts[1].parse().ok()?;
        let d: i64 = parts[2].parse().ok()?;
        Some(y * 366 + m * 31 + d)
    }
    match (to_days(a), to_days(b)) {
        (Some(da), Some(db)) => db - da == 1,
        _ => false,
    }
}

#[derive(Deserialize)]
struct WrappedParams {
    year: Option<i32>,
}

async fn wrapped(
    State(state): State<AppState>,
    Query(p): Query<WrappedParams>,
) -> Result<Json<Value>, AppError> {
    let year = p.year.unwrap_or(2026);
    let year_start = format!("{year}-01-01");
    let year_end = format!("{}-01-01", year + 1);
    let b = &state.backend;

    let date_trunc_day = |col: &str| match b.engine() {
        Engine::Sqlite => SqliteDialect.date_trunc_day(col),
        Engine::Postgres => PostgresDialect.date_trunc_day(col),
    };

    let row = b
        .query_one(
            "SELECT COUNT(*), COALESCE(SUM(duration_ms), 0) FROM listen_history WHERE listened_at >= ? AND listened_at < ?",
            &[&year_start as &dyn tune_core::db::backend::ToSqlValue, &year_end],
        )
        .map_err(|e| AppError::internal(e))?
        .unwrap_or_default();
    let total_listens = row.first().and_then(|v| v.as_i64()).unwrap_or(0);
    let total_ms = row.get(1).and_then(|v| v.as_i64()).unwrap_or(0);
    let total_hours = (total_ms as f64 / 3_600_000.0 * 10.0).round() / 10.0;

    let top_artists: Vec<Value> = b
        .query_many(
            "SELECT artist_name, COUNT(*) as plays FROM listen_history \
             WHERE listened_at >= ? AND listened_at < ? AND artist_name IS NOT NULL \
             GROUP BY artist_name ORDER BY plays DESC LIMIT 10",
            &[
                &year_start as &dyn tune_core::db::backend::ToSqlValue,
                &year_end,
            ],
        )
        .ou_defaut_journalise()
        .into_iter()
        .map(|cols| {
            json!({
                "artist": cols.first().and_then(|v| v.as_string()).unwrap_or_default(),
                "plays": cols.get(1).and_then(|v| v.as_i64()).unwrap_or(0),
            })
        })
        .collect();

    let top_tracks: Vec<Value> = b
        .query_many(
            "SELECT title, artist_name, COUNT(*) as plays FROM listen_history \
             WHERE listened_at >= ? AND listened_at < ? \
             GROUP BY title, artist_name ORDER BY plays DESC LIMIT 10",
            &[
                &year_start as &dyn tune_core::db::backend::ToSqlValue,
                &year_end,
            ],
        )
        .ou_defaut_journalise()
        .into_iter()
        .map(|cols| {
            json!({
                "title": cols.first().and_then(|v| v.as_string()).unwrap_or_default(),
                "artist": cols.get(1).and_then(|v| v.as_string()),
                "plays": cols.get(2).and_then(|v| v.as_i64()).unwrap_or(0),
            })
        })
        .collect();

    let day_expr = date_trunc_day("listened_at");
    let days_sql = format!(
        "SELECT DISTINCT {day_expr} as d FROM listen_history \
         WHERE listened_at >= ? AND listened_at < ? ORDER BY 1"
    );
    let days: Vec<String> = b
        .query_many(
            &days_sql,
            &[
                &year_start as &dyn tune_core::db::backend::ToSqlValue,
                &year_end,
            ],
        )
        .ou_defaut_journalise()
        .into_iter()
        .filter_map(|cols| cols.first().and_then(|v| v.as_string()))
        .collect();

    let mut max_streak = if days.is_empty() { 0u32 } else { 1u32 };
    let mut current_streak = 1u32;
    for w in days.windows(2) {
        if is_consecutive_days(&w[0], &w[1]) {
            current_streak += 1;
        } else {
            max_streak = max_streak.max(current_streak);
            current_streak = 1;
        }
    }
    max_streak = max_streak.max(current_streak);

    let stats_row = b
        .query_one(
            "SELECT COUNT(DISTINCT artist_name), COUNT(DISTINCT COALESCE(title, '') || COALESCE(artist_name, '')) \
             FROM listen_history WHERE listened_at >= ? AND listened_at < ?",
            &[&year_start as &dyn tune_core::db::backend::ToSqlValue, &year_end],
        )
        .unwrap_or(None)
        .unwrap_or_default();
    let unique_artists = stats_row.first().and_then(|v| v.as_i64()).unwrap_or(0);
    let unique_tracks = stats_row.get(1).and_then(|v| v.as_i64()).unwrap_or(0);

    Ok(Json(json!({
        "year": year,
        "total_listens": total_listens,
        "total_hours": total_hours,
        "unique_artists": unique_artists,
        "unique_tracks": unique_tracks,
        "max_streak_days": max_streak,
        "top_artists": top_artists,
        "top_tracks": top_tracks,
    })))
}

/// Les GENRES ÉCOUTÉS de `/dashboard/stats` — #4527.
///
/// Chantier accueil, Bertrand, 19/09/2026 : la ligne de chiffres de l'accueil
/// devient configurable, et la carte « Genres » existe aussi en version
/// écoutée. Elle voisine avec « Genres » de la bibliothèque (115 sur le .18) :
/// les deux doivent compter de la même façon.
#[cfg(test)]
mod genres_ecoutes_4527 {
    use super::*;

    fn etat() -> AppState {
        AppState::new(":memory:", 0, Default::default()).unwrap()
    }

    /// Un album au genre donné — `genres` est le tableau JSON, `genre` la
    /// colonne héritée.
    fn album(state: &AppState, titre: &str, genre: Option<&str>, genres: Option<&str>) -> i64 {
        use tune_core::db::album_repo::AlbumRepo;
        use tune_core::db::artist_repo::ArtistRepo;
        let ar = ArtistRepo::with_backend(state.backend.clone())
            .get_or_create("Artiste", None, None)
            .unwrap();
        let id = AlbumRepo::with_backend(state.backend.clone())
            .get_or_create(titre, ar.id.unwrap(), None)
            .unwrap()
            .id
            .unwrap();
        state
            .backend
            .execute(
                "UPDATE albums SET genre = ?, genres = ? WHERE id = ?",
                &[&genre.map(str::to_string), &genres.map(str::to_string), &id],
            )
            .unwrap();
        id
    }

    fn ecoute_d_album(state: &AppState, album_id: i64) {
        state
            .backend
            .execute(
                "INSERT INTO listen_history (title, source, duration_ms, album_id) VALUES ('t', 'local', 1000, ?)",
                &[&album_id],
            )
            .unwrap();
    }

    async fn stats(state: AppState) -> Value {
        let Json(v) = dashboard_stats(State(state)).await;
        v
    }

    #[tokio::test]
    async fn sans_ecoute_le_champ_vaut_zero_et_c_est_vrai() {
        // Ici 0 n'est pas un repli : il n'y a vraiment rien d'écouté. Le
        // champ est PRÉSENT — seul un échec de requête le fait disparaître.
        let s = stats(etat()).await;
        assert_eq!(s["unique_genres"], json!(0));
    }

    #[tokio::test]
    async fn deux_graphies_d_un_meme_genre_ne_font_qu_un() {
        // 🔴 LE témoin de « même notion que la bibliothèque ». Un
        // `COUNT(DISTINCT genre)` naïf rendrait 2 ici ; `/library/genres`
        // rend UNE carte « Trip Hop » (#1161). Les deux cartes voisines de
        // l'accueil se contrediraient.
        let st = etat();
        let a = album(&st, "A", Some("Trip Hop"), None);
        let b = album(&st, "B", Some("Trip-Hop"), None);
        ecoute_d_album(&st, a);
        ecoute_d_album(&st, b);
        assert_eq!(
            stats(st).await["unique_genres"],
            json!(1),
            "« Trip Hop » et « Trip-Hop » doivent compter pour UN genre, comme dans /library/genres"
        );
    }

    #[tokio::test]
    async fn un_album_non_ecoute_ne_compte_pas() {
        let st = etat();
        let ecoute = album(&st, "Écouté", Some("Jazz"), None);
        album(&st, "Jamais écouté", Some("Rock"), None);
        ecoute_d_album(&st, ecoute);
        assert_eq!(stats(st).await["unique_genres"], json!(1));
    }

    #[tokio::test]
    async fn plusieurs_genres_par_album_et_le_tableau_json_d_abord() {
        // Le tableau `genres` prime sur la colonne héritée — la règle de
        // `list_genres`, reprise par la fonction partagée.
        let st = etat();
        let a = album(&st, "A", Some("Ignoré"), Some(r#"["Jazz","Blues"]"#));
        let b = album(&st, "B", Some("Rock; Jazz"), None);
        ecoute_d_album(&st, a);
        ecoute_d_album(&st, b);
        // Jazz, Blues, Rock — « Ignoré » ne compte pas, le tableau l'emporte.
        assert_eq!(stats(st).await["unique_genres"], json!(3));
    }

    #[tokio::test]
    async fn une_ecoute_sans_album_passe_par_sa_piste() {
        // `listen_history.album_id` peut manquer : l'album vient alors de la
        // piste écoutée.
        let st = etat();
        let alb = album(&st, "Par la piste", Some("Blues"), None);
        let piste = st
            .backend
            .execute_returning_id(
                "INSERT INTO tracks (title, album_id) VALUES ('Piste', ?)",
                &[&alb],
            )
            .unwrap();
        st.backend
            .execute(
                "INSERT INTO listen_history (title, source, duration_ms, track_id) VALUES ('Piste', 'local', 1000, ?)",
                &[&piste],
            )
            .unwrap();
        assert_eq!(stats(st).await["unique_genres"], json!(1));
    }

    #[tokio::test]
    async fn un_album_ecoute_dix_fois_compte_une_fois() {
        let st = etat();
        let a = album(&st, "A", Some("Jazz"), None);
        for _ in 0..10 {
            ecoute_d_album(&st, a);
        }
        assert_eq!(stats(st).await["unique_genres"], json!(1));
    }

    #[tokio::test]
    async fn une_ecoute_de_service_sans_album_local_ne_compte_pas() {
        // Limite ASSUMÉE (#4527) : une écoute Qobuz sans album local n'a pas
        // de genre connu. Elle ne fait pas planter le calcul pour autant.
        let st = etat();
        st.backend
            .execute(
                "INSERT INTO listen_history (title, source, source_id, duration_ms) VALUES ('Q', 'qobuz', '42', 1000)",
                &[],
            )
            .unwrap();
        assert_eq!(stats(st).await["unique_genres"], json!(0));
    }

    #[tokio::test]
    async fn les_autres_champs_ne_bougent_pas() {
        // L'ajout n'enlève rien : l'écran d'aujourd'hui lit ces quatre-là.
        let s = stats(etat()).await;
        for champ in [
            "total_listens",
            "total_duration_ms",
            "unique_tracks",
            "unique_artists",
        ] {
            assert!(
                s.get(champ).is_some(),
                "{champ} a disparu de /dashboard/stats"
            );
        }
    }
}
