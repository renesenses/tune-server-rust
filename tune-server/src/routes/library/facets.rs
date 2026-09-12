use axum::Json;
use axum::extract::{Query, RawQuery, State};
use serde::Deserialize;
use serde_json::{Value, json};
use tune_http_types::panne_sql::OuDefautJournalise;

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
    collection: Option<&CollectionScope>,
) -> (Vec<String>, Vec<SqlValue>) {
    let (mut conds, params) = build_facet_conditions(q, engine, exclude, collection);
    // Albums masqués (#1391) : leurs pistes sortent de TOUS les effectifs de
    // facettes — le prédicat de SOCLE que `TrackRepo::list_filtered` pose de
    // son côté. Sans lui, « Jazz (12) » compterait des pistes que la liste ne
    // rend plus, précisément la divergence que ce fichier combat. Poussé en
    // DERNIER et sans marqueur : la numérotation des facettes ne bouge pas.
    conds.push(tune_core::db::facet_filter::hidden_tracks_excluded().to_string());
    (conds, params)
}

/// L'ensemble désigné par une sélection `collection`, résolu UNE fois par
/// [`resolve_collection`] — le MIROIR exact des deux champs que `TrackFilter`
/// porte de son côté (`collection_ids` / `collection_track_ids`).
///
/// #1864 : le compteur ne connaissait que les collections MANUELLES. Une
/// collection INTELLIGENTE ne résolvant aucun album, il recevait `Some([])`,
/// posait `1 = 0`, et TOUT le rail tombait à zéro pendant que la liste, elle,
/// rendait bien ses pistes.
#[derive(Default)]
pub(super) struct CollectionScope {
    /// Collection manuelle : des ids d'ALBUM (JSON des réglages).
    pub(super) albums: Option<Vec<i64>>,
    /// Collection intelligente : des ids de PISTE (règles compilées).
    pub(super) tracks: Option<Vec<i64>>,
}

/// Résout un nom de collection en l'ensemble qu'il désigne.
///
/// Une collection MANUELLE rend des ids d'album (JSON des réglages) ; une
/// collection INTELLIGENTE rend des ids de piste (requête de règles compilée).
/// Le manuel gagne en cas d'homonymie. Un nom inconnu rend un ensemble d'albums
/// VIDE, qui ne désigne rien — la collection demandée est simplement vide.
///
/// ⚠️ Appelée par les DEUX jumeaux (`/library/tracks` et `/library/facets`) :
/// c'est ce qui garantit qu'ils désignent le même ensemble (#1864).
pub(super) fn resolve_collection(state: &AppState, name: &str) -> CollectionScope {
    let albums = collection_album_ids(state, name);
    if !albums.is_empty() {
        return CollectionScope {
            albums: Some(albums),
            tracks: None,
        };
    }
    if let Some(tracks) = smart_collection_track_ids(state, name) {
        return CollectionScope {
            albums: None,
            tracks: Some(tracks),
        };
    }
    CollectionScope {
        albums: Some(Vec::new()),
        tracks: None,
    }
}

/// Les prédicats des seules FACETTES — testés à l'identique, sans le socle.
fn build_facet_conditions(
    q: &FacetQuery,
    engine: Engine,
    exclude: &str,
    // L'ensemble résolu de la sélection `collection` (le handler le résout une
    // fois, pour que ceci reste un pur constructeur de SQL).
    collection: Option<&CollectionScope>,
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
        // ⚠️ `or_like_ci` des DEUX côtés, comme le `in_list_ci` juste au-dessus
        // et comme les facettes sœurs (label, compositeur). Un `LIKE` nu est
        // insensible à la casse en SQLite mais SENSIBLE en PostgreSQL : le
        // même « JAZZ » dans `t.genres` était trouvé sur l'installation par
        // défaut et perdu sur PostgreSQL, alors que la colonne `t.genre`, elle,
        // était comparée sans la casse des deux côtés (#1821).
        let like_part = ph.or_like_ci("t.genres", n).expect("liste non vide");
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
    // CRD-6 : « instrument » est une facette à part entière — elle ne se filtre
    // pas elle-même — et vient des crédits, pas d'une colonne de `tracks`.
    if exclude != "instrument" {
        if let Some(c) = ph.in_list_ci("tc.instrument", sel.instruments.len()) {
            conds.push(tune_core::db::facet_filter::instrument_exists(engine, &c));
            for v in &sel.instruments {
                params.push(SqlValue::Text(v.clone()));
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
                tune_core::db::track_repo::like_escape_clause()
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
        if let Some(ids) = collection.and_then(|c| c.albums.as_deref()) {
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
        // Collection INTELLIGENTE : les règles ont été résolues en ids de piste
        // (nos propres i64), inlinés de la même façon sûre. Le JUMEAU
        // `TrackRepo::list_filtered` pose exactement ce prédicat.
        if let Some(ids) = collection.and_then(|c| c.tracks.as_deref()) {
            if ids.is_empty() {
                conds.push("1 = 0".to_string());
            } else {
                let list = ids
                    .iter()
                    .map(|id| id.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                conds.push(format!("t.id IN ({list})"));
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
    // Dynamic Range (#2144) : JUMEAU strict du prédicat de
    // `TrackRepo::list_filtered`, tous deux bâtis par `facet_filter` pour que
    // le rail ne puisse pas compter autrement que la liste qu'il filtre.
    if exclude != "dr" {
        if let Some(c) = ph.in_list(
            tune_core::db::facet_filter::DR_ALBUM_VALUE,
            sel.dynamic_ranges.len(),
        ) {
            conds.push(tune_core::db::facet_filter::dr_album_in(engine, &c));
            for v in &sel.dynamic_ranges {
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
        //
        // ⚠️ `unaccent()` des DEUX côtés, comme le jumeau `list_filtered` : sans
        // lui, `q=cafe` comptait sans « Café » mais la liste le rendait (#1864).
        let p = ph.take();
        let p2 = ph.take();
        conds.push(format!(
            "(LOWER(unaccent(t.title)) LIKE LOWER(unaccent({p})) OR t.artist_id IN \
             (SELECT id FROM artists WHERE LOWER(unaccent(name)) LIKE LOWER(unaccent({p2}))))"
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
///
/// `dr` (Dynamic Range, #2144) s'y lit aussi mais n'est PAS dans le jeu par
/// défaut : sur une bibliothèque sans tag DR — le cas de très loin le plus
/// courant — elle est vide, et une facette morte coûterait une requête à
/// chaque ouverture du rail pour ne rien montrer.
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
    let coll: Option<CollectionScope> = q
        .collection
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|name| resolve_collection(&state, name));
    let mut out = serde_json::Map::new();
    for field in requested {
        // Conditions narrow the count by the OTHER active facets (cumulative).
        let (conds, params) = build_conditions(&q, engine, &field, coll.as_ref());
        // The column / key is chosen from this fixed allow-list only, so the
        // formatted SQL below is never influenced by request input.
        let rows: Vec<(String, i64)> = match field.as_str() {
            "genre" => genre_facet(&state, limit, &conds, &params),
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
            // Dynamic Range (#2144). Absente du jeu par DÉFAUT : sur une
            // bibliothèque non taguée elle est vide, et une facette morte dans
            // le rail coûte une requête pour ne rien montrer. Un client la
            // demande explicitement (`fields=…,dr`).
            "dr" => dr_facet(&state, engine, limit, &conds, &params),
            // Instrument (CRD-6) : vient de `track_credits`, remplie par la passe
            // automatique (CRD-5). Comme `dr`, absente du jeu par défaut : vide
            // tant que les crédits ne sont pas là, un client la demande
            // explicitement (`fields=…,instrument`).
            "instrument" => instrument_facet(&state, engine, limit, &conds, &params),
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
        .ou_defaut_journalise()
        .into_iter()
        .filter_map(|row| {
            let mut it = row.into_iter();
            let y = it.next()?.as_i64()?;
            let count = it.next()?.as_i64().unwrap_or(0);
            Some((y.to_string(), count))
        })
        .collect()
}

/// Les Dynamic Range présents, du plus dynamique au plus compressé (#2144).
///
/// # Pourquoi une valeur par pastille, et non des tranches nommées
///
/// Le ticket demande de « classer et filtrer par tranches », MinimServer cité
/// en modèle — mais ses bornes exactes n'ont jamais été relevées, et la
/// couverture réelle des bibliothèques en tags DR n'a jamais été mesurée. Une
/// tranche gravée ici vivrait dans le contrat HTTP et survivrait à la mesure
/// qui la contredirait. Le rail rend donc les valeurs RÉELLES avec leurs
/// effectifs, et la sémantique de facette fait la tranche : cocher DR14, DR15
/// et DR16, c'est demander « DR14 et au-dessus » — en OU, comme trois formats.
///
/// La grille d'albums garde en parallèle sa tranche à bornes libres
/// (`?dr_min=`/`?dr_max=` sur `/library/albums`) : un intervalle ouvert n'a pas
/// sa place dans un rail de cases à cocher, et réciproquement.
///
/// `JOIN` (et non `LEFT JOIN`) : un album sans tag n'est pas une valeur de
/// facette. Un effectif nul ne remonte donc jamais, et une bibliothèque
/// entièrement non taguée rend un tableau vide — l'écran n'affiche alors pas
/// de facette plutôt qu'une facette morte.
fn dr_facet(
    state: &AppState,
    engine: Engine,
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
    // Décroissant, comme `year` : l'auditeur qui ouvre cette facette cherche
    // ses disques les plus dynamiques, pas les plus écrasés.
    let sql = format!(
        "SELECT dr.dr, COUNT(*) AS n FROM tracks t \
         JOIN ({source}) dr ON dr.album_id = t.album_id{where_clause} \
         GROUP BY dr.dr ORDER BY dr.dr DESC{limit_clause}",
        source = tune_core::db::facet_filter::dr_album_source(engine)
    );
    let bound: Vec<&dyn tune_core::db::backend::ToSqlValue> = params
        .iter()
        .map(|v| v as &dyn tune_core::db::backend::ToSqlValue)
        .collect();
    state
        .backend
        .query_many(&sql, &bound)
        .ou_defaut_journalise()
        .into_iter()
        .filter_map(|row| {
            let mut it = row.into_iter();
            let dr = it.next()?.as_i64()?;
            let count = it.next()?.as_i64().unwrap_or(0);
            Some((dr.to_string(), count))
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
    // Le filtre jumeau compare sans la casse (`in_list_ci` sur `pl.name`) :
    // deux listes « ListeA » et « listea » se cochent ensemble, elles doivent
    // donc aussi se compter ensemble (#1864).
    let brut: Vec<(String, i64)> = state
        .backend
        .query_many(&sql, &bound)
        .ou_defaut_journalise()
        .into_iter()
        .filter_map(|row| {
            let mut it = row.into_iter();
            let name = it.next()?.as_string()?;
            let count = it.next()?.as_i64().unwrap_or(0);
            (!name.is_empty()).then_some((name, count))
        })
        .collect();
    fusionner_les_casses(brut)
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
    let brut: Vec<(String, i64)> = state
        .backend
        .query_many(&sql, &bound)
        .ou_defaut_journalise()
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
        .collect();
    fusionner_les_casses(brut)
}

/// Rail « Genre » : une entrée par GENRE, pas par chaîne brute de colonne.
///
/// `column_facet` groupe sur la seule colonne `t.genre`. Or le filtre jumeau
/// (`build_facet_conditions` ici, `TrackRepo::list_filtered` côté liste) retient
/// une piste si `LOWER(t.genre) = LOWER(?)` **OU** si le tableau JSON
/// `t.genres` contient la valeur. Le rail comptait donc STRICTEMENT MOINS que
/// ce que son propre filtre sait trouver : tout genre SECONDAIRE restait
/// invisible, même coché il aurait rendu des pistes.
///
/// C'est ce qui rendait le classement dépendant du logiciel de gravure (#1821,
/// DEvir) : un disque gravé avec deux champs `GENRE` (Vorbis) ou deux atomes
/// `©gen` (MP4) n'apparaissait que sous son premier genre, tandis que le même
/// disque acheté ailleurs, avec « Jazz; Fusion » dans un unique `TCON`,
/// apparaissait sous les deux.
///
/// ⚠️ On compte exactement l'UNION que le filtre teste — `{t.genre}` ∪ les
/// éléments de `t.genres` — et RIEN de plus. En particulier on ne redécoupe
/// PAS `t.genre` : le filtre le compare en entier, donc annoncer « Jazz » pour
/// une ligne dont la colonne vaut « Jazz; Fusion » rendrait un compteur qui
/// ment, précisément le défaut de #1864.
fn genre_facet(
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
    // Pas de LIMIT en SQL : l'éclatement des valeurs multiples change les
    // effectifs, donc le classement. La troncature se fait après le compte.
    let sql = format!(
        "SELECT t.genre, t.genres, COUNT(*) AS n FROM tracks t \
         WHERE ((t.genre IS NOT NULL AND t.genre <> '') \
         OR (t.genres IS NOT NULL AND t.genres <> '')){extra} \
         GROUP BY t.genre, t.genres ORDER BY n DESC"
    );
    let bound: Vec<&dyn tune_core::db::backend::ToSqlValue> = params
        .iter()
        .map(|v| v as &dyn tune_core::db::backend::ToSqlValue)
        .collect();
    let mut brut: Vec<(String, i64)> = Vec::new();
    for row in state
        .backend
        .query_many(&sql, &bound)
        .ou_defaut_journalise()
    {
        let mut it = row.into_iter();
        let colonne = it.next().and_then(|v| v.as_string());
        let tableau = it.next().and_then(|v| v.as_string());
        let n = it.next().and_then(|v| v.as_i64()).unwrap_or(0);

        let mut valeurs: Vec<String> = Vec::new();
        if let Some(json) = tableau.as_deref() {
            if let Ok(arr) = serde_json::from_str::<Vec<String>>(json) {
                valeurs.extend(arr.into_iter().filter(|g| !g.trim().is_empty()));
            }
        }
        if let Some(g) = colonne.as_deref().filter(|g| !g.trim().is_empty()) {
            valeurs.push(g.to_string());
        }
        // Une ligne ne compte qu'une fois par genre, même si la colonne
        // répète le premier élément du tableau (le cas courant).
        let mut vus: std::collections::HashSet<String> = std::collections::HashSet::new();
        for v in valeurs {
            if vus.insert(v.to_lowercase()) {
                brut.push((v, n));
            }
        }
    }
    let mut sortie = fusionner_les_casses(brut);
    sortie.sort_by(|a, b| b.1.cmp(&a.1));
    if let Some(n) = limit {
        sortie.truncate(n.max(0) as usize);
    }
    sortie
}

/// Regroupe les orthographes qui ne diffèrent que par la casse.
///
/// #1864 : le `GROUP BY` de SQL distingue « Jazz » de « JAZZ », mais le FILTRE
/// jumeau (`in_list_ci`, des DEUX côtés) ne les distingue pas. Le rail
/// affichait donc deux valeurs à 1 piste dont chacune, une fois cochée, en
/// rendait 2 — précisément le compteur qui ment que cette issue combat.
///
/// L'orthographe retenue est la PLUS FRÉQUENTE (les lignes arrivent déjà
/// triées par effectif décroissant, donc c'est la première rencontrée) : une
/// bibliothèque où « Jazz » domine ne se met pas à afficher « JAZZ » parce
/// qu'une piste est mal étiquetée. Sur des valeurs numériques (année,
/// fréquence, profondeur) la fusion est un no-op.
fn fusionner_les_casses(brut: Vec<(String, i64)>) -> Vec<(String, i64)> {
    let mut sortie: Vec<(String, i64)> = Vec::with_capacity(brut.len());
    let mut retenue: Vec<i64> = Vec::with_capacity(brut.len());
    let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (valeur, n) in brut {
        match index.get(&valeur.to_lowercase()) {
            Some(&i) => {
                sortie[i].1 += n;
                // Ne dépend pas de l'ordre d'arrivée : à effectif supérieur,
                // l'orthographe affichée change.
                if n > retenue[i] {
                    retenue[i] = n;
                    sortie[i].0 = valeur;
                }
            }
            None => {
                index.insert(valeur.to_lowercase(), sortie.len());
                retenue.push(n);
                sortie.push((valeur, n));
            }
        }
    }
    sortie
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
        .ou_defaut_journalise()
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
        .ou_defaut_journalise();
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
        .ou_defaut_journalise()
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
/// La facette « instrument » (CRD-6) : chaque instrument des crédits, avec le
/// nombre de pistes qui le portent, resserré par les autres facettes actives.
/// Sur PostgreSQL `track_credits.track_id` est du texte : le sous-ensemble de
/// pistes est projeté dans le même type (`track_id_pour_track_credits`).
fn instrument_facet(
    state: &AppState,
    engine: Engine,
    limit: Option<i64>,
    conds: &[String],
    params: &[SqlValue],
) -> Vec<(String, i64)> {
    let narrow = if conds.is_empty() {
        String::new()
    } else {
        format!(
            " AND tc.track_id IN (SELECT {} FROM tracks t WHERE {})",
            tune_core::db::facet_filter::track_id_pour_track_credits(engine),
            conds.join(" AND ")
        )
    };
    let limit_clause = limit.map(|n| format!(" LIMIT {n}")).unwrap_or_default();
    let sql = format!(
        "SELECT tc.instrument AS value, COUNT(DISTINCT tc.track_id) AS n FROM track_credits tc \
         WHERE tc.instrument IS NOT NULL AND tc.instrument <> ''{narrow} \
         GROUP BY tc.instrument ORDER BY n DESC, value ASC{limit_clause}"
    );
    let bound: Vec<&dyn tune_core::db::backend::ToSqlValue> = params
        .iter()
        .map(|v| v as &dyn tune_core::db::backend::ToSqlValue)
        .collect();
    state
        .backend
        .query_many(&sql, &bound)
        .ou_defaut_journalise()
        .into_iter()
        .filter_map(|row| {
            let mut it = row.into_iter();
            let value = it.next()?.as_string()?;
            let count = it.next()?.as_i64().unwrap_or(0);
            Some((value, count))
        })
        .collect()
}

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
        .ou_defaut_journalise()
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

    /// CRD-6 : la facette « instrument » compte les pistes par instrument des
    /// crédits ; elle ne se filtre pas elle-même ; sa sélection resserre les
    /// autres facettes et la liste des pistes.
    #[tokio::test]
    async fn la_facette_instrument_compte_les_pistes_et_filtre_les_autres() {
        use tune_core::db::backend::ToSqlValue;
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let b = &state.backend;
        let piste = |titre: &str, annee: i64| {
            b.execute(
                "INSERT INTO tracks (title, file_path, year) VALUES (?1, ?2, ?3)",
                &[
                    &titre as &dyn ToSqlValue,
                    &format!("/m/{titre}.flac") as &dyn ToSqlValue,
                    &annee as &dyn ToSqlValue,
                ],
            )
            .unwrap();
            b.last_insert_rowid()
        };
        let credit = |track: i64, instrument: &str| {
            b.execute(
                "INSERT INTO track_credits (track_id, artist_name, role, instrument, position) VALUES (?1, ?2, 'performer', ?3, 0)",
                &[&track.to_string() as &dyn ToSqlValue, &"Musicien" as &dyn ToSqlValue, &instrument as &dyn ToSqlValue],
            )
            .unwrap();
        };
        let a = piste("A", 1998);
        let bb = piste("B", 2004);
        let _c = piste("C", 2004);
        credit(a, "oud");
        credit(bb, "oud");
        credit(bb, "piano");

        async fn facettes(state: &AppState, raw: &str) -> Value {
            // `fields` arrive par l'extracteur `Query`, les facettes à plusieurs
            // valeurs par la chaîne brute : on nourrit les deux, comme axum.
            let fields = raw
                .split('&')
                .find_map(|kv| kv.strip_prefix("fields="))
                .map(str::to_string);
            let Json(v) = library_facets(
                Query(FacetQuery {
                    fields,
                    ..Default::default()
                }),
                RawQuery(Some(raw.to_string())),
                State(state.clone()),
            )
            .await
            .ok()
            .expect("la route répond");
            v
        }
        let v = facettes(&state, "fields=instrument").await;
        assert_eq!(v["instrument"][0]["value"], "oud");
        assert_eq!(v["instrument"][0]["count"], 2);
        assert_eq!(v["instrument"][1]["value"], "piano");
        assert_eq!(v["instrument"][1]["count"], 1);

        // Sélectionner « piano » ne vide pas la facette instrument elle-même,
        // mais resserre les années à celle de la piste B.
        let v = facettes(&state, "fields=instrument,year&instrument=piano").await;
        assert_eq!(v["instrument"].as_array().unwrap().len(), 2, "{v}");
        assert_eq!(v["year"].as_array().unwrap().len(), 1, "{v}");
        assert_eq!(v["year"][0]["value"], "2004");

        // La liste des pistes suit le même filtre.
        let filtre = tune_core::db::facet_filter::TrackFilter {
            instruments: vec!["OUD".into()],
            ..Default::default()
        };
        let (pistes, total) =
            tune_core::db::track_repo::TrackRepo::with_backend(state.backend.clone())
                .list_filtered(&filtre, 50, 0)
                .unwrap();
        assert_eq!(total, 2, "casse indifférente");
        let titres: Vec<&str> = pistes.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(titres, ["A", "B"]);
    }

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

        let (conds, params) = build_facet_conditions(&q, Engine::Postgres, "", None);
        assert_eq!(conds, vec!["LOWER(t.format) IN (LOWER($1), LOWER($2))"]);
        assert_eq!(
            params.iter().map(texte).collect::<Vec<_>>(),
            vec!["aiff".to_string(), "flac".to_string()]
        );

        // SQLite : même prédicat, marqueurs anonymes — l'ordre EST le lien.
        let (conds, params) = build_facet_conditions(&q, Engine::Sqlite, "", None);
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
        let (conds, _) = build_facet_conditions(&q, Engine::Postgres, "", None);
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
        let (conds, params) = build_facet_conditions(&q, Engine::Postgres, "", None);
        assert_eq!(
            conds[0],
            "(LOWER(t.genre) IN (LOWER($1), LOWER($2)) OR (LOWER(t.genres) LIKE LOWER($3) OR LOWER(t.genres) LIKE LOWER($4)))"
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

        let (conds_pg, params_pg) = build_facet_conditions(&q, Engine::Postgres, "", None);
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

        let (conds_sq, params_sq) = build_facet_conditions(&q, Engine::Sqlite, "", None);
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
                let (conds, params) = build_facet_conditions(&depuis(raw), engine, "", None);
                assert!(conds.is_empty(), "{raw:?} → {conds:?}");
                assert!(params.is_empty(), "{raw:?} → {params:?}");
            }
        }
        // Et jamais de `IN ()` nulle part, quelle que soit la sélection.
        let (conds, _) = build_facet_conditions(
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

        let (conds, params) = build_facet_conditions(&q, Engine::Postgres, "format", None);
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
            "(LOWER(t.genre) IN (LOWER($1), LOWER($2)) OR (LOWER(t.genres) LIKE LOWER($3) OR LOWER(t.genres) LIKE LOWER($4)))"
        );
        assert_eq!(params.len(), 4);

        // Symétrique : en comptant le genre, c'est le format qui reste.
        let (conds, params) = build_facet_conditions(&q, Engine::Postgres, "genre", None);
        assert_eq!(conds, vec!["LOWER(t.format) IN (LOWER($1), LOWER($2))"]);
        assert_eq!(params.len(), 2);
    }

    /// Rétrocompatibilité : une URL d'avant #2168 produit EXACTEMENT le SQL
    /// d'avant — `= ?`, pas `IN (?)`. Rien à migrer, et les plans d'exécution
    /// ne changent pas pour la sélection simple, qui reste le cas courant.
    #[test]
    fn une_seule_valeur_produit_le_sql_davant() {
        let q = depuis("genre=Jazz&format=flac&year=1971&label=ECM&rating=4");
        let (conds, params) = build_facet_conditions(&q, Engine::Postgres, "", None);
        assert_eq!(
            conds,
            vec![
                "(LOWER(t.genre) = LOWER($1) OR LOWER(t.genres) LIKE LOWER($2))".to_string(),
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
        let (conds, params) = build_facet_conditions(
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
        let (conds, _) = build_facet_conditions(
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
        let (conds, params) = build_facet_conditions(&q, Engine::Postgres, "", None);
        assert_eq!(
            conds,
            vec!["(LOWER(t.genre) = LOWER($1) OR LOWER(t.genres) LIKE LOWER($2))"]
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
