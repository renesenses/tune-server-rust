use axum::Json;
use axum::extract::{Query, State};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::backend::SqlValue;
use tune_core::db::engine::Engine;

use crate::error::AppError;
use crate::state::AppState;

#[derive(Deserialize)]
pub(super) struct FacetQuery {
    /// Comma-separated facet fields to compute (default: the common set).
    fields: Option<String>,
    /// Max values per facet (default 200). Également réutilisé par
    /// `albums-detailed` comme taille de page (même jeu de paramètres).
    pub(super) limit: Option<i64>,
    /// Pagination de `albums-detailed` uniquement — sans effet sur les facettes.
    pub(super) offset: Option<i64>,
    /// Oxygen folder facet: absolute directory prefix. Not a facet field itself,
    /// but an active selection that narrows every other facet's counts (the
    /// folder-facet endpoint owns the drill-down; here it only filters).
    pub(super) folder: Option<String>,
    // Optional active filters — when present, each facet is counted over the
    // narrowed track set (cumulative faceting, Dominique: "le genre filtre les
    // labels"). Same names as /library/tracks. A facet never filters on its own
    // field, so its other values stay visible to switch to.
    genre: Option<String>,
    year: Option<i32>,
    format: Option<String>,
    sample_rate: Option<i32>,
    bit_depth: Option<i32>,
    source: Option<String>,
    label: Option<String>,
    composer: Option<String>,
    artist: Option<String>,
    country: Option<String>,
    mood: Option<String>,
    source_media: Option<String>,
    /// Album rating (1-5, profile 1) — tracks inherit their album's rating.
    pub(super) rating: Option<i32>,
    /// Manual collection name — tracks whose album is in that collection. The
    /// album membership lives as a JSON `album_ids` array in the `collections`
    /// setting, resolved to ids by the handler (not a joinable table).
    pub(super) collection: Option<String>,
    /// Favoris (profil 1) : `track` = la piste elle-même est en favori,
    /// `album` = son album l'est. Deux valeurs distinctes plutôt qu'un booléen
    /// unique : aimer un album et aimer un morceau ne se rangent pas pareil.
    pub(super) favorite: Option<String>,
    /// Nom d'une liste de lecture — les pistes qui en font partie.
    pub(super) playlist: Option<String>,
    /// Étiquette manquante : `genre`, `year`, `artist`, `album` ou `cover`.
    /// Sert au nettoyage de bibliothèque, pas à l'écoute.
    pub(super) untagged: Option<String>,
    /// Année d'ENREGISTREMENT (`albums.original_year`), distincte de `year` qui
    /// est celle de l'édition. L'écart n'est pas anecdotique : un Abbey Lincoln
    /// enregistré en 1959 et réédité en 1987 se range sous 1959 pour qui écoute
    /// du jazz, sous 1987 pour qui range des disques.
    original_year: Option<i32>,
    q: Option<String>,
}

/// Build the WHERE conditions (over alias `t` = tracks) for the active filters,
/// skipping the facet's own field so its alternatives remain countable. Values
/// are always bound parameters; column/key names come from fixed literals only.
pub(super) fn build_conditions(
    q: &FacetQuery,
    engine: Engine,
    exclude: &str,
    // Resolved album ids for the active `collection` selection (from settings
    // JSON — the handler resolves the name so this stays a pure SQL builder).
    collection_ids: Option<&[i64]>,
) -> (Vec<String>, Vec<SqlValue>) {
    let mut conds: Vec<String> = Vec::new();
    let mut params: Vec<SqlValue> = Vec::new();
    let mut idx = 1usize;
    let ph = |i: usize| match engine {
        Engine::Sqlite => "?".to_string(),
        Engine::Postgres => format!("${i}"),
    };

    if exclude != "genre" {
        if let Some(g) = q.genre.as_deref().filter(|s| !s.is_empty()) {
            conds.push(format!(
                "(LOWER(t.genre) = LOWER({p}) OR t.genres LIKE {p2})",
                p = ph(idx),
                p2 = ph(idx + 1)
            ));
            params.push(SqlValue::Text(g.to_string()));
            params.push(SqlValue::Text(format!("%\"{g}\"%")));
            idx += 2;
        }
    }
    if exclude != "year" {
        if let Some(y) = q.year {
            conds.push(format!("t.year = {}", ph(idx)));
            params.push(SqlValue::Int(y as i64));
            idx += 1;
        }
    }
    if exclude != "format" {
        if let Some(f) = q.format.as_deref().filter(|s| !s.is_empty()) {
            conds.push(format!("LOWER(t.format) = LOWER({})", ph(idx)));
            params.push(SqlValue::Text(f.to_string()));
            idx += 1;
        }
    }
    if exclude != "sample_rate" {
        if let Some(sr) = q.sample_rate {
            conds.push(format!("t.sample_rate = {}", ph(idx)));
            params.push(SqlValue::Int(sr as i64));
            idx += 1;
        }
    }
    if exclude != "bit_depth" {
        if let Some(bd) = q.bit_depth {
            conds.push(format!("t.bit_depth = {}", ph(idx)));
            params.push(SqlValue::Int(bd as i64));
            idx += 1;
        }
    }
    if let Some(s) = q.source.as_deref().filter(|s| !s.is_empty()) {
        conds.push(format!("t.source = {}", ph(idx)));
        params.push(SqlValue::Text(s.to_string()));
        idx += 1;
    }
    if exclude != "label" {
        if let Some(l) = q.label.as_deref().filter(|s| !s.is_empty()) {
            conds.push(format!("LOWER(t.label) LIKE LOWER({})", ph(idx)));
            params.push(SqlValue::Text(format!("%{l}%")));
            idx += 1;
        }
    }
    // `composer` est désormais une facette à part entière : comme les autres,
    // elle ne doit pas se filtrer elle-même, sinon sélectionner « Bach » ne
    // laisserait plus que « Bach » dans la liste des compositeurs.
    if exclude != "composer" {
        if let Some(c) = q.composer.as_deref().filter(|s| !s.is_empty()) {
            conds.push(format!("LOWER(t.composer) LIKE LOWER({})", ph(idx)));
            params.push(SqlValue::Text(format!("%{c}%")));
            idx += 1;
        }
    }
    if exclude != "artist" {
        if let Some(a) = q.artist.as_deref().filter(|s| !s.is_empty()) {
            // `tracks` has no artist_name column (artist is a FK to `artists`),
            // and these conditions run against `FROM tracks t` with no join, so
            // resolve the name via a subquery rather than the phantom
            // t.artist_name (forum #1189).
            conds.push(format!(
                "t.artist_id IN (SELECT id FROM artists WHERE name = {})",
                ph(idx)
            ));
            params.push(SqlValue::Text(a.to_string()));
            idx += 1;
        }
    }
    // Extended-tag filters via the open `track_metadata` k/v store. `source`
    // facet == source_media key. Key is a fixed literal; value is bound.
    for (opt, key, own) in [
        (&q.country, "release_country", "country"),
        (&q.mood, "mood", "mood"),
        (&q.source_media, "source_media", "source"),
    ] {
        if exclude == own {
            continue;
        }
        if let Some(v) = opt.as_deref().filter(|s| !s.is_empty()) {
            conds.push(format!(
                "EXISTS (SELECT 1 FROM track_metadata tm \
                 WHERE tm.track_id = t.id AND tm.key = '{key}' AND tm.value = {})",
                ph(idx)
            ));
            params.push(SqlValue::Text(v.to_string()));
            idx += 1;
        }
    }
    // Folder selection (Oxygen drill-down) narrows every facet's counts. The
    // folder-facet endpoint scopes by path prefix on its own, so it passes
    // exclude="folder" to skip this redundant predicate; the flat /facets
    // endpoint (exclude = a real field name) always applies it.
    if exclude != "folder" {
        if let Some(fld) = q.folder.as_deref().filter(|s| !s.is_empty()) {
            conds.push(format!(
                "t.file_path LIKE {}{}",
                ph(idx),
                tune_core::db::track_repo::like_escape_clause(engine)
            ));
            params.push(SqlValue::Text(
                tune_core::db::track_repo::folder_like_pattern(fld),
            ));
            idx += 1;
        }
    }
    // Album rating (profile 1). Tracks inherit their album's rating via a join
    // to `album_ratings`; EXISTS keeps it self-contained on alias `t`.
    if exclude != "rating" {
        if let Some(r) = q.rating {
            conds.push(format!(
                "EXISTS (SELECT 1 FROM album_ratings arr \
                 WHERE arr.album_id = t.album_id AND arr.profile_id = 1 AND arr.rating = {})",
                ph(idx)
            ));
            params.push(SqlValue::Int(r as i64));
            idx += 1;
        }
    }
    // Manual collection: the resolved album ids are our own i64s (parsed from the
    // settings JSON), so inlining them in the IN list is injection-safe. An empty
    // set matches nothing (an empty collection has zero tracks).
    if exclude != "collection" {
        if let Some(ids) = collection_ids {
            if ids.is_empty() {
                conds.push("1 = 0".to_string());
            } else {
                let list = ids
                    .iter()
                    .map(|id| id.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                conds.push(format!("t.album_id IN ({list})"));
            }
        }
    }
    // Favoris du profil 1. `EXISTS` reste sur l'alias `t`, comme la note.
    // ⚠️ Ces trois prédicats sont dupliqués dans `TrackRepo::list_filtered`
    // (crates distincts) : toute modification ici doit y être reportée, sans
    // quoi une facette compterait autrement que la liste qu'elle filtre.
    // L'année d'enregistrement vit sur l'ALBUM, pas sur la piste : jointure par
    // EXISTS pour rester sur l'alias `t`, comme la note et les favoris.
    if exclude != "original_year" {
        if let Some(y) = q.original_year {
            conds.push(format!(
                "EXISTS (SELECT 1 FROM albums alo WHERE alo.id = t.album_id AND alo.original_year = {})",
                ph(idx)
            ));
            params.push(SqlValue::Int(y as i64));
            idx += 1;
        }
    }
    if exclude != "favorite" {
        if let Some(kind) = q.favorite.as_deref().filter(|s| !s.is_empty()) {
            match kind {
                "album" => conds.push(
                    "EXISTS (SELECT 1 FROM favorites f WHERE f.profile_id = 1 \
                     AND f.item_type = 'album' AND f.item_id = t.album_id)"
                        .to_string(),
                ),
                "track" => conds.push(
                    "EXISTS (SELECT 1 FROM favorites f WHERE f.profile_id = 1 \
                     AND f.item_type = 'track' AND f.item_id = t.id)"
                        .to_string(),
                ),
                // Valeur inconnue : ne rien filtrer plutôt que tout exclure.
                _ => {}
            }
        }
    }
    if exclude != "playlist" {
        if let Some(name) = q.playlist.as_deref().filter(|s| !s.is_empty()) {
            conds.push(format!(
                "EXISTS (SELECT 1 FROM playlist_tracks pt JOIN playlists pl ON pl.id = pt.playlist_id \
                 WHERE pt.track_id = t.id AND LOWER(pl.name) = LOWER({}))",
                ph(idx)
            ));
            params.push(SqlValue::Text(name.to_string()));
            idx += 1;
        }
    }
    if exclude != "untagged" {
        if let Some(field) = q.untagged.as_deref().filter(|s| !s.is_empty()) {
            // Champ choisi dans une liste fermée : le SQL formaté ci-dessous ne
            // dépend jamais de l'entrée brute.
            if let Some(cond) = untagged_condition(field) {
                conds.push(cond);
            }
        }
    }
    if let Some(query) = q.q.as_deref().filter(|s| !s.is_empty()) {
        // Artist match via subquery — no artist_name column / no join here (#1189).
        conds.push(format!(
            "(LOWER(t.title) LIKE LOWER({p}) OR t.artist_id IN \
             (SELECT id FROM artists WHERE LOWER(name) LIKE LOWER({p2})))",
            p = ph(idx),
            p2 = ph(idx + 1)
        ));
        let like = format!("%{query}%");
        params.push(SqlValue::Text(like.clone()));
        params.push(SqlValue::Text(like));
    }
    (conds, params)
}

/// Resolve a manual collection's name to its album ids, read from the JSON
/// `collections` setting (each entry is `{ name, album_ids: [..] }`). Returns an
/// empty vec if the setting is absent or the name isn't found. Case-insensitive
/// on the name (the facet value is the stored name, so an exact match normally).
pub(super) fn collection_album_ids(state: &AppState, name: &str) -> Vec<i64> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let Some(raw) = settings.get("collections").ok().flatten() else {
        return Vec::new();
    };
    let Ok(cols) = serde_json::from_str::<Vec<Value>>(&raw) else {
        return Vec::new();
    };
    cols.iter()
        .find(|c| {
            c.get("name")
                .and_then(|v| v.as_str())
                .is_some_and(|n| n.eq_ignore_ascii_case(name))
        })
        .and_then(|c| c.get("album_ids").and_then(|v| v.as_array()))
        .map(|arr| arr.iter().filter_map(|v| v.as_i64()).collect())
        .unwrap_or_default()
}

/// GET /api/v1/library/facets?fields=genre,label,year,artist,country,mood,source
///
/// Returns `{ "<field>": [{ "value": string, "count": number }] }` for each
/// requested facet — full-library counts, unlike the client's loaded-window
/// aggregation. `country`/`mood`/`source` are read from the open `track_metadata`
/// key/value store (release_country / mood / source_media), which the client
/// cannot aggregate without a per-track fetch.
pub(super) async fn library_facets(
    Query(q): Query<FacetQuery>,
    State(state): State<AppState>,
) -> Result<Json<Value>, AppError> {
    // `limit <= 0` means "no limit" (show every facet value); otherwise clamp to
    // a sane ceiling. Absent → the historical default of 200.
    let limit: Option<i64> = match q.limit {
        Some(n) if n <= 0 => None,
        Some(n) => Some(n.clamp(1, 5000)),
        None => Some(200),
    };
    let requested: Vec<String> = q
        .fields
        .as_deref()
        .unwrap_or("genre,label,year,artist,country,mood,source")
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let engine = state.backend.engine();
    // Resolve the active collection selection once (name → album ids) for the
    // cumulative narrowing of every facet.
    let coll_ids: Option<Vec<i64>> = q
        .collection
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|name| collection_album_ids(&state, name));
    let mut out = serde_json::Map::new();
    for field in requested {
        // Conditions narrow the count by the OTHER active facets (cumulative).
        let (conds, params) = build_conditions(&q, engine, &field, coll_ids.as_deref());
        // The column / key is chosen from this fixed allow-list only, so the
        // formatted SQL below is never influenced by request input.
        let rows: Vec<(String, i64)> = match field.as_str() {
            "genre" => column_facet(&state, "genre", limit, &conds, &params),
            "label" => column_facet(&state, "label", limit, &conds, &params),
            // Le classique se navigue par compositeur avant de se naviguer par
            // artiste : colonne `tracks` directe, donc même facette de colonne.
            "composer" => column_facet(&state, "composer", limit, &conds, &params),
            "year" => column_facet(&state, "year", limit, &conds, &params),
            "artist" => artist_facet(&state, limit, &conds, &params),
            // Technical dimensions an audiophile browses by (Bertrand): direct
            // `tracks` columns, so a plain column facet — like genre/year.
            "format" => column_facet(&state, "format", limit, &conds, &params),
            "sample_rate" => column_facet(&state, "sample_rate", limit, &conds, &params),
            "bit_depth" => column_facet(&state, "bit_depth", limit, &conds, &params),
            "country" => kv_facet(&state, "release_country", limit, &conds, &params),
            "mood" => kv_facet(&state, "mood", limit, &conds, &params),
            "source" => kv_facet(&state, "source_media", limit, &conds, &params),
            "rating" => rating_facet(&state, limit, &conds, &params),
            "collection" => collection_facet(&state, &q, engine),
            "original_year" => original_year_facet(&state, limit, &conds, &params),
            "favorite" => favorite_facet(&state, &conds, &params),
            "playlist" => playlist_facet(&state, limit, &conds, &params),
            "untagged" => untagged_facet(&state, &conds, &params),
            _ => continue,
        };
        let arr: Vec<Value> = rows
            .into_iter()
            .map(|(value, count)| json!({ "value": value, "count": count }))
            .collect();
        out.insert(field, Value::Array(arr));
    }
    Ok(Json(Value::Object(out)))
}

/// Prédicat SQL d'une étiquette manquante. Liste FERMÉE : toute autre valeur
/// rend `None` et ne filtre rien, plutôt que d'injecter quoi que ce soit.
///
/// « Manquant » vaut ici NULL **ou** chaîne vide : un tag effacé par un éditeur
/// laisse souvent une chaîne vide, et l'utilisateur qui range sa bibliothèque
/// ne fait pas la différence entre les deux.
pub(super) fn untagged_condition(field: &str) -> Option<String> {
    let c = match field {
        "genre" => "(t.genre IS NULL OR t.genre = '')",
        "year" => "(t.year IS NULL OR t.year = 0)",
        "artist" => "t.artist_id IS NULL",
        "album" => "t.album_id IS NULL",
        // La pochette vit sur l'album : une piste sans album n'en a pas non plus.
        "cover" => {
            "(t.album_id IS NULL OR EXISTS (SELECT 1 FROM albums al \
              WHERE al.id = t.album_id AND (al.cover_path IS NULL OR al.cover_path = '')))"
        }
        _ => return None,
    };
    Some(c.to_string())
}

/// Les cinq étiquettes surveillées, dans l'ordre où elles gênent l'écoute :
/// sans artiste ni album, une piste est introuvable ; sans genre ni année, elle
/// échappe au tri ; sans pochette, elle est seulement laide.
const UNTAGGED_FIELDS: [&str; 5] = ["artist", "album", "genre", "year", "cover"];

/// Années d'enregistrement présentes, la plus récente d'abord — comme `year`,
/// dont le tri chronologique descendant avait été demandé (« pas facile de
/// trouver 2026 ! »).
fn original_year_facet(
    state: &AppState,
    limit: Option<i64>,
    conds: &[String],
    params: &[SqlValue],
) -> Vec<(String, i64)> {
    let where_clause = if conds.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", conds.join(" AND "))
    };
    let limit_clause = limit.map(|n| format!(" LIMIT {n}")).unwrap_or_default();
    // `> 0` écarte les années nulles ET les `0` que certains taggeurs écrivent
    // quand le champ est vide : « 0 » en tête de liste ne désigne aucun disque.
    let sql = format!(
        "SELECT alo.original_year, COUNT(*) AS n FROM tracks t \
         JOIN albums alo ON alo.id = t.album_id{where_clause} \
         {and_or_where} alo.original_year IS NOT NULL AND alo.original_year > 0 \
         GROUP BY alo.original_year ORDER BY alo.original_year DESC{limit_clause}",
        and_or_where = if conds.is_empty() { "WHERE" } else { "AND" }
    );
    let bound: Vec<&dyn tune_core::db::backend::ToSqlValue> = params
        .iter()
        .map(|v| v as &dyn tune_core::db::backend::ToSqlValue)
        .collect();
    state
        .backend
        .query_many(&sql, &bound)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|row| {
            let mut it = row.into_iter();
            let y = it.next()?.as_i64()?;
            let count = it.next()?.as_i64().unwrap_or(0);
            Some((y.to_string(), count))
        })
        .collect()
}

/// Favoris du profil 1, comptés par type. Deux valeurs au plus (`track`,
/// `album`) : pas de LIMIT à appliquer.
fn favorite_facet(state: &AppState, conds: &[String], params: &[SqlValue]) -> Vec<(String, i64)> {
    let bound: Vec<&dyn tune_core::db::backend::ToSqlValue> = params
        .iter()
        .map(|v| v as &dyn tune_core::db::backend::ToSqlValue)
        .collect();
    let mut out = Vec::new();
    for kind in ["track", "album"] {
        let own = match kind {
            "album" => {
                "EXISTS (SELECT 1 FROM favorites f WHERE f.profile_id = 1 \
                        AND f.item_type = 'album' AND f.item_id = t.album_id)"
            }
            _ => {
                "EXISTS (SELECT 1 FROM favorites f WHERE f.profile_id = 1 \
                  AND f.item_type = 'track' AND f.item_id = t.id)"
            }
        };
        let mut all: Vec<String> = conds.to_vec();
        all.push(own.to_string());
        let sql = format!("SELECT COUNT(*) FROM tracks t WHERE {}", all.join(" AND "));
        let n = state
            .backend
            .query_one(&sql, &bound)
            .ok()
            .flatten()
            .and_then(|row| row.into_iter().next())
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        // Une facette vide ne s'affiche pas : inutile d'offrir un filtre qui
        // ne rend rien.
        if n > 0 {
            out.push((kind.to_string(), n));
        }
    }
    out
}

/// Listes de lecture contenant au moins une piste du jeu courant.
fn playlist_facet(
    state: &AppState,
    limit: Option<i64>,
    conds: &[String],
    params: &[SqlValue],
) -> Vec<(String, i64)> {
    let where_clause = if conds.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", conds.join(" AND "))
    };
    let limit_clause = limit.map(|n| format!(" LIMIT {n}")).unwrap_or_default();
    let sql = format!(
        "SELECT pl.name, COUNT(*) AS n FROM tracks t \
         JOIN playlist_tracks pt ON pt.track_id = t.id \
         JOIN playlists pl ON pl.id = pt.playlist_id{where_clause} \
         GROUP BY pl.name ORDER BY n DESC{limit_clause}"
    );
    let bound: Vec<&dyn tune_core::db::backend::ToSqlValue> = params
        .iter()
        .map(|v| v as &dyn tune_core::db::backend::ToSqlValue)
        .collect();
    state
        .backend
        .query_many(&sql, &bound)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|row| {
            let mut it = row.into_iter();
            let name = it.next()?.as_string()?;
            let count = it.next()?.as_i64().unwrap_or(0);
            (!name.is_empty()).then_some((name, count))
        })
        .collect()
}

/// Combien de pistes il manque quoi. Un compte par étiquette surveillée.
fn untagged_facet(state: &AppState, conds: &[String], params: &[SqlValue]) -> Vec<(String, i64)> {
    let bound: Vec<&dyn tune_core::db::backend::ToSqlValue> = params
        .iter()
        .map(|v| v as &dyn tune_core::db::backend::ToSqlValue)
        .collect();
    let mut out = Vec::new();
    for field in UNTAGGED_FIELDS {
        let Some(missing) = untagged_condition(field) else {
            continue;
        };
        let mut all: Vec<String> = conds.to_vec();
        all.push(missing);
        let sql = format!("SELECT COUNT(*) FROM tracks t WHERE {}", all.join(" AND "));
        let n = state
            .backend
            .query_one(&sql, &bound)
            .ok()
            .flatten()
            .and_then(|row| row.into_iter().next())
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        if n > 0 {
            out.push((field.to_string(), n));
        }
    }
    out
}

/// Count distinct values of a fixed `tracks` column, optionally narrowed by the
/// active-facet conditions (over alias `t`).
fn column_facet(
    state: &AppState,
    col: &str,
    limit: Option<i64>,
    conds: &[String],
    params: &[SqlValue],
) -> Vec<(String, i64)> {
    let extra = if conds.is_empty() {
        String::new()
    } else {
        format!(" AND {}", conds.join(" AND "))
    };
    let limit_clause = limit.map(|n| format!(" LIMIT {n}")).unwrap_or_default();
    let sql = format!(
        "SELECT t.{col}, COUNT(*) AS n FROM tracks t \
         WHERE t.{col} IS NOT NULL AND CAST(t.{col} AS TEXT) <> ''{extra} \
         GROUP BY t.{col} ORDER BY n DESC{limit_clause}"
    );
    let bound: Vec<&dyn tune_core::db::backend::ToSqlValue> = params
        .iter()
        .map(|v| v as &dyn tune_core::db::backend::ToSqlValue)
        .collect();
    state
        .backend
        .query_many(&sql, &bound)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|row| {
            let mut it = row.into_iter();
            let v = it.next()?;
            let c = it.next()?;
            let value = v
                .as_string()
                .or_else(|| v.as_i64().map(|n| n.to_string()))?;
            Some((value, c.as_i64().unwrap_or(0)))
        })
        .collect()
}

/// Count tracks by their album's rating (profile 1). Tracks inherit the rating
/// via a join to `album_ratings`; unrated tracks are naturally excluded. The
/// `conds` reference alias `t`, so the join is additive.
fn rating_facet(
    state: &AppState,
    limit: Option<i64>,
    conds: &[String],
    params: &[SqlValue],
) -> Vec<(String, i64)> {
    let where_clause = if conds.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", conds.join(" AND "))
    };
    let limit_clause = limit.map(|n| format!(" LIMIT {n}")).unwrap_or_default();
    let sql = format!(
        "SELECT arr.rating, COUNT(*) AS n FROM tracks t \
         JOIN album_ratings arr ON t.album_id = arr.album_id AND arr.profile_id = 1{where_clause} \
         GROUP BY arr.rating ORDER BY arr.rating DESC{limit_clause}"
    );
    let bound: Vec<&dyn tune_core::db::backend::ToSqlValue> = params
        .iter()
        .map(|v| v as &dyn tune_core::db::backend::ToSqlValue)
        .collect();
    state
        .backend
        .query_many(&sql, &bound)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|row| {
            let mut it = row.into_iter();
            let rating = it.next()?.as_i64()?;
            let count = it.next()?.as_i64().unwrap_or(0);
            Some((rating.to_string(), count))
        })
        .collect()
}

/// Count tracks per manual collection. Collections aren't a SQL table — they
/// live as a JSON `album_ids` array in the `collections` setting — so this
/// resolves each collection's album ids and counts tracks in that set, narrowed
/// by the OTHER active facets (collection self-excluded, cumulative). Empty
/// collections are omitted (they'd read as 0, like other facets skip empties).
fn collection_facet(state: &AppState, q: &FacetQuery, engine: Engine) -> Vec<(String, i64)> {
    let (conds, params) = build_conditions(q, engine, "collection", None);
    let extra = if conds.is_empty() {
        String::new()
    } else {
        format!(" AND {}", conds.join(" AND "))
    };
    let bound: Vec<&dyn tune_core::db::backend::ToSqlValue> = params
        .iter()
        .map(|v| v as &dyn tune_core::db::backend::ToSqlValue)
        .collect();
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let raw = settings
        .get("collections")
        .ok()
        .flatten()
        .unwrap_or_default();
    let cols: Vec<Value> = serde_json::from_str(&raw).unwrap_or_default();
    let mut out: Vec<(String, i64)> = Vec::new();
    for c in &cols {
        let name = c
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if name.is_empty() {
            continue;
        }
        let ids: Vec<i64> = c
            .get("album_ids")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_i64()).collect())
            .unwrap_or_default();
        if ids.is_empty() {
            continue;
        }
        let id_list = ids
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!("SELECT COUNT(*) FROM tracks t WHERE t.album_id IN ({id_list}){extra}");
        let count = state
            .backend
            .query_one(&sql, &bound)
            .ok()
            .flatten()
            .and_then(|r| r.first().and_then(|v| v.as_i64()))
            .unwrap_or(0);
        if count > 0 {
            out.push((name, count));
        }
    }

    // Smart Collections (rule-based) sit in the same facet as the manual ones.
    // Counted with the SAME rule engine as the smart-collections endpoints
    // (`build_album_query`), so the facet always agrees with the collection
    // views. The legacy tune-core `compile_sql` engine used here before
    // diverged — raw `any` match_mode silently read as ALL, `added_at` hit the
    // phantom `t.created_at` column, unknown fields (`artist_name`) fell back
    // to `t.title` — and every such collection vanished from the facet (count
    // read 0) while placeholder rules counted the entire library. Collections
    // with genuinely 0 matching tracks stay omitted, like every other facet.
    // A name already used by a manual collection wins — skip the smart duplicate
    // so the facet value stays unambiguous when it is used to filter tracks.
    let manual_names: std::collections::HashSet<String> =
        out.iter().map(|(n, _)| n.to_lowercase()).collect();
    let rows = state
        .backend
        .query_many(
            "SELECT name, rules, match_mode FROM smart_collections ORDER BY name",
            &[],
        )
        .unwrap_or_default();
    for row in &rows {
        let name = row.first().and_then(|v| v.as_string()).unwrap_or_default();
        if name.is_empty() || manual_names.contains(&name.to_lowercase()) {
            continue;
        }
        let rules = row
            .get(1)
            .and_then(|v| v.as_string())
            .unwrap_or_else(|| "[]".into());
        let match_mode = row
            .get(2)
            .and_then(|v| v.as_string())
            .unwrap_or_else(|| "all".into());
        // Résolveur branché sur la base + profil par défaut (1), cohérent avec
        // la convention « profil 1 » du reste des facettes ; laisse les
        // nouveaux critères référence/favori compter correctement.
        let resolver = crate::routes::smart_refs::DbRefResolver::new(&state.backend);
        let ctx = crate::routes::smart_refs::RefCtx::root(
            &resolver,
            Some(crate::routes::active_profile::DEFAULT_PROFILE_ID),
        );
        let where_clause = smart_collection_where(&rules, &match_mode, &conds, &ctx);
        // COUNT(DISTINCT t.id): track-level rules count matching tracks;
        // album-level rules (added_at, play_count…) count every track of the
        // matching albums. Same figure as the list endpoint's `track_count`
        // (max_limit is a display cap on the album view, not a membership cap).
        let sql = format!(
            "SELECT COUNT(DISTINCT t.id) FROM albums al \
             LEFT JOIN artists ar ON al.artist_id = ar.id \
             LEFT JOIN tracks t ON t.album_id = al.id {where_clause}"
        );
        let count = state
            .backend
            .query_one(&sql, &bound)
            .ok()
            .flatten()
            .and_then(|r| r.first().and_then(|v| v.as_i64()))
            .unwrap_or(0);
        if count > 0 {
            out.push((name, count));
        }
    }

    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out
}

/// Compose the smart-collection rule engine's WHERE (aliases `al`/`ar`/`t`,
/// see `build_album_query`) with extra `t.`-aliased facet conditions. The rules
/// part is parenthesized so an `any` (OR-joined) collection isn't rebound by
/// the appended ANDs. An empty rules WHERE means "matches everything" — that is
/// the engine's semantic for rules it cannot compile.
fn smart_collection_where(
    rules_json: &str,
    match_mode: &str,
    extra_conds: &[String],
    ctx: &crate::routes::smart_refs::RefCtx,
) -> String {
    let (wc, _, _) = crate::routes::smart_collections::build_album_query(
        rules_json, match_mode, "title", "asc", None, ctx,
    );
    if extra_conds.is_empty() {
        return wc;
    }
    let extra = extra_conds.join(" AND ");
    match wc.strip_prefix("WHERE ") {
        Some(rules) => format!("WHERE ({rules}) AND {extra}"),
        None => format!("WHERE {extra}"),
    }
}

/// Resolve a SMART collection name to its member track ids, via the same rule
/// engine as the smart-collections endpoints (`build_album_query`) so
/// `/library/tracks?collection=<name>` shows exactly the set the facet counted.
/// Case-insensitive; `None` if no such smart collection (or the query fails).
pub(super) fn smart_collection_track_ids(state: &AppState, name: &str) -> Option<Vec<i64>> {
    let rows = state
        .backend
        .query_many("SELECT name, rules, match_mode FROM smart_collections", &[])
        .ok()?;
    let row = rows.iter().find(|r| {
        r.first()
            .and_then(|v| v.as_string())
            .is_some_and(|n| n.eq_ignore_ascii_case(name))
    })?;
    let rules = row
        .get(1)
        .and_then(|v| v.as_string())
        .unwrap_or_else(|| "[]".into());
    let match_mode = row
        .get(2)
        .and_then(|v| v.as_string())
        .unwrap_or_else(|| "all".into());
    let resolver = crate::routes::smart_refs::DbRefResolver::new(&state.backend);
    let ctx = crate::routes::smart_refs::RefCtx::root(
        &resolver,
        Some(crate::routes::active_profile::DEFAULT_PROFILE_ID),
    );
    let where_clause = smart_collection_where(&rules, &match_mode, &[], &ctx);
    let sql = format!(
        "SELECT DISTINCT t.id FROM albums al \
         LEFT JOIN artists ar ON al.artist_id = ar.id \
         LEFT JOIN tracks t ON t.album_id = al.id {where_clause}"
    );
    let rows = state.backend.query_many(&sql, &[]).ok()?;
    Some(
        rows.iter()
            .filter_map(|r| r.first().and_then(|v| v.as_i64()))
            .collect(),
    )
}

/// Count tracks per artist. Unlike other facets, the artist name is NOT a column
/// on `tracks` (it stores only `artist_id`); it lives on the joined `artists`
/// table. The old code queried the phantom `t.artist_name` → SQL error → empty
/// facet (forum #1189). Join `artists` and group on its name. The `conds` are
/// self-contained (they reference `t.*` or subqueries), so the join is additive.
fn artist_facet(
    state: &AppState,
    limit: Option<i64>,
    conds: &[String],
    params: &[SqlValue],
) -> Vec<(String, i64)> {
    let extra = if conds.is_empty() {
        String::new()
    } else {
        format!(" AND {}", conds.join(" AND "))
    };
    let limit_clause = limit.map(|n| format!(" LIMIT {n}")).unwrap_or_default();
    let sql = format!(
        "SELECT ar.name, COUNT(*) AS n FROM tracks t \
         JOIN artists ar ON t.artist_id = ar.id \
         WHERE ar.name IS NOT NULL AND ar.name <> ''{extra} \
         GROUP BY ar.name ORDER BY n DESC{limit_clause}"
    );
    let bound: Vec<&dyn tune_core::db::backend::ToSqlValue> = params
        .iter()
        .map(|v| v as &dyn tune_core::db::backend::ToSqlValue)
        .collect();
    state
        .backend
        .query_many(&sql, &bound)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|row| {
            let mut it = row.into_iter();
            let v = it.next()?;
            let c = it.next()?;
            let value = v.as_string()?;
            Some((value, c.as_i64().unwrap_or(0)))
        })
        .collect()
}

/// Count distinct values of an extended tag in the `track_metadata` k/v store,
/// optionally narrowed to the tracks matching the active-facet conditions.
fn kv_facet(
    state: &AppState,
    key: &str,
    limit: Option<i64>,
    conds: &[String],
    params: &[SqlValue],
) -> Vec<(String, i64)> {
    let narrow = if conds.is_empty() {
        String::new()
    } else {
        format!(
            " AND track_id IN (SELECT t.id FROM tracks t WHERE {})",
            conds.join(" AND ")
        )
    };
    let limit_clause = limit.map(|n| format!(" LIMIT {n}")).unwrap_or_default();
    let sql = format!(
        "SELECT value, COUNT(DISTINCT track_id) AS n FROM track_metadata \
         WHERE key = '{key}' AND value <> ''{narrow} \
         GROUP BY value ORDER BY n DESC{limit_clause}"
    );
    let bound: Vec<&dyn tune_core::db::backend::ToSqlValue> = params
        .iter()
        .map(|v| v as &dyn tune_core::db::backend::ToSqlValue)
        .collect();
    state
        .backend
        .query_many(&sql, &bound)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|row| {
            let mut it = row.into_iter();
            let value = it.next()?.as_string()?;
            let count = it.next()?.as_i64().unwrap_or(0);
            Some((value, count))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::smart_collection_where;
    use super::untagged_condition;
    use crate::routes::smart_refs::{EmptyResolver, RefCtx};

    /// La liste des étiquettes surveillées est FERMÉE : c'est elle qui garantit
    /// que le SQL formaté ne dépend jamais de l'entrée de la requête.
    #[test]
    fn untagged_n_accepte_que_ses_cinq_champs() {
        for field in ["artist", "album", "genre", "year", "cover"] {
            assert!(
                untagged_condition(field).is_some(),
                "{field} devrait être reconnu"
            );
        }
        // Tout le reste ne filtre RIEN plutôt que d'injecter quoi que ce soit.
        for hostile in ["", "id", "t.genre", "1=1", "genre; DROP TABLE tracks--"] {
            assert!(
                untagged_condition(hostile).is_none(),
                "{hostile:?} n'aurait pas dû être accepté"
            );
        }
    }

    /// « Manquant » doit couvrir NULL ET la chaîne vide : un tag effacé par un
    /// éditeur laisse souvent une chaîne vide, et l'utilisateur qui range sa
    /// bibliothèque ne fait pas la différence entre les deux.
    #[test]
    fn untagged_traite_la_chaine_vide_comme_une_absence() {
        let genre = untagged_condition("genre").unwrap();
        assert!(genre.contains("IS NULL"), "{genre}");
        assert!(genre.contains("= ''"), "{genre}");
        let cover = untagged_condition("cover").unwrap();
        // Une piste sans album n'a pas de pochette non plus — sinon elle
        // échapperait au ménage.
        assert!(cover.contains("t.album_id IS NULL"), "{cover}");
        assert!(cover.contains("cover_path"), "{cover}");
    }

    #[test]
    fn smart_where_parenthesizes_any_rules_before_extra_conds() {
        // An `any` collection is OR-joined; the appended facet conditions must
        // not rebind (`a OR b AND c` would read as `a OR (b AND c)`).
        let ctx = RefCtx::root(&EmptyResolver, Some(1));
        let rules = r#"[{"field":"genre","operator":"contains","value":"soul"},
                        {"field":"genre","operator":"contains","value":"funk"}]"#;
        let conds = vec!["t.year = ?".to_string()];
        let wc = smart_collection_where(rules, "any", &conds, &ctx);
        assert!(wc.starts_with("WHERE ("), "{wc}");
        assert!(wc.contains(") AND t.year = ?"), "{wc}");
    }

    #[test]
    fn smart_where_extra_conds_only() {
        // Rules the engine cannot compile mean "matches everything": the extra
        // facet conditions must still form a valid WHERE on their own.
        let ctx = RefCtx::root(&EmptyResolver, Some(1));
        let wc = smart_collection_where("[]", "all", &["t.year = ?".to_string()], &ctx);
        assert_eq!(wc, "WHERE t.year = ?");
        assert_eq!(smart_collection_where("[]", "all", &[], &ctx), "");
    }
}
