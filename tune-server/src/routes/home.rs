use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::backend::ToSqlValue;
use tune_core::db::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use tune_core::db::history_repo::HistoryRepo;
use tune_core::db::radio_repo::RadioRepo;
use tune_core::db::settings_repo::SettingsRepo;

use crate::error::AppError;
use crate::state::AppState;

#[derive(Deserialize)]
struct HomeParams {
    limit: Option<i64>,
    /// Optional zone filter: when provided, continue-listening only shows
    /// albums listened on this zone.  Clients should send the CURRENT active
    /// zone so the response is relevant (DEvir QA B-09: zone mismatch).
    zone_id: Option<i64>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(home_page))
        .route("/continue-listening", get(continue_listening))
        .route("/recently-added", get(recently_added))
        .route("/recommendations", get(home_recommendations))
        .route("/top-mixes", get(top_mixes))
        .route("/new-in-library", get(new_in_library))
        .route("/other-versions", get(other_versions))
        .route(
            "/artist-releases",
            get(super::artist_releases::artist_releases),
        )
        .route("/radio-picks", get(radio_picks))
        .route("/streaming-highlights", get(streaming_highlights))
}

/// Returns a placeholder string appropriate for the engine.
fn ph(engine: Engine, idx: usize) -> String {
    match engine {
        Engine::Sqlite => SqliteDialect.placeholder(idx),
        Engine::Postgres => PostgresDialect.placeholder(idx),
    }
}

/// Rapproche une ligne d'historique (`lh`) de son album (`a`).
///
/// Le titre SEUL ne designe pas un album : un « Live » de Police et un
/// « Live » de Pulp portent le meme titre et se retrouvaient comptes pour un
/// seul disque — un album jamais ecoute remontait dans « Continuer l'ecoute »,
/// et le compteur d'avancement additionnait les pistes des deux (#2731,
/// Tades, fil 1600).
///
/// L'identifiant fait foi quand il est ecrit. Il ne l'est PAS toujours :
/// `record_listen` le tire de la piste locale, donc toute ecoute en flux
/// (track_id absent) et toute ligne anterieure a la migration
/// `add_listen_history_source_id_album_id` l'ont a NULL. Joindre sur le seul
/// `album_id` viderait la section pour ces gens-la ; d'ou le repli sur
/// titre ET artiste.
///
/// Une ligne sans artiste reste rattachee au titre seul : on ne sait pas
/// departager, et perdre l'entree serait pire que la garder.
///
/// Le sous-select sur `artists` evite de dependre de l'ordre des jointures —
/// `ar` n'existe pas encore quand cette condition est evaluee, et le
/// GROUP BY de PostgreSQL n'accepte pas qu'on enveloppe `albums` dans une
/// table derivee (la dependance fonctionnelle ne vaut que pour la cle
/// primaire d'une vraie table).
const HISTORIQUE_VERS_ALBUM: &str = "(lh.album_id = a.id \
     OR (lh.album_id IS NULL AND lh.album_title = a.title \
         AND (lh.artist_name IS NULL \
              OR lh.artist_name = (SELECT ar_hist.name FROM artists ar_hist \
                                   WHERE ar_hist.id = a.artist_id))))";

/// Les cinq genres les plus ecoutes : celui de la piste s'il est connu, sinon
/// celui de l'album. Partage entre les recommandations et les « top mixes »,
/// qui prenaient tous deux le genre d'un album homonyme (#2731).
///
/// ATTENTION : telle quelle, cette requete ECHOUE sur les deux moteurs —
/// `WHERE genre IS NOT NULL` est ambigu entre `t.genre` et `a.genre`, et
/// l'erreur est avalee par le `unwrap_or_default` des appelants. La jointure
/// est corrigee ici pour le jour ou la requete sera reveillee ; la reveiller
/// releve d'un arbitrage produit (cf. le test
/// `les_genres_les_plus_ecoutes_ne_rendent_rien_ambiguite_sur_genre`), pas du
/// defaut d'homonymie.
fn sql_top_genres() -> String {
    format!(
        "SELECT genre, COUNT(*) as cnt \
         FROM (SELECT COALESCE(t.genre, a.genre) as genre \
               FROM listen_history lh \
               LEFT JOIN tracks t ON lh.track_id = t.id \
               LEFT JOIN albums a ON {HISTORIQUE_VERS_ALBUM} \
               WHERE genre IS NOT NULL AND genre != '') \
         GROUP BY genre ORDER BY cnt DESC LIMIT 5"
    )
}

/// Aggregated home page: returns all sections in a single response.
async fn home_page(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    // No zone filter for the aggregated home page — show all zones.
    let continue_items = fetch_continue_listening(&state, 10, None)?;
    let recent_items = fetch_recently_added(&state, 20)?;
    let top_tracks = fetch_top_tracks(&state, 20);
    let radios = fetch_radio_picks(&state)?;
    let discover = fetch_recommendations(&state, 20)?;

    let mut sections = Vec::new();

    if !continue_items.is_empty() {
        sections.push(json!({
            "id": "continue",
            "title": "Continuer l'\u{00e9}coute",
            "type": "albums",
            "items": continue_items,
        }));
    }

    if !recent_items.is_empty() {
        sections.push(json!({
            "id": "recent",
            "title": "Ajout\u{00e9}s r\u{00e9}cemment",
            "type": "albums",
            "items": recent_items,
        }));
    }

    if !top_tracks.is_empty() {
        sections.push(json!({
            "id": "top",
            "title": "Les plus \u{00e9}cout\u{00e9}s",
            "type": "tracks",
            "items": top_tracks,
        }));
    }

    if !radios.is_empty() {
        sections.push(json!({
            "id": "radios",
            "title": "Radios favorites",
            "type": "radios",
            "items": radios,
        }));
    }

    if !discover.is_empty() {
        sections.push(json!({
            "id": "discover",
            "title": "\u{00c0} d\u{00e9}couvrir",
            "type": "albums",
            "items": discover,
        }));
    }

    Ok(Json(json!({ "sections": sections })))
}

/// Albums from listen history where the user hasn't finished the album
/// (listened tracks < total tracks).
async fn continue_listening(
    State(state): State<AppState>,
    Query(p): Query<HomeParams>,
) -> Result<Json<Value>, AppError> {
    let limit = p.limit.unwrap_or(10);
    // When zone_id is provided, filter continue-listening items to albums
    // that were played on that specific zone. This prevents zone mismatch:
    // the client sends the CURRENT active zone, not a stored zone from
    // history (DEvir QA B-09).
    let items = fetch_continue_listening(&state, limit, p.zone_id)?;
    Ok(Json(json!(items)))
}

// #2441 — cette requete part de `albums` et se termine par
// `HAVING listened_tracks < a.track_count` : elle ne PEUT rien rendre d'autre
// qu'un album de la bibliotheque locale, quelle que soit la nature de ce que
// l'auditeur avait demande. C'est le defaut releve par FabienM (fil 1557).
//
// Depuis la migration 84, `listen_history` porte `context_type` /
// `context_id` : l'intention est desormais ECRITE. Ce qu'il faut en AFFICHER
// — mettre une playlist a cote d'un album, un artiste, un titre isole ; le
// devenir du `HAVING` qui fait disparaitre un album fini ; les badges par
// type — releve d'un arbitrage produit qui n'a pas ete rendu. La requete
// n'est donc pas touchee ici : il n'y a rien de moins fiable qu'une regle
// d'affichage inventee par celui qui pose le socle.
fn fetch_continue_listening(
    state: &AppState,
    limit: i64,
    zone_id: Option<i64>,
) -> Result<Vec<Value>, AppError> {
    let engine = state.backend.engine();
    // When a zone_id filter is provided, only show albums that were listened
    // to on that zone.  This ensures the "continue listening" section matches
    // the user's currently selected zone (B-09 fix).
    let zone_filter = match zone_id {
        Some(zid) => format!("AND lh.zone_id = {zid} "),
        None => String::new(),
    };
    let p1 = ph(engine, 1);
    let sql = format!(
        "SELECT a.id, a.title, ar.name, a.year, a.cover_path, a.genre, \
               COUNT(DISTINCT lh.title) as listened_tracks, a.track_count \
        FROM listen_history lh \
        JOIN albums a ON {HISTORIQUE_VERS_ALBUM} \
        LEFT JOIN artists ar ON a.artist_id = ar.id \
        WHERE a.track_count IS NOT NULL AND a.track_count > 0 \
        {zone_filter}\
        GROUP BY a.id \
        HAVING listened_tracks < a.track_count \
        ORDER BY MAX(lh.listened_at) DESC \
        LIMIT {p1}"
    );
    let params: [&dyn ToSqlValue; 1] = [&limit];
    let rows = state.backend.query_many(&sql, &params).unwrap_or_default();
    Ok(rows
        .iter()
        .map(|cols| {
            let album_id = cols.get(0).and_then(|v| v.as_i64()).unwrap_or(0);
            json!({
                "id": album_id,
                "album_id": album_id,
                "title": cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                "album_title": cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                "artist_name": cols.get(2).and_then(|v| v.as_string()),
                "year": cols.get(3).and_then(|v| v.as_i64()),
                "cover_path": cols.get(4).and_then(|v| v.as_string()),
                "genre": cols.get(5).and_then(|v| v.as_string()),
                "listened_tracks": cols.get(6).and_then(|v| v.as_i64()).unwrap_or(0),
                "track_count": cols.get(7).and_then(|v| v.as_i64()),
            })
        })
        .collect())
}

#[cfg(test)]
mod tests_homonymes {
    use super::*;

    /// Les deux disques de Tades : un « Live » de Police et un « Live » de
    /// Pulp, meme titre au caractere pres, cinq pistes chacun.
    /// Rend `(id du Live de Police, id du Live de Pulp)`.
    fn deux_live(state: &AppState, genre: Option<&str>) -> (i64, i64) {
        let b = &state.backend;
        let poser = |artiste: &str| -> i64 {
            b.execute(
                "INSERT INTO artists (name) VALUES (?1)",
                &[&artiste as &dyn ToSqlValue],
            )
            .unwrap();
            let artiste_id = b.last_insert_rowid();
            b.execute(
                "INSERT INTO albums (title, artist_id, track_count, genre) \
                 VALUES ('Live', ?1, 5, ?2)",
                &[&artiste_id as &dyn ToSqlValue, &genre as &dyn ToSqlValue],
            )
            .unwrap();
            b.last_insert_rowid()
        };
        let police = poser("The Police");
        let pulp = poser("Pulp");
        (police, pulp)
    }

    fn ecoute(state: &AppState, titre: &str, artiste: Option<&str>, album_id: Option<i64>) {
        state
            .backend
            .execute(
                "INSERT INTO listen_history \
                 (title, artist_name, album_title, album_id, listened_at) \
                 VALUES (?1, ?2, 'Live', ?3, '2026-08-28T22:45:00Z')",
                &[
                    &titre as &dyn ToSqlValue,
                    &artiste as &dyn ToSqlValue,
                    &album_id as &dyn ToSqlValue,
                ],
            )
            .unwrap();
    }

    /// Le defaut de Tades (#2731, fil 1600) : ecouter le « Live » de Pulp
    /// faisait remonter celui de Police, et le compteur d'avancement
    /// additionnait les pistes des deux.
    #[test]
    fn le_live_de_pulp_ne_fait_pas_remonter_celui_de_police() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let (police, pulp) = deux_live(&state, None);
        ecoute(&state, "Common People", Some("Pulp"), Some(pulp));
        ecoute(&state, "Disco 2000", Some("Pulp"), Some(pulp));

        let Ok(items) = fetch_continue_listening(&state, 10, None) else {
            panic!("la requete doit repondre")
        };

        assert_eq!(items.len(), 1, "albums rendus : {items:?}");
        assert_eq!(items[0]["album_id"].as_i64(), Some(pulp));
        assert_eq!(items[0]["artist_name"].as_str(), Some("Pulp"));
        assert_eq!(
            items[0]["listened_tracks"].as_i64(),
            Some(2),
            "le compteur ne doit compter que les pistes de CET album"
        );
        assert_ne!(items[0]["album_id"].as_i64(), Some(police));
    }

    /// `record_listen` ne connait l'album que par la piste locale : une ecoute
    /// en flux (`track_id` absent) et toute ligne anterieure a la migration
    /// `add_listen_history_source_id_album_id` ont `album_id` a NULL. Joindre
    /// sur le seul identifiant VIDERAIT la section pour ces gens-la — le repli
    /// titre + artiste doit tenir.
    #[test]
    fn une_ecoute_sans_album_id_reste_rattachee_par_titre_et_artiste() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let (_police, pulp) = deux_live(&state, None);
        ecoute(&state, "Common People", Some("Pulp"), None);

        let Ok(items) = fetch_continue_listening(&state, 10, None) else {
            panic!("la requete doit repondre")
        };

        assert_eq!(items.len(), 1, "albums rendus : {items:?}");
        assert_eq!(items[0]["album_id"].as_i64(), Some(pulp));
        assert_eq!(items[0]["artist_name"].as_str(), Some("Pulp"));
    }

    /// Sans artiste NI identifiant on ne sait pas departager : la ligne reste
    /// rattachee au titre seul. Perdre l'entree serait pire que la garder.
    #[test]
    fn une_ecoute_sans_artiste_ni_identifiant_n_est_pas_perdue() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        deux_live(&state, None);
        ecoute(&state, "Piste inconnue", None, None);

        let Ok(items) = fetch_continue_listening(&state, 10, None) else {
            panic!("la requete doit repondre")
        };

        assert!(!items.is_empty(), "la section ne doit pas se vider");
    }

    /// Constat de bordure, releve en voulant eprouver le meme defaut du cote
    /// des recommandations : `sql_top_genres` ne rend RIEN, sur les deux
    /// moteurs. `WHERE genre IS NOT NULL` se heurte a `t.genre` et `a.genre`
    /// — « ambiguous column name: genre » — et l'erreur est avalee par le
    /// `unwrap_or_default` de l'appelant. « A decouvrir » tire donc toujours
    /// au hasard, et « top mixes » est toujours vide.
    ///
    /// Consequence pour #2731 : la jointure corrigee dans `sql_top_genres` et
    /// le `NOT EXISTS` des recommandations sont ecrits juste, mais aucun test
    /// ne peut les atteindre tant que cette requete ne s'execute pas. Reveiller
    /// la requete change ce que l'accueil AFFICHE (le hasard cede la place aux
    /// albums d'un genre, qui peuvent etre zero) : c'est un arbitrage produit,
    /// pas le defaut de Tades. Il est laisse hors de ce correctif, et ce test
    /// le fige pour qu'on ne le decouvre pas deux fois.
    #[test]
    fn les_genres_les_plus_ecoutes_ne_rendent_rien_ambiguite_sur_genre() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let (_police, pulp) = deux_live(&state, Some("Rock"));
        ecoute(&state, "Common People", Some("Pulp"), Some(pulp));

        assert!(
            state.backend.query_many(&sql_top_genres(), &[]).is_err(),
            "si cette requete se met a repondre, les deux jointures corrigees \
             deviennent testables — et « A decouvrir » cesse d'etre aleatoire"
        );
    }
}

/// Albums added in the last 7 days (by file mtime of tracks).
async fn recently_added(
    State(state): State<AppState>,
    Query(p): Query<HomeParams>,
) -> Result<Json<Value>, AppError> {
    let limit = p.limit.unwrap_or(20);
    let items = fetch_recently_added(&state, limit)?;
    Ok(Json(json!(items)))
}

fn fetch_recently_added(state: &AppState, limit: i64) -> Result<Vec<Value>, AppError> {
    let engine = state.backend.engine();
    let seven_days_ago = chrono_epoch_seven_days_ago();
    let p1 = ph(engine, 1);
    let p2 = ph(engine, 2);
    let sql = format!(
        "SELECT DISTINCT a.id, a.title, ar.name, a.year, a.cover_path, a.genre, \
               a.format, a.sample_rate, a.bit_depth, a.track_count, \
               MAX(t.file_mtime) as newest_mtime \
        FROM tracks t \
        JOIN albums a ON t.album_id = a.id \
        LEFT JOIN artists ar ON a.artist_id = ar.id \
        WHERE t.file_mtime IS NOT NULL AND t.file_mtime > {p1} \
        GROUP BY a.id \
        ORDER BY newest_mtime DESC \
        LIMIT {p2}"
    );
    let params: [&dyn ToSqlValue; 2] = [&seven_days_ago, &limit];
    let rows = state.backend.query_many(&sql, &params).unwrap_or_default();
    Ok(rows
        .iter()
        .map(|cols| {
            json!({
                "id": cols.get(0).and_then(|v| v.as_i64()).unwrap_or(0),
                "title": cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                "artist_name": cols.get(2).and_then(|v| v.as_string()),
                "year": cols.get(3).and_then(|v| v.as_i64()),
                "cover_path": cols.get(4).and_then(|v| v.as_string()),
                "genre": cols.get(5).and_then(|v| v.as_string()),
                "format": cols.get(6).and_then(|v| v.as_string()),
                "sample_rate": cols.get(7).and_then(|v| v.as_i64()),
                "bit_depth": cols.get(8).and_then(|v| v.as_i64()),
                "track_count": cols.get(9).and_then(|v| v.as_i64()),
                "added_mtime": cols.get(10).and_then(|v| v.as_f64()),
            })
        })
        .collect())
}

/// Returns epoch seconds for 7 days ago.
fn chrono_epoch_seven_days_ago() -> f64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    now - (7.0 * 24.0 * 3600.0)
}

/// Recommendations based on listening history: find most-played genres/artists,
/// suggest albums from the same genres that haven't been listened to yet.
async fn home_recommendations(
    State(state): State<AppState>,
    Query(p): Query<HomeParams>,
) -> Result<Json<Value>, AppError> {
    let limit = p.limit.unwrap_or(20);
    let items = fetch_recommendations(&state, limit)?;
    Ok(Json(json!(items)))
}

fn fetch_recommendations(state: &AppState, limit: i64) -> Result<Vec<Value>, AppError> {
    let engine = state.backend.engine();

    // Find top genres from listen history
    let top_genres: Vec<String> = state
        .backend
        .query_many(&sql_top_genres(), &[])
        .unwrap_or_default()
        .into_iter()
        .filter_map(|cols| cols.into_iter().next().and_then(|v| v.as_string()))
        .collect();

    if top_genres.is_empty() {
        // Fallback: return random albums
        let p1 = ph(engine, 1);
        let sql = format!(
            "SELECT a.id, a.title, ar.name, a.year, a.cover_path, a.genre \
                   FROM albums a LEFT JOIN artists ar ON a.artist_id = ar.id \
                   ORDER BY RANDOM() LIMIT {p1}"
        );
        let params: [&dyn ToSqlValue; 1] = [&limit];
        let rows = state.backend.query_many(&sql, &params).unwrap_or_default();
        return Ok(rows
            .iter()
            .map(|cols| {
                json!({
                    "id": cols.get(0).and_then(|v| v.as_i64()).unwrap_or(0),
                    "title": cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                    "artist_name": cols.get(2).and_then(|v| v.as_string()),
                    "year": cols.get(3).and_then(|v| v.as_i64()),
                    "cover_path": cols.get(4).and_then(|v| v.as_string()),
                    "genre": cols.get(5).and_then(|v| v.as_string()),
                    "reason": "random",
                })
            })
            .collect());
    }

    // Find albums matching top genres that the user hasn't listened to.
    // Build engine-specific placeholders for the IN clause.
    let genre_placeholders: String = top_genres
        .iter()
        .enumerate()
        .map(|(i, _)| ph(engine, i + 1))
        .collect::<Vec<_>>()
        .join(",");
    let limit_ph = ph(engine, top_genres.len() + 1);
    let sql = format!(
        "SELECT a.id, a.title, ar.name, a.year, a.cover_path, a.genre \
         FROM albums a \
         LEFT JOIN artists ar ON a.artist_id = ar.id \
         WHERE a.genre IN ({genre_placeholders}) \
           AND NOT EXISTS (SELECT 1 FROM listen_history lh \
                           WHERE {HISTORIQUE_VERS_ALBUM}) \
         ORDER BY RANDOM() \
         LIMIT {limit_ph}"
    );

    // Build a Vec of owned SqlValue-able params: genres + limit.
    let mut param_vals: Vec<Box<dyn ToSqlValue>> = top_genres
        .iter()
        .map(|g| Box::new(g.clone()) as Box<dyn ToSqlValue>)
        .collect();
    param_vals.push(Box::new(limit));
    let param_refs: Vec<&dyn ToSqlValue> = param_vals.iter().map(|p| p.as_ref()).collect();

    let rows = state
        .backend
        .query_many(&sql, &param_refs)
        .unwrap_or_default();
    Ok(rows
        .iter()
        .map(|cols| {
            json!({
                "id": cols.get(0).and_then(|v| v.as_i64()).unwrap_or(0),
                "title": cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                "artist_name": cols.get(2).and_then(|v| v.as_string()),
                "year": cols.get(3).and_then(|v| v.as_i64()),
                "cover_path": cols.get(4).and_then(|v| v.as_string()),
                "genre": cols.get(5).and_then(|v| v.as_string()),
                "reason": "genre_match",
            })
        })
        .collect())
}

/// Auto-generated "mixes" by genre from top genres in history.
/// Each mix = playlist of 20 tracks from that genre.
async fn top_mixes(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let engine = state.backend.engine();

    // Get top 5 genres from history
    let top_genres: Vec<(String, i64)> = state
        .backend
        .query_many(&sql_top_genres(), &[])
        .unwrap_or_default()
        .into_iter()
        .filter_map(|cols| {
            let genre = cols.first()?.as_string()?;
            let cnt = cols.get(1).and_then(|v| v.as_i64()).unwrap_or(0);
            Some((genre, cnt))
        })
        .collect();

    let p1 = ph(engine, 1);
    let p2 = ph(engine, 2);
    let tracks_sql = format!(
        "SELECT t.id, t.title, ar.name, al.title, \
                CAST(t.duration_ms AS BIGINT), al.cover_path \
         FROM tracks t \
         LEFT JOIN albums al ON t.album_id = al.id \
         LEFT JOIN artists ar ON t.artist_id = ar.id \
         WHERE t.genre = {p1} OR al.genre = {p2} \
         ORDER BY RANDOM() LIMIT 20"
    );

    let mixes: Vec<Value> = top_genres
        .into_iter()
        .filter_map(|(genre, play_count)| {
            let params: [&dyn ToSqlValue; 2] = [&genre, &genre];
            let tracks: Vec<Value> = state
                .backend
                .query_many(&tracks_sql, &params)
                .unwrap_or_default()
                .iter()
                .map(|cols| {
                    json!({
                        "id": cols.get(0).and_then(|v| v.as_i64()).unwrap_or(0),
                        "title": cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                        "artist_name": cols.get(2).and_then(|v| v.as_string()),
                        "album_title": cols.get(3).and_then(|v| v.as_string()),
                        "duration_ms": cols.get(4).and_then(|v| v.as_i64()).unwrap_or(0),
                        "cover_path": cols.get(5).and_then(|v| v.as_string()),
                    })
                })
                .collect();

            if tracks.is_empty() {
                return None;
            }

            Some(json!({
                "genre": genre,
                "title": format!("Mix {}", genre),
                "play_count": play_count,
                "track_count": tracks.len(),
                "tracks": tracks,
            }))
        })
        .collect();

    Ok(Json(json!(mixes)))
}

/// Albums most recently added to the library, newest first.
///
/// Grouped by ALBUM, not by track. Returning tracks meant a freshly imported
/// 15-track record filled half the row with the same cover — "'New in your
/// library' can sometimes show 10-20 tracks from the same album" (Alex
/// Campbell, 9 Aug 2026).
///
/// The shape is what the home carousel has always assumed: it calls
/// `playAlbum(item.id)` and `navigateToAlbum(item.id)` and reads
/// `item.artist_id`. Sending tracks meant `id` was a TRACK id and `title` a
/// track title, so the covers were labelled with song names and clicking one
/// navigated by an id that means something else entirely. No client change is
/// needed — the server now sends what the client was already reading.
///
/// Tracks with no album are left out rather than shown as one-track entries:
/// this row is about records landing in the library, and a loose file has no
/// album to open.
async fn new_in_library(
    State(state): State<AppState>,
    Query(p): Query<HomeParams>,
) -> Result<Json<Value>, AppError> {
    let limit = p.limit.unwrap_or(30);
    let engine = state.backend.engine();
    let p1 = ph(engine, 1);
    // MAX(file_mtime) dates an album by its most recently imported track, so a
    // record whose files arrived together stays together in the ordering.
    let sql = format!(
        "SELECT al.id, al.title, al.artist_id, ar.name, al.cover_path, al.source, \
                MAX(t.file_mtime) AS newest \
        FROM tracks t \
        JOIN albums al ON t.album_id = al.id \
        LEFT JOIN artists ar ON al.artist_id = ar.id \
        WHERE t.file_mtime IS NOT NULL \
        GROUP BY al.id, al.title, al.artist_id, ar.name, al.cover_path, al.source \
        ORDER BY newest DESC \
        LIMIT {p1}"
    );
    let params: [&dyn ToSqlValue; 1] = [&limit];
    let items: Vec<Value> = state
        .backend
        .query_many(&sql, &params)
        .unwrap_or_default()
        .iter()
        .map(|cols| {
            json!({
                "id": cols.get(0).and_then(|v| v.as_i64()).unwrap_or(0),
                "title": cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                "artist_id": cols.get(2).and_then(|v| v.as_i64()),
                "artist_name": cols.get(3).and_then(|v| v.as_string()),
                "cover_path": cols.get(4).and_then(|v| v.as_string()),
                "source": cols.get(5).and_then(|v| v.as_string()),
                "file_mtime": cols.get(6).and_then(|v| v.as_f64()),
            })
        })
        .collect();
    Ok(Json(json!(items)))
}

/// Les ecoutes recentes examinees pour la recherche STREAMING. Chacune coute
/// une recherche par service connecte (moins le cache) : ce nombre est le
/// budget reseau de la section, pas un choix d'affichage.
const ECOUTES_STREAMING: usize = 6;

/// `GET /home/other-versions` — les autres versions, DANS LA BIBLIOTHEQUE, des
/// morceaux ecoutes RECEMMENT.
///
/// ## Pourquoi les N dernieres ecoutes, et non « aujourd'hui »
///
/// La premiere version bornait sur le jour CIVIL, en UTC. Deux defauts, vus
/// des la mise en service :
///
/// 1. **Minuit UTC coupe la soiree.** A 10 h du matin en France, tout ce qui
///    a ete ecoute la veille apres 2 h — donc toute la soiree — etait deja
///    hors fenetre. Le jour civil de l'utilisateur ne commence pas a la meme
///    heure que celui du serveur, et le fuseau du navigateur n'arrive pas
///    jusqu'ici.
/// 2. **Un jour ordinaire ne contient pas assez de matiere.** Mesure sur une
///    bibliotheque reelle : UNE ecoute dans la fenetre, et donc une section
///    vide la plupart du temps.
///
/// J'avais justifie l'UTC en invoquant le correctif des horaires de favoris
/// radio (#2179). C'etait un mauvais raisonnement : ce defaut-la portait sur
/// l'AFFICHAGE d'un horodatage, pas sur la definition d'une journee.
///
/// Les N dernieres ecoutes n'ont ni fuseau ni bord de journee. La fenetre ne
/// glisse pas, ne depend d'aucune horloge, et contient toujours de la matiere.
///
/// Le cas concret : on ecoute « Ordinary World » depuis The Wedding Album, et
/// on possede aussi la version acoustique sur une compilation. Rien ne le dit
/// aujourd'hui — il faut chercher le titre a la main pour s'en apercevoir.
///
/// ## Ce que cette route fait, et ce qu'elle ne fait PAS
///
/// Elle rapproche **titre + artiste**, et ne retient que les pistes d'un
/// **autre album** que celui ecoute. C'est volontairement etroit :
///
/// - pas de reprises par un autre interprete (« Comme d'habitude » / « My Way ») :
///   cela demande les relations d'oeuvre de MusicBrainz, donc un MBID, et la
///   couverture MBID de la bibliotheque est encore trop faible pour que le
///   resultat soit autre chose qu'un hasard ;
/// - les versions des services de streaming sont cherchees sur un vivier plus
///   petit et mises en cache six heures, afin de borner les appels distants.
///
/// Le rapprochement local est insensible a la casse et strict sur le coeur du
/// titre : seul un suffixe d'edition delimite par ` (` ou ` [` est admis. Il
/// ne s'agit pas d'une recherche floue.
async fn other_versions(
    State(state): State<AppState>,
    Query(p): Query<HomeParams>,
) -> Result<Json<Value>, AppError> {
    // Plafond borne cote serveur : ce nombre part dans le SQL, il ne doit pas
    // venir tel quel de l'URL.
    let limit = p.limit.unwrap_or(20).clamp(1, 100);
    // Le vivier d'ecoutes examine. Large devant `limit` : beaucoup de morceaux
    // n'ont aucune autre version, il en faut donc bien plus que de groupes
    // souhaites pour en remplir quelques-uns.
    const ECOUTES_EXAMINEES: usize = 200;

    // `listened_at` est ordonne comme chaine (ISO-8601), donc `ORDER BY` suffit
    // pour prendre les dernieres : aucun cast de date, donc aucun ecart entre
    // SQLite et PostgreSQL.
    // Le rapprochement lui-meme est ecrit UNE fois, dans
    // `routes::versions` : la route par piste (#2372) applique exactement
    // la meme regle a un vivier different.
    let predicat = crate::routes::versions::predicat_rapprochement(
        "lh.title",
        "lh.artist_name",
        "lh.album_title",
    );
    let sql = format!(
        "SELECT DISTINCT lh.title, lh.artist_name, lh.album_title, \
                t.id, al.id, al.title, al.cover_path, t.duration_ms \
        FROM (SELECT title, artist_name, album_title, listened_at \
              FROM listen_history \
              WHERE artist_name IS NOT NULL \
              ORDER BY listened_at DESC \
              LIMIT {ECOUTES_EXAMINEES}) lh \
        CROSS JOIN tracks t \
        JOIN albums al ON t.album_id = al.id \
        LEFT JOIN artists ar ON al.artist_id = ar.id \
        LEFT JOIN artists ar2 ON t.artist_id = ar2.id \
        WHERE {predicat} \
        ORDER BY lh.listened_at DESC \
        LIMIT {limit}"
    );

    // Une piste ecoutee, ses autres versions : on regroupe cote serveur pour
    // que l'ecran n'ait pas a le refaire (et a le refaire differemment sur
    // chacun des trois clients).
    let mut groupes: Vec<Value> = Vec::new();
    for cols in state.backend.query_many(&sql, &[]).unwrap_or_default() {
        let titre = cols.first().and_then(|v| v.as_string()).unwrap_or_default();
        let artiste = cols.get(1).and_then(|v| v.as_string()).unwrap_or_default();
        let joue = cols.get(2).and_then(|v| v.as_string()).unwrap_or_default();
        let version = json!({
            "track_id": cols.get(3).and_then(|v| v.as_i64()),
            "album_id": cols.get(4).and_then(|v| v.as_i64()),
            "album_title": cols.get(5).and_then(|v| v.as_string()),
            "cover_path": cols.get(6).and_then(|v| v.as_string()),
            "duration_ms": cols.get(7).and_then(|v| v.as_i64()),
        });
        match groupes.iter_mut().find(|g| {
            g["title"].as_str() == Some(titre.as_str())
                && g["artist_name"].as_str() == Some(artiste.as_str())
        }) {
            Some(g) => {
                if let Some(arr) = g["versions"].as_array_mut() {
                    arr.push(version);
                }
            }
            None => groupes.push(json!({
                "title": titre,
                "artist_name": artiste,
                "played_album": joue,
                "versions": [version],
            })),
        }
    }

    // ── Les versions et reprises DISPONIBLES EN STREAMING ──
    //
    // La doc de cette route promettait ce branchement « quand la section
    // aurait fait ses preuves en local » : c'est demande explicitement
    // maintenant. Budget borne : les ECOUTES_STREAMING dernieres ecoutes
    // distinctes, UNE recherche par service et par titre, cache six heures.
    // Les N derniers TITRES distincts — pas les N dernières lignes. Trois
    // réécoutes du même morceau mangeaient tout le budget : sur un accueil
    // réel, un seul groupe sur sept avait sa recherche streaming, et
    // « Billie Jean » — écoutée juste avant — n'en avait aucune (25/08).
    let sql_recentes = format!(
        "SELECT title, artist_name, MAX(COALESCE(album_title, '')) FROM (SELECT title, artist_name, album_title, listened_at FROM listen_history WHERE artist_name IS NOT NULL ORDER BY listened_at DESC LIMIT 200) le GROUP BY title, artist_name ORDER BY MAX(listened_at) DESC LIMIT {ECOUTES_STREAMING}"
    );
    let recentes: Vec<(String, String, String)> = state
        .backend
        .query_many(&sql_recentes, &[])
        .unwrap_or_default()
        .into_iter()
        .filter_map(|cols| {
            Some((
                cols.first().and_then(|v| v.as_string())?,
                cols.get(1).and_then(|v| v.as_string())?,
                cols.get(2).and_then(|v| v.as_string()).unwrap_or_default(),
            ))
        })
        .collect();

    for (titre, artiste, album) in recentes {
        let trouvees =
            crate::routes::versions::versions_streaming(&state, &titre, &artiste, &album).await;
        if trouvees.is_empty() {
            continue;
        }
        match groupes.iter_mut().find(|g| {
            g["title"]
                .as_str()
                .is_some_and(|t| t.eq_ignore_ascii_case(&titre))
                && g["artist_name"]
                    .as_str()
                    .is_some_and(|a| a.eq_ignore_ascii_case(&artiste))
        }) {
            Some(g) => g["streaming"] = json!(trouvees),
            // Un morceau sans autre version LOCALE forme quand meme un groupe
            // si le streaming en a : c'est le cas « Billie Jean » — aucune
            // autre version possedee, des dizaines disponibles.
            None => groupes.push(json!({
                "title": titre,
                "artist_name": artiste,
                "played_album": album,
                "versions": [],
                "streaming": trouvees,
            })),
        }
    }

    Ok(Json(json!(groupes)))
}

#[cfg(test)]
mod tests_other_versions {
    use super::*;

    /// Le second appelant du predicat partage (#2638) doit accepter la meme
    /// variante de titre que la route par piste, sans perdre l'artiste reel
    /// d'une piste rangee dans une compilation « Artistes divers ».
    #[tokio::test]
    async fn accueil_retrouve_une_edition_suffixee_du_titre_ecoute() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let b = &state.backend;

        b.execute("INSERT INTO artists (name) VALUES ('Kate Bush')", &[])
            .unwrap();
        let kate = b.last_insert_rowid();
        b.execute("INSERT INTO artists (name) VALUES ('Artistes divers')", &[])
            .unwrap();
        let divers = b.last_insert_rowid();
        b.execute(
            "INSERT INTO albums (title, artist_id) VALUES ('Hit Collection', ?1)",
            &[&divers as &dyn ToSqlValue],
        )
        .unwrap();
        b.execute(
            "INSERT INTO albums (title, artist_id) VALUES ('Before The Dawn', ?1)",
            &[&kate as &dyn ToSqlValue],
        )
        .unwrap();
        let before = b.last_insert_rowid();
        b.execute(
            "INSERT INTO tracks (title, album_id, artist_id, duration_ms, file_path) \
             VALUES ('Running Up That Hill (A Deal With God)', ?1, ?2, 296000, '/before.flac')",
            &[&before as &dyn ToSqlValue, &kate as &dyn ToSqlValue],
        )
        .unwrap();
        b.execute(
            "INSERT INTO listen_history \
             (title, artist_name, album_title, listened_at) \
             VALUES ('Running Up that Hill', 'Kate Bush', 'Hit Collection', \
                     '2026-08-28T09:32:00Z')",
            &[],
        )
        .unwrap();

        let resultat = other_versions(
            State(state),
            Query(HomeParams {
                limit: Some(20),
                zone_id: None,
            }),
        )
        .await;
        let Json(groupes) = match resultat {
            Ok(reponse) => reponse,
            Err(_) => panic!("la route doit repondre"),
        };

        let groupes = groupes.as_array().expect("groupes de versions");
        assert_eq!(groupes.len(), 1, "groupes rendus : {groupes:?}");
        assert_eq!(
            groupes[0]["versions"][0]["album_title"].as_str(),
            Some("Before The Dawn")
        );
    }
}

/// Favorite radios + recently played radios.
async fn radio_picks(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let items = fetch_radio_picks(&state)?;
    Ok(Json(json!(items)))
}

fn fetch_radio_picks(state: &AppState) -> Result<Vec<Value>, AppError> {
    let repo = RadioRepo::with_backend(state.backend.clone());

    let mut items: Vec<Value> = repo
        .favorites()
        .unwrap_or_default()
        .into_iter()
        .map(|r| json!(r))
        .collect();

    let recent: Vec<Value> = state
        .backend
        .query_many(
            "SELECT id, name, url, logo_url, genre, last_played, play_count \
             FROM radio_stations \
             WHERE is_favorite = 0 AND last_played IS NOT NULL \
             ORDER BY last_played DESC LIMIT 10",
            &[],
        )
        .unwrap_or_default()
        .iter()
        .map(|cols| {
            json!({
                "id": cols.get(0).and_then(|v| v.as_i64()).unwrap_or(0),
                "name": cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                "url": cols.get(2).and_then(|v| v.as_string()).unwrap_or_default(),
                "logo_url": cols.get(3).and_then(|v| v.as_string()),
                "genre": cols.get(4).and_then(|v| v.as_string()),
                "last_played": cols.get(5).and_then(|v| v.as_string()),
                "play_count": cols.get(6).and_then(|v| v.as_i64()).unwrap_or(0),
                "is_favorite": false,
            })
        })
        .collect();

    items.extend(recent);
    Ok(items)
}

fn fetch_top_tracks(state: &AppState, limit: i64) -> Vec<Value> {
    let repo = HistoryRepo::with_backend(state.backend.clone());
    repo.top_tracks(limit).unwrap_or_default()
}

/// If Tidal/Qobuz authenticated, fetch their featured/new-releases.
async fn streaming_highlights(State(state): State<AppState>) -> Json<Value> {
    let registry = state.services.lock().await;
    let statuses = registry.status_all().await;
    drop(registry);

    let mut highlights: Vec<Value> = Vec::new();

    for svc_status in &statuses {
        let name = svc_status
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("");
        let authenticated = svc_status
            .get("authenticated")
            .and_then(|a| a.as_bool())
            .unwrap_or(false);

        if !authenticated {
            continue;
        }

        match name {
            "tidal" | "qobuz" => {
                highlights.push(json!({
                    "service": name,
                    "authenticated": true,
                    "featured_url": format!("/api/v1/streaming/{}/featured", name),
                    "new_releases_url": format!("/api/v1/streaming/{}/new-releases", name),
                }));
            }
            "spotify" | "deezer" => {
                highlights.push(json!({
                    "service": name,
                    "authenticated": true,
                    "featured_url": format!("/api/v1/streaming/{}/featured", name),
                }));
            }
            _ => {}
        }
    }

    // If we have authenticated services, also add settings hint
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let preferred_service = settings
        .get("preferred_streaming_service")
        .ok()
        .flatten()
        .unwrap_or_default();

    Json(json!({
        "services": highlights,
        "preferred_service": preferred_service,
    }))
}
