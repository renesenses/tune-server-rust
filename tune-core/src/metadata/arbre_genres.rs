//! Arbre des genres tiré de données libres (#6001).
//!
//! # La décision
//!
//! Bertrand, 10/10/2026 : « L'arbre des genres vient de MusicBrainz (CC0) ».
//!
//! # Ce que MusicBrainz donne, et ce qu'il ne donne pas
//!
//! Relevé le 20/10/2026 contre `musicbrainz.org/ws/2` :
//!
//! * la LISTE des genres : `genre/all?limit=100&offset=…` rend `genre-count`
//!   (2 216 ce jour-là) et, par genre, `name` et `id` ;
//! * PAS leur hiérarchie : `genre/{id}?inc=genre-rels` (ou `inc=rels`)
//!   rend le genre sans aucune relation, alors que la page web du même genre
//!   affiche « subgenre of ». L'API ne publie pas les relations entre genres,
//!   et la recherche `genre?query=` répond « This hasn't been implemented yet ».
//!
//! La hiérarchie vient donc de **Wikidata**, aussi sous CC0 : chaque genre
//! Wikidata porte son identifiant de genre MusicBrainz (P8052) et ses genres
//! parents (P279, « sous-classe de »). Une seule requête SPARQL rend, pour un
//! lot d'identifiants MusicBrainz, les arêtes parent → enfant **entre genres
//! MusicBrainz** : l'arbre reste exprimé dans le vocabulaire de MusicBrainz.
//!
//! # Pas de téléchargement massif
//!
//! Rien au démarrage. La liste est lue à la première demande (une vingtaine de
//! pages, au rythme du limiteur MusicBrainz partagé), puis gardée
//! [`VALIDITE_INDEX_JOURS`] jours. La hiérarchie n'est demandée que pour les
//! genres de la bibliothèque, par lots de [`LOT_SPARQL`], et gardée
//! [`VALIDITE_HIERARCHIE_JOURS`] jours par genre.
//!
//! # Rien n'est imposé
//!
//! L'arbre de l'auditeur (`genre_tree`, réglé à la main) n'est pas touché :
//! cet arbre-ci est une proposition en lecture seule.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::genre_key;
pub use super::musicbrainz_release::SansReponse;
use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

/// Taille de page de `genre/all` (le plafond de MusicBrainz).
pub const PAGE_GENRES: u32 = 100;
/// Garde-fou : au-delà, la liste est tronquée plutôt que parcourue sans fin.
pub const PAGES_MAX: u32 = 60;
/// Identifiants MusicBrainz par requête SPARQL.
pub const LOT_SPARQL: usize = 100;
pub const VALIDITE_INDEX_JOURS: i64 = 30;
pub const VALIDITE_HIERARCHIE_JOURS: i64 = 30;

const CLE_INDEX: &str = "cache:mb:genres:index";
const CLE_HIERARCHIE: &str = "cache:wikidata:genres:hierarchie";
const SPARQL: &str = "https://query.wikidata.org/sparql";

/// Un genre de MusicBrainz.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenreMb {
    pub nom: String,
    pub mbid: String,
}

// ── Liste des genres (MusicBrainz) ──────────────────────────────────────────

/// Les genres d'une page `genre/all`, et le total annoncé.
pub fn genres_de_la_page(page: &Value) -> (Vec<GenreMb>, Option<u64>) {
    let genres = page["genres"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|g| {
                    let nom = g["name"].as_str()?.trim();
                    let mbid = g["id"].as_str()?.trim();
                    (!nom.is_empty() && super::liens_externes::mbid_plausible(mbid)).then(|| {
                        GenreMb {
                            nom: nom.to_string(),
                            mbid: mbid.to_string(),
                        }
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    (genres, page["genre-count"].as_u64())
}

/// La liste complète, page par page, par le transport injecté
/// `lire_page(offset)`. `Err` dès qu'une page manque : une liste partielle
/// ferait passer des genres connus pour inconnus.
pub async fn lire_index_par<F, Fut>(lire_page: &F) -> Result<Vec<GenreMb>, SansReponse>
where
    F: Fn(u32) -> Fut,
    Fut: Future<Output = Result<Value, SansReponse>>,
{
    let mut tous: Vec<GenreMb> = Vec::new();
    for n in 0..PAGES_MAX {
        let page = lire_page(n * PAGE_GENRES).await?;
        let (genres, total) = genres_de_la_page(&page);
        let vide = genres.is_empty();
        tous.extend(genres);
        if vide || total.is_some_and(|t| tous.len() as u64 >= t) {
            break;
        }
    }
    Ok(tous)
}

/// Transport de production d'une page de `genre/all`, sous le limiteur
/// MusicBrainz partagé.
pub async fn lire_page_de_genres(offset: u32) -> Result<Value, SansReponse> {
    super::musicbrainz_release::rate_limit_delay().await;
    super::musicbrainz_release::parcourir_sur_musicbrainz(
        "genre/all",
        &[
            ("limit", PAGE_GENRES.to_string()),
            ("offset", offset.to_string()),
        ],
    )
    .await
    .map_err(|_| SansReponse)
}

// ── Hiérarchie (Wikidata) ───────────────────────────────────────────────────

/// La requête SPARQL d'un lot : pour chaque identifiant, ses parents ET ses
/// enfants qui sont eux-mêmes des genres MusicBrainz. Les identifiants sont
/// vérifiés ([`super::liens_externes::mbid_plausible`]) avant d'entrer dans la
/// requête : rien d'autre que de l'hexadécimal et des tirets n'y est collé.
pub fn requete_sparql(mbids: &[String]) -> String {
    let valeurs: Vec<String> = mbids
        .iter()
        .filter(|m| super::liens_externes::mbid_plausible(m))
        .map(|m| format!("\"{m}\""))
        .collect();
    format!(
        "SELECT ?enfant ?parent WHERE {{ VALUES ?m {{ {} }} \
         {{ ?g wdt:P8052 ?m . ?g wdt:P279 ?p . ?p wdt:P8052 ?parent . BIND(?m AS ?enfant) }} \
         UNION \
         {{ ?g wdt:P8052 ?m . ?c wdt:P279 ?g . ?c wdt:P8052 ?enfant . BIND(?m AS ?parent) }} }}",
        valeurs.join(" ")
    )
}

/// L'adresse de la requête, `format=json`.
pub fn url_sparql(mbids: &[String]) -> String {
    reqwest::Url::parse_with_params(
        SPARQL,
        &[
            ("query", requete_sparql(mbids).as_str()),
            ("format", "json"),
        ],
    )
    .map(|u| u.to_string())
    .unwrap_or_default()
}

/// Les arêtes `(enfant, parent)` d'une réponse SPARQL JSON.
pub fn aretes_de_la_reponse(reponse: &Value) -> Vec<(String, String)> {
    reponse["results"]["bindings"]
        .as_array()
        .map(|b| {
            b.iter()
                .filter_map(|l| {
                    let e = l["enfant"]["value"].as_str()?.trim();
                    let p = l["parent"]["value"].as_str()?.trim();
                    (super::liens_externes::mbid_plausible(e)
                        && super::liens_externes::mbid_plausible(p)
                        && e != p)
                        .then(|| (e.to_string(), p.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parents et enfants connus d'un genre MusicBrainz.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Voisins {
    pub parents: BTreeSet<String>,
    pub enfants: BTreeSet<String>,
    /// Secondes depuis l'époque Unix.
    pub lu_le: i64,
}

/// Interroge Wikidata pour `mbids`, par lots, et rend les voisins de CHAQUE
/// identifiant demandé (vide s'il n'en a pas). `Err` si un lot échoue : rien
/// de ce lot n'est conclu.
pub async fn lire_hierarchie_par<F, Fut>(
    mbids: &[String],
    lire: &F,
    maintenant: i64,
) -> Result<HashMap<String, Voisins>, SansReponse>
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = Option<Value>>,
{
    let mut out: HashMap<String, Voisins> = HashMap::new();
    for lot in mbids.chunks(LOT_SPARQL) {
        let reponse = lire(url_sparql(lot)).await.ok_or(SansReponse)?;
        if reponse["results"]["bindings"].as_array().is_none() {
            return Err(SansReponse);
        }
        for m in lot {
            out.entry(m.clone()).or_insert_with(|| Voisins {
                lu_le: maintenant,
                ..Voisins::default()
            });
        }
        for (enfant, parent) in aretes_de_la_reponse(&reponse) {
            if let Some(v) = out.get_mut(&enfant) {
                v.parents.insert(parent.clone());
            }
            if let Some(v) = out.get_mut(&parent) {
                v.enfants.insert(enfant);
            }
        }
    }
    Ok(out)
}

// ── Assemblage ──────────────────────────────────────────────────────────────

/// Un genre de la bibliothèque, rapproché d'un genre MusicBrainz.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Noeud {
    /// Le genre tel qu'il est écrit dans la bibliothèque.
    pub genre: String,
    pub mbid: String,
    /// Le nom MusicBrainz (minuscules).
    pub nom_musicbrainz: String,
    pub parents: Vec<String>,
    pub sous_genres: Vec<String>,
}

/// Rapproche les genres de la bibliothèque des genres MusicBrainz, par
/// [`genre_key`] (casse et tirets ignorés). Rend les rapprochés et les autres.
pub fn rapprocher(locaux: &[String], index: &[GenreMb]) -> (Vec<(String, GenreMb)>, Vec<String>) {
    let par_cle: HashMap<String, &GenreMb> = index.iter().map(|g| (genre_key(&g.nom), g)).collect();
    let mut trouves = Vec::new();
    let mut autres = Vec::new();
    for g in locaux {
        match par_cle.get(&genre_key(g)) {
            Some(mb) => trouves.push((g.clone(), (*mb).clone())),
            None => autres.push(g.clone()),
        }
    }
    (trouves, autres)
}

/// Les nœuds, noms MusicBrainz à la place des identifiants. Un voisin absent de
/// la liste MusicBrainz (genre retiré depuis) est omis.
pub fn assembler(
    trouves: &[(String, GenreMb)],
    index: &[GenreMb],
    voisins: &HashMap<String, Voisins>,
) -> Vec<Noeud> {
    let noms: HashMap<&str, &str> = index
        .iter()
        .map(|g| (g.mbid.as_str(), g.nom.as_str()))
        .collect();
    let nommer = |ids: &BTreeSet<String>| -> Vec<String> {
        let mut v: Vec<String> = ids
            .iter()
            .filter_map(|id| noms.get(id.as_str()).map(|n| n.to_string()))
            .collect();
        v.sort();
        v
    };
    trouves
        .iter()
        .map(|(genre, mb)| {
            let v = voisins.get(&mb.mbid).cloned().unwrap_or_default();
            Noeud {
                genre: genre.clone(),
                mbid: mb.mbid.clone(),
                nom_musicbrainz: mb.nom.clone(),
                parents: nommer(&v.parents),
                sous_genres: nommer(&v.enfants),
            }
        })
        .collect()
}

// ── Mémoire (table `settings`) ──────────────────────────────────────────────

fn lire_reglage(backend: &Arc<dyn DbBackend>, cle: &str) -> Option<Value> {
    SettingsRepo::with_backend(backend.clone())
        .get(cle)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
}

fn ecrire_reglage(backend: &Arc<dyn DbBackend>, cle: &str, v: &Value) {
    let _ = SettingsRepo::with_backend(backend.clone()).set(cle, &v.to_string());
}

/// La liste gardée, si elle a moins de [`VALIDITE_INDEX_JOURS`] jours.
pub fn index_garde(backend: &Arc<dyn DbBackend>, maintenant: i64) -> Option<Vec<GenreMb>> {
    let v = lire_reglage(backend, CLE_INDEX)?;
    let lu_le = v["lu_le"].as_i64()?;
    if maintenant - lu_le > VALIDITE_INDEX_JOURS * 86_400 {
        return None;
    }
    serde_json::from_value(v["genres"].clone()).ok()
}

pub fn garder_index(backend: &Arc<dyn DbBackend>, genres: &[GenreMb], maintenant: i64) {
    ecrire_reglage(
        backend,
        CLE_INDEX,
        &json!({ "lu_le": maintenant, "genres": genres }),
    );
}

fn hierarchie_gardee(backend: &Arc<dyn DbBackend>) -> BTreeMap<String, Voisins> {
    lire_reglage(backend, CLE_HIERARCHIE)
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

/// Ajoute des voisins lus à ceux qui sont gardés.
pub fn garder_voisins(backend: &Arc<dyn DbBackend>, lus: HashMap<String, Voisins>) {
    let mut gardee = hierarchie_gardee(backend);
    gardee.extend(lus);
    if let Ok(v) = serde_json::to_value(&gardee) {
        ecrire_reglage(backend, CLE_HIERARCHIE, &v);
    }
}

/// Les voisins des genres `trouves`, en ne demandant à Wikidata que ceux qui
/// manquent ou ont expiré. Une panne de Wikidata rend ce qui est gardé, et le
/// dit (`false`).
pub async fn voisins_par<F, Fut>(
    backend: &Arc<dyn DbBackend>,
    trouves: &[(String, GenreMb)],
    lire: &F,
    maintenant: i64,
) -> (HashMap<String, Voisins>, bool)
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = Option<Value>>,
{
    let gardee = hierarchie_gardee(backend);
    let a_lire: Vec<String> = trouves
        .iter()
        .map(|(_, mb)| mb.mbid.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|m| {
            gardee
                .get(m)
                .is_none_or(|v| maintenant - v.lu_le > VALIDITE_HIERARCHIE_JOURS * 86_400)
        })
        .collect();
    let mut complet = true;
    if !a_lire.is_empty() {
        match lire_hierarchie_par(&a_lire, lire, maintenant).await {
            Ok(lus) => garder_voisins(backend, lus),
            Err(SansReponse) => complet = false,
        }
    }
    (hierarchie_gardee(backend).into_iter().collect(), complet)
}

#[cfg(test)]
#[path = "arbre_genres_tests.rs"]
mod tests;
