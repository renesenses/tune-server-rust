//! #8129 — les biographies des fiches Qobuz (artiste et album), cliquables
//! comme celles de la bibliothèque (#7910).
//!
//! Qobuz livre sa bio (`biography.content`) et sa présentation d'album
//! (`description`) en HTML, avec des liens. Ce module garde ce HTML le temps
//! d'en tirer des [`BioSegment`] :
//! - un lien vers un artiste ou un album Qobuz (`open.qobuz.com/artist/…`,
//!   `play.qobuz.com/album/…`, `www.qobuz.com/fr-fr/interpreter/<nom>/<id>`,
//!   `www.qobuz.com/fr-fr/album/<titre>/<id>`, ou le même chemin relatif)
//!   devient un [`LienDeService`] : Tune ouvre sa propre fiche Qobuz ;
//! - un autre lien n'est gardé qu'en `https` sur un domaine de source
//!   ([`bio_segments::url_autorisee`]) ;
//! - les noms connus de la bibliothèque deviennent des liens internes, par
//!   l'index de #7910 ; l'artiste (ou l'album) de la page ne l'est pas.
//!
//! Le texte découpé est celui de
//! [`super::presentation_album::texte_de_presentation`] : aucun balisage ne
//! sort vers le client.

use crate::metadata::bio_segments::{
    self, BioSegment, GenreDeLien, IndexDesNoms, LienDeService, LienExterne, LienInterne,
};

use super::presentation_album::texte_de_presentation;

/// Le nom du service dans les liens produits (celui des routes
/// `/streaming/{service}/…`).
pub const SERVICE: &str = "qobuz";

/// La page Qobuz qu'un lien désigne, si c'en est une : (genre, identifiant).
///
/// Seuls `qobuz.com` et ses sous-domaines comptent, en `http` ou `https`
/// (on n'ouvre pas l'adresse, on en lit l'identifiant), ou un chemin relatif
/// au site. Un artiste a un identifiant numérique ; un album, alphanumérique.
/// Un chemin sans identifiant (`/interpreter/miles-davis`) ne désigne rien.
pub fn page_qobuz(href: &str) -> Option<(GenreDeLien, String)> {
    let href = href.trim();
    if href.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return None;
    }
    let absolue = if href.starts_with('/') && !href.starts_with("//") {
        format!("https://www.qobuz.com{href}")
    } else {
        href.to_string()
    };
    let url = reqwest::Url::parse(&absolue).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return None;
    }
    let hote = url.host_str()?.to_ascii_lowercase();
    let hote = hote.trim_end_matches('.');
    if hote != "qobuz.com" && !hote.ends_with(".qobuz.com") {
        return None;
    }
    let morceaux: Vec<&str> = url.path_segments()?.filter(|m| !m.is_empty()).collect();
    let (p, genre) =
        morceaux
            .iter()
            .enumerate()
            .find_map(|(i, m)| match m.to_ascii_lowercase().as_str() {
                "artist" | "interpreter" => Some((i, GenreDeLien::Artiste)),
                "album" => Some((i, GenreDeLien::Album)),
                _ => None,
            })?;
    let suite = &morceaux[p + 1..];
    if suite.is_empty() || suite.len() > 2 {
        return None;
    }
    let id = *suite.last()?;
    let valide = match genre {
        GenreDeLien::Artiste => id.len() <= 20 && id.chars().all(|c| c.is_ascii_digit()),
        GenreDeLien::Album => id.len() <= 40 && id.chars().all(|c| c.is_ascii_alphanumeric()),
    };
    (valide && !id.is_empty()).then(|| (genre, id.to_string()))
}

/// La page Qobuz où la bio est affichée : ses liens vers elle-même restent
/// du texte, et son nom (et, sur un album, celui de son artiste) n'est pas
/// lié vers la bibliothèque.
pub struct PageDeLaBio<'a> {
    pub genre: GenreDeLien,
    pub id: &'a str,
    pub noms: &'a [&'a str],
}

/// La bio en texte (comme [`texte_de_presentation`]) et ses segments. `None`
/// quand il ne reste rien à lire.
pub fn bio_qobuz_segmentee(
    html: &str,
    index: &IndexDesNoms,
    page: &PageDeLaBio<'_>,
) -> Option<(String, Vec<BioSegment>)> {
    let texte = texte_de_presentation(html)?;
    let mut de_service = Vec::new();
    let mut externes = Vec::new();
    for (href, contenu) in bio_segments::ancres(html) {
        let Some(libelle) = texte_de_presentation(contenu) else {
            continue;
        };
        match page_qobuz(&href) {
            Some((genre, id)) => {
                if genre == page.genre && id == page.id {
                    continue;
                }
                de_service.push((
                    libelle,
                    LienDeService {
                        service: SERVICE.to_string(),
                        genre,
                        id,
                    },
                ));
            }
            None => externes.push(LienExterne {
                texte: libelle,
                url: href,
            }),
        }
    }
    let exclus: Vec<LienInterne> = page
        .noms
        .iter()
        .filter_map(|n| index.lien_du_nom(n))
        .collect();
    let segments = bio_segments::segments_avec_liens_de_service(
        &texte,
        index,
        &exclus,
        &de_service,
        &externes,
    );
    Some((texte, segments))
}

#[cfg(test)]
#[path = "bio_qobuz_8129_tests.rs"]
mod bio_qobuz_8129_tests;
