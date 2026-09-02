use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tune_core::db::backend::ToSqlValue;

use crate::SmartHttpState;
use crate::smart_refs::{self, DbRefResolver, RefCtx, RefKind, RefResolver};
use tune_http_types::{ActiveProfile, AppError};

#[derive(Deserialize)]
struct CreateCollection {
    name: String,
    rules: Value,
    match_mode: Option<String>,
    sort_by: Option<String>,
    sort_order: Option<String>,
    max_limit: Option<i64>,
    description: Option<String>,
    icon: Option<String>,
    color: Option<String>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct UpdateCollection {
    name: Option<String>,
    rules: Option<Value>,
    match_mode: Option<String>,
    sort_by: Option<String>,
    sort_order: Option<String>,
    max_limit: Option<i64>,
    description: Option<String>,
    icon: Option<String>,
    color: Option<String>,
}

#[derive(Deserialize)]
struct PreviewRequest {
    rules: Value,
    match_mode: Option<String>,
    sort_by: Option<String>,
    sort_order: Option<String>,
    max_limit: Option<i64>,
}

pub fn router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    SmartHttpState: axum::extract::FromRef<S>,
    ActiveProfile: axum::extract::FromRequestParts<S>,
    <ActiveProfile as axum::extract::FromRequestParts<S>>::Rejection: IntoResponse,
{
    Router::new()
        .route("/", get(list_collections).post(create_collection))
        .route(
            "/{id}",
            get(get_collection)
                .put(update_collection)
                .delete(delete_collection),
        )
        .route("/{id}/albums", get(resolve_albums))
        .route("/preview", post(preview_albums))
}

/// Normalize a stored `sort_order` value to the bare `asc`/`desc` the
/// SortOrder enum expects. The tune-core save path stores it JSON-encoded
/// (`"asc"` with quotes) while this route's save path stores it raw (`asc`);
/// stripping surrounding quotes tolerates both, avoiding the compile error
/// `unknown variant "asc", expected asc or desc`.
fn normalize_sort_order(raw: Option<String>) -> String {
    raw.map(|s| s.trim_matches('"').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "asc".into())
}

/// Decode a row from `smart_collections` into a JSON object.
/// Column order: id(0), name(1), rules(2), match_mode(3), sort_by(4),
/// sort_order(5), max_limit(6), description(7), icon(8), color(9), created_at(10).
fn decode_collection_row(r: &[tune_core::db::backend::SqlValue]) -> Value {
    let rules_str = r
        .get(2)
        .and_then(|v| v.as_string())
        .unwrap_or_else(|| "[]".into());
    let rules = serde_json::from_str::<Value>(&rules_str).unwrap_or(json!([]));
    json!({
        "id": r.get(0).and_then(|v| v.as_i64()),
        "name": r.get(1).and_then(|v| v.as_string()),
        "rules": rules,
        "match_mode": r.get(3).and_then(|v| v.as_string()).unwrap_or_else(|| "all".into()),
        "sort_by": r.get(4).and_then(|v| v.as_string()),
        "sort_order": normalize_sort_order(r.get(5).and_then(|v| v.as_string())),
        "max_limit": r.get(6).and_then(|v| v.as_i64()),
        "description": r.get(7).and_then(|v| v.as_string()),
        "icon": r.get(8).and_then(|v| v.as_string()),
        "color": r.get(9).and_then(|v| v.as_string()),
        "created_at": r.get(10).and_then(|v| v.as_string()),
    })
}

async fn list_collections(
    State(state): State<SmartHttpState>,
    profile: ActiveProfile,
) -> Result<Json<Value>, AppError> {
    let rows = state
        .backend
        .query_many(
            "SELECT id, name, rules, match_mode, sort_by, sort_order, max_limit, \
         description, icon, color, created_at \
         FROM smart_collections ORDER BY name",
            &[],
        )
        .map_err(AppError::internal)?;

    let resolver = DbRefResolver::new(&state.backend);
    let ctx = RefCtx::root(&resolver, Some(profile.id()));
    let items: Vec<Value> = rows
        .iter()
        .map(|r| {
            let mut col = decode_collection_row(r);
            let rules_str = r
                .get(2)
                .and_then(|v| v.as_string())
                .unwrap_or_else(|| "[]".into());
            // Count with the SAME album-query engine as resolve_albums so the
            // count is always produced and matches the album view. The old
            // SmartCollection::compile_sql path silently failed to deserialize
            // richer rule operators (>=, <=, in, is_null…) and then dropped
            // album_count entirely, so a collection using those showed no count
            // (Jean Marie). build_album_query inlines escaped values → no bound
            // params (mirrors execute_album_query). max_limit=None reports the
            // full membership, not the capped view.
            let match_mode = col["match_mode"].as_str().unwrap_or("all");
            let sort_by = col["sort_by"].as_str().unwrap_or("title");
            let sort_order = col["sort_order"].as_str().unwrap_or("asc");
            let (where_clause, _order, _limit) =
                build_album_query(&rules_str, match_mode, sort_by, sort_order, None, &ctx);

            let album_count_sql = format!(
                "SELECT COUNT(DISTINCT al.id) FROM albums al \
                 LEFT JOIN artists ar ON al.artist_id = ar.id \
                 LEFT JOIN tracks t ON t.album_id = al.id {where_clause}"
            );
            if let Ok(rs) = state.backend.query_many(&album_count_sql, &[]) {
                col["album_count"] = json!(
                    rs.first()
                        .and_then(|r| r.first())
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0)
                );
            }
            let track_count_sql = format!(
                "SELECT COUNT(DISTINCT t.id) FROM albums al \
                 LEFT JOIN artists ar ON al.artist_id = ar.id \
                 LEFT JOIN tracks t ON t.album_id = al.id {where_clause}"
            );
            if let Ok(rs) = state.backend.query_many(&track_count_sql, &[]) {
                col["track_count"] = json!(
                    rs.first()
                        .and_then(|r| r.first())
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0)
                );
            }

            // Pochettes de la mosaïque — quatre au plus, DISTINCTES.
            //
            // Une collection intelligente n'a pas de table d'appartenance :
            // c'est une RÈGLE, évaluée à la demande. On ne peut donc pas
            // joindre ses pochettes d'un coup comme pour une playlist ; il faut
            // rejouer sa requête. C'est déjà ce que font les deux comptages
            // ci-dessus — une troisième requête s'inscrit dans le même modèle
            // et ne change pas la nature de cette route.
            //
            // `HAVING` plutôt qu'un `AND` ajouté à `{where_clause}` : cette
            // clause est TANTÔT VIDE, tantôt un `WHERE …`. Y coller un `AND`
            // produirait « ... AND al.cover_path ... » sans `WHERE` sur une
            // collection sans règle, donc une erreur SQL sur le cas le plus
            // banal.
            //
            // Ordre par `MIN(al.title)` : déterministe, donc la mosaïque ne
            // change pas d'un rafraîchissement à l'autre. Ce n'est PAS le tri
            // propre de la collection — une vignette de quatre cases n'est pas
            // un aperçu du classement, et rejouer le tri complet ici coûterait
            // une jointure de plus pour un gain invisible.
            let covers = pochettes_mosaique(&state.backend, &where_clause);
            col["covers"] = json!(covers);

            col
        })
        .collect();
    Ok(Json(json!(items)))
}

/// Les quatre pochettes de la mosaïque d'une collection intelligente.
///
/// Extraite du corps de la route pour être TESTABLE contre une vraie base. Les
/// deux tests plus bas en portaient chacun une copie mot pour mot : une copie
/// ne garde rien — elle reste verte pendant que la production dérive.
///
/// `where_clause` est celle de la règle. Elle est TANTÔT VIDE, tantôt un
/// `WHERE …` : d'où le `HAVING`, et non un `AND` accolé, qui produirait
/// « … AND al.cover_path … » sans `WHERE` sur une collection sans règle — donc
/// une erreur SQL sur le cas le plus banal.
///
/// L'ordre par `MIN(al.title)` est déterministe, donc la mosaïque ne change pas
/// d'un rafraîchissement à l'autre. Ce n'est PAS le tri propre de la
/// collection : une vignette de quatre cases n'est pas un aperçu du classement.
fn pochettes_mosaique(
    backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
    where_clause: &str,
) -> Vec<String> {
    // Groupe sur le TITRE de l'album, insensible a la casse, et non sur
    // le chemin de la pochette.
    //
    // Un meme disque est stocke comme PLUSIEURS lignes d'`albums`, une
    // par artiste credite, chacune avec son propre fichier de pochette
    // en cache : autant de chemins, une seule image. Groupe sur le
    // chemin, un seul disque remplissait la mosaique — la collection
    // « Classique » de Bertrand montrait quatre fois le coffret Gorecki
    // parmi ses 139 albums (02/09/2026).
    //
    // 🔴 Le titre SEUL, sans l'artiste : c'est l'artiste qui varie d'une
    // ligne a l'autre. « Les indispensables du piano » en compte treize,
    // un par pianiste. Grouper sur artiste + titre les laissait passer
    // tous les treize.
    //
    // On demande SEIZE groupes pour n'en garder que quatre. La marge
    // sert deux fois : deux titres distincts peuvent partager un chemin
    // (une compilation et sa reedition), et `cle_pochette` en rapproche
    // d'autres encore en retirant les suffixes « (24bit) ».
    let covers_sql = format!(
        "SELECT MIN(al.cover_path) AS cover, MIN(al.title) AS t FROM albums al \
         LEFT JOIN artists ar ON al.artist_id = ar.id \
         LEFT JOIN tracks t ON t.album_id = al.id {where_clause} \
         GROUP BY LOWER(COALESCE(al.title, al.cover_path, '')) \
         HAVING MIN(al.cover_path) IS NOT NULL AND MIN(al.cover_path) <> '' \
         ORDER BY t LIMIT 16"
    );
    let mut covers: Vec<String> = Vec::new();
    let mut cles: Vec<String> = Vec::new();
    if let Ok(rs) = backend.query_many(&covers_sql, &[]) {
        for r in &rs {
            let Some(c) = r.first().and_then(|v| v.as_string()) else {
                continue;
            };
            if covers.len() == 4 {
                break;
            }
            let titre = r.get(1).and_then(|v| v.as_string());
            let cle = tune_core::library::mosaique::cle_pochette(titre.as_deref(), &c);
            if cles.iter().any(|k| k == &cle) || covers.iter().any(|x| x == &c) {
                continue;
            }
            cles.push(cle);
            covers.push(c);
        }
    }

    covers
}

async fn create_collection(
    State(state): State<SmartHttpState>,
    Json(body): Json<CreateCollection>,
) -> Result<impl IntoResponse, AppError> {
    let rules_json = body.rules.to_string();
    let match_mode = body.match_mode.clone().unwrap_or_else(|| "all".into());
    let sort_by = body.sort_by.clone();
    let sort_order = body.sort_order.clone().unwrap_or_else(|| "asc".into());

    // Refuse les références circulaires (A ⊂ B ⊂ A) entre entités smart.
    let resolver = DbRefResolver::new(&state.backend);
    smart_refs::check_no_cycle(
        &resolver,
        RefKind::SmartCollection,
        None,
        &body.name,
        &rules_json,
    )
    .map_err(AppError::bad_request)?;

    let id = state
        .backend
        .execute_returning_id(
            "INSERT INTO smart_collections \
         (name, rules, match_mode, sort_by, sort_order, max_limit, description, icon, color) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            &[
                &body.name as &dyn ToSqlValue,
                &rules_json as &dyn ToSqlValue,
                &match_mode as &dyn ToSqlValue,
                &sort_by as &dyn ToSqlValue,
                &sort_order as &dyn ToSqlValue,
                &body.max_limit as &dyn ToSqlValue,
                &body.description as &dyn ToSqlValue,
                &body.icon as &dyn ToSqlValue,
                &body.color as &dyn ToSqlValue,
            ],
        )
        .map_err(AppError::internal)?;

    // Relire l'objet persisté au lieu de fabriquer une réponse partielle :
    // le client réutilise immédiatement ce contrat et `created_at` fait partie
    // des champs annoncés par SmartCollection (#2732).
    let row = state
        .backend
        .query_one(
            "SELECT id, name, rules, match_mode, sort_by, sort_order, max_limit, \
         description, icon, color, created_at \
         FROM smart_collections WHERE id = $1",
            &[&id as &dyn ToSqlValue],
        )
        .map_err(AppError::internal)?
        .ok_or_else(|| AppError::internal("collection créée mais introuvable"))?;

    Ok((StatusCode::CREATED, Json(decode_collection_row(&row))).into_response())
}

async fn get_collection(
    State(state): State<SmartHttpState>,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, AppError> {
    let row = state
        .backend
        .query_one(
            "SELECT id, name, rules, match_mode, sort_by, sort_order, max_limit, \
         description, icon, color, created_at \
         FROM smart_collections WHERE id = $1",
            &[&id as &dyn ToSqlValue],
        )
        .map_err(AppError::internal)?;

    match row {
        Some(r) => Ok(Json(decode_collection_row(&r)).into_response()),
        None => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

async fn update_collection(
    State(state): State<SmartHttpState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateCollection>,
) -> Result<impl IntoResponse, AppError> {
    // Refuse les références circulaires avant d'écrire quoi que ce soit.
    if let Some(ref rules) = body.rules {
        let self_name = body
            .name
            .clone()
            .or_else(|| {
                DbRefResolver::new(&state.backend)
                    .smart_entity(RefKind::SmartCollection, id)
                    .map(|e| e.name)
            })
            .unwrap_or_else(|| format!("#{id}"));
        let resolver = DbRefResolver::new(&state.backend);
        smart_refs::check_no_cycle(
            &resolver,
            RefKind::SmartCollection,
            Some(id),
            &self_name,
            &rules.to_string(),
        )
        .map_err(AppError::bad_request)?;
    }
    if let Some(ref name) = body.name {
        state.backend.execute(
            "UPDATE smart_collections SET name = $1, updated_at = CURRENT_TIMESTAMP WHERE id = $2",
            &[name as &dyn ToSqlValue, &id as &dyn ToSqlValue],
        ).ok();
    }
    if let Some(ref rules) = body.rules {
        let rules_json = rules.to_string();
        state.backend.execute(
            "UPDATE smart_collections SET rules = $1, updated_at = CURRENT_TIMESTAMP WHERE id = $2",
            &[&rules_json as &dyn ToSqlValue, &id as &dyn ToSqlValue],
        ).ok();
    }
    if let Some(ref match_mode) = body.match_mode {
        state.backend.execute(
            "UPDATE smart_collections SET match_mode = $1, updated_at = CURRENT_TIMESTAMP WHERE id = $2",
            &[match_mode as &dyn ToSqlValue, &id as &dyn ToSqlValue],
        ).ok();
    }
    if let Some(ref sort_by) = body.sort_by {
        state.backend.execute(
            "UPDATE smart_collections SET sort_by = $1, updated_at = CURRENT_TIMESTAMP WHERE id = $2",
            &[sort_by as &dyn ToSqlValue, &id as &dyn ToSqlValue],
        ).ok();
    }
    if let Some(ref sort_order) = body.sort_order {
        state.backend.execute(
            "UPDATE smart_collections SET sort_order = $1, updated_at = CURRENT_TIMESTAMP WHERE id = $2",
            &[sort_order as &dyn ToSqlValue, &id as &dyn ToSqlValue],
        ).ok();
    }
    if let Some(ref max_limit) = body.max_limit {
        state.backend.execute(
            "UPDATE smart_collections SET max_limit = $1, updated_at = CURRENT_TIMESTAMP WHERE id = $2",
            &[max_limit as &dyn ToSqlValue, &id as &dyn ToSqlValue],
        ).ok();
    }
    if let Some(ref description) = body.description {
        state.backend.execute(
            "UPDATE smart_collections SET description = $1, updated_at = CURRENT_TIMESTAMP WHERE id = $2",
            &[description as &dyn ToSqlValue, &id as &dyn ToSqlValue],
        ).ok();
    }
    if let Some(ref icon) = body.icon {
        state.backend.execute(
            "UPDATE smart_collections SET icon = $1, updated_at = CURRENT_TIMESTAMP WHERE id = $2",
            &[icon as &dyn ToSqlValue, &id as &dyn ToSqlValue],
        ).ok();
    }
    if let Some(ref color) = body.color {
        state.backend.execute(
            "UPDATE smart_collections SET color = $1, updated_at = CURRENT_TIMESTAMP WHERE id = $2",
            &[color as &dyn ToSqlValue, &id as &dyn ToSqlValue],
        ).ok();
    }

    // Return the updated collection as JSON
    let row = state
        .backend
        .query_one(
            "SELECT id, name, rules, match_mode, sort_by, sort_order, max_limit, \
         description, icon, color, created_at \
         FROM smart_collections WHERE id = $1",
            &[&id as &dyn ToSqlValue],
        )
        .map_err(AppError::internal)?;

    match row {
        Some(r) => Ok(Json(decode_collection_row(&r)).into_response()),
        None => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

async fn delete_collection(
    State(state): State<SmartHttpState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    state
        .backend
        .execute(
            "DELETE FROM smart_collections WHERE id = $1",
            &[&id as &dyn ToSqlValue],
        )
        .ok();
    Json(json!({"deleted": true, "id": id}))
}

fn resolve_timestamp_sql(input: &str) -> String {
    // Relative forms: "now-90d", "90d", "90" — N days ago. The seeded
    // "🆕 Récents" collection stores the bare "90d" form, which used to fall
    // through to a literal string ('90d') that no date ever compares against.
    let rest = input.strip_prefix("now-").unwrap_or(input);
    let digits = rest.strip_suffix('d').unwrap_or(rest);
    if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
        let days: i64 = digits.parse().unwrap_or(30);
        return format!("DATETIME('now', '-{days} days')");
    }
    format!("'{}'", input.replace('\'', "''"))
}

/// Build WHERE, ORDER, LIMIT clauses from smart collection criteria (album-level).
///
/// This is THE smart-collection rule engine: the list/albums/preview endpoints,
/// the Oxygen `collection` facet and `/library/tracks?collection=` all go
/// through it, so a collection always counts and filters the same set
/// everywhere. (The legacy `SmartCollection::compile_sql` in tune-core diverged
/// — raw `any` match_mode read as ALL, `added_at`/`rating`/`play_count` hit
/// phantom `tracks` columns, unknown fields fell back to `t.title` — which made
/// whole collections vanish from the facet or count the entire library.)
/// The WHERE references aliases `al` (albums), `ar` (artists), `t` (tracks).
pub fn build_album_query(
    rules_json: &str,
    match_mode: &str,
    sort_by: &str,
    sort_order: &str,
    max_limit: Option<i64>,
    ctx: &RefCtx,
) -> (String, String, String) {
    let rules: Vec<Value> = serde_json::from_str(rules_json).unwrap_or_default();

    let mut conditions = Vec::new();
    for rule in &rules {
        let field = rule.get("field").and_then(|v| v.as_str()).unwrap_or("");
        let raw_op = rule
            .get("operator")
            .or_else(|| rule.get("op"))
            .and_then(|v| v.as_str())
            .unwrap_or("contains");
        let op = match raw_op {
            "=" | "eq" | "equals" => "=",
            "!=" | "ne" | "not_equals" => "!=",
            ">=" | "gte" | "greater_than" | "greater_equal" => ">=",
            ">" | "gt" => ">",
            "<=" | "lte" | "less_than" | "less_equal" => "<=",
            "<" | "lt" => "<",
            // tune-core seed/editor spelling — same semantics as is_null.
            "is_empty" | "empty" => "is_null",
            "is_not_empty" | "not_empty" => "is_not_null",
            other => other,
        };
        let value_raw = rule.get("value");
        let value = value_raw
            .map(|v| match v {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                _ => v.to_string(),
            })
            .unwrap_or_default();
        let esc = value.replace('\'', "''");

        // --- règles « référence » (collection / playlist / favori) ---
        if smart_refs::is_ref_field(field) {
            conditions.push(smart_refs::album_ref_condition(field, op, &value, ctx));
            continue;
        }

        // --- credit rules use a subquery, handle separately ---
        if field == "credit" {
            if op == "has" {
                if let Some(obj) = value_raw.and_then(|v| v.as_object()) {
                    let mut sub_conds = Vec::new();
                    if let Some(role) = obj
                        .get("role")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                    {
                        sub_conds.push(format!(
                            "LOWER(tc.role) LIKE LOWER('%{}%')",
                            role.replace('\'', "''")
                        ));
                    }
                    if let Some(artist) = obj
                        .get("artist_name")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                    {
                        sub_conds.push(format!(
                            "LOWER(tc.artist_name) LIKE LOWER('%{}%')",
                            artist.replace('\'', "''")
                        ));
                    }
                    if let Some(instr) = obj
                        .get("instrument")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                    {
                        // MÊME canonisation qu'à l'écriture des crédits
                        // (#2799 §4). L'enrichissement range désormais
                        // « grand piano » / « electric piano » sous `piano` ;
                        // si la règle cherchait le libellé brut saisi par
                        // l'utilisateur, une collection `instrument: Grand
                        // Piano` ne trouverait plus rien alors que les lignes
                        // existent. Deux normalisations, deux résultats.
                        let canon = tune_core::metadata::instruments::canoniser_instrument(instr);
                        let motif = if canon.is_empty() { instr } else { &canon };
                        sub_conds.push(format!(
                            "LOWER(tc.instrument) LIKE LOWER('%{}%')",
                            motif.replace('\'', "''")
                        ));
                    }
                    if !sub_conds.is_empty() {
                        conditions.push(format!(
                            "al.id IN (SELECT DISTINCT t2.album_id FROM tracks t2 \
                             JOIN track_credits tc ON tc.track_id = t2.id WHERE {})",
                            sub_conds.join(" AND ")
                        ));
                    }
                }
            }
            continue;
        }

        // --- added_at / last_played_at use timestamp logic ---
        if field == "added_at" {
            let ts = resolve_timestamp_sql(&value);
            // NB: "greater_than"/"less_than" normalize to ">="/"<=" above, so
            // both spellings must be matched here — the seeded "🆕 Récents"
            // (added_at greater_than 90d) used to fall through `_ => continue`,
            // dropping its only rule and matching the ENTIRE library.
            let cond = match op {
                ">" | ">=" => format!(
                    "al.id IN (SELECT DISTINCT t2.album_id FROM tracks t2 \
                     WHERE DATETIME(t2.file_mtime, 'unixepoch') {op} {ts})"
                ),
                "<" | "<=" => format!(
                    "al.id IN (SELECT DISTINCT t2.album_id FROM tracks t2 \
                     WHERE DATETIME(t2.file_mtime, 'unixepoch') {op} {ts})"
                ),
                "between" => {
                    if let Some(arr) = value_raw.and_then(|v| v.as_array()) {
                        let lo = arr.first().and_then(|v| v.as_str()).unwrap_or("2000-01-01");
                        let hi = arr.get(1).and_then(|v| v.as_str()).unwrap_or("2099-01-01");
                        let lo_sql = resolve_timestamp_sql(lo);
                        let hi_sql = resolve_timestamp_sql(hi);
                        format!(
                            "al.id IN (SELECT DISTINCT t2.album_id FROM tracks t2 \
                             WHERE DATETIME(t2.file_mtime, 'unixepoch') BETWEEN {lo_sql} AND {hi_sql})"
                        )
                    } else {
                        continue;
                    }
                }
                _ => continue,
            };
            conditions.push(cond);
            continue;
        }

        if field == "play_count" || field == "last_played_at" {
            let int_v = value.parse::<i64>().unwrap_or(0);
            let cond = match (field, op) {
                ("play_count", "=" | "==") if int_v == 0 => format!(
                    "al.id NOT IN (SELECT DISTINCT t3.album_id FROM tracks t3 \
                     JOIN listen_history lh ON lh.track_id = t3.id)"
                ),
                ("play_count", "=") => format!(
                    "al.id IN (SELECT t3.album_id FROM tracks t3 \
                     JOIN listen_history lh ON lh.track_id = t3.id \
                     GROUP BY t3.album_id HAVING COUNT(*) = {int_v})"
                ),
                ("play_count", ">=") => format!(
                    "al.id IN (SELECT t3.album_id FROM tracks t3 \
                     JOIN listen_history lh ON lh.track_id = t3.id \
                     GROUP BY t3.album_id HAVING COUNT(*) >= {int_v})"
                ),
                ("play_count", ">") => format!(
                    "al.id IN (SELECT t3.album_id FROM tracks t3 \
                     JOIN listen_history lh ON lh.track_id = t3.id \
                     GROUP BY t3.album_id HAVING COUNT(*) > {int_v})"
                ),
                ("play_count", "<") => format!(
                    "al.id NOT IN (SELECT t3.album_id FROM tracks t3 \
                     JOIN listen_history lh ON lh.track_id = t3.id \
                     GROUP BY t3.album_id HAVING COUNT(*) >= {int_v})"
                ),
                ("last_played_at", ">" | ">=") => {
                    let ts = resolve_timestamp_sql(&value);
                    format!(
                        "al.id IN (SELECT t3.album_id FROM tracks t3 \
                         JOIN listen_history lh ON lh.track_id = t3.id \
                         WHERE lh.listened_at {op} {ts} GROUP BY t3.album_id)"
                    )
                }
                ("last_played_at", "<" | "<=") => {
                    let ts = resolve_timestamp_sql(&value);
                    format!(
                        "al.id IN (SELECT t3.album_id FROM tracks t3 \
                         JOIN listen_history lh ON lh.track_id = t3.id \
                         GROUP BY t3.album_id HAVING MAX(lh.listened_at) {op} {ts})"
                    )
                }
                ("last_played_at", "is_null") => format!(
                    "al.id NOT IN (SELECT DISTINCT t3.album_id FROM tracks t3 \
                     JOIN listen_history lh ON lh.track_id = t3.id)"
                ),
                _ => continue,
            };
            conditions.push(cond);
            continue;
        }

        let col = match field {
            "genre" => "t.genre",
            "artist" | "artist_name" => "ar.name",
            "album" | "album_title" | "title" => "al.title",
            "composer" => "t.composer",
            "label" => "al.label",
            "format" => "t.format",
            "source" => "t.source",
            "cover_path" => "al.cover_path",
            // Fall back to the track year: many albums have a NULL al.year even
            // though the tracks carry the year in their tags (Elie — genre, a
            // track-level field, matched but year, album-level, didn't).
            "year" => "CAST(COALESCE(al.year, t.year) AS INTEGER)",
            "sample_rate" => "CAST(t.sample_rate AS INTEGER)",
            "bit_depth" => "CAST(t.bit_depth AS INTEGER)",
            "track_count" => "al.track_count",
            "duration" => "CAST(t.duration_ms AS INTEGER)",
            "track_number" => "CAST(t.track_number AS INTEGER)",
            "disc_number" => "CAST(t.disc_number AS INTEGER)",
            "bpm" => "CAST(t.bpm AS INTEGER)",
            "rating" => "CAST(t.rating AS INTEGER)",
            _ => continue,
        };

        let is_text = matches!(
            field,
            "genre"
                | "artist"
                | "artist_name"
                | "album"
                | "album_title"
                | "title"
                | "composer"
                | "label"
                | "format"
                | "source"
        );
        let int_val = || value.parse::<i64>().unwrap_or(0);

        let cond = match op {
            "=" if is_text => format!("LOWER({col}) = LOWER('{esc}')"),
            "!=" if is_text => format!("LOWER({col}) != LOWER('{esc}')"),
            "contains" => format!("LOWER({col}) LIKE LOWER('%{esc}%')"),
            "starts_with" => format!("LOWER({col}) LIKE LOWER('{esc}%')"),
            "is_null" => format!("({col} IS NULL OR {col} = '')"),
            "is_not_null" => format!("({col} IS NOT NULL AND {col} != '')"),
            "=" => format!("{col} = {}", int_val()),
            "!=" => format!("{col} != {}", int_val()),
            ">=" => format!("{col} >= {}", int_val()),
            ">" => format!("{col} > {}", int_val()),
            "<=" => format!("{col} <= {}", int_val()),
            "<" => format!("{col} < {}", int_val()),
            "between" => {
                if let Some(arr) = value_raw.and_then(|v| v.as_array()) {
                    let lo = arr.first().and_then(|v| v.as_i64()).unwrap_or(0);
                    let hi = arr.get(1).and_then(|v| v.as_i64()).unwrap_or(i64::MAX);
                    format!("{col} BETWEEN {lo} AND {hi}")
                } else {
                    let parts: Vec<&str> = value.splitn(2, ',').collect();
                    if parts.len() == 2 {
                        let lo = parts[0].trim().parse::<i64>().unwrap_or(0);
                        let hi = parts[1].trim().parse::<i64>().unwrap_or(i64::MAX);
                        format!("{col} BETWEEN {lo} AND {hi}")
                    } else {
                        format!("{col} = {}", int_val())
                    }
                }
            }
            "in" => {
                if let Some(arr) = value_raw.and_then(|v| v.as_array()) {
                    let items: Vec<String> = arr
                        .iter()
                        .map(|v| {
                            if let Some(s) = v.as_str() {
                                format!("'{}'", s.replace('\'', "''"))
                            } else {
                                v.to_string()
                            }
                        })
                        .collect();
                    format!("{col} IN ({})", items.join(","))
                } else {
                    let items: Vec<String> = value
                        .split(',')
                        .map(|s| format!("'{}'", s.trim().replace('\'', "''")))
                        .collect();
                    format!("LOWER({col}) IN ({})", items.join(","))
                }
            }
            _ => continue,
        };
        conditions.push(cond);
    }

    let joiner = if match_mode == "any" { " OR " } else { " AND " };

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
                "artist" | "artist_name" => "ar.name",
                "album" | "title" => "al.title",
                "year" => "al.year",
                "added_at" => "al.id",
                "track_count" => "track_count",
                "sample_rate" => "t.sample_rate",
                "label" => "al.label",
                _ => "al.title",
            },
            if sort_order == "desc" { "DESC" } else { "ASC" }
        )
    };

    let limit_clause = max_limit.map(|n| format!("LIMIT {n}")).unwrap_or_default();

    (where_clause, order, limit_clause)
}

/// Execute a smart album query and return album rows as JSON values.
fn execute_album_query(
    state: &SmartHttpState,
    where_clause: &str,
    order: &str,
    limit_clause: &str,
) -> Result<Vec<Value>, AppError> {
    let sql = format!(
        "SELECT al.id, al.title, ar.name, al.year, al.cover_path, al.genre, \
         COUNT(t.id) AS track_count \
         FROM albums al \
         LEFT JOIN artists ar ON al.artist_id = ar.id \
         LEFT JOIN tracks t ON t.album_id = al.id \
         {} \
         GROUP BY al.id, al.title, ar.name, al.year, al.cover_path, al.genre \
         {} {}",
        where_clause, order, limit_clause
    );
    tracing::debug!(sql = %sql, "smart_collection_album_query");

    let rows = state
        .backend
        .query_many(&sql, &[])
        .map_err(AppError::internal)?;

    Ok(rows
        .into_iter()
        .map(|r| {
            json!({
                "id": r.get(0).and_then(|v| v.as_i64()),
                "title": r.get(1).and_then(|v| v.as_string()),
                "artist_name": r.get(2).and_then(|v| v.as_string()),
                "year": r.get(3).and_then(|v| v.as_i64()),
                "cover_path": r.get(4).and_then(|v| v.as_string()),
                "genre": r.get(5).and_then(|v| v.as_string()),
                "track_count": r.get(6).and_then(|v| v.as_i64()).unwrap_or(0),
            })
        })
        .collect())
}

/// Load a smart collection's criteria from the DB.
fn load_collection_criteria(
    state: &SmartHttpState,
    id: i64,
) -> Result<Option<(String, String, String, String, Option<i64>)>, AppError> {
    let row = state
        .backend
        .query_one(
            "SELECT rules, match_mode, sort_by, sort_order, max_limit \
         FROM smart_collections WHERE id = $1",
            &[&id as &dyn ToSqlValue],
        )
        .map_err(AppError::internal)?;

    Ok(row.map(|r| {
        (
            r.get(0)
                .and_then(|v| v.as_string())
                .unwrap_or_else(|| "[]".into()),
            r.get(1)
                .and_then(|v| v.as_string())
                .unwrap_or_else(|| "all".into()),
            r.get(2)
                .and_then(|v| v.as_string())
                .unwrap_or_else(|| "title".into()),
            r.get(3)
                .and_then(|v| v.as_string())
                .unwrap_or_else(|| "asc".into()),
            r.get(4).and_then(|v| v.as_i64()),
        )
    }))
}

async fn resolve_albums(
    State(state): State<SmartHttpState>,
    profile: ActiveProfile,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, AppError> {
    let Some((rules_json, match_mode, sort_by, sort_order, max_limit)) =
        load_collection_criteria(&state, id)?
    else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };

    let resolver = DbRefResolver::new(&state.backend);
    let ctx = RefCtx::root(&resolver, Some(profile.id()));
    let (where_clause, order, limit_clause) = build_album_query(
        &rules_json,
        &match_mode,
        &sort_by,
        &sort_order,
        max_limit,
        &ctx,
    );
    let albums = execute_album_query(&state, &where_clause, &order, &limit_clause)?;

    // Return a bare array, matching the regular collections endpoint
    // (GET /library/collections/{id}/albums). The previous {"albums":[…],
    // "total":N} wrapper made the iOS client fail to decode
    // (DecodingError.typeMismatch: expected Array, found dictionary) when
    // opening a smart collection in remote mode; the web client already
    // accepts either shape.
    Ok(Json(albums).into_response())
}

async fn preview_albums(
    State(state): State<SmartHttpState>,
    profile: ActiveProfile,
    Json(body): Json<PreviewRequest>,
) -> Result<Json<Value>, AppError> {
    let rules_json = body.rules.to_string();
    let match_mode = body.match_mode.as_deref().unwrap_or("all");
    let sort_by = body.sort_by.as_deref().unwrap_or("title");
    let sort_order = body.sort_order.as_deref().unwrap_or("asc");

    let resolver = DbRefResolver::new(&state.backend);
    let ctx = RefCtx::root(&resolver, Some(profile.id()));
    let (where_clause, order, limit_clause) = build_album_query(
        &rules_json,
        match_mode,
        sort_by,
        sort_order,
        body.max_limit,
        &ctx,
    );
    let albums = execute_album_query(&state, &where_clause, &order, &limit_clause)?;

    Ok(Json(json!({"albums": albums, "total": albums.len()})))
}

#[cfg(test)]
mod tests {
    use super::{build_album_query, normalize_sort_order, resolve_timestamp_sql};
    use crate::smart_refs::{EmptyResolver, RefCtx};

    #[test]
    fn resolve_timestamp_relative_forms() {
        // "now-Nd" (editor form) and bare "Nd" (seeded "🆕 Récents") are both
        // N-days-ago; anything else stays a quoted literal.
        assert_eq!(
            resolve_timestamp_sql("now-30d"),
            "DATETIME('now', '-30 days')"
        );
        assert_eq!(resolve_timestamp_sql("90d"), "DATETIME('now', '-90 days')");
        assert_eq!(resolve_timestamp_sql("90"), "DATETIME('now', '-90 days')");
        assert_eq!(resolve_timestamp_sql("2024-01-01"), "'2024-01-01'");
    }

    #[test]
    fn added_at_greater_than_compiles_instead_of_matching_everything() {
        // Seeded "🆕 Récents": greater_than normalizes to ">=", which the
        // added_at branch used to drop entirely — empty WHERE — so the
        // collection counted the ENTIRE library.
        let ctx = RefCtx::root(&EmptyResolver, Some(1));
        let rules = r#"[{"field":"added_at","operator":"greater_than","value":"90d"}]"#;
        let (where_clause, _, _) = build_album_query(rules, "all", "title", "asc", None, &ctx);
        assert!(
            where_clause.contains("DATETIME('now', '-90 days')"),
            "added_at rule must compile: {where_clause}"
        );
        assert!(where_clause.contains(">="));
    }

    /// #2799 §4 — la règle `credit`/`instrument` doit chercher le MÊME libellé
    /// que celui que l'enrichissement écrit.
    ///
    /// L'enrichissement range désormais `grand piano` sous `piano` : une règle
    /// qui compilerait le libellé BRUT (`%grand piano%`) ne trouverait plus
    /// aucune ligne, alors que les crédits sont bien là. Deux normalisations
    /// des deux côtés, et la collection reste vide sans rien signaler.
    #[test]
    fn regle_credit_instrument_canonisee_comme_a_l_ecriture() {
        let ctx = RefCtx::root(&EmptyResolver, Some(1));
        let rules = r#"[{"field":"credit","operator":"has","value":{"instrument":"Grand Piano"}}]"#;
        let (where_clause, _, _) = build_album_query(rules, "all", "title", "asc", None, &ctx);
        assert!(
            where_clause.contains("LOWER(tc.instrument) LIKE LOWER('%piano%')"),
            "l'instrument doit etre canonise avant compilation : {where_clause}"
        );
        assert!(
            !where_clause.contains("grand piano"),
            "le libelle brut ne doit plus servir de motif : {where_clause}"
        );
    }

    /// TÉMOIN ANTI-RÉGRESSION : les deux autres clés de la règle `credit` sont
    /// intactes — seul `instrument` est canonisé. Un nom d'artiste passé à la
    /// moulinette des instruments serait détruit.
    #[test]
    fn temoin_regle_credit_role_et_artiste_inchanges() {
        let ctx = RefCtx::root(&EmptyResolver, Some(1));
        let rules = r#"[{"field":"credit","operator":"has","value":{"role":"producer","artist_name":"Teo Macero"}}]"#;
        let (where_clause, _, _) = build_album_query(rules, "all", "title", "asc", None, &ctx);
        assert!(
            where_clause.contains("LOWER(tc.role) LIKE LOWER('%producer%')"),
            "{where_clause}"
        );
        assert!(
            where_clause.contains("LOWER(tc.artist_name) LIKE LOWER('%Teo Macero%')"),
            "{where_clause}"
        );
        assert!(
            where_clause.contains("JOIN track_credits tc ON tc.track_id = t2.id"),
            "{where_clause}"
        );
    }

    #[test]
    fn is_not_empty_alias_compiles() {
        // tune-core spelling ("is_not_empty") used to be dropped — the seeded
        // "🖼️ Sans pochette" placeholder rule then matched the whole library.
        let ctx = RefCtx::root(&EmptyResolver, Some(1));
        let rules = r#"[{"field":"cover_path","operator":"is_empty","value":""}]"#;
        let (where_clause, _, _) = build_album_query(rules, "all", "title", "asc", None, &ctx);
        assert!(
            where_clause.contains("al.cover_path IS NULL"),
            "is_empty alias must compile: {where_clause}"
        );

        let rules = r#"[{"field":"format","operator":"is_not_empty","value":""}]"#;
        let (where_clause, _, _) = build_album_query(rules, "all", "title", "asc", None, &ctx);
        assert!(where_clause.contains("t.format IS NOT NULL"));
    }

    #[test]
    fn any_mode_joins_with_or() {
        // Raw 'any' from the seed rows (unquoted in DB) must keep OR semantics;
        // the legacy tune-core engine silently fell back to ALL.
        let ctx = RefCtx::root(&EmptyResolver, Some(1));
        let rules = r#"[{"field":"genre","operator":"contains","value":"soul"},
                        {"field":"genre","operator":"contains","value":"funk"}]"#;
        let (where_clause, _, _) = build_album_query(rules, "any", "title", "asc", None, &ctx);
        assert!(where_clause.contains(" OR "), "{where_clause}");
        assert!(!where_clause.contains(" AND "));
    }

    #[test]
    fn artist_name_field_compiles() {
        // Web-editor rules use field "artist_name" and op "=" (Coltrane); the
        // legacy engine fell back to t.title and matched nothing.
        let ctx = RefCtx::root(&EmptyResolver, Some(1));
        let rules = r#"[{"field":"artist_name","op":"=","value":"John Coltrane"}]"#;
        let (where_clause, _, _) = build_album_query(rules, "all", "random", "desc", None, &ctx);
        assert!(
            where_clause.contains("LOWER(ar.name) = LOWER('John Coltrane')"),
            "{where_clause}"
        );
    }

    #[test]
    fn favorite_rule_flows_into_album_where_clause() {
        let ctx = RefCtx::root(&EmptyResolver, Some(2));
        let (w, _o, _l) = build_album_query(
            r#"[{"field":"favorite","op":"is","value":"album"}]"#,
            "all",
            "title",
            "asc",
            None,
            &ctx,
        );
        assert!(w.contains("favorites"), "{w}");
        assert!(w.contains("profile_id = 2"), "{w}");
    }

    #[test]
    fn ref_rule_combines_with_classic_rule() {
        let ctx = RefCtx::root(&EmptyResolver, Some(1));
        let (w, _o, _l) = build_album_query(
            r#"[{"field":"genre","op":"contains","value":"Jazz"},
                {"field":"in_playlist","op":"in","value":"classic:4"}]"#,
            "all",
            "title",
            "asc",
            None,
            &ctx,
        );
        assert!(w.contains(" AND "), "{w}");
        assert!(w.contains("playlist_tracks"), "{w}");
    }

    /// #1426 (Jean Valjean, forum « F5 obligatoire ») : « Dans la Smart
    /// Collection "World Music" [il] n'a pas les bons albums, c'est un peu
    /// mélangé (Folk, Folk Métal, Folk Rock) ».
    ///
    /// Le défaut est dans le PRÉRÉGLAGE, pas dans le moteur : `contient folk`
    /// compile en `LIKE '%folk%'`, ce qui ramasse « Folk Metal » et « Folk
    /// Rock » par construction. La migration SQLite 93 (jumelle PG 045) resserre
    /// `folk` en égalité stricte et laisse `world` / `ethnic` en « contient ».
    ///
    /// On rejoue la chaîne ENTIÈRE, parce que c'est le seul niveau où le défaut
    /// est visible : migrations tune-core → règles LUES EN BASE →
    /// `build_album_query` (le compilateur qui sert réellement l'écran, et non
    /// celui de `tune-core/library/smart_collections.rs`) → SQL exécuté sur une
    /// bibliothèque témoin. Un test qui se contenterait de comparer la chaîne
    /// de règles ne dirait rien de ce que l'utilisateur voit.
    /// La requete des pochettes doit tenir avec une clause vide.
    ///
    /// `where_clause` est TANTOT vide, tantot un `WHERE …`. Y coller un `AND`
    /// pour ecarter les pochettes nulles ne produit PAS une erreur de syntaxe,
    /// et c'est bien pire : sans `WHERE`, le `AND` s'attache au `ON` du
    /// `LEFT JOIN`. La condition cesse alors de FILTRER pour devenir une
    /// condition de jointure — un album sans pochette reste dans le resultat,
    /// avec un groupe NULL qui mange une des quatre cases.
    ///
    /// ⚠️ Ma premiere version de ce test ne l'attrapait pas : l'album sans
    /// pochette s'appelait « sans » et triait donc APRES les quatre autres, si
    /// bien que `LIMIT 4` le laissait dehors de toute facon. Il s'appelle
    /// desormais « a0_… » et trie EN PREMIER : la variante fautive lui donne
    /// une case, celle-ci non.
    #[test]
    fn les_pochettes_sortent_distinctes_et_plafonnees_a_quatre() {
        use tune_core::db::backend::DbBackend;
        use tune_core::db::sqlite::SqliteDb;

        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();

        // Six albums, dont DEUX partageant la pochette « A » : elle ne doit
        // occuper qu'une seule case.
        db.execute_batch(
            "INSERT INTO artists (id, name) VALUES (1,'Un'),(2,'Deux'); \
             INSERT INTO albums (id, title, artist_id, cover_path) VALUES \
               (1,'a1',1,'A'),(2,'a2',2,'A'),(3,'b',1,'B'),(4,'c',1,'C'), \
               (5,'d',1,'D'),(6,'e',1,'E'); \
             INSERT INTO albums (id, title, artist_id, cover_path) VALUES \
               (7,'a0_sans_pochette',1,NULL);",
        )
        .unwrap();

        // La fonction de PRODUCTION, pas une copie. Ce test en portait une
        // recopie mot pour mot : elle serait restee verte pendant que la route
        // changeait de cle de groupement — c'est exactement ce qui est arrive.
        let backend: std::sync::Arc<dyn DbBackend> = std::sync::Arc::new(db);
        let requete = |where_clause: &str| super::pochettes_mosaique(&backend, where_clause);

        // Clause VIDE — une collection sans regle. C'est le cas qui casserait
        // avec un `AND` accole.
        assert_eq!(
            requete(""),
            vec!["A", "B", "C", "D"],
            "sans regle : quatre pochettes distinctes, la nulle ecartee"
        );

        // Clause NON vide — le cas courant.
        assert_eq!(
            requete("WHERE al.title <> 'a1'"),
            vec!["A", "B", "C", "D"],
            "avec regle : « A » survit par son second album, la nulle reste ecartee"
        );

        // Et le plafond mord vraiment : sans LIMIT on en aurait cinq.
        let sans_plafond = backend
            .query_many(
                "SELECT al.cover_path FROM albums al GROUP BY al.cover_path \
                 HAVING al.cover_path IS NOT NULL AND al.cover_path <> ''",
                &[],
            )
            .unwrap()
            .len();
        assert_eq!(sans_plafond, 5, "cinq pochettes distinctes en base");
    }

    /// Un meme DISQUE ne remplit pas la mosaique a lui seul.
    ///
    /// Un disque est stocke comme PLUSIEURS lignes d'`albums`, une par artiste
    /// credite, chacune avec son propre fichier de pochette en cache : autant
    /// de chemins, une seule image.
    ///
    /// Les donnees ci-dessous sont RELEVEES sur le serveur de Bertrand le
    /// 02/09/2026, collection « Classique », 139 albums — dont la mosaique
    /// montrait quatre fois la meme pochette.
    ///
    /// ⚠️ Ma premiere version groupait sur artiste + titre et laissait passer
    /// les quatre : l'artiste est justement ce qui VARIE. C'est pourquoi les
    /// quatre lignes du coffret portent ici quatre artistes differents, comme
    /// en base — avec un seul artiste, la version fautive serait passee.
    #[test]
    fn un_meme_disque_ne_prend_qu_une_case_dans_une_collection() {
        use tune_core::db::backend::DbBackend;
        use tune_core::db::sqlite::SqliteDb;

        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        db.execute_batch(
            "INSERT INTO artists (id, name) VALUES \
               (1,'Henryk Gorecki'),(2,'Dawn Upshaw'),(3,'Kronos Quartet'), \
               (4,'London Philharmonic Orchestra'),(9,'Autre'); \
             INSERT INTO albums (id, title, artist_id, cover_path) VALUES \
               (1,'A Nonesuch Retrospective',1,'C1'), \
               (2,'A Nonesuch Retrospective',2,'C2'), \
               (3,'a nonesuch retrospective',3,'C3'), \
               (4,'A Nonesuch Retrospective',4,'C4'), \
               (5,'A Nonesuch Retrospective (24bit)',2,'C5'), \
               (6,'Zeta',9,'D'),(7,'Zeta Deux',9,'E');",
        )
        .unwrap();

        let backend: std::sync::Arc<dyn DbBackend> = std::sync::Arc::new(db);
        let out = super::pochettes_mosaique(&backend, "");
        assert_eq!(
            out.len(),
            3,
            "le disque, ses quatre artistes et sa reedition 24 bits ne doivent \
             compter que pour UNE case : {out:?}"
        );
        assert!(
            out.contains(&"D".to_string()) && out.contains(&"E".to_string()),
            "les deux autres albums doivent y figurer : {out:?}"
        );
    }

    /// Le `GROUP BY` protege la FENETRE, pas le dedoublonnage.
    ///
    /// ⚠️ Constat qui a coute une reecriture : remettre la cle fautive
    /// (artiste + titre) dans la requete laissait les deux tests ci-dessus au
    /// VERT. `cle_pochette`, cote Rust, rattrapait tout — les gardes ne
    /// gardaient donc rien de ce qu'ils annoncaient.
    ///
    /// Ce que le groupement SQL tient vraiment : les seize lignes remontees.
    /// Groupees par titre, un disque n'en occupe qu'UNE et les albums suivants
    /// atteignent la mosaique. Groupees par artiste + titre, un disque assez
    /// credite remplit la fenetre a lui seul, et le Rust n'a plus rien a
    /// dedoublonner — il ne voit jamais les autres albums.
    ///
    /// Vingt artistes ici, la ou le plus gros cas mesure chez Bertrand en
    /// compte quatorze (« I Give It A Year », 02/09/2026). La marge est mince :
    /// six credits de plus sur un disque et la mosaique tombait a une case.
    #[test]
    fn un_disque_tres_credite_ne_mange_pas_la_fenetre() {
        use tune_core::db::backend::DbBackend;
        use tune_core::db::sqlite::SqliteDb;

        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        let mut sql = String::from("INSERT INTO artists (id, name) VALUES (99,'Divers')");
        for i in 1..=20 {
            sql.push_str(&format!(",({i},'Credite {i}')"));
        }
        sql.push_str("; INSERT INTO albums (id, title, artist_id, cover_path) VALUES ");
        // Titre en « A… » : ces vingt lignes trient AVANT les autres.
        for i in 1..=20 {
            sql.push_str(&format!("({i},'A Un Seul Disque',{i},'P{i}'),"));
        }
        sql.push_str("(30,'Zeta',99,'D'),(31,'Zeta Deux',99,'E');");
        db.execute_batch(&sql).unwrap();

        let backend: std::sync::Arc<dyn DbBackend> = std::sync::Arc::new(db);
        let out = super::pochettes_mosaique(&backend, "");
        assert!(
            out.contains(&"D".to_string()) && out.contains(&"E".to_string()),
            "un disque a vingt credits a mange les seize lignes de la fenetre : \
             les albums suivants n'atteignent plus la mosaique. Obtenu {out:?}"
        );
    }

    /// Treize pianistes, un seul disque — le cas le plus gros mesure.
    ///
    /// « Les indispensables du piano (96kHz/24bit) » existe en treize lignes
    /// d'album sur le serveur de Bertrand. Sans les albums voisins, la mosaique
    /// de « Classique » n'aurait qu'une seule case remplie treize fois.
    #[test]
    fn treize_lignes_d_un_meme_disque_laissent_la_place_aux_autres() {
        use tune_core::db::backend::DbBackend;
        use tune_core::db::sqlite::SqliteDb;

        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        let mut sql = String::from("INSERT INTO artists (id, name) VALUES (99,'Divers')");
        for i in 1..=13 {
            sql.push_str(&format!(",({i},'Pianiste {i}')"));
        }
        sql.push_str("; INSERT INTO albums (id, title, artist_id, cover_path) VALUES ");
        // Les treize lignes trient AVANT les autres : sans regroupement elles
        // prennent les quatre cases. C'est le piege qui avait rendu vert un
        // premier test — le doublon triait apres le plafond.
        for i in 1..=13 {
            sql.push_str(&format!(
                "({i},'Les indispensables du piano (96kHz/24bit)',{i},'P{i}'),"
            ));
        }
        sql.push_str("(20,'Zeta',99,'D'),(21,'Zeta Deux',99,'E');");
        db.execute_batch(&sql).unwrap();

        let backend: std::sync::Arc<dyn DbBackend> = std::sync::Arc::new(db);
        let out = super::pochettes_mosaique(&backend, "");
        assert_eq!(
            out.len(),
            3,
            "treize lignes d'un meme disque ne valent qu'une case : {out:?}"
        );
        assert!(
            out.contains(&"D".to_string()) && out.contains(&"E".to_string()),
            "les albums qui suivent doivent atteindre la mosaique : {out:?}"
        );
    }

    #[test]
    fn le_prereglage_world_music_ne_ramasse_plus_folk_metal_ni_folk_rock() {
        use tune_core::db::backend::DbBackend;
        use tune_core::db::sqlite::SqliteDb;

        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        tune_core::db::migrations::run_migrations(&db).unwrap();

        // Un album par genre, une piste chacun — le genre vit sur la PISTE
        // (`t.genre`), c'est la colonne que compile la règle.
        db.execute_batch(
            "INSERT INTO albums (id, title) VALUES \
               (1,'Kanyaleng'),(2,'Ethnic Jazz Session'),(3,'Chants de Bretagne'), \
               (4,'Tuonela'),(5,'Sweetheart of the Rodeo'),(6,'Kind of Blue'); \
             INSERT INTO tracks (album_id, title, genre, file_path) VALUES \
               (1,'a','World','/m/1.flac'), \
               (2,'b','Ethnic Jazz','/m/2.flac'), \
               (3,'c','Folk','/m/3.flac'), \
               (4,'d','Folk Metal','/m/4.flac'), \
               (5,'e','Folk Rock','/m/5.flac'), \
               (6,'f','Jazz','/m/6.flac');",
        )
        .unwrap();

        // Les règles telles qu'elles SONT EN BASE après migrations — pas une
        // copie recollée ici, sinon le test ne garde plus le préréglage livré.
        let rows = db
            .query_many(
                "SELECT rules, match_mode FROM smart_collections WHERE name LIKE '%World%'",
                &[],
            )
            .unwrap();
        assert_eq!(
            rows.len(),
            1,
            "le préréglage « World Music » doit exister en un seul exemplaire"
        );
        let regles = rows[0][0].as_string().unwrap_or_default();
        let mode = rows[0][1].as_string().unwrap_or_default();

        let ctx = RefCtx::root(&EmptyResolver, Some(1));
        let (where_clause, order, _) =
            build_album_query(&regles, &mode, "title", "asc", None, &ctx);
        let sql = format!(
            "SELECT al.title FROM albums al \
             LEFT JOIN artists ar ON al.artist_id = ar.id \
             LEFT JOIN tracks t ON t.album_id = al.id \
             {where_clause} GROUP BY al.id, al.title {order}"
        );
        let titres: Vec<String> = db
            .query_many(&sql, &[])
            .unwrap()
            .iter()
            .map(|r| r[0].as_string().unwrap_or_default())
            .collect();

        for attendu in ["Kanyaleng", "Ethnic Jazz Session", "Chants de Bretagne"] {
            assert!(
                titres.iter().any(|t| t == attendu),
                "« {attendu} » doit rester dans World Music : {titres:?}\n{sql}"
            );
        }
        for indesirable in ["Tuonela", "Sweetheart of the Rodeo", "Kind of Blue"] {
            assert!(
                !titres.iter().any(|t| t == indesirable),
                "« {indesirable} » n'a rien à faire dans World Music (#1426) : \
                 {titres:?}\n{sql}"
            );
        }
    }

    #[test]
    fn normalize_sort_order_tolerates_encodings() {
        // Raw form (this route's save path).
        assert_eq!(normalize_sort_order(Some("asc".into())), "asc");
        assert_eq!(normalize_sort_order(Some("desc".into())), "desc");
        // Legacy JSON-encoded form (tune-core save path) — the bug source.
        assert_eq!(normalize_sort_order(Some("\"asc\"".into())), "asc");
        assert_eq!(normalize_sort_order(Some("\"desc\"".into())), "desc");
        // Missing / empty -> default.
        assert_eq!(normalize_sort_order(None), "asc");
        assert_eq!(normalize_sort_order(Some(String::new())), "asc");
    }
}
