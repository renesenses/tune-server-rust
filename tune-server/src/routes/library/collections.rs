use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::AppError;
use crate::state::AppState;
use tune_core::db::album_repo::AlbumRepo;

use super::album_order::{CollectionSort, sort_albums};
use super::now_iso_utc;

#[derive(Deserialize)]
pub(super) struct CreateCollectionBody {
    name: String,
    description: Option<String>,
    /// Nom d’icône libre, rendu par le client. Envoyé par
    /// `createCollection(name, description, icon, color)` depuis toujours.
    icon: Option<String>,
    /// Couleur de la pastille du dossier, `#RGB` ou `#RRGGBB` (#3044).
    color: Option<String>,
}

/// Mise à jour partielle : seuls les champs présents sont écrasés. Les
/// dossiers créés avant #3044 n’ont pas de couleur et aucun écran ne
/// permettait de leur en donner une — `api.updateCollection` existait côté
/// client sans route en face.
#[derive(Deserialize, Default)]
pub(super) struct UpdateCollectionBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    icon: Option<String>,
    #[serde(default)]
    color: Option<String>,
}

/// `col.color` est injecté tel quel dans un attribut `style` du client
/// (`style="background:{col.color}"`). On n’accepte donc que la forme rendue
/// par un `<input type="color">` : `#RGB` ou `#RRGGBB`. Tout le reste est
/// refusé à la porte plutôt que stocké puis recraché dans du CSS.
fn couleur_valide(couleur: &str) -> bool {
    let Some(chiffres) = couleur.strip_prefix('#') else {
        return false;
    };
    matches!(chiffres.len(), 3 | 6) && chiffres.chars().all(|c| c.is_ascii_hexdigit())
}

/// Refuse une couleur hors format ; `None` reste `None`.
fn verifier_couleur(couleur: &Option<String>) -> Result<(), AppError> {
    match couleur {
        Some(c) if !couleur_valide(c) => Err(AppError::bad_request(
            "color doit être au format #RGB ou #RRGGBB",
        )),
        _ => Ok(()),
    }
}

#[derive(Deserialize)]
pub(super) struct CollectionAlbumPath {
    id: i64,
    album_id: i64,
}

#[derive(Deserialize, Default)]
pub(super) struct CollectionAlbumsQuery {
    /// `artist` (défaut), `title`, `year`, ou `added` pour l'ordre d'ajout.
    sort: Option<String>,
}

/// Au plus quatre pochettes par collection : la mosaïque du client en compte
/// quatre, en chercher davantage serait du trafic pour rien.
const POCHETTES_MAX: usize = 4;

/// Garde-fou sur la requête : une collection peut porter des milliers d'albums,
/// et on n'a besoin que des premiers de CHACUNE. On ne demande donc jamais plus
/// que ce plafond d'identifiants, toutes collections confondues.
const POCHETTES_IDS_MAX: usize = 600;

/// Combien d'albums lire par collection pour y trouver quatre pochettes
/// DISTINCTES.
///
/// Seize fois la cible, et non quatre : un album peut n'avoir aucune pochette —
/// il ne consomme alors pas de case mais bien une place dans cette fenêtre — et
/// plusieurs albums d'une même édition partagent la leur. Une fenêtre trop
/// courte rendrait deux ou trois pochettes là où la collection en a dix, sans
/// que rien ne le signale : la mosaïque cyclerait, ce qui reste crédible à
/// l'œil.
///
/// Ce n'est pas une garantie. Une collection dont les soixante-quatre premiers
/// albums partagent trois pochettes en montrera trois — « si possible », a dit
/// Bertrand, et c'est bien la limite du possible sans lire la collection
/// entière.
const POCHETTES_FENETRE: usize = POCHETTES_MAX * 16;

/// Les quatre pochettes d'une collection, dans SON ordre à elle.
///
/// `par_album` associe un identifiant d'album à son chemin de pochette et à sa
/// clé de mosaïque. Extraite du corps de la route pour être testable : cette
/// route n'avait aucun test, et c'est ici qu'une mosaïque se remplit d'une
/// seule image.
fn quatre_pochettes(
    ids: &[i64],
    par_album: &std::collections::HashMap<i64, (String, String)>,
) -> Vec<String> {
    let mut vues: Vec<String> = Vec::new();
    let mut cles: Vec<String> = Vec::new();
    for id in ids {
        let Some((cover, cle)) = par_album.get(id) else {
            continue;
        };
        // Dédoublonnage sur le DISQUE d'abord : un même disque est stocké comme
        // plusieurs lignes d'albums, une par artiste crédité, chacune avec son
        // propre fichier de pochette en cache — autant de chemins, une seule
        // image (« Les indispensables du piano », treize pianistes, 02/09/2026).
        if cles.iter().any(|k| k == cle) {
            continue;
        }
        // Puis sur le CHEMIN : deux disques distincts peuvent malgré tout
        // partager une pochette.
        if vues.iter().any(|v| v == cover) {
            continue;
        }
        cles.push(cle.clone());
        vues.push(cover.clone());
        if vues.len() == POCHETTES_MAX {
            break;
        }
    }
    vues
}

pub(super) async fn list_collections(State(state): State<AppState>) -> Json<Value> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let mut data = settings
        .get("collections")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Vec<Value>>(&s).ok())
        .unwrap_or_default();

    // Pochettes de mosaïque, jointes ICI plutôt que réclamées album par album.
    //
    // Le nouveau client dessine la pochette d'une collection comme une mosaïque
    // des pochettes qu'elle contient. La collection ne porte que des
    // `album_ids` : sans ce champ, le client irait chercher chaque album un par
    // un, pour n'en garder que quatre.
    //
    // UNE SEULE requête pour toutes les collections : on rassemble les premiers
    // identifiants de chacune, on lit leurs pochettes d'un coup, puis on
    // recompose en respectant l'ordre PROPRE à chaque collection.
    //
    // Champ ADDITIF : `covers` s'ajoute, rien n'est retiré. Un client qui
    // l'ignore ne voit aucune différence.
    let mut voulus: Vec<i64> = Vec::new();
    for c in &data {
        if let Some(ids) = c.get("album_ids").and_then(|v| v.as_array()) {
            // Fenêtre large : voir `POCHETTES_FENETRE`.
            for id in ids
                .iter()
                .filter_map(|v| v.as_i64())
                .take(POCHETTES_FENETRE)
            {
                if voulus.len() >= POCHETTES_IDS_MAX {
                    break;
                }
                if !voulus.contains(&id) {
                    voulus.push(id);
                }
            }
        }
    }

    // id -> (chemin de pochette, cle de mosaique)
    let mut par_album: std::collections::HashMap<i64, (String, String)> =
        std::collections::HashMap::new();
    if !voulus.is_empty() {
        // Les identifiants viennent de `as_i64` : ce sont des entiers, jamais du
        // texte. Les insérer directement ne peut donc pas porter d'injection, et
        // évite d'avoir à lier un nombre variable de paramètres.
        let liste = voulus
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            // Pas de jointure sur `artists` : la clé de mosaïque ne retient que
            // le TITRE. L'artiste est justement ce qui varie entre les lignes
            // d'un même disque.
            "SELECT al.id, al.cover_path, al.title \
             FROM albums al \
             WHERE al.id IN ({liste}) \
             AND al.cover_path IS NOT NULL AND al.cover_path <> ''"
        );
        if let Ok(rows) = state.backend.query_many(&sql, &[]) {
            for r in &rows {
                if let (Some(id), Some(c)) = (
                    r.first().and_then(|v| v.as_i64()),
                    r.get(1).and_then(|v| v.as_string()),
                ) {
                    let titre = r.get(2).and_then(|v| v.as_string());
                    let cle = tune_core::library::mosaique::cle_pochette(titre.as_deref(), &c);
                    par_album.insert(id, (c, cle));
                }
            }
        }
    }

    for c in &mut data {
        let ids: Vec<i64> = c
            .get("album_ids")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_i64()).collect())
            .unwrap_or_default();
        let vues = quatre_pochettes(&ids, &par_album);
        if let Some(obj) = c.as_object_mut() {
            obj.insert("covers".to_string(), json!(vues));
        }
    }

    Json(json!(data))
}

pub(super) async fn create_collection(
    State(state): State<AppState>,
    Json(body): Json<CreateCollectionBody>,
) -> Result<impl IntoResponse, AppError> {
    verifier_couleur(&body.color)?;
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let mut collections: Vec<Value> = settings
        .get("collections")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let id = collections
        .iter()
        .filter_map(|c| c.get("id").and_then(|v| v.as_i64()))
        .max()
        .unwrap_or(0)
        + 1;

    let collection = json!({
        "id": id,
        "name": body.name,
        "description": body.description,
        "icon": body.icon,
        "color": body.color,
        "album_ids": [],
        "created_at": now_iso_utc(),
    });
    collections.push(collection.clone());
    settings
        .set("collections", &serde_json::to_string(&collections)?)
        .ok();

    Ok((StatusCode::CREATED, Json(collection)))
}

pub(super) async fn get_collection(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let collections: Vec<Value> = settings
        .get("collections")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let found = collections
        .iter()
        .find(|c| c.get("id").and_then(|v| v.as_i64()) == Some(id));
    match found {
        Some(c) => Json(c.clone()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// `PUT /library/collections/{id}` — renomme un dossier, ou lui pose une
/// icône et une couleur (#3044). Mise à jour partielle : les albums déjà
/// rangés et les champs non fournis sont laissés intacts.
pub(super) async fn update_collection(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateCollectionBody>,
) -> Result<impl IntoResponse, AppError> {
    verifier_couleur(&body.color)?;
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let mut collections: Vec<Value> = settings
        .get("collections")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let found = collections
        .iter_mut()
        .find(|c| c.get("id").and_then(|v| v.as_i64()) == Some(id));
    let Some(collection) = found else {
        return Err(AppError::not_found("collection not found"));
    };
    let Some(obj) = collection.as_object_mut() else {
        return Err(AppError::internal("collection mal formée"));
    };
    if let Some(name) = body.name {
        obj.insert("name".into(), json!(name));
    }
    if let Some(description) = body.description {
        obj.insert("description".into(), json!(description));
    }
    if let Some(icon) = body.icon {
        obj.insert("icon".into(), json!(icon));
    }
    if let Some(color) = body.color {
        obj.insert("color".into(), json!(color));
    }
    let mise_a_jour = collection.clone();
    settings
        .set("collections", &serde_json::to_string(&collections)?)
        .ok();
    Ok(Json(mise_a_jour))
}

pub(super) async fn delete_collection(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, AppError> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let mut collections: Vec<Value> = settings
        .get("collections")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let before = collections.len();
    collections.retain(|c| c.get("id").and_then(|v| v.as_i64()) != Some(id));
    if collections.len() == before {
        return Err(AppError::not_found("collection not found"));
    }
    settings
        .set("collections", &serde_json::to_string(&collections)?)
        .ok();
    Ok(StatusCode::NO_CONTENT)
}

/// Les albums d'un dossier.
///
/// L'ordre par défaut est alphabétique par artiste (puis année, puis titre),
/// et non plus l'ordre d'ajout : personne ne pouvait remettre un dossier en
/// ordre, faute d'endpoint de réordonnancement (Lulu/JLuc, fil 1591, #2675).
/// `?sort=added` rend l'ordre historique, pour un dossier monté comme une
/// séquence d'écoute. Le tri est fait en Rust — voir `album_order`.
pub(super) async fn collection_albums(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(query): Query<CollectionAlbumsQuery>,
) -> impl IntoResponse {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let collections: Vec<Value> = settings
        .get("collections")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let found = collections
        .iter()
        .find(|c| c.get("id").and_then(|v| v.as_i64()) == Some(id));
    let Some(collection) = found else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let album_ids: Vec<i64> = collection
        .get("album_ids")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_i64()).collect())
        .unwrap_or_default();
    let album_repo = AlbumRepo::with_backend(state.backend.clone());
    let mut albums: Vec<tune_core::db::models::Album> = album_ids
        .iter()
        .filter_map(|&aid| album_repo.get(aid).ok().flatten())
        .collect();
    sort_albums(&mut albums, CollectionSort::parse(query.sort.as_deref()));
    let albums: Vec<Value> = albums.iter().map(|a| a.to_json()).collect();
    Json(json!(albums)).into_response()
}

pub(super) async fn add_album_to_collection(
    State(state): State<AppState>,
    Path(path): Path<CollectionAlbumPath>,
) -> Result<impl IntoResponse, AppError> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let mut collections: Vec<Value> = settings
        .get("collections")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let found = collections
        .iter_mut()
        .find(|c| c.get("id").and_then(|v| v.as_i64()) == Some(path.id));
    let Some(collection) = found else {
        return Err(AppError::not_found("collection not found"));
    };
    let album_ids = collection
        .get_mut("album_ids")
        .and_then(|v| v.as_array_mut());
    match album_ids {
        Some(arr) => {
            let already = arr.iter().any(|v| v.as_i64() == Some(path.album_id));
            if !already {
                arr.push(json!(path.album_id));
            }
        }
        None => {
            if let Some(obj) = collection.as_object_mut() {
                obj.insert("album_ids".into(), json!([path.album_id]));
            }
        }
    }
    settings
        .set("collections", &serde_json::to_string(&collections)?)
        .ok();
    Ok(Json(
        json!({"added": true, "collection_id": path.id, "album_id": path.album_id}),
    ))
}

pub(super) async fn remove_album_from_collection(
    State(state): State<AppState>,
    Path(path): Path<CollectionAlbumPath>,
) -> Result<impl IntoResponse, AppError> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let mut collections: Vec<Value> = settings
        .get("collections")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let found = collections
        .iter_mut()
        .find(|c| c.get("id").and_then(|v| v.as_i64()) == Some(path.id));
    let Some(collection) = found else {
        return Err(AppError::not_found("collection not found"));
    };
    if let Some(arr) = collection
        .get_mut("album_ids")
        .and_then(|v| v.as_array_mut())
    {
        arr.retain(|v| v.as_i64() != Some(path.album_id));
    }
    settings
        .set("collections", &serde_json::to_string(&collections)?)
        .ok();
    Ok(Json(
        json!({"removed": true, "collection_id": path.id, "album_id": path.album_id}),
    ))
}

#[cfg(test)]
mod tests_pochettes {
    use super::*;
    use tune_core::library::mosaique::cle_pochette;

    /// Construit la table `id -> (chemin, clé)` comme le fait la route.
    fn table(albums: &[(i64, &str, &str)]) -> std::collections::HashMap<i64, (String, String)> {
        albums
            .iter()
            .map(|(id, titre, chemin)| {
                (
                    *id,
                    ((*chemin).to_string(), cle_pochette(Some(titre), chemin)),
                )
            })
            .collect()
    }

    /// Un même disque ne remplit pas la mosaïque à lui seul.
    ///
    /// Données RELEVÉES sur le serveur de Bertrand le 02/09/2026 : le coffret
    /// Górecki existe en quatre lignes d'album, une par artiste crédité, plus
    /// sa réédition 24 bits — cinq chemins de pochette, une seule image.
    ///
    /// ⚠️ Les artistes DIFFÈRENT, et c'est tout le point : une clé
    /// « artiste + titre » — la première version — laissait passer les cinq.
    #[test]
    fn un_meme_disque_ne_prend_qu_une_case() {
        let t = table(&[
            (1, "A Nonesuch Retrospective", "C1"),
            (2, "A Nonesuch Retrospective", "C2"),
            (3, "a nonesuch retrospective", "C3"),
            (4, "A Nonesuch Retrospective", "C4"),
            (5, "A Nonesuch Retrospective (24bit)", "C5"),
            (6, "Vrai Deux", "D"),
            (7, "Vrai Trois", "E"),
        ]);
        let out = quatre_pochettes(&[1, 2, 3, 4, 5, 6, 7], &t);
        assert_eq!(out, vec!["C1", "D", "E"], "obtenu {out:?}");
    }

    /// Treize pianistes, un seul disque — le plus gros cas mesuré sur
    /// « Classique ». Les albums qui suivent doivent atteindre la mosaïque.
    #[test]
    fn treize_lignes_laissent_la_place_aux_suivants() {
        let mut albums: Vec<(i64, String, String)> = (1..=13)
            .map(|i| {
                (
                    i,
                    "Les indispensables du piano (96kHz/24bit)".to_string(),
                    format!("P{i}"),
                )
            })
            .collect();
        for (i, titre) in [(20i64, "Alpha"), (21, "Beta"), (22, "Gamma")] {
            albums.push((i, titre.to_string(), format!("X{i}")));
        }
        let refs: Vec<(i64, &str, &str)> = albums
            .iter()
            .map(|(i, t, c)| (*i, t.as_str(), c.as_str()))
            .collect();
        let ids: Vec<i64> = refs.iter().map(|(i, _, _)| *i).collect();
        let out = quatre_pochettes(&ids, &table(&refs));
        assert_eq!(out, vec!["P1", "X20", "X21", "X22"], "obtenu {out:?}");
    }

    /// La contre-épreuve : la clé doit encore SÉPARER ce qui est différent.
    /// Sans elle, « ne rendre qu'une pochette » passerait aussi le test
    /// ci-dessus.
    #[test]
    fn des_disques_differents_remplissent_les_quatre_cases() {
        let t = table(&[
            (1, "Way Out West", "A"),
            (2, "Come Away With Me (5.1 Remix)", "B"),
            (3, "Standards, Vol. 2", "C"),
            (4, "The Koln Concert (Live)", "D"),
            (5, "Somethin' Else", "E"),
        ]);
        let out = quatre_pochettes(&[1, 2, 3, 4, 5], &t);
        assert_eq!(
            out,
            vec!["A", "B", "C", "D"],
            "plafonne a quatre, dans l'ordre"
        );
    }

    /// Deux disques distincts partageant un chemin : une seule case, sinon la
    /// mosaïque montrerait deux fois la même image.
    #[test]
    fn un_chemin_partage_ne_compte_qu_une_fois() {
        let t = table(&[
            (1, "Compilation", "A"),
            (2, "Reedition", "A"),
            (3, "Autre", "B"),
        ]);
        assert_eq!(quatre_pochettes(&[1, 2, 3], &t), vec!["A", "B"]);
    }

    /// Un identifiant absent de la table — album sans pochette, ou supprimé —
    /// est sauté sans faire perdre de case.
    #[test]
    fn un_album_inconnu_ne_mange_pas_de_case() {
        let t = table(&[(1, "Un", "A"), (3, "Trois", "B")]);
        assert_eq!(quatre_pochettes(&[1, 2, 3], &t), vec!["A", "B"]);
    }
}
