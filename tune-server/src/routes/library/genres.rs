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

/// Les albums ÉCOUTÉS et le NOMBRE d'écoutes de chacun — v0.9.168.
///
/// 🔴 Même jointure que `ALBUMS_ECOUTES` de `dashboard.rs`, à UNE différence
/// qui est tout le sujet : celle-là fait `SELECT DISTINCT` et perd donc les
/// répétitions, parce qu'elle ne répond qu'à « combien de genres distincts ».
/// On ne peut pas en tirer un classement par volume : deux albums de mêmes
/// colonnes de genre y tiennent une seule ligne, et cent écoutes d'un album y
/// pèsent autant qu'une. Ici on veut le VOLUME — donc `GROUP BY` sur les deux
/// colonnes de genre avec son `COUNT(*)`, une écoute de plus doit peser.
///
/// Grouper sur les colonnes de genre plutôt que sur `al.id` suffit — seul le
/// couple (genre, genres) sert ensuite — et remonte d'autant moins de lignes.
/// `TEXT` sous SQLite comme sous PostgreSQL : le `GROUP BY` est portable,
/// comme l'était le `DISTINCT`.
///
/// Comme pour `unique_genres`, une écoute de service (Qobuz…) sans album local
/// n'a pas de genre connu et ne compte pas — limite assumée, dite dans #4527.
const ECOUTES_PAR_GENRES_D_ALBUM: &str = "SELECT al.genre, al.genres, COUNT(*) \
     FROM listen_history lh \
     LEFT JOIN tracks t ON t.id = lh.track_id \
     JOIN albums al ON al.id = COALESCE(lh.album_id, t.album_id) \
     WHERE (al.genre IS NOT NULL AND al.genre != '') \
        OR (al.genres IS NOT NULL AND al.genres != '') \
     GROUP BY al.genre, al.genres";

/// Le cumul des écoutes PAR CLÉ de genre.
///
/// 🔴 Un album porte souvent PLUSIEURS genres, et ses écoutes comptent pour
/// CHACUN d'eux : dix écoutes d'un album « Jazz ; Blues » font dix écoutes de
/// Jazz ET dix de Blues. On ne divise pas — la question posée par le panneau
/// est « ce genre, l'écoutes-tu ? », pas « quelle part de ton temps ».
///
/// Le découpage passe par `genres_de_l_album`, la seule définition du genre :
/// le classement se range donc exactement sur les mêmes clés que les pastilles
/// qu'il ordonne, et sur celles que compte `unique_genres`.
///
/// Fonction pure, sans base : c'est elle que gardent les témoins.
fn cumul_des_ecoutes(
    lignes: &[(Option<String>, Option<String>, i64)],
) -> std::collections::HashMap<String, i64> {
    let mut par_cle: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    for (genre, genres, ecoutes) in lignes {
        for (cle, _) in genres_de_l_album(genre.as_deref(), genres.as_deref()) {
            *par_cle.entry(cle).or_insert(0) += *ecoutes;
        }
    }
    par_cle
}

/// Les écoutes par clé de genre, ou `None` si la requête échoue.
///
/// `None` et non une carte vide : le champ `plays` est alors ABSENT de la
/// réponse, au lieu de valoir 0 partout. Même doctrine que `unique_genres`
/// (#4527) — sauf qu'ici l'absence porte en plus une consigne d'affichage :
/// l'écran, ne voyant aucune écoute, retombe sur l'ordre de la bibliothèque
/// plutôt que de montrer un panneau vide. Un `plays: 0` généralisé
/// produirait le même écran, mais en AFFIRMANT que rien n'a été écouté.
fn ecoutes_par_cle(state: &AppState) -> Option<std::collections::HashMap<String, i64>> {
    let lignes = match state.backend.query_many(ECOUTES_PAR_GENRES_D_ALBUM, &[]) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(error = %e, "library_genres_ecoutes_error");
            return None;
        }
    };
    let tuples: Vec<(Option<String>, Option<String>, i64)> = lignes
        .iter()
        .map(|row| {
            (
                row.first().and_then(|v| v.as_string()),
                row.get(1).and_then(|v| v.as_string()),
                row.get(2).and_then(|v| v.as_i64()).unwrap_or(0),
            )
        })
        .collect();
    Some(cumul_des_ecoutes(&tuples))
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

    // Le VOLUME D'ÉCOUTE de chaque genre — v0.9.168, décision de Bertrand du
    // 27/09/2026. Le panneau « Genres » de la première ligne de l'accueil
    // triait sur `count`, c'est-à-dire sur ce que l'utilisateur POSSÈDE : d'où
    // Pop-Rock en tête d'une bibliothèque qui en est pleine, quoi qu'on
    // écoute. Il doit trier sur ce qu'on ÉCOUTE.
    //
    // 🔴 Le classement est servi ICI, dans la réponse que le panneau appelle
    // DÉJÀ, et non à côté de `unique_genres` dans `/dashboard/stats` : la
    // première ligne se peint au démarrage, une requête de plus s'y verrait.
    // Et le repli « aucune écoute → ordre de la bibliothèque » a besoin des
    // DEUX chiffres dans la même main : les servir séparément obligerait
    // l'écran à attendre deux réponses pour savoir laquelle il doit croire.
    let ecoutes = ecoutes_par_cle(&state);

    // Filter by query parameter (case-insensitive LIKE match)
    let filter = params.query.map(|q| q.to_lowercase());

    let items: Vec<Value> = groups
        .iter()
        .filter_map(|(cle, variants)| {
            let count: i64 = variants.values().sum();
            // Display label = the most common spelling; ties broken by the
            // lexicographically smallest for a stable, deterministic label.
            let name = variants
                .iter()
                .max_by(|(an, ac), (bn, bc)| ac.cmp(bc).then_with(|| bn.cmp(an)))
                .map(|(name, _)| name.clone())?;
            if let Some(q) = &filter
                && !name.to_lowercase().contains(q)
            {
                return None;
            }
            let mut item = json!({ "name": name, "count": count });
            // `plays` est ABSENT si la requête d'écoutes a échoué, et vaut 0
            // pour un genre possédé mais jamais écouté. Les deux se lisent
            // différemment côté écran : voir `ecoutes_par_cle`.
            if let Some(par_cle) = &ecoutes {
                item["plays"] = json!(par_cle.get(cle).copied().unwrap_or(0));
            }
            Some(item)
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

/// 🔴 LE CLASSEMENT PAR VOLUME D'ÉCOUTE — v0.9.168, décision de Bertrand du
/// 27/09/2026.
///
/// Le panneau « Genres » de la première ligne de l'accueil triait sur `count`,
/// le nombre d'albums EN BIBLIOTHÈQUE. Pop-Rock y arrivait donc en tête d'une
/// collection qui en est pleine, même si son propriétaire n'écoute que du
/// jazz. `plays` porte désormais ce qu'il ÉCOUTE, et l'écran s'y range.
///
/// Ces témoins gardent les trois choses qui peuvent silencieusement se perdre :
/// que le classement suit bien le volume et non la taille de la bibliothèque,
/// qu'un album à plusieurs genres compte pour CHACUN, et que `unique_genres`
/// — servi par `/dashboard/stats`, voisin de ce panneau sur le même écran —
/// garde exactement la valeur qu'il avait.
#[cfg(test)]
mod classement_par_ecoutes_168 {
    use super::*;
    use tune_core::metadata::genre_key;

    /// La forme que rend `ECOUTES_PAR_GENRES_D_ALBUM` : (genre, genres, n).
    fn lignes(
        brut: &[(Option<&str>, Option<&str>, i64)],
    ) -> Vec<(Option<String>, Option<String>, i64)> {
        brut.iter()
            .map(|(g, gs, n)| (g.map(str::to_string), gs.map(str::to_string), *n))
            .collect()
    }

    /// Le classement décroissant, tel que l'écran le lira.
    fn classement(par_cle: &std::collections::HashMap<String, i64>) -> Vec<(String, i64)> {
        let mut v: Vec<(String, i64)> = par_cle.iter().map(|(k, n)| (k.clone(), *n)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v
    }

    /// 🔴 Dix écoutes passent devant deux. C'est tout le chantier.
    #[test]
    fn un_genre_ecoute_dix_fois_passe_devant_un_ecoute_deux_fois() {
        let par_cle = cumul_des_ecoutes(&lignes(&[
            (Some("Blues"), None, 2),
            (Some("Jazz"), None, 10),
        ]));
        assert_eq!(par_cle.get(&genre_key("Jazz")).copied(), Some(10));
        assert_eq!(par_cle.get(&genre_key("Blues")).copied(), Some(2));
        let rangs = classement(&par_cle);
        assert_eq!(
            rangs.first().map(|(k, n)| (k.as_str(), *n)),
            Some((genre_key("Jazz").as_str(), 10)),
            "le plus écouté doit être en tête : {rangs:?}"
        );
    }

    /// 🔴 Plusieurs lignes d'un même genre S'ADDITIONNENT. Le `GROUP BY` porte
    /// sur les colonnes de genre, pas sur la clé canonique : « Trip Hop » et
    /// « Trip-Hop » arrivent en DEUX lignes et doivent se cumuler en une seule
    /// pastille, sinon la pastille affichée ne pèse que la moitié de ce qu'on
    /// écoute et rate son rang.
    #[test]
    fn deux_graphies_du_meme_genre_cumulent_leurs_ecoutes() {
        let par_cle = cumul_des_ecoutes(&lignes(&[
            (Some("Trip Hop"), None, 4),
            (Some("Trip-Hop"), None, 3),
            (Some("Jazz"), None, 5),
        ]));
        assert_eq!(par_cle.get(&genre_key("Trip Hop")).copied(), Some(7));
        let rangs = classement(&par_cle);
        assert_eq!(
            rangs.first().map(|(k, _)| k.clone()),
            Some(genre_key("Trip Hop")),
            "4 + 3 = 7 doit passer devant 5 : {rangs:?}"
        );
    }

    /// 🔴 Un album à PLUSIEURS genres compte pour CHACUN d'eux, sans division.
    #[test]
    fn un_album_a_plusieurs_genres_compte_pour_chacun() {
        // Colonne héritée découpée…
        let par_cle = cumul_des_ecoutes(&lignes(&[(Some("Jazz; Blues"), None, 10)]));
        assert_eq!(par_cle.get(&genre_key("Jazz")).copied(), Some(10));
        assert_eq!(par_cle.get(&genre_key("Blues")).copied(), Some(10));
        // …et tableau JSON, qui prime sur elle.
        let par_cle = cumul_des_ecoutes(&lignes(&[(
            Some("Ignoré"),
            Some(r#"["Rock","Pop","Folk"]"#),
            6,
        )]));
        assert_eq!(par_cle.get(&genre_key("Rock")).copied(), Some(6));
        assert_eq!(par_cle.get(&genre_key("Pop")).copied(), Some(6));
        assert_eq!(par_cle.get(&genre_key("Folk")).copied(), Some(6));
        assert_eq!(par_cle.get(&genre_key("Ignoré")).copied(), None);
    }

    /// 🔴 `unique_genres` ne change pas de valeur.
    ///
    /// `/dashboard/stats` compte les genres écoutés en versant les clés dans un
    /// `HashSet` et en rendant son `len()`. Ce chantier n'a pas touché
    /// `dashboard.rs` — mais il ne suffit pas de le dire : ce témoin rejoue
    /// l'algorithme du tableau de bord sur les mêmes lignes et exige que
    /// l'ENSEMBLE DES CLÉS du nouveau compteur soit exactement le même. Un
    /// compteur qui se mettrait à retenir une clé de plus (ou une de moins)
    /// déplacerait le chiffre « N genres écoutés » affiché juste à côté.
    #[test]
    fn le_compteur_garde_le_meme_jeu_de_cles_que_unique_genres() {
        let brut = &[
            (Some("Jazz; Blues"), None, 10),
            (Some("Pop-Rock"), None, 1),
            (Some("Ignoré"), Some(r#"["Rock","Pop"]"#), 3),
            (Some("Trip Hop"), None, 4),
            (Some("Trip-Hop"), None, 2),
            (None, None, 7),
        ];
        // L'algorithme de `dashboard::genres_ecoutes` — un ensemble, pas un
        // compteur : les écoutes n'y entrent pas.
        let mut cles: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (genre, genres, _) in brut {
            for (cle, _) in genres_de_l_album(*genre, *genres) {
                cles.insert(cle);
            }
        }
        let par_cle = cumul_des_ecoutes(&lignes(brut));
        let nouvelles: std::collections::HashSet<String> = par_cle.keys().cloned().collect();
        assert_eq!(
            nouvelles, cles,
            "le compteur doit porter EXACTEMENT les clés que compte unique_genres"
        );
        assert_eq!(
            par_cle.len(),
            cles.len(),
            "unique_genres = {} genres écoutés, et ce chiffre ne bouge pas",
            cles.len()
        );
    }

    /// 🔴 LA ROUTE : `plays` arrive vraiment dans le JSON, et il DÉSACCORDE
    /// `count` — c'est à cela qu'on voit qu'il ne le recopie pas.
    ///
    /// Les données sont choisies pour que les deux ordres se contredisent :
    /// Pop-Rock est le genre le plus POSSÉDÉ (3 albums) et presque pas écouté
    /// (1 écoute) ; Jazz n'a qu'un album et dix écoutes. Un tri par `count`
    /// mettrait Pop-Rock en tête, un tri par `plays` met Jazz. Le témoin exige
    /// les deux affirmations à la fois.
    #[tokio::test]
    async fn la_route_sert_les_ecoutes_et_elles_contredisent_la_taille() {
        use tune_core::db::artist_repo::ArtistRepo;
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let ar = ArtistRepo::with_backend(state.backend.clone())
            .get_or_create("Artiste", None, None)
            .unwrap();
        let mut ids = std::collections::HashMap::new();
        for (titre, genre) in [
            ("PR1", "Pop-Rock"),
            ("PR2", "Pop-Rock"),
            ("PR3", "Pop-Rock"),
            ("JB", "Jazz; Blues"),
            ("CL", "Classique"),
        ] {
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
            ids.insert(titre, id);
        }
        // Dix écoutes de l'album « Jazz ; Blues », par `listen_history.album_id`.
        for i in 0..10 {
            state
                .backend
                .execute(
                    "INSERT INTO listen_history (title, album_id, listened_at) VALUES (?, ?, ?)",
                    &[
                        &format!("piste {i}"),
                        &ids["JB"],
                        &"2026-09-27T10:00:00Z".to_string(),
                    ],
                )
                .unwrap();
        }
        // UNE écoute d'un album Pop-Rock, par la PISTE : l'album se retrouve
        // par `COALESCE(lh.album_id, t.album_id)`, l'autre branche de la
        // jointure. Sans elle, une écoute sur deux ne compterait pour rien.
        state
            .backend
            .execute(
                "INSERT INTO tracks (id, title, album_id, artist_id, file_path, duration_ms) \
                 VALUES (901, 'une piste pop', ?, ?, '/music/pr.flac', 200000)",
                &[&ids["PR1"], &ar.id.unwrap()],
            )
            .unwrap();
        state
            .backend
            .execute(
                "INSERT INTO listen_history (title, track_id, listened_at) \
                 VALUES ('une piste pop', 901, '2026-09-27T11:00:00Z')",
                &[],
            )
            .unwrap();

        // `AppError` n'implémente pas `Debug` : pas de `.unwrap()` ici.
        let Ok(Json(v)) = list_genres(State(state), Query(GenreQuery { query: None })).await else {
            panic!("GET /library/genres a échoué");
        };
        let rangs = v.as_array().expect("une liste").clone();
        let par_nom = |nom: &str| -> (i64, i64) {
            let item = rangs
                .iter()
                .find(|r| r["name"].as_str() == Some(nom))
                .unwrap_or_else(|| panic!("le genre {nom} a disparu de {v}"));
            (
                item["count"].as_i64().unwrap_or(-1),
                item["plays"]
                    .as_i64()
                    .unwrap_or_else(|| panic!("`plays` absent pour {nom} dans {v}")),
            )
        };

        // Un album à deux genres : ses dix écoutes comptent pour CHACUN.
        assert_eq!(par_nom("Jazz"), (1, 10), "Jazz : 1 album, 10 écoutes");
        assert_eq!(par_nom("Blues"), (1, 10), "Blues : le même album, idem");
        // Le plus POSSÉDÉ est presque pas écouté.
        assert_eq!(
            par_nom("Pop-Rock"),
            (3, 1),
            "Pop-Rock : 3 albums en rayon, 1 seule écoute"
        );
        // Possédé, jamais écouté : `plays` vaut 0, et non « absent ».
        assert_eq!(
            par_nom("Classique"),
            (1, 0),
            "Classique : en rayon, jamais écouté"
        );

        // 🔴 Les deux ordres se contredisent, et c'est le volume qui gagne.
        let par_ecoutes = |a: &Value, b: &Value| {
            b["plays"]
                .as_i64()
                .unwrap_or(0)
                .cmp(&a["plays"].as_i64().unwrap_or(0))
        };
        let mut sur_ecoutes = rangs.clone();
        sur_ecoutes.sort_by(par_ecoutes);
        assert_eq!(
            sur_ecoutes.first().map(|r| r["plays"].as_i64()),
            Some(Some(10)),
            "le tri par écoutes doit commencer par les 10 écoutes : {sur_ecoutes:?}"
        );
        let mut sur_taille = rangs.clone();
        sur_taille.sort_by(|a, b| {
            b["count"]
                .as_i64()
                .unwrap_or(0)
                .cmp(&a["count"].as_i64().unwrap_or(0))
        });
        assert_eq!(
            sur_taille.first().map(|r| r["name"].as_str()),
            Some(Some("Pop-Rock")),
            "le tri par taille de bibliothèque, lui, mettrait Pop-Rock en tête"
        );
        assert_ne!(
            sur_taille.first().map(|r| r["name"].clone()),
            sur_ecoutes.first().map(|r| r["name"].clone()),
            "si les deux ordres coïncidaient, ce témoin ne prouverait rien"
        );
    }
}
