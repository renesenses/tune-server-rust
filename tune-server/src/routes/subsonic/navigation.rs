//! Navigation et recherche (#7945, étape 2) : getArtists, getArtist,
//! getAlbum, getSong, getAlbumList2, search3, getGenres, getSongsByGenre,
//! getRandomSongs et getScanStatus.
//!
//! Chaque méthode lit la bibliothèque locale exposée ([`objets`]) et passe
//! ses résultats par la [`Garde`] parentale du profil de la clé.

use std::collections::BTreeMap;

use axum::Extension;
use axum::extract::{Query, State};
use axum::response::Response;
use serde_json::{Value, json};
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::backend::ToSqlValue;
use tune_core::db::track_repo::TrackRepo;

use super::identifiants::{Genre, decoder};
use super::objets::{self, Garde};
use super::reponse::{self, CodeErreur, Format};
use super::{ClientSubsonic, Parametres, format_de, parametre};
use crate::state::AppState;

/// Les articles ignorés pour l'index de getArtists.
pub const ARTICLES_IGNORES: &str = "The El La Los Las Le Les";

/// La garde parentale, ou la réponse d'erreur à rendre.
pub(super) fn garde(
    state: &AppState,
    client: &ClientSubsonic,
    format: Format,
) -> Result<Garde, Response> {
    Garde::du_profil(state, client.profile_id).map_err(|e| {
        tracing::warn!(error = %e, "subsonic_politique_parentale_illisible");
        reponse::erreur(format, CodeErreur::Generique, "Parental policy unavailable")
    })
}

fn entier(params: &Parametres, nom: &str, defaut: i64, max: i64) -> i64 {
    parametre(params, nom)
        .and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or(defaut)
        .clamp(0, max)
}

fn manquant(format: Format, nom: &str) -> Response {
    reponse::erreur(
        format,
        CodeErreur::ParametreManquant,
        &format!("Required parameter is missing: {nom}"),
    )
}

fn introuvable(format: Format, quoi: &str) -> Response {
    reponse::erreur(
        format,
        CodeErreur::Introuvable,
        &format!("{quoi} not found"),
    )
}

/// Le nom sans article initial, pour le tri et l'index.
fn sans_article(nom: &str) -> &str {
    let n = nom.trim();
    for article in ARTICLES_IGNORES.split(' ') {
        if let Some(reste) = n.get(article.len()..)
            && n[..article.len()].eq_ignore_ascii_case(article)
            && reste.starts_with(' ')
        {
            return reste.trim_start();
        }
    }
    n
}

/// La lettre d'index d'un nom : son premier caractère alphabétique en
/// capitale, `#` pour un chiffre ou un signe.
fn lettre_d_index(cle: &str) -> String {
    match cle.chars().next() {
        Some(c) if c.is_alphabetic() => c.to_uppercase().collect(),
        _ => "#".into(),
    }
}

pub(super) async fn get_artists(
    State(state): State<AppState>,
    Extension(client): Extension<ClientSubsonic>,
    Query(params): Query<Parametres>,
) -> Response {
    let format = format_de(&params);
    let garde = match garde(&state, &client, format) {
        Ok(g) => g,
        Err(r) => return r,
    };
    let sql = format!(
        "SELECT ar.id, ar.name, ar.sort_name, COUNT(a.id) FROM artists ar \
         JOIN albums a ON a.artist_id = ar.id WHERE {} GROUP BY ar.id, ar.name, ar.sort_name",
        objets::album_expose()
    );
    let lignes = state.backend.query_many(&sql, &[]).unwrap_or_default();
    let mut artistes: Vec<(String, Value)> = lignes
        .iter()
        .filter_map(|l| {
            let id = l.first()?.as_i64()?;
            let nom = l.get(1)?.as_string()?;
            let tri = l.get(2).and_then(|v| v.as_string());
            if garde.artiste_bloque(id, &nom) {
                return None;
            }
            let cle = tri
                .clone()
                .filter(|t| !t.trim().is_empty())
                .unwrap_or_else(|| sans_article(&nom).to_string());
            let n = l.get(3).and_then(|v| v.as_i64()).unwrap_or(0);
            Some((cle, objets::artiste(id, &nom, tri.as_deref(), n)))
        })
        .collect();
    artistes.sort_by_cached_key(|(cle, _)| cle.to_lowercase());
    let mut index: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for (cle, artiste) in artistes {
        index.entry(lettre_d_index(&cle)).or_default().push(artiste);
    }
    let index: Vec<Value> = index
        .into_iter()
        .map(|(nom, artist)| json!({"name": nom, "artist": artist}))
        .collect();
    reponse::ok(
        format,
        json!({"artists": {"ignoredArticles": ARTICLES_IGNORES, "index": index}}),
    )
}

pub(super) async fn get_artist(
    State(state): State<AppState>,
    Extension(client): Extension<ClientSubsonic>,
    Query(params): Query<Parametres>,
) -> Response {
    let format = format_de(&params);
    let Some(texte) = parametre(&params, "id") else {
        return manquant(format, "id");
    };
    let garde = match garde(&state, &client, format) {
        Ok(g) => g,
        Err(r) => return r,
    };
    let artiste = decoder(Genre::Artiste, texte)
        .and_then(|id| {
            ArtistRepo::with_backend(state.backend.clone())
                .get(id)
                .ok()
                .flatten()
        })
        .filter(|a| !garde.artiste_bloque(a.id.unwrap_or_default(), &a.name));
    let Some(artiste) = artiste else {
        return introuvable(format, "Artist");
    };
    let id = artiste.id.unwrap_or_default();
    let sql = format!(
        "SELECT a.id FROM albums a WHERE a.artist_id = ? AND {} \
         ORDER BY COALESCE(a.year, 0), LOWER(a.title), a.id",
        objets::album_expose()
    );
    let albums = objets::albums(&state, &garde, &objets::ids(&state, &sql, &[&id]));
    let mut corps = objets::artiste(
        id,
        &artiste.name,
        artiste.sort_name.as_deref(),
        albums.len() as i64,
    );
    corps["album"] = Value::Array(albums);
    reponse::ok(format, json!({ "artist": corps }))
}

/// L'album s'il est exposé et que la garde le laisse paraître.
fn album_visible(state: &AppState, garde: &Garde, id: i64) -> Option<tune_core::db::models::Album> {
    let sql = format!(
        "SELECT a.id FROM albums a WHERE a.id = ? AND {}",
        objets::album_expose()
    );
    if objets::ids(state, &sql, &[&id]).is_empty() {
        return None;
    }
    AlbumRepo::with_backend(state.backend.clone())
        .get(id)
        .ok()
        .flatten()
        .filter(|a| !garde.album_bloque(a))
}

/// La piste si elle est exposée, visible et que la garde la laisse paraître.
pub(super) fn piste_visible(
    state: &AppState,
    garde: &Garde,
    id: i64,
) -> Option<tune_core::db::models::Track> {
    let sql = format!(
        "SELECT t.id FROM tracks t WHERE t.id = ? AND {}",
        objets::piste_visible()
    );
    if objets::ids(state, &sql, &[&id]).is_empty() {
        return None;
    }
    TrackRepo::with_backend(state.backend.clone())
        .get(id)
        .ok()
        .flatten()
        .filter(|t| !garde.piste_bloquee(t))
}

pub(super) async fn get_album(
    State(state): State<AppState>,
    Extension(client): Extension<ClientSubsonic>,
    Query(params): Query<Parametres>,
) -> Response {
    let format = format_de(&params);
    let Some(texte) = parametre(&params, "id") else {
        return manquant(format, "id");
    };
    let garde = match garde(&state, &client, format) {
        Ok(g) => g,
        Err(r) => return r,
    };
    let Some(album) = decoder(Genre::Album, texte).and_then(|id| album_visible(&state, &garde, id))
    else {
        return introuvable(format, "Album");
    };
    let id = album.id.unwrap_or_default();
    let sql = format!(
        "SELECT t.id FROM tracks t WHERE t.album_id = ? AND {} \
         ORDER BY t.disc_number, t.track_number, t.id",
        objets::piste_visible()
    );
    let pistes = objets::pistes(&state, &garde, &objets::ids(&state, &sql, &[&id]));
    let totaux = objets::totaux(&state, &[id])
        .remove(&id)
        .unwrap_or_default();
    let mut corps = objets::album(&album, totaux);
    corps["song"] = Value::Array(pistes);
    reponse::ok(format, json!({ "album": corps }))
}

pub(super) async fn get_song(
    State(state): State<AppState>,
    Extension(client): Extension<ClientSubsonic>,
    Query(params): Query<Parametres>,
) -> Response {
    let format = format_de(&params);
    let Some(texte) = parametre(&params, "id") else {
        return manquant(format, "id");
    };
    let garde = match garde(&state, &client, format) {
        Ok(g) => g,
        Err(r) => return r,
    };
    match decoder(Genre::Piste, texte).and_then(|id| piste_visible(&state, &garde, id)) {
        Some(t) => reponse::ok(format, json!({ "song": objets::piste(&t) })),
        None => introuvable(format, "Song"),
    }
}

/// Les années de `fromYear`/`toYear`, dans l'ordre demandé.
fn annees(params: &Parametres) -> (Option<i64>, Option<i64>) {
    let lire = |n| parametre(params, n).and_then(|v: &str| v.trim().parse::<i64>().ok());
    (lire("fromYear"), lire("toYear"))
}

pub(super) async fn get_album_list2(
    State(state): State<AppState>,
    Extension(client): Extension<ClientSubsonic>,
    Query(params): Query<Parametres>,
) -> Response {
    let format = format_de(&params);
    let Some(type_) = parametre(&params, "type") else {
        return manquant(format, "type");
    };
    let garde = match garde(&state, &client, format) {
        Ok(g) => g,
        Err(r) => return r,
    };
    let taille = entier(&params, "size", 10, 500);
    let decalage = entier(&params, "offset", 0, i64::MAX / 2);
    let filtre = objets::album_expose();
    let genre;
    let (annee_min, annee_max);
    let mut liees: Vec<&dyn ToSqlValue> = Vec::new();
    let (condition, ordre) = match type_ {
        "random" => (String::new(), "random()".to_string()),
        "newest" => (String::new(), "a.id DESC".to_string()),
        "alphabeticalByName" => (String::new(), "LOWER(a.title), a.id".to_string()),
        "alphabeticalByArtist" => (
            String::new(),
            "LOWER(COALESCE(ar.sort_name, ar.name, '')), LOWER(a.title), a.id".to_string(),
        ),
        "byYear" => {
            let (Some(de), Some(a)) = annees(&params) else {
                return manquant(format, "fromYear/toYear");
            };
            annee_min = de.min(a);
            annee_max = de.max(a);
            liees.push(&annee_min);
            liees.push(&annee_max);
            let sens = if de > a { "DESC" } else { "ASC" };
            (
                " AND COALESCE(a.year, 0) BETWEEN ? AND ?".to_string(),
                format!("COALESCE(a.year, 0) {sens}, LOWER(a.title), a.id"),
            )
        }
        "byGenre" => {
            let Some(g) = parametre(&params, "genre") else {
                return manquant(format, "genre");
            };
            genre = g.to_lowercase();
            liees.push(&genre);
            (
                " AND LOWER(a.genre) = ?".to_string(),
                "LOWER(a.title), a.id".to_string(),
            )
        }
        // Étape 3 : écoutes, notes et favoris. Une liste vide est une réponse
        // valable, pas une erreur, pour un client qui les demande.
        "frequent" | "recent" | "highest" | "starred" => {
            return reponse::ok(format, json!({"albumList2": {"album": []}}));
        }
        _ => {
            return reponse::erreur(
                format,
                CodeErreur::Generique,
                &format!("Unknown list type: {type_}"),
            );
        }
    };
    let sql = format!(
        "SELECT a.id FROM albums a LEFT JOIN artists ar ON a.artist_id = ar.id \
         WHERE {filtre}{condition} ORDER BY {ordre} LIMIT {taille} OFFSET {decalage}"
    );
    let albums = objets::albums(&state, &garde, &objets::ids(&state, &sql, &liees));
    reponse::ok(format, json!({"albumList2": {"album": albums}}))
}

/// Le texte cherché : sans guillemets (certains clients les ajoutent), en
/// minuscules, avec `%` et `_` neutralisés ; vide = tout.
fn motif(requete: &str) -> String {
    let nettoye: String = requete
        .trim()
        .trim_matches('"')
        .trim()
        .to_lowercase()
        .chars()
        .filter(|c| !matches!(c, '%' | '_'))
        .collect();
    format!("%{nettoye}%")
}

pub(super) async fn search3(
    State(state): State<AppState>,
    Extension(client): Extension<ClientSubsonic>,
    Query(params): Query<Parametres>,
) -> Response {
    let format = format_de(&params);
    let Some(requete) = parametre(&params, "query") else {
        return manquant(format, "query");
    };
    let garde = match garde(&state, &client, format) {
        Ok(g) => g,
        Err(r) => return r,
    };
    let motif = motif(requete);
    let page = |nom: &str, defaut| {
        (
            entier(&params, &format!("{nom}Count"), defaut, 500),
            entier(&params, &format!("{nom}Offset"), 0, i64::MAX / 2),
        )
    };

    let (n, d) = page("artist", 20);
    let sql = format!(
        "SELECT ar.id, ar.name, ar.sort_name, COUNT(a.id) FROM artists ar \
         JOIN albums a ON a.artist_id = ar.id WHERE LOWER(ar.name) LIKE ? AND {} \
         GROUP BY ar.id, ar.name, ar.sort_name ORDER BY LOWER(ar.name), ar.id \
         LIMIT {n} OFFSET {d}",
        objets::album_expose()
    );
    let artistes: Vec<Value> = state
        .backend
        .query_many(&sql, &[&motif])
        .unwrap_or_default()
        .iter()
        .filter_map(|l| {
            let id = l.first()?.as_i64()?;
            let nom = l.get(1)?.as_string()?;
            if garde.artiste_bloque(id, &nom) {
                return None;
            }
            let tri = l.get(2).and_then(|v| v.as_string());
            let nb = l.get(3).and_then(|v| v.as_i64()).unwrap_or(0);
            Some(objets::artiste(id, &nom, tri.as_deref(), nb))
        })
        .collect();

    let (n, d) = page("album", 20);
    let sql = format!(
        "SELECT a.id FROM albums a LEFT JOIN artists ar ON a.artist_id = ar.id \
         WHERE (LOWER(a.title) LIKE ? OR LOWER(COALESCE(ar.name, '')) LIKE ?) AND {} \
         ORDER BY LOWER(a.title), a.id LIMIT {n} OFFSET {d}",
        objets::album_expose()
    );
    let albums = objets::albums(
        &state,
        &garde,
        &objets::ids(&state, &sql, &[&motif, &motif]),
    );

    let (n, d) = page("song", 20);
    let sql = format!(
        "SELECT t.id FROM tracks t LEFT JOIN albums al ON t.album_id = al.id \
         LEFT JOIN artists ar ON t.artist_id = ar.id \
         WHERE (LOWER(t.title) LIKE ? OR LOWER(COALESCE(ar.name, '')) LIKE ? \
         OR LOWER(COALESCE(al.title, '')) LIKE ?) AND {} \
         ORDER BY LOWER(t.title), t.id LIMIT {n} OFFSET {d}",
        objets::piste_visible()
    );
    let pistes = objets::pistes(
        &state,
        &garde,
        &objets::ids(&state, &sql, &[&motif, &motif, &motif]),
    );

    reponse::ok(
        format,
        json!({"searchResult3": {"artist": artistes, "album": albums, "song": pistes}}),
    )
}

pub(super) async fn get_genres(
    State(state): State<AppState>,
    Query(params): Query<Parametres>,
) -> Response {
    let format = format_de(&params);
    let sql = format!(
        "SELECT t.genre, COUNT(*), COUNT(DISTINCT t.album_id) FROM tracks t \
         WHERE t.genre IS NOT NULL AND t.genre <> '' AND {} GROUP BY t.genre \
         ORDER BY LOWER(t.genre)",
        objets::piste_visible()
    );
    let genres: Vec<Value> = state
        .backend
        .query_many(&sql, &[])
        .unwrap_or_default()
        .iter()
        .filter_map(|l| {
            Some(json!({
                "value": l.first()?.as_string()?,
                "songCount": l.get(1).and_then(|v| v.as_i64()).unwrap_or(0),
                "albumCount": l.get(2).and_then(|v| v.as_i64()).unwrap_or(0),
            }))
        })
        .collect();
    reponse::ok(format, json!({"genres": {"genre": genres}}))
}

pub(super) async fn get_songs_by_genre(
    State(state): State<AppState>,
    Extension(client): Extension<ClientSubsonic>,
    Query(params): Query<Parametres>,
) -> Response {
    let format = format_de(&params);
    let Some(genre) = parametre(&params, "genre") else {
        return manquant(format, "genre");
    };
    let garde = match garde(&state, &client, format) {
        Ok(g) => g,
        Err(r) => return r,
    };
    let n = entier(&params, "count", 10, 500);
    let d = entier(&params, "offset", 0, i64::MAX / 2);
    let genre = genre.to_string();
    let sql = format!(
        "SELECT t.id FROM tracks t WHERE t.genre = ? AND {} \
         ORDER BY t.album_id, t.disc_number, t.track_number, t.id LIMIT {n} OFFSET {d}",
        objets::piste_visible()
    );
    let pistes = objets::pistes(&state, &garde, &objets::ids(&state, &sql, &[&genre]));
    reponse::ok(format, json!({"songsByGenre": {"song": pistes}}))
}

pub(super) async fn get_random_songs(
    State(state): State<AppState>,
    Extension(client): Extension<ClientSubsonic>,
    Query(params): Query<Parametres>,
) -> Response {
    let format = format_de(&params);
    let garde = match garde(&state, &client, format) {
        Ok(g) => g,
        Err(r) => return r,
    };
    let n = entier(&params, "size", 10, 500);
    let genre = parametre(&params, "genre").map(str::to_string);
    let (de, a) = annees(&params);
    let mut condition = String::new();
    let mut liees: Vec<&dyn ToSqlValue> = Vec::new();
    if let Some(g) = &genre {
        condition.push_str(" AND t.genre = ?");
        liees.push(g);
    }
    if let Some(de) = &de {
        condition.push_str(" AND COALESCE(t.year, 0) >= ?");
        liees.push(de);
    }
    if let Some(a) = &a {
        condition.push_str(" AND COALESCE(t.year, 0) <= ?");
        liees.push(a);
    }
    let sql = format!(
        "SELECT t.id FROM tracks t WHERE {}{condition} ORDER BY random() LIMIT {n}",
        objets::piste_visible()
    );
    let pistes = objets::pistes(&state, &garde, &objets::ids(&state, &sql, &liees));
    reponse::ok(format, json!({"randomSongs": {"song": pistes}}))
}

pub(super) async fn get_scan_status(
    State(state): State<AppState>,
    Query(params): Query<Parametres>,
) -> Response {
    let format = format_de(&params);
    let sql = format!(
        "SELECT COUNT(*) FROM tracks t WHERE {}",
        objets::piste_visible()
    );
    let nombre = state
        .backend
        .query_one(&sql, &[])
        .ok()
        .flatten()
        .and_then(|l| l.first().and_then(|v| v.as_i64()))
        .unwrap_or(0);
    let en_cours = crate::routes::system::scan::droit_de_scanner_tenu();
    reponse::ok(
        format,
        json!({"scanStatus": {"scanning": en_cours, "count": nombre}}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn l_index_ignore_les_articles_et_range_les_chiffres_a_part_7945() {
        assert_eq!(sans_article("The Beatles"), "Beatles");
        assert_eq!(sans_article("Les Négresses Vertes"), "Négresses Vertes");
        assert_eq!(sans_article("Theremin"), "Theremin");
        assert_eq!(lettre_d_index("beatles"), "B");
        assert_eq!(lettre_d_index("2Pac"), "#");
    }

    #[test]
    fn le_motif_neutralise_les_jokers_et_les_guillemets_7945() {
        assert_eq!(motif("\"Abc\""), "%abc%");
        assert_eq!(motif("a%b_c"), "%abc%");
        assert_eq!(motif(""), "%%");
    }
}
