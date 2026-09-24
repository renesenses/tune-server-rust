use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tune_core::db::backend::ToSqlValue;
use tune_core::db::engine::Engine;

use crate::SmartHttpState;
use crate::catalogue;
use crate::regles_sql;
use crate::smart_refs::{self, DbRefResolver, RefCtx, RefKind, RefResolver};
use crate::source_streaming::{self, Objet};
use tune_http_types::{ActiveProfile, AppError};

#[derive(Deserialize)]
struct CreateSmartPlaylist {
    name: String,
    rules: Value,
    match_mode: Option<String>,
    sort_by: Option<String>,
    sort_order: Option<String>,
    max_tracks: Option<i64>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct UpdateSmartPlaylist {
    name: Option<String>,
    rules: Option<Value>,
    match_mode: Option<String>,
    sort_by: Option<String>,
    sort_order: Option<String>,
    max_tracks: Option<i64>,
}

#[derive(Deserialize)]
struct PreviewRequest {
    rules: Value,
    match_mode: Option<String>,
    sort_by: Option<String>,
    sort_order: Option<String>,
    max_tracks: Option<i64>,
}

pub fn router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    SmartHttpState: axum::extract::FromRef<S>,
    ActiveProfile: axum::extract::FromRequestParts<S>,
    <ActiveProfile as axum::extract::FromRequestParts<S>>::Rejection: IntoResponse,
{
    Router::new()
        .route("/", get(list_smart_playlists).post(create_smart_playlist))
        .route(
            "/{id}",
            get(get_smart_playlist)
                .put(update_smart_playlist)
                .delete(delete_smart_playlist),
        )
        .route("/{id}/tracks", get(resolve_tracks))
        .route("/{id}/albums", get(smart_collection_albums))
        .route("/preview", post(preview_smart_collection))
}

async fn list_smart_playlists(
    State(state): State<SmartHttpState>,
) -> Result<Json<Value>, AppError> {
    let rows = state
        .backend
        .query_many(
            "SELECT id, name, rules, sort_by, sort_order, max_tracks, created_at, match_mode FROM smart_playlists ORDER BY name",
            &[],
        )
        .map_err(|e| AppError::internal(e))?;
    let items: Vec<Value> = rows
        .iter()
        .map(|cols| {
            let rules_str = cols
                .get(2)
                .and_then(|v| v.as_string())
                .unwrap_or_else(|| "[]".into());
            let rules = serde_json::from_str::<Value>(&rules_str).unwrap_or(json!([]));
            json!({
                "id": cols.get(0).and_then(|v| v.as_i64()),
                "name": cols.get(1).and_then(|v| v.as_string()),
                "rules": rules,
                "match_mode": cols.get(7).and_then(|v| v.as_string()).unwrap_or_else(|| "all".into()),
                "sort_by": cols.get(3).and_then(|v| v.as_string()),
                "sort_order": cols.get(4).and_then(|v| v.as_string()),
                "max_tracks": cols.get(5).and_then(|v| v.as_i64()),
                "created_at": cols.get(6).and_then(|v| v.as_string()),
            })
        })
        .collect();
    Ok(Json(json!(items)))
}

async fn create_smart_playlist(
    State(state): State<SmartHttpState>,
    Json(body): Json<CreateSmartPlaylist>,
) -> Result<impl IntoResponse, AppError> {
    let rules_json = body.rules.to_string();
    let match_mode = body.match_mode.clone().unwrap_or_else(|| "all".into());
    let sort_by = body.sort_by.clone().unwrap_or_else(|| "title".into());
    let sort_order = body.sort_order.clone().unwrap_or_else(|| "asc".into());

    // Refuse les références circulaires (A ⊂ B ⊂ A) entre entités smart.
    let resolver = DbRefResolver::new(&state.backend);
    smart_refs::check_no_cycle(
        &resolver,
        RefKind::SmartPlaylist,
        None,
        &body.name,
        &rules_json,
    )
    .map_err(AppError::bad_request)?;

    let sql = if state.backend.engine() == Engine::Postgres {
        "INSERT INTO smart_playlists (name, rules, match_mode, sort_by, sort_order, max_tracks) VALUES ($1, $2, $3, $4, $5, $6)"
    } else {
        "INSERT INTO smart_playlists (name, rules, match_mode, sort_by, sort_order, max_tracks) VALUES (?, ?, ?, ?, ?, ?)"
    };

    let result = state
        .backend
        .execute_returning_id(
            sql,
            &[
                &body.name as &dyn ToSqlValue,
                &rules_json as &dyn ToSqlValue,
                &match_mode as &dyn ToSqlValue,
                &sort_by as &dyn ToSqlValue,
                &sort_order as &dyn ToSqlValue,
                &body.max_tracks as &dyn ToSqlValue,
            ],
        )
        .map_err(|e| AppError::internal(e));

    match result {
        Ok(id) => {
            let created = json!({
                "id": id,
                "name": body.name,
                "rules": body.rules,
                "match_mode": match_mode,
                "sort_by": sort_by,
                "sort_order": sort_order,
                "max_tracks": body.max_tracks,
            });
            Ok((StatusCode::CREATED, Json(created)).into_response())
        }
        Err(e) => Err(e),
    }
}

async fn get_smart_playlist(
    State(state): State<SmartHttpState>,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, AppError> {
    let sql = if state.backend.engine() == Engine::Postgres {
        "SELECT id, name, rules, sort_by, sort_order, max_tracks, created_at, match_mode FROM smart_playlists WHERE id = $1"
    } else {
        "SELECT id, name, rules, sort_by, sort_order, max_tracks, created_at, match_mode FROM smart_playlists WHERE id = ?"
    };
    let result = state
        .backend
        .query_one(sql, &[&id as &dyn ToSqlValue])
        .map_err(|e| AppError::internal(e))?;

    match result {
        Some(cols) => {
            let rules_str = cols
                .get(2)
                .and_then(|v| v.as_string())
                .unwrap_or_else(|| "[]".into());
            let rules = serde_json::from_str::<Value>(&rules_str).unwrap_or(json!([]));
            Ok(Json(json!({
                "id": cols.get(0).and_then(|v| v.as_i64()),
                "name": cols.get(1).and_then(|v| v.as_string()),
                "rules": rules,
                "match_mode": cols.get(7).and_then(|v| v.as_string()).unwrap_or_else(|| "all".into()),
                "sort_by": cols.get(3).and_then(|v| v.as_string()),
                "sort_order": cols.get(4).and_then(|v| v.as_string()),
                "max_tracks": cols.get(5).and_then(|v| v.as_i64()),
                "created_at": cols.get(6).and_then(|v| v.as_string()),
            }))
            .into_response())
        }
        None => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

async fn update_smart_playlist(
    State(state): State<SmartHttpState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateSmartPlaylist>,
) -> Result<impl IntoResponse, AppError> {
    let pg = state.backend.engine() == Engine::Postgres;

    // Refuse les références circulaires avant d'écrire quoi que ce soit.
    if let Some(ref rules) = body.rules {
        let self_name = body
            .name
            .clone()
            .or_else(|| {
                DbRefResolver::new(&state.backend)
                    .smart_entity(RefKind::SmartPlaylist, id)
                    .map(|e| e.name)
            })
            .unwrap_or_else(|| format!("#{id}"));
        let resolver = DbRefResolver::new(&state.backend);
        smart_refs::check_no_cycle(
            &resolver,
            RefKind::SmartPlaylist,
            Some(id),
            &self_name,
            &rules.to_string(),
        )
        .map_err(AppError::bad_request)?;
    }

    if let Some(ref name) = body.name {
        let sql = if pg {
            "UPDATE smart_playlists SET name = $1 WHERE id = $2"
        } else {
            "UPDATE smart_playlists SET name = ? WHERE id = ?"
        };
        state
            .backend
            .execute(sql, &[name as &dyn ToSqlValue, &id as &dyn ToSqlValue])
            .ok();
    }
    if let Some(ref rules) = body.rules {
        let rules_str = rules.to_string();
        let sql = if pg {
            "UPDATE smart_playlists SET rules = $1 WHERE id = $2"
        } else {
            "UPDATE smart_playlists SET rules = ? WHERE id = ?"
        };
        state
            .backend
            .execute(
                sql,
                &[&rules_str as &dyn ToSqlValue, &id as &dyn ToSqlValue],
            )
            .ok();
    }
    if let Some(ref sort_by) = body.sort_by {
        let sql = if pg {
            "UPDATE smart_playlists SET sort_by = $1 WHERE id = $2"
        } else {
            "UPDATE smart_playlists SET sort_by = ? WHERE id = ?"
        };
        state
            .backend
            .execute(sql, &[sort_by as &dyn ToSqlValue, &id as &dyn ToSqlValue])
            .ok();
    }
    if let Some(ref sort_order) = body.sort_order {
        let sql = if pg {
            "UPDATE smart_playlists SET sort_order = $1 WHERE id = $2"
        } else {
            "UPDATE smart_playlists SET sort_order = ? WHERE id = ?"
        };
        state
            .backend
            .execute(
                sql,
                &[sort_order as &dyn ToSqlValue, &id as &dyn ToSqlValue],
            )
            .ok();
    }
    if let Some(ref max_tracks) = body.max_tracks {
        let sql = if pg {
            "UPDATE smart_playlists SET max_tracks = $1 WHERE id = $2"
        } else {
            "UPDATE smart_playlists SET max_tracks = ? WHERE id = ?"
        };
        state
            .backend
            .execute(
                sql,
                &[max_tracks as &dyn ToSqlValue, &id as &dyn ToSqlValue],
            )
            .ok();
    }
    if let Some(ref match_mode) = body.match_mode {
        let sql = if pg {
            "UPDATE smart_playlists SET match_mode = $1 WHERE id = $2"
        } else {
            "UPDATE smart_playlists SET match_mode = ? WHERE id = ?"
        };
        state
            .backend
            .execute(
                sql,
                &[match_mode as &dyn ToSqlValue, &id as &dyn ToSqlValue],
            )
            .ok();
    }

    // Return the updated smart playlist as JSON
    let sql = if pg {
        "SELECT id, name, rules, sort_by, sort_order, max_tracks, created_at, match_mode FROM smart_playlists WHERE id = $1"
    } else {
        "SELECT id, name, rules, sort_by, sort_order, max_tracks, created_at, match_mode FROM smart_playlists WHERE id = ?"
    };
    let result = state
        .backend
        .query_one(sql, &[&id as &dyn ToSqlValue])
        .map_err(|e| AppError::internal(e))?;

    match result {
        Some(cols) => {
            let rules_str = cols
                .get(2)
                .and_then(|v| v.as_string())
                .unwrap_or_else(|| "[]".into());
            let rules = serde_json::from_str::<Value>(&rules_str).unwrap_or(json!([]));
            Ok(Json(json!({
                "id": cols.get(0).and_then(|v| v.as_i64()),
                "name": cols.get(1).and_then(|v| v.as_string()),
                "rules": rules,
                "sort_by": cols.get(3).and_then(|v| v.as_string()),
                "sort_order": cols.get(4).and_then(|v| v.as_string()),
                "max_tracks": cols.get(5).and_then(|v| v.as_i64()),
                "created_at": cols.get(6).and_then(|v| v.as_string()),
            }))
            .into_response())
        }
        None => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

async fn delete_smart_playlist(
    State(state): State<SmartHttpState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let sql = if state.backend.engine() == Engine::Postgres {
        "DELETE FROM smart_playlists WHERE id = $1"
    } else {
        "DELETE FROM smart_playlists WHERE id = ?"
    };
    state.backend.execute(sql, &[&id as &dyn ToSqlValue]).ok();
    Json(json!({"deleted": true, "id": id}))
}

/// Build WHERE, ORDER, LIMIT clauses from smart playlist criteria.
pub(crate) fn build_smart_query(
    rules_json: &str,
    match_mode: &str,
    sort_by: &str,
    sort_order: &str,
    max_tracks: Option<i64>,
    ctx: &RefCtx,
) -> (String, String, String) {
    let (w, o, l, _) =
        build_smart_query_rapport(rules_json, match_mode, sort_by, sort_order, max_tracks, ctx);
    (w, o, l)
}

/// La même construction, plus la liste des règles REFUSÉES.
///
/// 🔴 #4467 — depuis #4469 une règle intraduisible rend FAUX au lieu de la
/// bibliothèque entière ; c'était la moitié du défaut. L'autre moitié tient
/// dans le corps de l'issue : « rien ne le dit à l'utilisateur, la playlist
/// rend simplement moins de titres ». Le seul témoin était un `warn` dans le
/// journal du serveur, que personne ne lit depuis l'écran d'édition.
///
/// Chaque entrée est lisible telle quelle : `champ opérateur`, la même forme
/// que [`catalogue::regles_hors_service`], pour que l'aperçu puisse la
/// montrer. Une liste vide veut dire « toutes les règles ont été appliquées ».
pub(crate) fn build_smart_query_rapport(
    rules_json: &str,
    match_mode: &str,
    sort_by: &str,
    sort_order: &str,
    max_tracks: Option<i64>,
    ctx: &RefCtx,
) -> (String, String, String, Vec<String>) {
    let mut refusees: Vec<String> = Vec::new();
    let rules: Vec<Value> = serde_json::from_str(rules_json).unwrap_or_default();
    let joiner = if match_mode == "any" { " OR " } else { " AND " };

    let mut conditions = Vec::new();
    for rule in &rules {
        let field = rule.get("field").and_then(|v| v.as_str()).unwrap_or("");
        // 🔴 #4467 — les DEUX clés, comme les collections, le catalogue et les
        // favoris de service. Ce `.get("op")` seul était le dernier analyseur à
        // n'en lire qu'une : une règle en `operator` y retombait sur le défaut
        // `contains` et rendait le contraire de ce qu'elle demandait
        // (`{"operator":"!="}` → 338 pistes au lieu de 42 507 sur le .18).
        let raw_op = regles_sql::lire_op(rule);
        // 🔴 #1231 — la normalisation du module partagé, qui connaît AUSSI
        // `=`, `!=`, `<`, `>` et `is_empty`. Celle d'avant n'en reconnaissait
        // que quatre formes : une règle écrite `{"op": "="}` tombait donc sur
        // `_ => continue` et la playlist rendait toute la bibliothèque.
        let op = regles_sql::normaliser_op(raw_op);
        let value = rule.get("value").and_then(|v| v.as_str()).unwrap_or("");

        // --- règles « référence » (collection / playlist / favori) ---
        if smart_refs::is_ref_field(field) {
            conditions.push(smart_refs::track_ref_condition(field, raw_op, value, ctx));
            continue;
        }

        let val_clean = value.replace('\'', "''");
        let val_unaccented = strip_accents(&val_clean);
        let has_accents = val_clean != val_unaccented;

        // Les deux champs qui ne sont pas une colonne : ils se comptent
        // ailleurs, dans l'historique d'écoute.
        let cond = if field == "play_count" {
            let n = value.parse::<i64>().unwrap_or(0);
            let comparateur = match op {
                "=" => "=",
                ">=" => ">=",
                ">" => ">",
                "<=" => "<=",
                "<" => "<",
                _ => "=",
            };
            if n == 0 && comparateur == "=" {
                // « Jamais écoutée » : l'absence de ligne, pas un compte nul.
                "t.id NOT IN (SELECT track_id FROM listen_history WHERE track_id IS NOT NULL)"
                    .to_string()
            } else {
                format!(
                    "t.id IN (SELECT track_id FROM listen_history WHERE track_id IS NOT NULL \
                     GROUP BY track_id HAVING COUNT(*) {comparateur} {n})"
                )
            }
        } else {
            // 🔴 #1231 — une règle qu'on ne sait pas traduire rend FAUX, jamais
            // « pas de condition ». Le `_ => continue` d'avant valait « vrai
            // pour tout » : l'utilisateur voulait restreindre et obtenait la
            // bibliothèque entière. Soixante-six combinaisons que l'éditeur
            // propose passaient par là — dont `composer` en entier et `title`
            // avec tout autre opérateur que « contient ».
            match regles_sql::colonne_piste(field)
                .and_then(|col| regles_sql::condition(col, op, &value))
            {
                Some(c) => c,
                None => {
                    tracing::warn!(
                        champ = %field,
                        operateur = %op,
                        "regle_intraduisible_playlist_faux"
                    );
                    // #4467 — et on le DIT : l'appelant remonte cette liste à
                    // l'aperçu, au lieu de laisser l'utilisateur deviner
                    // pourquoi sa playlist s'est vidée.
                    refusees.push(format!("{field} {op}"));
                    regles_sql::FAUX.to_string()
                }
            }
        };
        conditions.push(cond);
    }

    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(joiner))
    };

    let order = if sort_by == "random" {
        "ORDER BY RANDOM()".to_string()
    } else {
        format!(
            "ORDER BY {} {}",
            match sort_by {
                "artist" => "ar.name",
                "album" => "al.title",
                "year" => "t.year",
                "duration" => "t.duration_ms",
                "added_at" => "t.id",
                "play_count" => "t.play_count",
                _ => "t.title",
            },
            if sort_order == "desc" { "DESC" } else { "ASC" }
        )
    };

    let limit_clause = max_tracks.map(|n| format!("LIMIT {n}")).unwrap_or_default();

    (where_clause, order, limit_clause, refusees)
}

/// #4806 — le socle de TOUTE résolution de smart playlist : un titre banni
/// par ce profil en est exclu d'office, sans règle à configurer (décision
/// Bertrand, 23/09/2026). Posé ICI, à l'exécution, et non dans
/// `build_smart_query` : la clause des règles est parenthésée en bloc, donc
/// un `match_mode = any` (`a OR b`) ne peut pas contourner le socle par
/// précédence — `WHERE (a OR b) AND socle`. Sans règle : `WHERE socle`.
///
/// `build_smart_query` sert aussi aux références imbriquées (`smart_refs`,
/// sous-requête `t.id IN (…)`) : le socle de la requête ENGLOBANTE suffit,
/// un titre banni n'y passe pas davantage.
fn avec_le_socle_des_bannis(where_clause: &str, profile_id: i64) -> String {
    let socle = tune_core::db::facet_filter::banned_tracks_excluded(profile_id);
    match where_clause.trim().strip_prefix("WHERE ") {
        Some(regles) => format!("WHERE ({regles}) AND {socle}"),
        None => format!("WHERE {socle}"),
    }
}

/// Execute a smart query and return track rows as JSON values.
fn execute_smart_track_query(
    state: &SmartHttpState,
    profile_id: i64,
    where_clause: &str,
    order: &str,
    limit_clause: &str,
) -> Result<Vec<Value>, AppError> {
    let where_clause = avec_le_socle_des_bannis(where_clause, profile_id);
    let needs_play_count = order.contains("play_count");
    let play_count_join = if needs_play_count {
        "LEFT JOIN (SELECT track_id, COUNT(*) AS play_count FROM listen_history WHERE track_id IS NOT NULL GROUP BY track_id) lh ON t.id = lh.track_id"
    } else {
        ""
    };
    // Replace play_count reference with the computed column (COALESCE for never-played)
    let order = if needs_play_count {
        order.replace("t.play_count", "COALESCE(lh.play_count, 0)")
    } else {
        order.to_string()
    };
    let sql = format!(
        // `al.is_compilation` en 10 (#1957) : la ligne porte déjà quatre
        // colonnes d'album (`al.title`, `al.id`, `al.cover_path`, l'année),
        // et c'est d'elles que `smart_collection_albums` bâtit ses albums.
        "SELECT t.id, t.title, ar.name, al.title, t.duration_ms, t.format, t.genre, t.year, al.id, al.cover_path, al.is_compilation \
         FROM tracks t \
         LEFT JOIN albums al ON t.album_id = al.id \
         LEFT JOIN artists ar ON t.artist_id = ar.id \
         {} {} {} {}",
        play_count_join, where_clause, order, limit_clause
    );

    let rows = state
        .backend
        .query_many(&sql, &[])
        .map_err(|e| AppError::internal(format!("{e}")))?;
    Ok(rows
        .iter()
        .map(|cols| {
            json!({
                "id": cols.get(0).and_then(|v| v.as_i64()),
                "title": cols.get(1).and_then(|v| v.as_string()),
                "artist_name": cols.get(2).and_then(|v| v.as_string()),
                "album_title": cols.get(3).and_then(|v| v.as_string()),
                "duration_ms": cols.get(4).and_then(|v| v.as_i64()).unwrap_or(0),
                "format": cols.get(5).and_then(|v| v.as_string()),
                "genre": cols.get(6).and_then(|v| v.as_string()),
                "year": cols.get(7).and_then(|v| v.as_i64()).map(|y| y as i32),
                "album_id": cols.get(8).and_then(|v| v.as_i64()),
                "cover_path": cols.get(9).and_then(|v| v.as_string()),
                // Drapeau de l'ALBUM de la piste (#1957), aux côtés des
                // autres champs d'album déjà portés ici. Ajout : aucune clé
                // existante ne bouge.
                "is_compilation": tune_core::db::album_repo::drapeau_compilation(cols.get(10)),
            })
        })
        .collect())
}

/// Ajoute au résultat les pistes FAVORITES des services que nomme une règle
/// « Source » (#4299), puis applique la borne sur l'ensemble.
///
/// Les pistes de la bibliothèque viennent d'abord, dans leur tri ; les favoris
/// de service suivent, triés selon la même clé quand elle a un sens pour eux
/// (titre, artiste, album, date d'ajout). Un tri propre à la bibliothèque —
/// année, durée, écoutes — ne s'applique pas à une ligne qui n'a pas la donnée.
#[allow(clippy::too_many_arguments)]
fn avec_favoris_de_service(
    state: &SmartHttpState,
    mut pistes: Vec<Value>,
    rules_json: &str,
    match_mode: &str,
    profile_id: i64,
    sort_by: &str,
    sort_order: &str,
    max_tracks: Option<i64>,
) -> Result<Vec<Value>, AppError> {
    let Some(sql) = source_streaming::requete(
        rules_json,
        match_mode,
        Objet::Piste,
        profile_id,
        sort_by,
        sort_order,
        max_tracks,
    ) else {
        return Ok(pistes);
    };
    let lignes = state
        .backend
        .query_many(&sql, &[])
        .map_err(AppError::internal)?;
    pistes.extend(lignes.iter().map(|c| source_streaming::piste_json(c)));
    if let Some(n) = max_tracks.filter(|n| *n >= 0) {
        pistes.truncate(n as usize);
    }
    Ok(pistes)
}

/// Les pistes du CATALOGUE d'un service que les règles demandent — #4473,
/// second volet.
///
/// ## Pourquoi les deux chemins avaient divergé
///
/// La v0.9.158 a porté le catalogue au chemin des ALBUMS
/// (`smart_collections::avec_albums_de_catalogue`), la v0.9.159 à son aperçu.
/// Le chemin des PISTES, lui, n'a jamais rien su : `regles_sql::colonne_piste`
/// traduit `source` par `COALESCE(NULLIF(t.source,''),'local')`, donc
/// `catalogue:qobuz` devenait `t.source = 'catalogue:qobuz'` — une condition
/// que rien ne satisfait, puisqu'une piste de service n'est pas dans `tracks`.
/// La playlist d'essai `Test Qobuz Coltrane` rendait zéro piste, et c'est
/// précisément le cas d'origine de l'issue.
///
/// ## Ce qui est RÉUTILISÉ
///
/// La lecture des règles est celle des collections, à l'identique :
/// [`catalogue::lire`] décide du service, de la cible et des refus. Seul
/// l'aller-retour change, parce que l'objet rendu change — une playlist veut
/// des pistes.
///
/// ## Comment on obtient des PISTES
///
/// * artiste seul → les titres phares de l'artiste chez ce service
///   (`get_artist_top_tracks`). Un service n'offre pas « toutes les pistes de
///   cet artiste » d'un bloc : il faudrait un aller-retour par album.
/// * un album nommé (seul, ou à côté d'un artiste) → les pistes de cet album
///   (`get_album_tracks`), ce qui est exact et borné. L'artiste, s'il est
///   nommé, TRIE encore le résultat.
/// * un titre de piste nommé → il trie le résultat, comme le titre d'album
///   trie une discographie côté collections.
async fn avec_pistes_de_catalogue(
    state: &SmartHttpState,
    mut pistes: Vec<Value>,
    rules_json: &str,
    max_tracks: Option<i64>,
) -> Result<Vec<Value>, AppError> {
    let demande = match catalogue::lire(rules_json, catalogue::Objet::Piste) {
        catalogue::Lecture::Aucune => return Ok(pistes),
        catalogue::Lecture::Refus(motif) => return Err(AppError::bad_request(motif)),
        catalogue::Lecture::Demande(d) => d,
    };
    // Cloné, pas emprunté : ce qui vit en travers d'un `.await` doit être
    // `Send` (même raison que `smart_collections::avec_albums_de_catalogue`).
    let Some(distant) = state.catalogue.clone() else {
        return Err(AppError::bad_request(
            "Le catalogue des services n'est pas disponible ici.",
        ));
    };

    let service = demande.service;
    let egal = |a: &str, b: &str| a.trim().to_lowercase() == b.trim().to_lowercase();
    let mut trouves = match (&demande.cible, &demande.titre_album) {
        // Un album nommé : ses pistes, c'est exact et borné.
        (catalogue::Cible::Album(titre), _) => distant.pistes_par_album(&service, titre).await,
        (catalogue::Cible::Artiste(nom), Some(titre)) => {
            let mut p = distant.pistes_par_album(&service, titre).await;
            // L'artiste était nommé aussi : il trie les éditions homonymes.
            p.retain(|t| egal(&t.artist, nom));
            p
        }
        (catalogue::Cible::Artiste(nom), None) => distant.pistes_par_artiste(&service, nom).await,
    };
    if let Some(titre) = &demande.titre_piste {
        trouves.retain(|t| egal(&t.title, titre));
    }
    // La même forme qu'une piste de service (`source_streaming::piste_json`) :
    // pas d'`id` local, une provenance et un identifiant de service.
    pistes.extend(trouves.into_iter().map(|t| {
        json!({
            "id": Value::Null,
            "source": t.service,
            "source_id": t.source_id,
            "title": t.title,
            "artist_name": t.artist,
            "album_title": t.album,
            "cover_path": t.cover_url,
            "duration_ms": t.duration_ms.unwrap_or(0),
            "format": Value::Null,
            "genre": Value::Null,
            "year": Value::Null,
            "album_id": Value::Null,
            "is_compilation": false,
        })
    }));
    if let Some(n) = max_tracks.filter(|n| *n >= 0) {
        pistes.truncate(n as usize);
    }
    Ok(pistes)
}

/// Load a smart playlist's criteria from the DB. Returns (rules_json, sort_by, sort_order, max_tracks).
fn load_smart_criteria(
    state: &SmartHttpState,
    id: i64,
) -> Result<Option<(String, String, String, String, Option<i64>)>, AppError> {
    let sql = if state.backend.engine() == Engine::Postgres {
        "SELECT rules, sort_by, sort_order, max_tracks, match_mode FROM smart_playlists WHERE id = $1"
    } else {
        "SELECT rules, sort_by, sort_order, max_tracks, match_mode FROM smart_playlists WHERE id = ?"
    };
    let result = state
        .backend
        .query_one(sql, &[&id as &dyn ToSqlValue])
        .map_err(|e| AppError::internal(e))?;
    Ok(result.map(|cols| {
        (
            cols.get(0)
                .and_then(|v| v.as_string())
                .unwrap_or_else(|| "[]".into()),
            cols.get(1)
                .and_then(|v| v.as_string())
                .unwrap_or_else(|| "title".into()),
            cols.get(2)
                .and_then(|v| v.as_string())
                .unwrap_or_else(|| "asc".into()),
            cols.get(4)
                .and_then(|v| v.as_string())
                .unwrap_or_else(|| "all".into()),
            cols.get(3).and_then(|v| v.as_i64()),
        )
    }))
}

async fn resolve_tracks(
    State(state): State<SmartHttpState>,
    profile: ActiveProfile,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, AppError> {
    let Some((rules_json, sort_by, sort_order, match_mode, max_tracks)) =
        load_smart_criteria(&state, id)?
    else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };

    // Le résolveur et son contexte tiennent des RÉFÉRENCES à l'état : ils
    // doivent mourir avant le `.await` du catalogue, sinon le futur n'est plus
    // `Send` et axum refuse le handler (même contrainte que
    // `smart_collections::resolve_albums`).
    let items = {
        let resolver = DbRefResolver::new(&state.backend);
        let ctx = RefCtx::root(&resolver, Some(profile.id()));
        let (where_clause, order, limit_clause) = build_smart_query(
            &rules_json,
            &match_mode,
            &sort_by,
            &sort_order,
            max_tracks,
            &ctx,
        );
        let items =
            execute_smart_track_query(&state, profile.id(), &where_clause, &order, &limit_clause)?;
        avec_favoris_de_service(
            &state,
            items,
            &rules_json,
            &match_mode,
            profile.id(),
            &sort_by,
            &sort_order,
            max_tracks,
        )?
    };
    // 🔴 #4473 — le catalogue du service, comme le chemin des ALBUMS le fait
    // depuis la v0.9.158. Sans cet appel, `Test Qobuz Coltrane` rend 0 piste.
    let items = avec_pistes_de_catalogue(&state, items, &rules_json, max_tracks).await?;

    Ok(Json(json!(items)).into_response())
}

async fn smart_collection_albums(
    State(state): State<SmartHttpState>,
    profile: ActiveProfile,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, AppError> {
    let Some((rules_json, sort_by, sort_order, match_mode, max_tracks)) =
        load_smart_criteria(&state, id)?
    else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };

    // Les références à l'état meurent avant le `.await` du catalogue (voir
    // `resolve_tracks`).
    let tracks = {
        let resolver = DbRefResolver::new(&state.backend);
        let ctx = RefCtx::root(&resolver, Some(profile.id()));
        let (where_clause, order, limit_clause) = build_smart_query(
            &rules_json,
            &match_mode,
            &sort_by,
            &sort_order,
            max_tracks,
            &ctx,
        );
        execute_smart_track_query(&state, profile.id(), &where_clause, &order, &limit_clause)?
    };
    // 🔴 #4473 — cette vue regroupe les pistes de la playlist : elle doit voir
    // le catalogue et surtout porter les MÊMES refus. Sans cet appel, une
    // règle « catalogue » y serait ignorée en silence, ce qui est exactement
    // le défaut que l'issue reproche au chemin des pistes.
    let tracks = avec_pistes_de_catalogue(&state, tracks, &rules_json, max_tracks).await?;

    // Group tracks by album_id, dedup albums. Une piste de service n'a pas
    // d'`album_id` : son album se reconnaît à son titre et à son artiste.
    let mut seen = std::collections::HashSet::new();
    let mut albums: Vec<Value> = Vec::new();
    for track in &tracks {
        let clef = match track.get("album_id").and_then(|v| v.as_i64()) {
            Some(id) => format!("id:{id}"),
            None => {
                let texte = |c: &str| {
                    track
                        .get(c)
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_lowercase()
                };
                if texte("album_title").is_empty() {
                    continue;
                }
                format!("distant:{}|{}", texte("album_title"), texte("artist_name"))
            }
        };
        if seen.insert(clef) {
            albums.push(json!({
                "album_id": track.get("album_id").cloned().unwrap_or(Value::Null),
                "album_title": track.get("album_title"),
                "artist_name": track.get("artist_name"),
                "cover_path": track.get("cover_path"),
                "year": track.get("year"),
                // #1957 — l'album que cette vue sert porte son drapeau,
                // comme partout ailleurs. Toujours un booléen : la ligne
                // vient du même décodeur, qui ne rend jamais `null`.
                "is_compilation": track
                    .get("is_compilation")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
            }));
        }
    }

    Ok(Json(json!({"albums": albums, "total": albums.len()})).into_response())
}

async fn preview_smart_collection(
    State(state): State<SmartHttpState>,
    profile: ActiveProfile,
    Json(body): Json<PreviewRequest>,
) -> Result<Json<Value>, AppError> {
    let rules_json = body.rules.to_string();
    let match_mode = body.match_mode.as_deref().unwrap_or("all");
    let sort_by = body.sort_by.as_deref().unwrap_or("title");
    let sort_order = body.sort_order.as_deref().unwrap_or("asc");

    // Les références à l'état meurent avant le `.await` du catalogue (voir
    // `resolve_tracks`).
    let (items, refusees) = {
        let resolver = DbRefResolver::new(&state.backend);
        let ctx = RefCtx::root(&resolver, Some(profile.id()));
        let (where_clause, order, limit_clause, refusees) = build_smart_query_rapport(
            &rules_json,
            match_mode,
            sort_by,
            sort_order,
            body.max_tracks,
            &ctx,
        );
        let items =
            execute_smart_track_query(&state, profile.id(), &where_clause, &order, &limit_clause)?;
        (
            avec_favoris_de_service(
                &state,
                items,
                &rules_json,
                match_mode,
                profile.id(),
                sort_by,
                sort_order,
                body.max_tracks,
            )?,
            refusees,
        )
    };
    // 🔴 #4473 — l'aperçu est ce que la playlist rendra : sans cet appel, une
    // règle « catalogue » s'y montrerait vide et sans refus.
    let items = avec_pistes_de_catalogue(&state, items, &rules_json, body.max_tracks).await?;

    // 🔴 #4467 — l'aperçu DIT ce qu'il n'a pas su appliquer. Une règle
    // intraduisible rend FAUX depuis #4469 : la playlist se vide sans rien
    // annoncer, et l'utilisateur n'a que le journal du serveur pour le savoir.
    // Champ toujours présent, vide quand tout a été appliqué : un client qui
    // teste sa longueur n'a pas à distinguer « absent » de « aucune ».
    Ok(Json(
        json!({"tracks": items, "total": items.len(), "regles_refusees": refusees}),
    ))
}

pub(crate) fn strip_accents(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'À' | 'Á' | 'Â' | 'Ã' | 'Ä' | 'Å' => {
                if c.is_uppercase() {
                    'A'
                } else {
                    'a'
                }
            }
            'è' | 'é' | 'ê' | 'ë' | 'È' | 'É' | 'Ê' | 'Ë' => {
                if c.is_uppercase() {
                    'E'
                } else {
                    'e'
                }
            }
            'ì' | 'í' | 'î' | 'ï' | 'Ì' | 'Í' | 'Î' | 'Ï' => {
                if c.is_uppercase() {
                    'I'
                } else {
                    'i'
                }
            }
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'Ò' | 'Ó' | 'Ô' | 'Õ' | 'Ö' => {
                if c.is_uppercase() { 'O' } else { 'o' }
            }
            'ù' | 'ú' | 'û' | 'ü' | 'Ù' | 'Ú' | 'Û' | 'Ü' => {
                if c.is_uppercase() {
                    'U'
                } else {
                    'u'
                }
            }
            'ñ' | 'Ñ' => {
                if c.is_uppercase() {
                    'N'
                } else {
                    'n'
                }
            }
            'ç' | 'Ç' => {
                if c.is_uppercase() {
                    'C'
                } else {
                    'c'
                }
            }
            'ÿ' | 'Ÿ' => {
                if c.is_uppercase() {
                    'Y'
                } else {
                    'y'
                }
            }
            'æ' | 'Æ' => {
                if c.is_uppercase() {
                    'A'
                } else {
                    'a'
                }
            }
            'œ' | 'Œ' => {
                if c.is_uppercase() {
                    'O'
                } else {
                    'o'
                }
            }
            'ø' | 'Ø' => {
                if c.is_uppercase() {
                    'O'
                } else {
                    'o'
                }
            }
            _ => c,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{build_smart_query, build_smart_query_rapport};
    use crate::smart_refs::{EmptyResolver, RefCtx};

    fn where_of(rules: &str) -> String {
        let ctx = RefCtx::root(&EmptyResolver, Some(1));
        let (w, _order, _limit) = build_smart_query(rules, "all", "title", "asc", None, &ctx);
        w
    }

    fn refusees_de(rules: &str) -> Vec<String> {
        let ctx = RefCtx::root(&EmptyResolver, Some(1));
        let (_w, _o, _l, refusees) =
            build_smart_query_rapport(rules, "all", "title", "asc", None, &ctx);
        refusees
    }

    /// 🔴 #4467 — la clé `operator` était ignorée côté PLAYLISTS.
    ///
    /// Mesuré sur le .18 en v0.9.162 le 23/09/2026 sur 42 844 pistes,
    /// `artist` = « Miles Davis » : `{"op":"!="}` rendait 42 507 pistes,
    /// `{"operator":"!="}` en rendait **338** — l'exact contraire, sans une
    /// ligne de journal, parce que le défaut `contains` prenait la place de
    /// l'opérateur écrit.
    #[test]
    fn les_deux_cles_d_operateur_donnent_la_meme_clause() {
        for (op_, operator) in [
            (
                r#"[{"field":"artist","op":"!=","value":"Miles Davis"}]"#,
                r#"[{"field":"artist","operator":"!=","value":"Miles Davis"}]"#,
            ),
            (
                r#"[{"field":"artist","op":"is_empty","value":""}]"#,
                r#"[{"field":"artist","operator":"is_empty","value":""}]"#,
            ),
            (
                r#"[{"field":"year","op":"greater_than","value":"2000"}]"#,
                r#"[{"field":"year","operator":"greater_than","value":"2000"}]"#,
            ),
        ] {
            assert_eq!(
                where_of(op_),
                where_of(operator),
                "la clé `operator` doit produire la MÊME clause que `op` : {operator}"
            );
        }
    }

    /// Et la clause obtenue est bien celle de l'opérateur écrit, pas celle du
    /// repli `contains` : sans cette assertion, deux clauses également fausses
    /// passeraient la comparaison ci-dessus.
    #[test]
    fn operator_seul_applique_l_operateur_ecrit_pas_le_repli_contains() {
        let w = where_of(r#"[{"field":"artist","operator":"!=","value":"Miles Davis"}]"#);
        assert!(
            w.contains("!="),
            "`operator: !=` doit nier, pas retomber sur `contains` : {w}"
        );
        assert!(
            !w.contains("LIKE"),
            "le repli `contains` produit un LIKE — il ne doit plus être pris : {w}"
        );

        // `is_empty` lu comme `contains ''` donnait `LIKE '%%'`, vrai pour
        // toute la bibliothèque : 42 844 pistes mesurées sur le .18.
        let vide = where_of(r#"[{"field":"artist","operator":"is_empty","value":""}]"#);
        assert!(
            vide.contains("IS NULL"),
            "`operator: is_empty` doit tester la nullité : {vide}"
        );
    }

    /// 🔴 #4467, seconde moitié — une règle refusée doit se DIRE.
    ///
    /// Depuis #4469 elle rend FAUX (zéro piste) au lieu de la bibliothèque
    /// entière ; mais rien ne l'annonçait hors du journal du serveur, et
    /// l'utilisateur voyait seulement sa playlist se vider.
    #[test]
    fn une_regle_refusee_est_nommee_dans_le_rapport() {
        let r = refusees_de(r#"[{"field":"zzz_inexistant","op":"equals","value":"x"}]"#);
        assert_eq!(r.len(), 1, "une règle refusée, un signalement : {r:?}");
        assert!(
            r[0].contains("zzz_inexistant"),
            "le signalement doit nommer le champ : {r:?}"
        );
        // Une règle qui s'applique ne se signale pas.
        assert!(
            refusees_de(r#"[{"field":"composer","op":"equals","value":"Mozart"}]"#).is_empty(),
            "une règle traduite ne doit rien signaler"
        );
        // Et la règle refusée reste FAUSSE : le rapport s'ajoute au garde-fou
        // de #4469, il ne le remplace pas.
        assert!(
            where_of(r#"[{"field":"zzz_inexistant","op":"equals","value":"x"}]"#).contains("1 = 0")
        );
    }
    /// 🔴 #1231 — une règle que le moteur ne sait pas traduire rendait
    /// « pas de condition », c'est-à-dire VRAI pour tout.
    ///
    /// Mesuré sur le .18 le 19/09/2026 : `composer = Mozart` rendait les
    /// 47 118 pistes de la bibliothèque, et `title = <n'importe quoi>` aussi.
    /// Soixante-six combinaisons que l'éditeur propose tombaient dans le
    /// `_ => continue` final. L'utilisateur voulait restreindre ; il obtenait
    /// tout.
    #[test]
    fn une_regle_intraduisible_rend_faux_jamais_tout() {
        let w = where_of(r#"[{"field":"zzz_inexistant","op":"equals","value":"x"}]"#);
        assert!(
            w.contains("1 = 0"),
            "un champ inconnu doit rendre FAUX, pas rien — obtenu : {w:?}"
        );
        assert!(
            !w.is_empty(),
            "une clause vide vaut « toute la bibliothèque »"
        );
    }

    /// Le champ que l'éditeur proposait et que le moteur ignorait en entier.
    #[test]
    fn le_compositeur_filtre_enfin() {
        let w = where_of(r#"[{"field":"composer","op":"equals","value":"Mozart"}]"#);
        assert!(w.contains("t.composer"), "{w}");
        assert!(w.contains("LOWER"), "la comparaison ignore la casse : {w}");
        assert!(
            !w.contains("1 = 0"),
            "la règle doit être TRADUITE, pas refusée : {w}"
        );
    }

    /// « Titre = X » ne rendait que « contient » ; tout le reste tombait.
    #[test]
    fn le_titre_accepte_autre_chose_que_contient() {
        for op in ["equals", "not_equals", "starts_with", "is_empty"] {
            let regle = format!(r#"[{{"field":"title","op":"{op}","value":"Kind of Blue"}}]"#);
            let w = where_of(&regle);
            assert!(w.contains("t.title"), "opérateur {op} : {w}");
            assert!(
                !w.contains("1 = 0"),
                "opérateur {op} doit être traduit : {w}"
            );
        }
    }

    /// Les graphies d'opérateur qui circulent — éditeur, semis, anciennes
    /// règles — mènent au même SQL.
    #[test]
    fn les_graphies_d_operateur_se_rejoignent() {
        let a = where_of(r#"[{"field":"artist","op":"="  ,"value":"Coltrane"}]"#);
        let b = where_of(r#"[{"field":"artist","op":"eq" ,"value":"Coltrane"}]"#);
        let c = where_of(r#"[{"field":"artist","op":"equals","value":"Coltrane"}]"#);
        assert_eq!(a, b);
        assert_eq!(b, c);
        assert!(a.contains("ar.name"), "{a}");
    }

    /// « Jamais écoutée » reste l'absence de ligne d'historique, pas un
    /// compte nul — la remise à plat ne devait pas l'emporter.
    #[test]
    fn jamais_ecoutee_reste_l_absence_d_historique() {
        let w = where_of(r#"[{"field":"play_count","op":"equals","value":"0"}]"#);
        assert!(
            w.contains("NOT IN (SELECT track_id FROM listen_history"),
            "{w}"
        );
    }

    // #1008: "Album/Année/Format contient X" produced no WHERE at all (the
    // (field, op) pairs were unhandled → `_ => continue`), so every rule was
    // silently dropped and the playlist returned all tracks. Each must now
    // build a real condition on the right column.
    #[test]
    fn album_contains_builds_condition() {
        // #1231 — la forme a changé : la traduction générique compare en
        // minuscules des DEUX côtés, ce que l'ancienne ne faisait pas pour
        // l'album. « lux » trouve désormais « LUX ». On tient le comportement,
        // pas la graphie du SQL.
        let w = where_of(r#"[{"field":"album","op":"contains","value":"LUX"}]"#);
        assert!(w.contains("al.title"), "got: {w}");
        assert!(
            w.contains("LOWER"),
            "la comparaison doit ignorer la casse : {w}"
        );
        assert!(
            w.contains("'%LUX%'") || w.contains("LOWER('%LUX%')"),
            "got: {w}"
        );
    }

    #[test]
    fn year_contains_builds_condition() {
        let w = where_of(r#"[{"field":"year","op":"contains","value":"2025"}]"#);
        assert!(w.contains("CAST(t.year AS TEXT) LIKE '%2025%'"), "got: {w}");
    }

    #[test]
    fn format_contains_is_case_insensitive() {
        // #1231 — `LOWER`/`LOWER` au lieu de `UPPER`/`UPPER` : même
        // insensibilité, une seule façon de l'écrire pour tous les champs.
        let w = where_of(r#"[{"field":"format","op":"contains","value":"flac"}]"#);
        assert!(
            w.contains("LOWER(t.format) LIKE LOWER('%flac%')"),
            "got: {w}"
        );
    }

    #[test]
    fn three_contains_rules_are_all_applied_not_dropped() {
        // The exact combination Sergio reported: three rules that previously
        // all fell through to `continue`, yielding an empty WHERE. Now each
        // contributes its own AND-ed condition.
        let w = where_of(
            r#"[{"field":"album","op":"contains","value":"LUX"},
                {"field":"year","op":"contains","value":"2025"},
                {"field":"format","op":"contains","value":"FLAC"}]"#,
        );
        assert!(
            w.starts_with("WHERE"),
            "expected a non-empty WHERE, got: {w}"
        );
        assert_eq!(w.matches(" AND ").count(), 2, "three rules → two ANDs: {w}");
    }

    // #4299 — la règle « Source » était abandonnée (`_ => continue`).
    #[test]
    fn source_rule_is_applied_not_dropped() {
        let w = where_of(r#"[{"field":"source","op":"equals","value":"upnp"}]"#);
        assert!(
            w.contains("LOWER(COALESCE(NULLIF(t.source, ''), 'local')) = LOWER('upnp')"),
            "got: {w}"
        );
        let w = where_of(r#"[{"field":"source","op":"not_equals","value":"local"}]"#);
        assert!(w.contains("!= LOWER('local')"), "got: {w}");
    }

    #[test]
    fn favorite_track_rule_builds_condition() {
        let w = where_of(r#"[{"field":"favorite","op":"is","value":"track"}]"#);
        assert!(w.contains("t.id IN (SELECT item_id FROM favorites"), "{w}");
        assert!(w.contains("item_type = 'track'"), "{w}");
    }

    #[test]
    fn in_playlist_rule_combines_with_genre() {
        let w = where_of(
            r#"[{"field":"genre","op":"contains","value":"Rock"},
                {"field":"in_playlist","op":"not_in","value":"classic:3"}]"#,
        );
        assert!(w.contains(" AND "), "{w}");
        assert!(
            w.contains("t.id NOT IN (SELECT track_id FROM playlist_tracks"),
            "{w}"
        );
    }

    /// #4806 — le socle « pas banni » enveloppe la clause des règles : avec
    /// `match_mode = any`, `a OR b` est parenthésé, sinon le socle ne
    /// s'appliquerait qu'à `b`. Sans règle, le socle seul.
    #[test]
    fn le_socle_des_bannis_enveloppe_les_regles() {
        let ctx = RefCtx::root(&EmptyResolver, Some(7));
        let (w, _, _) = build_smart_query(
            r#"[{"field":"genre","op":"contains","value":"Rock"},
                {"field":"genre","op":"contains","value":"Jazz"}]"#,
            "any",
            "title",
            "asc",
            None,
            &ctx,
        );
        assert!(w.contains(" OR "), "témoin : {w}");
        let socle = super::avec_le_socle_des_bannis(&w, 7);
        assert!(socle.starts_with("WHERE ("), "{socle}");
        assert!(
            socle.contains(
                ") AND NOT EXISTS (SELECT 1 FROM hidden_items hb WHERE hb.profile_id = 7"
            ),
            "{socle}"
        );
        assert!(
            socle.contains("hb.item_type = 'track' AND hb.item_id = t.id)"),
            "{socle}"
        );

        // Sans règle : `WHERE socle`, pas `WHERE () AND …`.
        let vide = super::avec_le_socle_des_bannis("", 7);
        assert!(vide.starts_with("WHERE NOT EXISTS"), "{vide}");
        assert!(!vide.contains("()"), "{vide}");
    }
}

/// 🔴 #4473, second volet — le CATALOGUE d'un service dans une PLAYLIST.
///
/// Le cas d'origine de Bertrand, `Test Qobuz Coltrane`, est une playlist. Au
/// tag `v0.9.159`, `git grep catalogue -- tune-smart-http/src/regles_sql.rs`
/// ne rendait rien : la règle `source = catalogue:qobuz` se traduisait en
/// `COALESCE(NULLIF(t.source,''),'local') = 'catalogue:qobuz'`, une condition
/// que rien ne satisfait — zéro piste, sans un mot.
///
/// Un service simulé : la garde porte sur ce que le module DÉCIDE, pas sur ce
/// que Qobuz répond. Aucun réseau, aucune clé.
#[cfg(test)]
mod catalogue_de_service {
    use crate::SmartHttpState;
    use crate::catalogue::{AlbumDistant, CatalogueDistant, PisteDistante};
    use std::sync::Arc;
    use tune_core::db::sqlite::SqliteDb;

    struct ServiceSimule;

    fn p(service: &str, titre: &str, artiste: &str, album: &str) -> PisteDistante {
        PisteDistante {
            service: service.into(),
            source_id: format!("t-{titre}"),
            title: titre.into(),
            artist: artiste.into(),
            album: album.into(),
            cover_url: None,
            duration_ms: Some(300_000),
        }
    }

    #[async_trait::async_trait]
    impl CatalogueDistant for ServiceSimule {
        async fn albums_par_artiste(&self, _s: &str, _n: &str) -> Vec<AlbumDistant> {
            Vec::new()
        }
        async fn albums_par_titre(&self, _s: &str, _t: &str) -> Vec<AlbumDistant> {
            Vec::new()
        }
        async fn pistes_par_artiste(&self, service: &str, nom: &str) -> Vec<PisteDistante> {
            vec![
                p(service, "Giant Steps", nom, "Giant Steps"),
                p(service, "Naima", nom, "Giant Steps"),
            ]
        }
        async fn pistes_par_album(&self, service: &str, titre: &str) -> Vec<PisteDistante> {
            vec![
                p(service, "Blue Train", "John Coltrane", titre),
                // Une réédition d'un homonyme : c'est l'artiste nommé par la
                // règle qui doit trancher.
                p(service, "Blue Train", "Un hommage", titre),
            ]
        }
    }

    fn etat(avec_service: bool) -> SmartHttpState {
        let db = SqliteDb::open_in_memory().expect("base");
        db.init_schema().expect("schéma");
        let backend: Arc<dyn tune_core::db::backend::DbBackend> = Arc::new(db);
        let e = SmartHttpState::new(backend);
        if avec_service {
            e.avec_catalogue(Arc::new(ServiceSimule))
        } else {
            e
        }
    }

    /// Les règles exactes de `Test Qobuz Coltrane`.
    const FABIENM: &str = r#"[{"field":"source","op":"=","value":"catalogue:qobuz"},
                              {"field":"artist","op":"=","value":"John Coltrane"}]"#;

    /// 🔴 Le témoin : rouge avant (0 piste), vert après.
    #[tokio::test]
    async fn la_playlist_de_fabienm_rend_des_pistes_du_catalogue() {
        let locales = vec![serde_json::json!({"id": 1, "title": "Une piste locale"})];
        let Ok(r) = super::avec_pistes_de_catalogue(&etat(true), locales, FABIENM, None).await
        else {
            panic!("le catalogue doit répondre")
        };
        assert_eq!(r.len(), 3, "la piste locale et les deux distantes : {r:?}");
        assert_eq!(r[1]["source"], "qobuz");
        assert_eq!(r[1]["title"], "Giant Steps");
        assert_eq!(r[1]["artist_name"], "John Coltrane");
        assert_eq!(
            r[1]["id"],
            serde_json::Value::Null,
            "une piste distante n'a pas d'id local"
        );
        assert_eq!(r[1]["duration_ms"], 300_000, "la durée du service");
    }

    /// La contre-épreuve : sans règle `catalogue:`, rien ne bouge — une
    /// playlist « source = qobuz » reste les FAVORIS, et l'ancienne valeur ne
    /// change pas de sens.
    #[tokio::test]
    async fn sans_regle_de_catalogue_rien_ne_change() {
        let locales = vec![serde_json::json!({"id": 1})];
        for regles in [
            r#"[{"field":"source","op":"=","value":"qobuz"},
                {"field":"artist","op":"=","value":"John Coltrane"}]"#,
            r#"[{"field":"artist","op":"=","value":"John Coltrane"}]"#,
        ] {
            let Ok(r) =
                super::avec_pistes_de_catalogue(&etat(true), locales.clone(), regles, None).await
            else {
                panic!("aucun catalogue demandé : {regles}")
            };
            assert_eq!(r, locales, "{regles}");
        }
    }

    /// Les mêmes bornes que le premier volet : sans artiste ni album nommé,
    /// on REFUSE — on ne rend ni tout ni rien (leçon de #4469).
    #[tokio::test]
    async fn sans_cible_la_playlist_est_refusee() {
        let sans = r#"[{"field":"source","op":"=","value":"catalogue:qobuz"},
                       {"field":"year","op":"=","value":"2025"}]"#;
        let Err(e) = super::avec_pistes_de_catalogue(&etat(true), Vec::new(), sans, None).await
        else {
            panic!("sans cible, il faut refuser")
        };
        assert_eq!(e.status, axum::http::StatusCode::BAD_REQUEST);
        assert!(
            e.message.contains("artiste ou un album"),
            "le refus doit dire ce qui manque : {}",
            e.message
        );
    }

    /// Une règle que le service ne sait pas filtrer est NOMMÉE, jamais
    /// ignorée en silence. C'est le cas MIXTE : une règle locale à côté d'une
    /// règle de service.
    #[tokio::test]
    async fn une_regle_locale_a_cote_du_catalogue_est_refusee_en_la_nommant() {
        let mixte = r#"[{"field":"source","op":"=","value":"catalogue:qobuz"},
                        {"field":"artist","op":"=","value":"John Coltrane"},
                        {"field":"format","op":"=","value":"FLAC"},
                        {"field":"play_count","op":">=","value":"3"}]"#;
        let Err(e) = super::avec_pistes_de_catalogue(&etat(true), Vec::new(), mixte, None).await
        else {
            panic!("une règle hors service doit être refusée")
        };
        let m = e.message;
        assert!(m.contains("format ="), "la règle doit être nommée : {m}");
        assert!(
            m.contains("play_count >="),
            "les deux, pas la première : {m}"
        );
    }

    /// 🔴 Le champ `title` d'une PLAYLIST est le titre de la PISTE : il trie
    /// ce que le service rend, il ne cherche pas un album de ce nom.
    #[tokio::test]
    async fn le_titre_de_piste_trie_ce_que_le_service_rend() {
        let r = format!(
            r#"[{{"field":"source","op":"=","value":"catalogue:qobuz"}},
                {{"field":"artist","op":"=","value":"John Coltrane"}},
                {{"field":"title","op":"=","value":"Naima"}}]"#
        );
        let Ok(r) = super::avec_pistes_de_catalogue(&etat(true), Vec::new(), &r, None).await else {
            panic!("demande valide")
        };
        assert_eq!(r.len(), 1, "une seule piste porte ce titre : {r:?}");
        assert_eq!(r[0]["title"], "Naima");
    }

    /// Un album nommé à côté d'un artiste : ce sont les pistes de l'ALBUM, et
    /// l'artiste écarte l'édition homonyme.
    #[tokio::test]
    async fn un_album_nomme_rend_ses_pistes_et_l_artiste_tranche() {
        let r = r#"[{"field":"source","op":"=","value":"catalogue:qobuz"},
                    {"field":"artist","op":"=","value":"John Coltrane"},
                    {"field":"album","op":"=","value":"Blue Train"}]"#;
        let Ok(r) = super::avec_pistes_de_catalogue(&etat(true), Vec::new(), r, None).await else {
            panic!("demande valide")
        };
        assert_eq!(r.len(), 1, "l'hommage homonyme est écarté : {r:?}");
        assert_eq!(r[0]["album_title"], "Blue Train");
        assert_eq!(r[0]["artist_name"], "John Coltrane");
    }

    /// Sans registre de services — en épreuve, et partout où il n'existe pas —
    /// on refuse plutôt que de rendre vide en silence.
    #[tokio::test]
    async fn sans_registre_on_refuse_au_lieu_de_rendre_vide() {
        let Err(e) = super::avec_pistes_de_catalogue(&etat(false), Vec::new(), FABIENM, None).await
        else {
            panic!("sans registre, il faut refuser")
        };
        assert!(
            e.message.contains("pas disponible"),
            "le refus doit se lire : {}",
            e.message
        );
    }

    /// 🔴 Le témoin par la ROUTE : « écrit mais pas branché » est le défaut
    /// que cette issue reproche déjà une fois. L'aperçu de l'éditeur de
    /// playlists doit appeler le catalogue, pas seulement le savoir faire.
    async fn apercu(
        regles: &str,
    ) -> Result<axum::Json<serde_json::Value>, tune_http_types::AppError> {
        let e = etat(true);
        e.backend
            .execute(
                "CREATE TABLE IF NOT EXISTS streaming_favorites (
                     id INTEGER PRIMARY KEY AUTOINCREMENT, profile_id INTEGER,
                     item_type TEXT, service TEXT, service_id TEXT, title TEXT,
                     artist TEXT, album TEXT, cover_url TEXT, created_at TEXT,
                     position TEXT)",
                &[],
            )
            .expect("favoris");
        super::preview_smart_collection(
            axum::extract::State(e),
            tune_http_types::ActiveProfile(1),
            axum::Json(super::PreviewRequest {
                rules: serde_json::from_str(regles).expect("json"),
                match_mode: None,
                sort_by: None,
                sort_order: None,
                max_tracks: None,
            }),
        )
        .await
    }

    /// 🔴 Rouge au tag `v0.9.159` : l'aperçu rendait `total = 0`, parce que la
    /// seule traduction de `catalogue:qobuz` était
    /// `COALESCE(NULLIF(t.source,''),'local') = 'catalogue:qobuz'`.
    #[tokio::test]
    async fn l_apercu_d_une_playlist_montre_le_catalogue() {
        let Ok(r) = apercu(FABIENM).await else {
            panic!("l'aperçu doit répondre")
        };
        assert_eq!(r.0["total"], 2, "{}", r.0);
        assert_eq!(r.0["tracks"][0]["title"], "Giant Steps");
        assert_eq!(r.0["tracks"][0]["source"], "qobuz");
    }

    /// … et il REFUSE ce que la playlist refusera, au moment où on l'écrit.
    #[tokio::test]
    async fn l_apercu_d_une_playlist_refuse_comme_elle() {
        let sans = r#"[{"field":"source","op":"=","value":"catalogue:qobuz"},
                       {"field":"year","op":"=","value":"2025"}]"#;
        assert!(apercu(sans).await.is_err(), "sans cible : refus");
    }

    /// 🔴 La contre-épreuve du DÉFAUT lui-même : la traduction SQL locale,
    /// seule, ne peut RIEN rendre. C'est elle qui rendait 0 piste au tag
    /// `v0.9.159`, et elle n'a pas changé — c'est l'étape d'après qui manquait.
    #[test]
    fn la_traduction_sql_seule_ne_peut_rien_rendre() {
        use crate::smart_refs::{EmptyResolver, RefCtx};
        let ctx = RefCtx::root(&EmptyResolver, Some(1));
        let (w, _, _) = super::build_smart_query(FABIENM, "all", "title", "asc", None, &ctx);
        assert!(
            w.contains("'catalogue:qobuz'"),
            "la règle se traduit toujours en une condition sur t.source : {w}"
        );
        assert!(
            w.contains("COALESCE(NULLIF(t.source, ''), 'local')"),
            "et aucune piste de la table `tracks` ne porte cette provenance : {w}"
        );
    }

    /// La borne de la playlist s'applique à l'ENSEMBLE, local et distant.
    #[tokio::test]
    async fn la_borne_coupe_l_ensemble() {
        let locales = vec![serde_json::json!({"id": 1})];
        let Ok(r) = super::avec_pistes_de_catalogue(&etat(true), locales, FABIENM, Some(2)).await
        else {
            panic!("demande valide")
        };
        assert_eq!(r.len(), 2, "une locale et une distante : {r:?}");
    }
}
