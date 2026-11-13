//! web#3001 (go de Bertrand du 13/11, rc12) — la pochette d'un album de
//! contact, prise dans MES sources quand sa vignette manque.
//!
//! Suite de web#2985 : 11 des 50 premiers albums de la bibliothèque de Tades
//! n'ont pas de vignette. L'écran envoie, PAR PAGE affichée et pour ces albums
//! seulement, leurs titres et artistes ; le serveur cherche le même album :
//!
//! 1. dans ma bibliothèque (recherche d'albums de la base, la même que
//!    `/library/albums?search=`), un album qui a une pochette ;
//! 2. sinon chez Qobuz, puis TIDAL, s'ils sont connectés
//!    ([`Resolveur::service_utilisable`], la règle unique) — leur `search`,
//!    par le client HTTP du service lui-même : aucun client neuf.
//!
//! Le verdict est celui de « Chercher dans mes sources » (#7224,
//! `meilleureCorrespondance` côté web) : MÊME titre une fois normalisé (les
//! mentions d'édition en fin de titre ne comptent pas) ET même artiste (tous
//! les artistes du contact chez le candidat). Rien d'approché : mieux vaut
//! l'icône générique qu'une autre pochette.
//!
//! Lecture seule, rien n'est écrit (ni en base, ni chez un service). Les
//! résultats restent en mémoire un temps borné ([`CacheBorne`], celui des
//! playlists de cercle, #7022) : la réouverture d'une page ne recherche rien.
//!
//! `POST /my-sources/album-covers`
//! `{ "albums": [{ "title": "…", "artist_name": "…" }, …] }` (au plus
//! [`ALBUMS_PAR_DEMANDE_MAX`]) → `{ "covers": [ { "source", "cover_path",
//! "album_id"? } | null, … ] }`, dans l'ordre de la demande. `cover_path` est
//! une adresse déjà servable (`/api/v1/library/artwork/{condensat}` pour un
//! album à moi, l'adresse du service sinon) — jamais un chemin de fichier.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use futures_util::stream::{self, StreamExt};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tracing::debug;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::backend::DbBackend;
use tune_core::library::track_matcher::normalize;
use tune_core::streaming::ServiceRegistry;

use crate::resolution::Resolveur;
use crate::resolution_groupee::{CacheBorne, PARALLELISME_MAX};

/// Une page de l'écran fait 50 albums : un peu de marge, pas davantage.
pub const ALBUMS_PAR_DEMANDE_MAX: usize = 60;
/// Les services interrogés, dans cet ordre (issue web#3001).
pub const SERVICES_DE_POCHETTE: [&str; 2] = ["qobuz", "tidal"];
/// Résultats lus par recherche : assez pour retrouver un album précis.
const LIMITE_RECHERCHE: usize = 12;
/// Une pochette trouvée se garde une heure ; un introuvable, dix minutes.
pub const DUREE_TROUVEE: Duration = Duration::from_secs(60 * 60);
pub const DUREE_INTROUVABLE: Duration = Duration::from_secs(10 * 60);
const ENTREES_MAX: usize = 4096;

/// La pochette retrouvée dans mes sources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PochetteTrouvee {
    /// `local`, ou le service (`qobuz`, `tidal`).
    pub source: String,
    /// Une adresse servable telle quelle par le client web (`artworkUrl`).
    pub cover_path: String,
    /// L'album de MA bibliothèque, pour une pochette locale.
    pub album_id: Option<i64>,
}

impl PochetteTrouvee {
    pub fn en_json(&self) -> Value {
        let mut v = json!({ "source": self.source, "cover_path": self.cover_path });
        if let Some(id) = self.album_id {
            v["album_id"] = json!(id);
        }
        v
    }
}

/// Clé du cache : titre et artiste normalisés, et les services connectés à
/// cet instant (une connexion change la réponse).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Cle {
    titre: String,
    artiste: String,
    services: Vec<String>,
}

// Le verdict -----------------------------------------------------------------

/// Minuscules, sans accents ni ponctuation, « & » lu « and », espaces simples.
pub fn cle_de_texte(s: &str) -> String {
    let plie = normalize(&s.replace('&', " and "));
    plie.split(|c: char| !c.is_alphanumeric())
        .filter(|m| !m.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Les mentions d'ÉDITION : une réédition reste le même disque. Pas « live »
/// ni « remix » (sauf ce que `normalize` retire déjà) : d'autres disques.
const EDITION: [&str; 10] = [
    "remaster",
    "deluxe",
    "edition",
    "expanded",
    "anniversary",
    "bonus",
    "explicit",
    "mono",
    "stereo",
    "hi-res",
];

/// Le titre sans ses mentions d'édition finales : « (Remastered 2011) »,
/// « [Deluxe] », « - 2011 Remaster ».
fn titre_nu(titre: &str) -> String {
    let mut t = titre.trim().to_lowercase();
    loop {
        let debut = if t.ends_with(')') {
            t.rfind('(')
        } else if t.ends_with(']') {
            t.rfind('[')
        } else {
            t.rfind(" - ")
        };
        match debut {
            Some(i) if EDITION.iter().any(|m| t[i..].contains(m)) => {
                t.truncate(i);
                t = t.trim_end().to_string();
            }
            _ => break,
        }
    }
    cle_de_texte(&t)
}

pub fn meme_titre(voulu: &str, candidat: &str) -> bool {
    let v = cle_de_texte(voulu);
    if v.is_empty() {
        return false;
    }
    if v == cle_de_texte(candidat) {
        return true;
    }
    let nu = titre_nu(voulu);
    !nu.is_empty() && nu == titre_nu(candidat)
}

/// Les artistes d'un libellé : « A feat. B », « A & B », « A, B »…
fn artistes_de(s: &str) -> Vec<String> {
    let bas = format!(" {} ", s.to_lowercase());
    let mut unifie = bas;
    for sep in [
        " featuring ",
        " feat. ",
        " feat ",
        " ft. ",
        " ft ",
        " and ",
        " & ",
        ",",
        ";",
        "/",
    ] {
        unifie = unifie.replace(sep, "|");
    }
    unifie
        .split('|')
        .map(cle_de_texte)
        .filter(|a| !a.is_empty())
        .collect()
}

/// Même artiste : libellés identiques une fois normalisés, ou TOUS les
/// artistes du contact chez le candidat (« Adele » ≈ « Adele feat. X »,
/// « Simon & Garfunkel » ≠ « Simon »). Sans artiste d'un côté : non.
pub fn meme_artiste(voulu: &str, candidat: &str) -> bool {
    let (v, c) = (cle_de_texte(voulu), cle_de_texte(candidat));
    if v.is_empty() || c.is_empty() {
        return false;
    }
    if v == c {
        return true;
    }
    let chez = artistes_de(candidat);
    artistes_de(voulu).iter().all(|a| chez.contains(a))
}

/// Les termes d'une recherche : le titre sans mentions d'édition, puis l'artiste.
fn termes(titre: &str, artiste: &str) -> String {
    format!("{} {}", titre_nu(titre), cle_de_texte(artiste))
}

pub fn meme_album(
    titre: &str,
    artiste: &str,
    titre_candidat: &str,
    artiste_candidat: &str,
) -> bool {
    meme_titre(titre, titre_candidat) && meme_artiste(artiste, artiste_candidat)
}

/// L'adresse servable de la pochette d'un album de MA bibliothèque, comme
/// `GET /library/albums/{id}/artwork` la calcule : une adresse distante reste
/// telle quelle ; un condensat ou un chemin devient
/// `/api/v1/library/artwork/{condensat}`. Jamais le chemin lui-même.
pub fn adresse_de_pochette_locale(cover_path: &str) -> Option<String> {
    let c = cover_path.trim();
    if c.is_empty() {
        return None;
    }
    if c.starts_with("http://") || c.starts_with("https://") || c.starts_with("/api/") {
        return Some(c.to_string());
    }
    let condensat = if (c.len() == 32 || c.len() == 64) && c.chars().all(|x| x.is_ascii_hexdigit())
    {
        c.to_string()
    } else {
        tune_core::library::artwork::artwork_hash(c)
    };
    Some(format!("/api/v1/library/artwork/{condensat}"))
}

// La recherche ---------------------------------------------------------------

pub struct PochettesDeMesSources {
    resolveur: Resolveur,
    cache: CacheBorne<Cle, PochetteTrouvee>,
}

impl PochettesDeMesSources {
    pub fn new(backend: Arc<dyn DbBackend>, services: Arc<Mutex<ServiceRegistry>>) -> Self {
        Self {
            resolveur: Resolveur::new(backend, services),
            cache: CacheBorne::avec(DUREE_TROUVEE, DUREE_INTROUVABLE, ENTREES_MAX),
        }
    }

    /// Un album de ma bibliothèque, de même titre et même artiste, qui a une pochette.
    fn en_bibliotheque(&self, titre: &str, artiste: &str) -> Option<PochetteTrouvee> {
        let repo = AlbumRepo::with_backend(self.resolveur.backend().clone());
        let albums = repo
            .search(&termes(titre, artiste), LIMITE_RECHERCHE as i64)
            .map_err(|e| debug!(error = %e, "circle_pochette_mes_sources_base"))
            .ok()?;
        albums.into_iter().find_map(|a| {
            if !meme_album(
                titre,
                artiste,
                &a.title,
                a.artist_name.as_deref().unwrap_or(""),
            ) {
                return None;
            }
            Some(PochetteTrouvee {
                source: "local".into(),
                cover_path: adresse_de_pochette_locale(a.cover_path.as_deref()?)?,
                album_id: a.id,
            })
        })
    }

    /// Chez chaque service connecté, dans l'ordre de [`SERVICES_DE_POCHETTE`].
    async fn chez_les_services(
        &self,
        titre: &str,
        artiste: &str,
        services: &[String],
    ) -> Option<PochetteTrouvee> {
        for nom in services {
            let Some(service) = self.resolveur.service_utilisable(nom).await else {
                continue;
            };
            let lu = service
                .read()
                .await
                .search(&termes(titre, artiste), LIMITE_RECHERCHE)
                .await;
            let resultats = match lu {
                Ok(r) => r,
                Err(e) => {
                    debug!(service = %nom, error = %e, "circle_pochette_mes_sources_service");
                    continue;
                }
            };
            let trouvee = resultats.albums.into_iter().find_map(|a| {
                let cover = a.cover_path.filter(|c| !c.trim().is_empty())?;
                meme_album(titre, artiste, &a.title, &a.artist).then(|| PochetteTrouvee {
                    source: nom.clone(),
                    cover_path: cover,
                    album_id: None,
                })
            });
            if trouvee.is_some() {
                return trouvee;
            }
        }
        None
    }

    async fn chercher_un(
        &self,
        titre: &str,
        artiste: &str,
        services: &[String],
    ) -> Option<PochetteTrouvee> {
        if cle_de_texte(titre).is_empty() || cle_de_texte(artiste).is_empty() {
            return None;
        }
        let cle = Cle {
            titre: cle_de_texte(titre),
            artiste: cle_de_texte(artiste),
            services: services.to_vec(),
        };
        if let Some(connue) = self.cache.lire(&cle) {
            return connue;
        }
        let trouvee = match self.en_bibliotheque(titre, artiste) {
            Some(t) => Some(t),
            None => self.chez_les_services(titre, artiste, services).await,
        };
        self.cache.garder(cle, trouvee.clone());
        trouvee
    }

    /// Les pochettes de ces albums (titre, artiste), DANS L'ORDRE,
    /// [`PARALLELISME_MAX`] à la fois. Au plus [`ALBUMS_PAR_DEMANDE_MAX`].
    pub async fn chercher(&self, albums: Vec<(String, String)>) -> Vec<Option<PochetteTrouvee>> {
        let mut services = Vec::new();
        for nom in SERVICES_DE_POCHETTE {
            if self.resolveur.service_utilisable(nom).await.is_some() {
                services.push(nom.to_string());
            }
        }
        let recherches: Vec<_> = albums
            .into_iter()
            .take(ALBUMS_PAR_DEMANDE_MAX)
            .map(|(titre, artiste)| {
                let services = services.clone();
                async move { self.chercher_un(&titre, &artiste, &services).await }
            })
            .collect();
        stream::iter(recherches)
            .buffered(PARALLELISME_MAX)
            .collect()
            .await
    }
}

// La route -------------------------------------------------------------------

pub fn router(etat: Arc<PochettesDeMesSources>) -> Router<()> {
    Router::new()
        .route("/my-sources/album-covers", post(pochettes))
        .with_state(etat)
}

fn texte(v: &Value, cle: &str) -> String {
    v.get(cle).and_then(Value::as_str).unwrap_or("").to_string()
}

async fn pochettes(State(etat): State<Arc<PochettesDeMesSources>>, corps: Bytes) -> Json<Value> {
    let corps = serde_json::from_slice::<Value>(&corps).unwrap_or(Value::Null);
    let albums: Vec<(String, String)> = corps
        .get("albums")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(ALBUMS_PAR_DEMANDE_MAX)
        .map(|a| (texte(a, "title"), texte(a, "artist_name")))
        .collect();
    let trouvees = etat.chercher(albums).await;
    Json(json!({
        "covers": trouvees
            .iter()
            .map(|t| t.as_ref().map_or(Value::Null, PochetteTrouvee::en_json))
            .collect::<Vec<_>>(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meme_titre_ignore_casse_accents_et_mentions_d_edition() {
        assert!(meme_titre("Électrique", "electrique"));
        assert!(meme_titre("Abbey Road", "Abbey Road (Remastered 2019)"));
        assert!(meme_titre("Kind of Blue", "Kind of Blue [Deluxe Edition]"));
        assert!(meme_titre("Rumours", "Rumours - 2004 Remaster"));
        assert!(meme_titre(
            "Simon & Garfunkel's Hits",
            "Simon and Garfunkel's Hits"
        ));
        assert!(!meme_titre("Abbey Road", "Abbey Road Revisited"));
        assert!(!meme_titre("", "Abbey Road"));
    }

    #[test]
    fn meme_artiste_exige_tous_les_artistes_du_contact() {
        assert!(meme_artiste("Joël Grare", "Joel Grare"));
        assert!(meme_artiste("Adele", "Adele feat. X"));
        assert!(!meme_artiste("Simon & Garfunkel", "Simon"));
        assert!(!meme_artiste("", "Adele"));
        assert!(!meme_artiste("Adele", ""));
    }

    #[test]
    fn une_pochette_locale_n_expose_jamais_un_chemin() {
        let a = adresse_de_pochette_locale("/Users/secret/Music/cover.jpg").unwrap();
        assert!(a.starts_with("/api/v1/library/artwork/"), "{a}");
        assert!(!a.contains("secret"));
        let h = "0123456789abcdef0123456789abcdef";
        assert_eq!(
            adresse_de_pochette_locale(h).unwrap(),
            format!("/api/v1/library/artwork/{h}")
        );
        assert_eq!(
            adresse_de_pochette_locale("https://static.qobuz.com/x.jpg").unwrap(),
            "https://static.qobuz.com/x.jpg"
        );
        assert_eq!(adresse_de_pochette_locale("  "), None);
    }
}
