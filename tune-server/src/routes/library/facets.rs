use axum::Json;
use axum::extract::{Query, RawQuery, State};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::backend::SqlValue;
use tune_core::db::engine::Engine;
use tune_core::db::facet_filter::{
    Placeholders, TrackFilter, any_of, favorite_condition, untagged_condition,
};

use super::query_multi::track_filter_from_raw;
use crate::error::AppError;
use crate::state::AppState;

#[derive(Deserialize, Default)]
pub(super) struct FacetQuery {
    /// Comma-separated facet fields to compute (default: the common set).
    fields: Option<String>,
    /// Max values per facet (default 200). Également réutilisé par
    /// `albums-detailed` comme taille de page (même jeu de paramètres).
    pub(super) limit: Option<i64>,
    /// Pagination de `albums-detailed` uniquement — sans effet sur les facettes.
    pub(super) offset: Option<i64>,
    /// Manual collection name — tracks whose album is in that collection. The
    /// album membership lives as a JSON `album_ids` array in the `collections`
    /// setting, resolved to ids by the handler (not a joinable table).
    ///
    /// MONOVALUÉE (#2168) : une collection n'est pas une valeur de métadonnée
    /// mais un ensemble enregistré, résolu par deux moteurs distincts — leur
    /// union appelle un autre chantier. Elle reste donc lisible par `serde`.
    pub(super) collection: Option<String>,
    /// Les valeurs actives de CHAQUE facette, lues dans la chaîne de requête
    /// BRUTE par [`FacetQuery::hydrate`].
    ///
    /// ⚠️ Les facettes ne peuvent PAS être des champs de cette structure : la
    /// `Deserialize` dérivée refuse une clé en double (`duplicate field`), donc
    /// `?format=aiff&format=flac` rendait 400 tant qu'un champ `format`
    /// existait ici. Elles se lisent toutes dans `query_multi`, qui reprend au
    /// passage la validation de type que `serde` assurait (`?year=abc` → 400).
    #[serde(skip)]
    pub(super) sel: TrackFilter,
}

impl FacetQuery {
    /// Lit les facettes dans la chaîne de requête brute.
    ///
    /// À appeler dans CHAQUE point d'entrée qui déserialise un `FacetQuery` :
    /// sans elle, `sel` reste vide et plus aucune facette ne filtre.
    pub(super) fn hydrate(mut self, raw: Option<&str>) -> Result<Self, AppError> {
        self.sel = track_filter_from_raw(raw)?;
        Ok(self)
    }
}

/// Build the WHERE conditions (over alias `t` = tracks) for the active filters,
/// skipping the facet's own field so its alternatives remain countable.
///
/// **Sémantique (#2168)** : plusieurs valeurs DANS une facette se combinent en
/// **OU**, deux facettes différentes en **ET**.
///
/// **Les effectifs restent justes en sélection multiple** grâce à `exclude`,
/// qui était déjà là : en comptant la facette F, on applique toutes les AUTRES
/// facettes mais jamais F elle-même. L'effectif affiché à côté d'une valeur v
/// est donc « combien de pistes j'obtiendrais si v était la seule valeur cochée
/// de F », ce qui reste vrai quel que soit le nombre de valeurs déjà cochées
/// dans F — et ne change pas quand on en coche une deuxième, ce qui est
/// précisément ce qu'un utilisateur attend d'une case à cocher.
///
/// Values are always bound parameters; column/key names come from fixed
/// literals only.
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
    // ⚠️ UN SEUL compteur de marqueurs pour tout le WHERE. En SQLite ils
    // s'écrivent tous `?` et seul l'ORDRE de liaison compte ; en PostgreSQL ils
    // sont numérotés. Chaque valeur doit donc être empilée exactement quand son
    // marqueur est demandé — c'est la règle qu'un `IN (…)` bâti à la main avait
    // déjà enfreinte ici.
    let mut ph = Placeholders::new(engine);
    let sel = &q.sel;

    // Genre : la colonne `t.genre` OU le tableau JSON `t.genres`. Avec
    // plusieurs genres cochés, les deux tests s'étendent ensemble ; les valeurs
    // sont empilées dans l'ordre de sortie des marqueurs (les N du `IN`, puis
    // les N motifs `LIKE`).
    if exclude != "genre" && !sel.genres.is_empty() {
        let n = sel.genres.len();
        let in_part = ph.in_list_ci("t.genre", n).expect("liste non vide");
        let like_part = (0..n)
            .map(|_| format!("t.genres LIKE {}", ph.take()))
            .collect::<Vec<_>>()
            .join(" OR ");
        conds.push(format!("({in_part} OR {like_part})"));
        for g in &sel.genres {
            params.push(SqlValue::Text(g.clone()));
        }
        for g in &sel.genres {
            params.push(SqlValue::Text(format!("%\"{g}\"%")));
        }
    }
    if exclude != "year" {
        if let Some(c) = ph.in_list("t.year", sel.years.len()) {
            conds.push(c);
            for v in &sel.years {
                params.push(SqlValue::Int(*v));
            }
        }
    }
    if exclude != "format" {
        if let Some(c) = ph.in_list_ci("t.format", sel.formats.len()) {
            conds.push(c);
            for v in &sel.formats {
                params.push(SqlValue::Text(v.clone()));
            }
        }
    }
    if exclude != "sample_rate" {
        if let Some(c) = ph.in_list("t.sample_rate", sel.sample_rates.len()) {
            conds.push(c);
            for v in &sel.sample_rates {
                params.push(SqlValue::Int(*v));
            }
        }
    }
    if exclude != "bit_depth" {
        if let Some(c) = ph.in_list("t.bit_depth", sel.bit_depths.len()) {
            conds.push(c);
            for v in &sel.bit_depths {
                params.push(SqlValue::Int(*v));
            }
        }
    }
    // `source` (colonne `tracks.source`) n'est pas une facette du rail : elle ne
    // s'auto-exclut donc pas, comme avant #2168.
    if let Some(c) = ph.in_list("t.source", sel.sources.len()) {
        conds.push(c);
        for v in &sel.sources {
            params.push(SqlValue::Text(v.clone()));
        }
    }
    if exclude != "label" {
        if let Some(c) = ph.or_like_ci("t.label", sel.labels.len()) {
            conds.push(c);
            for v in &sel.labels {
                params.push(SqlValue::Text(format!("%{v}%")));
            }
        }
    }
    // `composer` est une facette à part entière : comme les autres, elle ne
    // doit pas se filtrer elle-même, sinon sélectionner « Bach » ne laisserait
    // plus que « Bach » dans la liste des compositeurs.
    if exclude != "composer" {
        if let Some(c) = ph.or_like_ci("t.composer", sel.composers.len()) {
            conds.push(c);
            for v in &sel.composers {
                params.push(SqlValue::Text(format!("%{v}%")));
            }
        }
    }
    if exclude != "artist" {
        // `tracks` has no artist_name column (artist is a FK to `artists`), and
        // these conditions run against `FROM tracks t` with no join, so resolve
        // the name via a subquery rather than the phantom t.artist_name (#1189).
        if let Some(c) = ph.in_list("name", sel.artists.len()) {
            conds.push(format!("t.artist_id IN (SELECT id FROM artists WHERE {c})"));
            for v in &sel.artists {
                params.push(SqlValue::Text(v.clone()));
            }
        }
    }
    // Extended-tag filters via the open `track_metadata` k/v store. `source`
    // facet == source_media key. Key is a fixed literal; values are bound.
    for (values, key, own) in [
        (&sel.countries, "release_country", "country"),
        (&sel.moods, "mood", "mood"),
        (&sel.source_medias, "source_media", "source"),
    ] {
        if exclude == own {
            continue;
        }
        if let Some(c) = ph.in_list("tm.value", values.len()) {
            conds.push(format!(
                "EXISTS (SELECT 1 FROM track_metadata tm \
                 WHERE tm.track_id = t.id AND tm.key = '{key}' AND {c})"
            ));
            for v in values {
                params.push(SqlValue::Text(v.clone()));
            }
        }
    }
    // Folder selection (Oxygen drill-down) narrows every facet's counts. The
    // folder-facet endpoint scopes by path prefix on its own, so it passes
    // exclude="folder" to skip this redundant predicate; the flat /facets
    // endpoint (exclude = a real field name) always applies it.
    if exclude != "folder" {
        if let Some(fld) = sel.folder.as_deref().filter(|s| !s.is_empty()) {
            conds.push(format!(
                "t.file_path LIKE {}{}",
                ph.take(),
                tune_core::db::track_repo::like_escape_clause(engine)
            ));
            params.push(SqlValue::Text(
                tune_core::db::track_repo::folder_like_pattern(fld),
            ));
        }
    }
    // Album rating (profile 1). Tracks inherit their album's rating via a join
    // to `album_ratings`; EXISTS keeps it self-contained on alias `t`.
    if exclude != "rating" {
        if let Some(c) = ph.in_list("arr.rating", sel.ratings.len()) {
            conds.push(format!(
                "EXISTS (SELECT 1 FROM album_ratings arr \
                 WHERE arr.album_id = t.album_id AND arr.profile_id = 1 AND {c})"
            ));
            for v in &sel.ratings {
                params.push(SqlValue::Int(*v));
            }
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
    // ⚠️ Les prédicats de ce bloc sont les JUMEAUX de ceux de
    // `TrackRepo::list_filtered` (autre crate). Depuis #2168 les deux passent
    // par `tune_core::db::facet_filter` : les vocabulaires FERMÉS (favoris,
    // étiquette manquante) et la construction des `IN (…)` n'y sont écrits
    // qu'une fois. Ce qui reste écrit deux fois, ce sont les colonnes et les
    // jointures — la liste dispose d'un `JOIN artists`, pas le compteur.
    // L'année d'enregistrement vit sur l'ALBUM, pas sur la piste : jointure par
    // EXISTS pour rester sur l'alias `t`.
    if exclude != "original_year" {
        if let Some(c) = ph.in_list("alo.original_year", sel.original_years.len()) {
            conds.push(format!(
                "EXISTS (SELECT 1 FROM albums alo WHERE alo.id = t.album_id AND {c})"
            ));
            for v in &sel.original_years {
                params.push(SqlValue::Int(*v));
            }
        }
    }
    if exclude != "favorite" {
        // Vocabulaire FERMÉ : une valeur inconnue ne filtre RIEN plutôt que
        // de tout exclure — ou, pire, de tout laisser passer.
        if let Some(c) = any_of(
            sel.favorites
                .iter()
                .filter_map(|k| favorite_condition(k))
                .map(str::to_string)
                .collect(),
        ) {
            conds.push(c);
        }
    }
    if exclude != "playlist" {
        if let Some(c) = ph.in_list_ci("pl.name", sel.playlists.len()) {
            conds.push(format!(
                "EXISTS (SELECT 1 FROM playlist_tracks pt JOIN playlists pl ON pl.id = pt.playlist_id \
                 WHERE pt.track_id = t.id AND {c})"
            ));
            for v in &sel.playlists {
                params.push(SqlValue::Text(v.clone()));
            }
        }
    }
    if exclude != "untagged" {
        // Champs choisis dans une liste FERMÉE : le SQL formaté ci-dessous ne
        // dépend jamais de l'entrée brute.
        if let Some(c) = any_of(
            sel.untagged
                .iter()
                .filter_map(|f| untagged_condition(f))
                .map(str::to_string)
                .collect(),
        ) {
            conds.push(c);
        }
    }
    if let Some(query) = sel.q.as_deref().filter(|s| !s.is_empty()) {
        // Artist match via subquery — no artist_name column / no join here (#1189).
        let p = ph.take();
        let p2 = ph.take();
        conds.push(format!(
            "(LOWER(t.title) LIKE LOWER({p}) OR t.artist_id IN \
             (SELECT id FROM artists WHERE LOWER(name) LIKE LOWER({p2})))"
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
    RawQuery(raw): RawQuery,
    State(state): State<AppState>,
) -> Result<Json<Value>, AppError> {
    // Les facettes à plusieurs valeurs se lisent dans la chaîne BRUTE : voir
    // `FacetQuery::hydrate`. Sans cet appel, `sel` reste vide et plus rien ne
    // filtre.
    let q = q.hydrate(raw.as_deref())?;
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
        all.push(missing.to_string());
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
        let resolver = crate::routes::smart_refs::DbRefResolver::new(state);
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
    let resolver = crate::routes::smart_refs::DbRefResolver::new(state);
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
    use super::*;
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

    // ── #2168 : plusieurs valeurs par facette ────────────────────────────
    //
    // Ces tests portent sur le SQL TEXTUEL et sur l'ORDRE des valeurs liées.
    // C'est le seul niveau où l'on peut prouver la correction sur les DEUX
    // moteurs sans deux bases : en SQLite tous les marqueurs s'écrivent `?`
    // (`SqliteDialect::placeholder` ignore son indice), donc seul l'ordre de
    // liaison compte ; en PostgreSQL ils sont numérotés et doivent former la
    // suite 1..N sans trou ni répétition.

    /// Construit la sélection comme le fait la route : depuis la chaîne de
    /// requête BRUTE, clé répétée comprise.
    fn depuis(raw: &str) -> FacetQuery {
        match FacetQuery::default().hydrate(Some(raw)) {
            Ok(q) => q,
            Err(_) => panic!("requête acceptable : {raw}"),
        }
    }

    fn texte(v: &SqlValue) -> String {
        match v {
            SqlValue::Text(s) => s.clone(),
            SqlValue::Int(i) => i.to_string(),
            autre => format!("{autre:?}"),
        }
    }

    /// La demande du fil 1513 : `aiff` OU `flac` dans la même facette.
    #[test]
    fn deux_valeurs_dans_une_facette_donnent_un_in_et_lient_dans_lordre() {
        let q = depuis("format=aiff&format=flac");

        let (conds, params) = build_conditions(&q, Engine::Postgres, "", None);
        assert_eq!(conds, vec!["LOWER(t.format) IN (LOWER($1), LOWER($2))"]);
        assert_eq!(
            params.iter().map(texte).collect::<Vec<_>>(),
            vec!["aiff".to_string(), "flac".to_string()]
        );

        // SQLite : même prédicat, marqueurs anonymes — l'ordre EST le lien.
        let (conds, params) = build_conditions(&q, Engine::Sqlite, "", None);
        assert_eq!(conds, vec!["LOWER(t.format) IN (LOWER(?), LOWER(?))"]);
        assert_eq!(
            params.iter().map(texte).collect::<Vec<_>>(),
            vec!["aiff".to_string(), "flac".to_string()]
        );
    }

    /// La sémantique retenue : OU dans une facette, ET entre facettes. Et le
    /// OU interne est PARENTHÉSÉ — sans quoi `a OU b ET c` se lirait
    /// `a OU (b ET c)` et la seconde facette cesserait de restreindre.
    #[test]
    fn ou_dans_une_facette_et_entre_facettes() {
        let q = depuis("format=aiff&format=flac&sample_rate=44100&sample_rate=96000");
        let (conds, _) = build_conditions(&q, Engine::Postgres, "", None);
        assert_eq!(conds.len(), 2, "deux facettes = deux conditions ET-ées");
        let where_clause = conds.join(" AND ");
        assert!(
            where_clause.contains("LOWER(t.format) IN ("),
            "{where_clause}"
        );
        assert!(
            where_clause.contains("t.sample_rate IN ($3, $4)"),
            "{where_clause}"
        );

        // Le genre teste DEUX colonnes (colonne + tableau JSON) : son OU doit
        // rester enfermé dans ses parenthèses.
        let q = depuis("genre=Jazz&genre=Blues&year=1971");
        let (conds, params) = build_conditions(&q, Engine::Postgres, "", None);
        assert_eq!(
            conds[0],
            "(LOWER(t.genre) IN (LOWER($1), LOWER($2)) OR t.genres LIKE $3 OR t.genres LIKE $4)"
        );
        assert_eq!(conds[1], "t.year = $5");
        assert_eq!(
            params.iter().map(texte).collect::<Vec<_>>(),
            vec![
                "Jazz".to_string(),
                "Blues".to_string(),
                "%\"Jazz\"%".to_string(),
                "%\"Blues\"%".to_string(),
                "1971".to_string(),
            ]
        );
    }

    /// ⚠️ LE test des deux moteurs. Un `IN (…)` bâti à la main qui n'avance pas
    /// le compteur produit un SQL parfaitement valide en SQLite et FAUX en
    /// PostgreSQL. On exige donc, sur une requête qui active TOUTES les formes
    /// de prédicat :
    ///   * PostgreSQL : les marqueurs, lus de gauche à droite, sont exactement
    ///     `$1 … $N` — pas de trou, pas de répétition, pas de décalage ;
    ///   * SQLite : autant de `?` que de valeurs liées ;
    ///   * les deux moteurs lient les MÊMES valeurs, dans le MÊME ordre.
    #[test]
    fn les_marqueurs_et_les_valeurs_restent_alignes_sur_les_deux_moteurs() {
        let raw = "genre=Jazz&genre=Blues\
                   &year=1971&year=1972\
                   &format=aiff&format=flac\
                   &sample_rate=44100&sample_rate=96000\
                   &bit_depth=16&bit_depth=24\
                   &source=local&source=qobuz\
                   &label=ECM&label=Blue+Note\
                   &composer=Bach&composer=Ravel\
                   &artist=Miles+Davis&artist=Bill+Evans\
                   &country=FR&country=US\
                   &mood=calme&mood=intense\
                   &source_media=CD&source_media=SACD\
                   &rating=4&rating=5\
                   &original_year=1959&original_year=1960\
                   &playlist=Ma+liste&playlist=Autre\
                   &favorite=track&favorite=album\
                   &untagged=genre&untagged=cover\
                   &folder=%2Fmnt%2Fmusic\
                   &q=so+what";
        let q = depuis(raw);

        let (conds_pg, params_pg) = build_conditions(&q, Engine::Postgres, "", None);
        let where_pg = conds_pg.join(" AND ");
        let numeros: Vec<usize> = where_pg
            .match_indices('$')
            .map(|(i, _)| {
                where_pg[i + 1..]
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
                    .parse::<usize>()
                    .expect("marqueur PostgreSQL numéroté")
            })
            .collect();
        assert_eq!(
            numeros,
            (1..=params_pg.len()).collect::<Vec<_>>(),
            "les marqueurs PostgreSQL doivent former 1..N dans l'ordre : {where_pg}"
        );
        assert!(
            params_pg.len() >= 30,
            "la requête d'épreuve doit couvrir toutes les formes de prédicat, \
             {} valeurs liées seulement",
            params_pg.len()
        );

        let (conds_sq, params_sq) = build_conditions(&q, Engine::Sqlite, "", None);
        let where_sq = conds_sq.join(" AND ");
        assert_eq!(
            where_sq.matches('?').count(),
            params_sq.len(),
            "autant de marqueurs que de valeurs liées : {where_sq}"
        );
        assert_eq!(
            conds_sq.len(),
            conds_pg.len(),
            "les deux moteurs doivent produire les mêmes prédicats"
        );
        assert_eq!(
            params_sq.iter().map(texte).collect::<Vec<_>>(),
            params_pg.iter().map(texte).collect::<Vec<_>>(),
            "les deux moteurs doivent lier les mêmes valeurs dans le même ordre"
        );
    }

    /// ⚠️ Le défaut redouté : une facette SANS valeur ne doit produire AUCUN
    /// prédicat. Ni `IN ()` (erreur de syntaxe), ni `1 = 1` (bibliothèque
    /// entière rendue en silence sous un filtre affiché).
    #[test]
    fn une_facette_sans_valeur_ne_produit_aucun_predicat() {
        for raw in ["", "format=&genre=&label=&year=", "limit=200&fields=format"] {
            for engine in [Engine::Sqlite, Engine::Postgres] {
                let (conds, params) = build_conditions(&depuis(raw), engine, "", None);
                assert!(conds.is_empty(), "{raw:?} → {conds:?}");
                assert!(params.is_empty(), "{raw:?} → {params:?}");
            }
        }
        // Et jamais de `IN ()` nulle part, quelle que soit la sélection.
        let (conds, _) = build_conditions(
            &depuis("format=flac&genre=Jazz&rating=5"),
            Engine::Postgres,
            "",
            None,
        );
        assert!(
            !conds.join(" AND ").contains("IN ()"),
            "un IN vide est une erreur SQL : {conds:?}"
        );
    }

    /// **Les effectifs restent justes en sélection multiple.** En comptant la
    /// facette F, on applique toutes les AUTRES facettes et jamais F elle-même
    /// — y compris quand F porte déjà plusieurs valeurs. L'effectif affiché à
    /// côté d'une valeur v répond donc toujours à la même question : « combien
    /// de pistes si v était cochée ? », et il ne bouge pas quand on coche une
    /// deuxième valeur de la même facette.
    #[test]
    fn la_facette_comptee_sexclut_elle_meme_meme_en_multi() {
        let q = depuis("format=aiff&format=flac&genre=Jazz&genre=Blues");

        let (conds, params) = build_conditions(&q, Engine::Postgres, "format", None);
        let where_clause = conds.join(" AND ");
        assert!(
            !where_clause.contains("t.format"),
            "la facette comptée ne doit pas se filtrer elle-même : {where_clause}"
        );
        assert!(where_clause.contains("t.genre"), "{where_clause}");
        // Les marqueurs se renumérotent depuis 1 : le prédicat retiré ne doit
        // pas laisser de trou dans la liaison.
        assert_eq!(
            conds[0],
            "(LOWER(t.genre) IN (LOWER($1), LOWER($2)) OR t.genres LIKE $3 OR t.genres LIKE $4)"
        );
        assert_eq!(params.len(), 4);

        // Symétrique : en comptant le genre, c'est le format qui reste.
        let (conds, params) = build_conditions(&q, Engine::Postgres, "genre", None);
        assert_eq!(conds, vec!["LOWER(t.format) IN (LOWER($1), LOWER($2))"]);
        assert_eq!(params.len(), 2);
    }

    /// Rétrocompatibilité : une URL d'avant #2168 produit EXACTEMENT le SQL
    /// d'avant — `= ?`, pas `IN (?)`. Rien à migrer, et les plans d'exécution
    /// ne changent pas pour la sélection simple, qui reste le cas courant.
    #[test]
    fn une_seule_valeur_produit_le_sql_davant() {
        let q = depuis("genre=Jazz&format=flac&year=1971&label=ECM&rating=4");
        let (conds, params) = build_conditions(&q, Engine::Postgres, "", None);
        assert_eq!(
            conds,
            vec![
                "(LOWER(t.genre) = LOWER($1) OR t.genres LIKE $2)".to_string(),
                "t.year = $3".to_string(),
                "LOWER(t.format) = LOWER($4)".to_string(),
                "LOWER(t.label) LIKE LOWER($5)".to_string(),
                "EXISTS (SELECT 1 FROM album_ratings arr WHERE arr.album_id = t.album_id \
                 AND arr.profile_id = 1 AND arr.rating = $6)"
                    .to_string(),
            ]
        );
        assert_eq!(
            params.iter().map(texte).collect::<Vec<_>>(),
            vec![
                "Jazz".to_string(),
                "%\"Jazz\"%".to_string(),
                "1971".to_string(),
                "flac".to_string(),
                "%ECM%".to_string(),
                "4".to_string(),
            ]
        );
    }

    /// Vocabulaires FERMÉS en sélection multiple : les deux conditions se
    /// combinent en OU et restent parenthésées ; une valeur inconnue
    /// disparaît sans rien laisser passer.
    #[test]
    fn les_vocabulaires_fermes_se_combinent_en_ou() {
        let (conds, params) = build_conditions(
            &depuis("favorite=track&favorite=album&untagged=genre&untagged=cover"),
            Engine::Postgres,
            "",
            None,
        );
        assert_eq!(conds.len(), 2);
        assert!(conds[0].starts_with("(EXISTS") && conds[0].contains(" OR EXISTS"));
        assert!(conds[1].starts_with('(') && conds[1].contains(" OR "));
        assert!(params.is_empty(), "ces prédicats ne lient aucune valeur");

        // Valeurs hors vocabulaire : aucun prédicat, et surtout pas un `1 = 1`.
        let (conds, _) = build_conditions(
            &depuis("favorite=1&untagged=mbid"),
            Engine::Postgres,
            "",
            None,
        );
        assert!(conds.is_empty(), "{conds:?}");
    }

    /// La virgule dans une valeur : la raison d'être de la clé répétée. Un
    /// genre « Jazz, Blues » doit rester UNE valeur.
    #[test]
    fn une_valeur_a_virgule_reste_une_seule_valeur() {
        let q = depuis("genre=Jazz%2C+Blues");
        let (conds, params) = build_conditions(&q, Engine::Postgres, "", None);
        assert_eq!(
            conds,
            vec!["(LOWER(t.genre) = LOWER($1) OR t.genres LIKE $2)"]
        );
        assert_eq!(texte(&params[0]), "Jazz, Blues");
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
