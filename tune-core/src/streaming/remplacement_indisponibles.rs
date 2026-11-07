//! #8066 — remplacer, dans une playlist, un titre de service devenu
//! indisponible par sa nouvelle référence chez le même service.
//!
//! Demande du forum (fil 2691, 07/11/2026) : Qobuz, TIDAL ou Bandcamp retirent
//! parfois un album puis le republient sous un nouvel identifiant. Les
//! playlists personnelles, locales comme celles des services, gardent alors
//! des titres qui ne se jouent plus.
//!
//! ## Comment Tune sait qu'un titre est indisponible
//!
//! Une seule source de vérité, celle que le client web lit déjà pour griser
//! une ligne « Indisponible » (`pisteIndisponible`, `p.disponible === false`) :
//! le champ [`StreamTrack::disponible`]. Qobuz le remplit avec `streamable`,
//! TIDAL avec `allowStreaming` / `streamReady`. `None` veut dire « le service
//! ne dit rien » et ne fait PAS un titre indisponible : on ne remplace pas ce
//! qu'on ne sait pas cassé. Une erreur de lecture, une date de sortie à venir
//! ou un 404 du service ne sont pas lus ici — ce module n'invente pas une
//! seconde définition. Voir [`est_indisponible`].
//!
//! ## Les candidats et leur score
//!
//! Les candidats viennent d'une recherche sur LE MÊME service (« titre
//! artiste »). Un résultat n'est candidat que s'il remplit TOUS les critères
//! de l'issue :
//!
//! * même titre normalisé ([`track_matcher::normalize`] : casse, accents,
//!   mentions « (Remastered …) », « feat. ») ;
//! * même artiste normalisé (ou même artiste principal, avant la première
//!   virgule ou « & ») ;
//! * album proche : similarité des titres d'album normalisés au moins
//!   [`SIMILARITE_ALBUM_MIN`] quand les deux sont connus ;
//! * durée à ±2 s ([`ECART_DUREE_MAX_MS`]) quand les deux sont connues ;
//! * écoutable (pas `disponible == Some(false)`) et d'un autre identifiant.
//!
//! Le score (0 à 1) dit la confiance :
//!
//! | composante | poids |
//! |---|---|
//! | titre + artiste (obligatoires) | 0,5 |
//! | album : `0,3 × similarité` ; inconnu d'un côté : 0,15 | 0,3 |
//! | durée : de 0,2 (écart nul) à 0,1 (écart de 2 s) ; inconnue : 0 | 0,2 |
//!
//! Même ISRC des deux côtés : 1,0. ISRC connus des deux côtés mais
//! différents : plafonné à 0,85 — c'est sans doute un autre enregistrement
//! (remasterisation), l'utilisateur choisit.
//!
//! ## Le seuil « sûr » : [`SEUIL_SUR`] = 0,9
//!
//! Il faut, en plus du titre et de l'artiste, un album identique et une durée
//! à 2 s au plus, ou un album très proche (≥ 0,84) et une durée identique.
//! Un album inconnu ne passe jamais le seuil (0,85 au mieux), une durée
//! inconnue non plus (0,8 au mieux) : sans ces deux preuves, Tune propose et
//! l'utilisateur choisit. Le remplacement DIRECT n'a lieu que si UN SEUL
//! candidat passe le seuil ([`decider`]).

use serde::Serialize;

use crate::TuneError;
use crate::library::track_matcher::{normaliser_isrc, normalize, similarity};
use crate::streaming::traits::{StreamTrack, StreamingService};

/// Score à partir duquel un candidat est « sûr » (voir l'en-tête du module).
pub const SEUIL_SUR: f64 = 0.9;

/// Écart de durée maximal entre le titre disparu et un candidat : ±2 s.
pub const ECART_DUREE_MAX_MS: u64 = 2_000;

/// Similarité minimale de deux titres d'album pour les dire « proches ».
pub const SIMILARITE_ALBUM_MIN: f64 = 0.6;

/// Plafond d'un candidat dont l'ISRC contredit celui du titre disparu.
pub const PLAFOND_ISRC_DIFFERENT: f64 = 0.85;

/// Résultats de recherche examinés pour un titre.
pub const LIMITE_RECHERCHE: usize = 25;

/// Le titre est-il indisponible ? LA définition de Tune, celle du client web
/// (`pisteIndisponible`) : seul `Some(false)` vaut « indisponible ».
pub fn est_indisponible(piste: &StreamTrack) -> bool {
    piste.disponible == Some(false)
}

/// Ce que l'on sait du titre disparu.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TitreCherche {
    pub source_id: String,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    /// `None` ou `Some(0)` : durée inconnue.
    pub duration_ms: Option<u64>,
    pub isrc: Option<String>,
}

impl From<&StreamTrack> for TitreCherche {
    fn from(p: &StreamTrack) -> Self {
        Self {
            source_id: p.id.clone(),
            title: p.title.clone(),
            artist: p.artist.clone(),
            album: p.album.clone(),
            duration_ms: Some(p.duration_ms).filter(|d| *d > 0),
            isrc: p.isrc.clone(),
        }
    }
}

/// Un titre de remplacement proposé, avec son score de confiance.
///
/// Sérialisé À PLAT : les champs de la piste (`source_id`, `title`,
/// `artist_name`, `album_title`, `duration_ms`, `cover_path`…) puis `score`
/// et `sure`. Le client affiche une ligne de candidat comme une piste de
/// service.
#[derive(Debug, Clone, Serialize)]
pub struct Candidat {
    #[serde(flatten)]
    pub piste: StreamTrack,
    pub score: f64,
    /// `score >= SEUIL_SUR`.
    pub sure: bool,
}

impl Candidat {
    /// Le candidat tel que les routes le rendent, avec son service
    /// (`source`), pour que le client le traite comme une piste de service.
    pub fn en_json(&self, service: &str) -> serde_json::Value {
        let mut v = serde_json::to_value(self).unwrap_or(serde_json::Value::Null);
        if let Some(o) = v.as_object_mut() {
            o.insert("source".into(), serde_json::Value::from(service));
        }
        v
    }
}

/// Ce que Tune fait des candidats d'un titre.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// Un seul candidat sûr : Tune peut remplacer sans demander.
    Direct,
    /// Des candidats, mais pas un seul sûr : l'utilisateur choisit.
    Choix,
    /// Aucun candidat.
    Aucun,
}

fn non_vide(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

/// L'artiste principal d'un nom déjà normalisé : avant la première virgule,
/// « & », « / » ou « ; ».
fn artiste_principal(normalise: &str) -> &str {
    normalise
        .split([',', '&', '/', ';'])
        .next()
        .unwrap_or(normalise)
        .trim()
}

fn meme_artiste(a: &str, b: &str) -> bool {
    let (a, b) = (normalize(a), normalize(b));
    if a.is_empty() || b.is_empty() {
        return false;
    }
    a == b || artiste_principal(&a) == artiste_principal(&b)
}

/// Le score de `candidat` pour `cible`, ou `None` s'il manque un critère
/// obligatoire (voir l'en-tête du module).
pub fn score(cible: &TitreCherche, candidat: &StreamTrack) -> Option<f64> {
    if candidat.id == cible.source_id || est_indisponible(candidat) {
        return None;
    }
    let titre = normalize(&cible.title);
    if titre.is_empty() || titre != normalize(&candidat.title) {
        return None;
    }
    if !meme_artiste(&cible.artist, &candidat.artist) {
        return None;
    }
    let mut total = 0.5;

    match (
        non_vide(cible.album.as_deref()),
        non_vide(candidat.album.as_deref()),
    ) {
        (Some(a), Some(b)) => {
            let sim = similarity(&normalize(a), &normalize(b));
            if sim < SIMILARITE_ALBUM_MIN {
                return None;
            }
            total += 0.3 * sim;
        }
        _ => total += 0.15,
    }

    let duree_cible = cible.duration_ms.filter(|d| *d > 0);
    let duree_cand = Some(candidat.duration_ms).filter(|d| *d > 0);
    if let (Some(a), Some(b)) = (duree_cible, duree_cand) {
        let ecart = a.abs_diff(b);
        if ecart > ECART_DUREE_MAX_MS {
            return None;
        }
        total += 0.2 * (1.0 - ecart as f64 / (2.0 * ECART_DUREE_MAX_MS as f64));
    }

    let isrc_a = non_vide(cible.isrc.as_deref()).map(normaliser_isrc);
    let isrc_b = non_vide(candidat.isrc.as_deref()).map(normaliser_isrc);
    if let (Some(a), Some(b)) = (isrc_a, isrc_b) {
        total = if a == b {
            1.0
        } else {
            total.min(PLAFOND_ISRC_DIFFERENT)
        };
    }

    Some((total * 1000.0).round() / 1000.0)
}

/// Les candidats parmi `pistes`, du plus sûr au moins sûr, sans doublon
/// d'identifiant.
pub fn classer(cible: &TitreCherche, pistes: &[StreamTrack]) -> Vec<Candidat> {
    let mut vus = std::collections::HashSet::new();
    let mut candidats: Vec<Candidat> = pistes
        .iter()
        .filter(|p| vus.insert(p.id.clone()))
        .filter_map(|p| {
            score(cible, p).map(|s| Candidat {
                piste: p.clone(),
                score: s,
                sure: s >= SEUIL_SUR,
            })
        })
        .collect();
    candidats.sort_by(|a, b| b.score.total_cmp(&a.score));
    candidats
}

/// Direct s'il y a UN SEUL candidat sûr ; choix s'il y a des candidats ;
/// aucun sinon.
pub fn decider(candidats: &[Candidat]) -> Decision {
    match candidats.iter().filter(|c| c.sure).count() {
        _ if candidats.is_empty() => Decision::Aucun,
        1 => Decision::Direct,
        _ => Decision::Choix,
    }
}

/// Le candidat à prendre sans demander, s'il y en a un.
pub fn candidat_direct(candidats: &[Candidat]) -> Option<&Candidat> {
    (decider(candidats) == Decision::Direct)
        .then(|| candidats.iter().find(|c| c.sure))
        .flatten()
}

/// Cherche les candidats de `cible` sur `service` (« titre artiste »).
pub async fn chercher_candidats(
    service: &dyn StreamingService,
    cible: &TitreCherche,
) -> Result<Vec<Candidat>, TuneError> {
    let requete = format!("{} {}", cible.title.trim(), cible.artist.trim());
    let resultats = service.search(requete.trim(), LIMITE_RECHERCHE).await?;
    Ok(classer(cible, &resultats.tracks))
}

/// Un faux service pour les bancs d'essai des routes (#8066).
#[cfg(any(test, feature = "services-factices"))]
#[path = "remplacement_indisponibles_factice.rs"]
pub mod factice;

#[cfg(test)]
#[path = "remplacement_indisponibles_tests.rs"]
mod tests;
