use axum::Json;
use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};
use tune_http_types::panne_sql::OuDefautJournalise;

use crate::error::AppError;
use crate::state::AppState;
use tune_core::db::album_repo::AlbumRepo;

#[derive(Deserialize)]
pub(super) struct GenreQuery {
    query: Option<String>,
}

pub(super) async fn genre_tree(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    // Collect all individual genres from both the `genres` JSON array
    // and the legacy `genre` text column (splitting multi-genre strings).
    let raw_genres: Vec<(Option<String>, Option<String>)> = state
        .backend
        .query_many(
            "SELECT genre, genres FROM tracks WHERE (genre IS NOT NULL AND genre != '') OR (genres IS NOT NULL AND genres != '') GROUP BY genre, genres",
            &[],
        )
        .ou_defaut_journalise()
        .iter()
        .map(|row| {
            (
                row.first().and_then(|v| v.as_string()),
                row.get(1).and_then(|v| v.as_string()),
            )
        })
        .collect();

    let mut genre_set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (genre_col, genres_col) in &raw_genres {
        // Prefer the structured genres JSON array if present
        if let Some(json_str) = genres_col
            && let Ok(arr) = serde_json::from_str::<Vec<String>>(json_str)
        {
            for g in arr {
                let trimmed = g.trim().to_string();
                if !trimmed.is_empty() {
                    genre_set.insert(trimmed);
                }
            }
            continue;
        }
        // Fall back to splitting the legacy genre column
        if let Some(raw) = genre_col {
            for g in tune_core::metadata::split_genre_tag(raw) {
                if !g.is_empty() {
                    genre_set.insert(g);
                }
            }
        }
    }

    // Canonical dedup (case- AND separator-insensitive) so "Classique"/"classique"
    // and "Trip Hop"/"Trip-Hop" collapse into a single genre instead of appearing
    // as duplicate rows (Bilou, #1161). genre_set is a BTreeSet, so the variant
    // that sorts first is the one kept.
    let mut seen_lc: std::collections::HashSet<String> = std::collections::HashSet::new();
    let genres: Vec<String> = genre_set
        .into_iter()
        .filter(|g| seen_lc.insert(tune_core::metadata::genre_key(g)))
        .collect();

    // Load saved tree from settings (persisted by PUT /genre-tree).
    // If a saved tree exists, use it as the base and add any new genres
    // found in the library that aren't already in any branch.
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let mut tree: std::collections::BTreeMap<String, Vec<String>> = settings
        .get("genre_tree")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    if tree.is_empty() {
        for genre in &genres {
            tree.entry(genre.clone()).or_default();
        }
    }

    let mut classified: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (parent, children) in &tree {
        classified.insert(tune_core::metadata::genre_key(parent));
        for child in children {
            classified.insert(tune_core::metadata::genre_key(child));
        }
    }
    let unclassified: Vec<String> = genres
        .iter()
        .filter(|g| !classified.contains(&tune_core::metadata::genre_key(g)))
        .cloned()
        .collect();

    Ok(Json(json!({
        "tree": tree,
        "genres": genres,
        "unclassified": unclassified,
        "total": genres.len(),
    })))
}

pub(super) async fn update_genre_tree(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let tree_val = body.get("tree").unwrap_or(&body);
    settings.set("genre_tree", &tree_val.to_string()).ok();
    Json(json!({"updated": true}))
}

#[derive(Deserialize)]
pub(super) struct RenameGenreBody {
    from: String,
    to: String,
}

/// Rewrite one genre list, replacing every entry whose canonical key equals
/// `from_key` with `to`, then de-duplicating by canonical key (this is what
/// makes a rename into a MERGE when `to` already exists). Returns `None` when
/// nothing in the list matched (so the row is left untouched — non-target rows
/// are never re-normalised).
fn rewrite_genre_list(items: &[String], from_key: &str, to: &str) -> Option<Vec<String>> {
    if !items
        .iter()
        .any(|g| tune_core::metadata::genre_key(g) == from_key)
    {
        return None;
    }
    let mut out: Vec<String> = Vec::with_capacity(items.len());
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for g in items {
        let replaced = if tune_core::metadata::genre_key(g) == from_key {
            to.to_string()
        } else {
            g.clone()
        };
        if seen.insert(tune_core::metadata::genre_key(&replaced)) {
            out.push(replaced);
        }
    }
    Some(out)
}

/// Compute the rewritten (genre TEXT, genres JSON) for a row, or `None` if the
/// row doesn't reference `from_key` at all.
fn rewrite_row(
    genre_col: Option<&str>,
    genres_col: Option<&str>,
    from_key: &str,
    to: &str,
) -> Option<(String, String)> {
    let mut changed = false;

    // genre TEXT column (may hold a multi-genre string).
    let new_genre = match genre_col {
        Some(raw) if !raw.trim().is_empty() => {
            let items = tune_core::metadata::split_genre_tag(raw);
            match rewrite_genre_list(&items, from_key, to) {
                Some(rw) => {
                    changed = true;
                    rw.join("; ")
                }
                None => raw.to_string(),
            }
        }
        _ => String::new(),
    };

    // genres JSON array column.
    let new_genres = match genres_col {
        Some(json_str) if !json_str.trim().is_empty() => {
            match serde_json::from_str::<Vec<String>>(json_str) {
                Ok(arr) => match rewrite_genre_list(&arr, from_key, to) {
                    Some(rw) => {
                        changed = true;
                        serde_json::to_string(&rw).unwrap_or_else(|_| json_str.to_string())
                    }
                    None => json_str.to_string(),
                },
                Err(_) => json_str.to_string(),
            }
        }
        _ => String::new(),
    };

    if changed {
        Some((new_genre, new_genres))
    } else {
        None
    }
}

/// Rename (or merge) a genre across the whole library: rewrites the `genre` and
/// `genres` columns on every album and track, so a mis-spelled genre disappears
/// from the library views and Oxygen instead of lingering on the tags (forum:
/// "Arbre des genres" — a deleted branch reappeared because the genre stayed on
/// the albums). If `to` already exists the rename collapses into it (merge).
pub(super) async fn rename_genre(
    State(state): State<AppState>,
    Json(body): Json<RenameGenreBody>,
) -> Result<Json<Value>, AppError> {
    use tune_core::db::backend::ToSqlValue;

    let from = body.from.trim().to_string();
    let to = body.to.trim().to_string();
    if from.is_empty() || to.is_empty() {
        return Err(AppError::bad_request("from and to are required"));
    }
    let from_key = tune_core::metadata::genre_key(&from);
    if from_key == tune_core::metadata::genre_key(&to) && from == to {
        return Ok(Json(json!({"albums": 0, "tracks": 0, "unchanged": true})));
    }

    // Collect the rows that need rewriting from both tables.
    let collect = |table: &str| -> Vec<(i64, String, String)> {
        let sql = format!("SELECT id, genre, genres FROM {table}");
        let rows = state.backend.query_many(&sql, &[]).ou_defaut_journalise();
        rows.into_iter()
            .filter_map(|cols| {
                let id = cols.first().and_then(|v| v.as_i64())?;
                let genre = cols.get(1).and_then(|v| v.as_string());
                let genres = cols.get(2).and_then(|v| v.as_string());
                let (g, gs) = rewrite_row(genre.as_deref(), genres.as_deref(), &from_key, &to)?;
                Some((id, g, gs))
            })
            .collect()
    };
    let album_updates = collect("albums");
    let track_updates = collect("tracks");
    let n_albums = album_updates.len();
    let n_tracks = track_updates.len();

    // Apply all updates in a single transaction.
    let result = state.backend.write_tx(&mut |tx| {
        for (table, updates) in [("albums", &album_updates), ("tracks", &track_updates)] {
            let sql = format!("UPDATE {table} SET genre = ?, genres = ? WHERE id = ?");
            for (id, g, gs) in updates {
                let params: [&dyn ToSqlValue; 3] = [g, gs, id];
                tx.execute(&sql, &params)?;
            }
        }
        Ok(())
    });
    if let Err(e) = result {
        return Err(AppError::internal(format!("genre rename failed: {e}")));
    }

    // Keep the saved genre tree consistent (rename the branch/child too).
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    if let Some(Ok(mut tree)) = settings
        .get("genre_tree")
        .ok()
        .flatten()
        .map(|s| serde_json::from_str::<std::collections::BTreeMap<String, Vec<String>>>(&s))
    {
        let mut tree_changed = false;
        let renamed: std::collections::BTreeMap<String, Vec<String>> = std::mem::take(&mut tree)
            .into_iter()
            .map(|(k, children)| {
                let new_k = if tune_core::metadata::genre_key(&k) == from_key {
                    tree_changed = true;
                    to.clone()
                } else {
                    k
                };
                let new_children = match rewrite_genre_list(&children, &from_key, &to) {
                    Some(c) => {
                        tree_changed = true;
                        c
                    }
                    None => children,
                };
                (new_k, new_children)
            })
            .collect();
        if tree_changed && let Ok(s) = serde_json::to_string(&renamed) {
            settings.set("genre_tree", &s).ok();
        }
    }
    tracing::info!(from = %from, to = %to, albums = n_albums, tracks = n_tracks, "genre_renamed");
    Ok(Json(json!({
        "from": from,
        "to": to,
        "albums": n_albums,
        "tracks": n_tracks,
    })))
}

/// Les genres d'UN album, chacun avec sa clé canonique — `(clé, libellé)`.
///
/// 🔴 LA définition du genre dans Tune, et il n'en existe qu'une. Deux routes
/// l'appellent : `GET /library/genres` (les genres de la bibliothèque) et
/// `GET /dashboard/stats` (les genres ÉCOUTÉS, #4527). Les deux cartes se
/// retrouvent côte à côte sur l'accueil : si elles ne découpaient pas les
/// genres de la même façon, « Genres 115 » et « Genres écoutés 40 » ne
/// seraient pas comparables, et personne ne le verrait. Une seule fonction,
/// donc, et pas deux copies libres de diverger au premier correctif.
///
/// La règle, reprise telle qu'elle était dans `list_genres` :
///
/// * le tableau JSON `albums.genres` d'abord, s'il est lisible et non vide ;
/// * sinon la colonne héritée `albums.genre`, découpée par `split_genre_tag` ;
/// * chaque genre ramené à `genre_key`, pour que « Trip Hop » et
///   « Trip-Hop » ne fassent qu'un (#1161) ;
/// * dédoublonné DANS l'album : un album qui porte deux graphies d'un même
///   genre ne compte qu'une fois pour lui.
pub(crate) fn genres_de_l_album(
    genre: Option<&str>,
    genres: Option<&str>,
) -> Vec<(String, String)> {
    let mut noms: Vec<String> = Vec::new();
    if let Some(json_str) = genres
        && let Ok(arr) = serde_json::from_str::<Vec<String>>(json_str)
    {
        noms = arr
            .into_iter()
            .map(|g| g.trim().to_string())
            .filter(|g| !g.is_empty())
            .collect();
    }
    if noms.is_empty()
        && let Some(brut) = genre
    {
        noms = tune_core::metadata::split_genre_tag(brut);
    }
    let mut vues: std::collections::HashSet<String> = std::collections::HashSet::new();
    noms.into_iter()
        .filter_map(|g| {
            let cle = tune_core::metadata::genre_key(&g);
            (!cle.is_empty() && vues.insert(cle.clone())).then_some((cle, g))
        })
        .collect()
}

pub(super) async fn list_genres(
    State(state): State<AppState>,
    Query(params): Query<GenreQuery>,
) -> Result<Json<Value>, AppError> {
    // Collect genre + genres columns from all albums
    let raw: Vec<(Option<String>, Option<String>)> = state
        .backend
        .query_many(
            "SELECT genre, genres FROM albums WHERE (genre IS NOT NULL AND genre != '') OR (genres IS NOT NULL AND genres != '')",
            &[],
        )
        .ou_defaut_journalise()
        .iter()
        .map(|row| {
            (
                row.first().and_then(|v| v.as_string()),
                row.get(1).and_then(|v| v.as_string()),
            )
        })
        .collect();

    // Split multi-genre values and count albums per genre. Genres are grouped
    // by a canonical key that ignores case and the space-vs-hyphen separator,
    // so "Trip Hop" and "Trip-Hop" collapse into a single card instead of two
    // (#1161). For each key we keep per-spelling tallies to choose a stable
    // display label.
    let mut groups: std::collections::BTreeMap<String, std::collections::BTreeMap<String, i64>> =
        std::collections::BTreeMap::new();
    for (genre_col, genres_col) in &raw {
        for (key, g) in genres_de_l_album(genre_col.as_deref(), genres_col.as_deref()) {
            *groups.entry(key).or_default().entry(g).or_insert(0) += 1;
        }
    }

    // Filter by query parameter (case-insensitive LIKE match)
    let filter = params.query.map(|q| q.to_lowercase());

    let items: Vec<Value> = groups
        .values()
        .filter_map(|variants| {
            let count: i64 = variants.values().sum();
            // Display label = the most common spelling; ties broken by the
            // lexicographically smallest for a stable, deterministic label.
            let name = variants
                .iter()
                .max_by(|(an, ac), (bn, bc)| ac.cmp(bc).then_with(|| bn.cmp(an)))
                .map(|(name, _)| name.clone())?;
            match &filter {
                Some(q) if !name.to_lowercase().contains(q) => None,
                _ => Some(json!({ "name": name, "count": count })),
            }
        })
        .collect();

    Ok(Json(json!(items)))
}

pub(super) async fn genre_albums(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Json<Value> {
    let decoded = urlencoding::decode(&name).unwrap_or_else(|_| name.clone().into());
    let repo = AlbumRepo::with_backend(state.backend.clone());
    let items = repo.list_by_genre(&decoded).unwrap_or_default();
    let items: Vec<Value> = items.iter().map(|a| a.to_json()).collect();
    Json(json!(items))
}

#[cfg(test)]
mod tests {
    use super::{rewrite_genre_list, rewrite_row};
    use tune_core::metadata::genre_key;

    #[test]
    fn rewrite_list_replaces_and_merges() {
        let k = genre_key("Rok");
        // simple rename
        assert_eq!(
            rewrite_genre_list(&["Rok".into()], &k, "Rock"),
            Some(vec!["Rock".to_string()])
        );
        // merge: Rok + existing Rock → single Rock (dedup by key)
        assert_eq!(
            rewrite_genre_list(&["Rock".into(), "Rok".into()], &k, "Rock"),
            Some(vec!["Rock".to_string()])
        );
        // untouched when the list doesn't contain the source
        assert_eq!(rewrite_genre_list(&["Jazz".into()], &k, "Rock"), None);
        // other genres preserved, order kept
        assert_eq!(
            rewrite_genre_list(&["Jazz".into(), "Rok".into()], &k, "Rock"),
            Some(vec!["Jazz".to_string(), "Rock".to_string()])
        );
    }

    #[test]
    fn rewrite_row_handles_both_columns() {
        let k = genre_key("Rok");
        // genre TEXT (multi) + genres JSON both rewritten
        let (g, gs) = rewrite_row(Some("Jazz; Rok"), Some(r#"["Rok","Pop"]"#), &k, "Rock").unwrap();
        assert_eq!(g, "Jazz; Rock");
        assert_eq!(gs, r#"["Rock","Pop"]"#);
        // no match → None (row left untouched)
        assert!(rewrite_row(Some("Jazz"), Some(r#"["Pop"]"#), &k, "Rock").is_none());
    }
}

/// `genres_de_l_album` — LA définition du genre, partagée depuis #4527.
///
/// Elle a été extraite de `list_genres` pour que `/dashboard/stats` compte les
/// genres écoutés exactement comme la bibliothèque compte les siens. Aucun
/// banc ne gardait le comptage de `list_genres` avant l'extraction : ceux-ci
/// le font, sur la fonction ET sur la route.
#[cfg(test)]
mod genres_de_l_album_4527 {
    use super::*;

    fn cles(genre: Option<&str>, genres: Option<&str>) -> Vec<String> {
        genres_de_l_album(genre, genres)
            .into_iter()
            .map(|(k, _)| k)
            .collect()
    }

    #[test]
    fn le_tableau_json_prime_sur_la_colonne_heritee() {
        let v = genres_de_l_album(Some("Ignoré"), Some(r#"["Jazz","Blues"]"#));
        let noms: Vec<&str> = v.iter().map(|(_, n)| n.as_str()).collect();
        assert_eq!(noms, ["Jazz", "Blues"]);
    }

    #[test]
    fn sans_tableau_la_colonne_heritee_est_decoupee() {
        assert_eq!(cles(Some("Rock; Jazz"), None).len(), 2);
    }

    #[test]
    fn un_tableau_illisible_ou_vide_retombe_sur_la_colonne() {
        assert_eq!(cles(Some("Jazz"), Some("pas du json")).len(), 1);
        assert_eq!(cles(Some("Jazz"), Some("[]")).len(), 1);
    }

    #[test]
    fn deux_graphies_dans_un_meme_album_comptent_une_fois() {
        // #1161 : « Trip Hop » et « Trip-Hop » ont la même clé canonique.
        assert_eq!(cles(None, Some(r#"["Trip Hop","Trip-Hop"]"#)).len(), 1);
    }

    #[test]
    fn rien_ne_rend_rien() {
        assert!(cles(None, None).is_empty());
        assert!(cles(Some(""), Some("")).is_empty());
    }

    /// 🔴 La route elle-même : l'extraction n'a rien changé à ce qu'elle rend.
    #[tokio::test]
    async fn la_route_fusionne_les_graphies_et_compte_les_albums() {
        use tune_core::db::artist_repo::ArtistRepo;
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let ar = ArtistRepo::with_backend(state.backend.clone())
            .get_or_create("Artiste", None, None)
            .unwrap();
        for (titre, genre) in [("A", "Trip Hop"), ("B", "Trip-Hop"), ("C", "Jazz")] {
            let id = AlbumRepo::with_backend(state.backend.clone())
                .get_or_create(titre, ar.id.unwrap(), None)
                .unwrap()
                .id
                .unwrap();
            state
                .backend
                .execute(
                    "UPDATE albums SET genre = ? WHERE id = ?",
                    &[&genre.to_string(), &id],
                )
                .unwrap();
        }
        // `AppError` n'implémente pas `Debug` : pas de `.unwrap()` ici.
        let Ok(Json(v)) = list_genres(State(state), Query(GenreQuery { query: None })).await else {
            panic!("GET /library/genres a échoué");
        };
        let rangs = v.as_array().unwrap();
        assert_eq!(rangs.len(), 2, "trip hop (2 albums) + jazz (1) : {v}");
        let trip = rangs
            .iter()
            .find(|r| {
                r["name"]
                    .as_str()
                    .unwrap_or("")
                    .to_lowercase()
                    .starts_with("trip")
            })
            .expect("la carte Trip Hop a disparu");
        assert_eq!(
            trip["count"],
            json!(2),
            "les deux graphies doivent se fondre en UNE carte de 2 albums"
        );
    }
}
