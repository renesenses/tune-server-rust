//! #8022 — biographies d'artiste et notes d'album de la bibliothèque LOCALE,
//! prises dans les textes éditoriaux d'un service de streaming connecté
//! (Qobuz d'abord, TIDAL ensuite).
//!
//! Ce module ne lit ni n'écrit la base : il reçoit ce que la bibliothèque sait
//! de l'album ou de l'artiste (codes, nom) et rend un [`EditorialTrouve`], ou
//! rien. La route décide de le servir, de le ranger, et dans quel ordre
//! (`tune-server/src/routes/library/editorial_service.rs`).
//!
//! # Correspondance SÛRE, sinon rien
//!
//! - **Album** : par code-barres (UPC/EAN), sinon par les ISRC de ses titres
//!   (un seul album du service doit les porter tous) — voir
//!   [`super::StreamingService::album_par_codes`]. Jamais par le titre.
//! - **Artiste** : par l'artiste de l'album du service retrouvé, quand son nom
//!   est celui de l'artiste local (même clé [`cle_artiste`]) ; sinon par le
//!   nom, si UN SEUL artiste du service porte exactement ce nom.
//!
//! # Langue
//!
//! Celle que le service annonce pour le texte, sinon celle du magasin du
//! compte. Aucune des deux : le texte n'est pas retenu — une bio de langue
//! inconnue n'est servie à aucune interface (#7027).

use super::traits::{StreamingService, TexteEditorial};
use crate::TuneError;
use crate::db::artist_repo::cle_artiste;

/// Longueur au-delà de laquelle un texte est coupé (en caractères). Une bio
/// Qobuz complète dépasse rarement 5 000 caractères ; au-delà, la fiche n'a
/// plus rien d'une présentation.
pub const LONGUEUR_MAX: usize = 6000;

/// Albums locaux examinés au plus pour retrouver un artiste.
pub const ALBUMS_EXAMINES: usize = 2;

/// Ce que la bibliothèque sait d'un album pour le retrouver chez un service.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodesAlbum {
    pub upc: Option<String>,
    pub isrcs: Vec<String>,
}

impl CodesAlbum {
    pub fn est_vide(&self) -> bool {
        self.upc.as_deref().is_none_or(|u| u.trim().is_empty()) && self.isrcs.is_empty()
    }
}

/// Un texte éditorial retenu, prêt à ranger : texte borné, langue CONNUE
/// (code de base, `fr`), source (le nom du service) et lien vers sa page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorialTrouve {
    pub texte: String,
    pub langue: String,
    pub source: String,
    pub source_url: Option<String>,
}

/// La langue d'un texte : `fr-FR` → `fr`. Vide ou illisible : `None`.
pub fn langue_de_base(langue: &str) -> Option<String> {
    let base = langue
        .trim()
        .split(['-', '_'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    (base.len() >= 2 && base.len() <= 3 && base.chars().all(|c| c.is_ascii_alphabetic()))
        .then_some(base)
}

/// Coupe un texte trop long à la dernière fin de paragraphe (ou de phrase)
/// avant [`LONGUEUR_MAX`], et le dit par « … ».
pub fn borner(texte: &str) -> String {
    let texte = texte.trim();
    if texte.chars().count() <= LONGUEUR_MAX {
        return texte.to_string();
    }
    let fin = texte
        .char_indices()
        .nth(LONGUEUR_MAX)
        .map_or(texte.len(), |(i, _)| i);
    let tete = &texte[..fin];
    let coupe = tete
        .rfind("\n\n")
        .or_else(|| tete.rfind(". ").map(|i| i + 1))
        .filter(|i| *i > fin / 2)
        .unwrap_or(fin);
    format!("{}…", tete[..coupe].trim_end())
}

fn retenir(texte: TexteEditorial, magasin: Option<&str>, source: &str) -> Option<EditorialTrouve> {
    let langue = texte
        .langue
        .as_deref()
        .and_then(langue_de_base)
        .or_else(|| magasin.and_then(langue_de_base))?;
    let corps = borner(&texte.texte);
    if corps.is_empty() {
        return None;
    }
    Some(EditorialTrouve {
        texte: corps,
        langue,
        source: source.to_string(),
        source_url: texte.url,
    })
}

/// La note éditoriale d'un album local, si le service le retrouve SÛREMENT.
pub async fn note_d_album(
    svc: &dyn StreamingService,
    codes: &CodesAlbum,
) -> Result<Option<EditorialTrouve>, TuneError> {
    if codes.est_vide() {
        return Ok(None);
    }
    let Some(album) = svc
        .album_par_codes(codes.upc.as_deref(), &codes.isrcs)
        .await?
    else {
        return Ok(None);
    };
    let Some(texte) = svc.note_editoriale_album(&album.id).await? else {
        return Ok(None);
    };
    Ok(retenir(
        texte,
        svc.langue_du_magasin().as_deref(),
        svc.name(),
    ))
}

/// La biographie éditoriale d'un artiste local.
///
/// `albums` : les codes de quelques albums locaux de cet artiste (les
/// [`ALBUMS_EXAMINES`] premiers qui en portent). L'artiste de l'album retrouvé
/// n'est retenu que s'il porte le nom de l'artiste local : une compilation ou
/// un album en invité ne désigne pas l'artiste.
pub async fn bio_d_artiste(
    svc: &dyn StreamingService,
    nom: &str,
    albums: &[CodesAlbum],
) -> Result<Option<EditorialTrouve>, TuneError> {
    let cle = cle_artiste(nom);
    if cle.is_empty() {
        return Ok(None);
    }
    let mut artist_id = None;
    for codes in albums
        .iter()
        .filter(|c| !c.est_vide())
        .take(ALBUMS_EXAMINES)
    {
        if let Some(album) = svc
            .album_par_codes(codes.upc.as_deref(), &codes.isrcs)
            .await?
            && cle_artiste(&album.artist_name) == cle
            && let Some(id) = album.artist_id
        {
            artist_id = Some(id);
            break;
        }
    }
    let artist_id = match artist_id {
        Some(id) => id,
        None => match svc.artiste_par_nom_exact(nom).await? {
            Some(id) => id,
            None => return Ok(None),
        },
    };
    let Some(texte) = svc.bio_editoriale_artiste(&artist_id).await? else {
        return Ok(None);
    };
    Ok(retenir(
        texte,
        svc.langue_du_magasin().as_deref(),
        svc.name(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn langue_de_base_lit_le_code_et_refuse_le_reste() {
        assert_eq!(langue_de_base("fr-FR").as_deref(), Some("fr"));
        assert_eq!(langue_de_base(" EN ").as_deref(), Some("en"));
        assert_eq!(langue_de_base(""), None);
        assert_eq!(langue_de_base("français"), None);
    }

    #[test]
    fn un_texte_trop_long_est_coupe_a_un_paragraphe() {
        let para = "a".repeat(4000);
        let texte = format!("{para}\n\n{para}");
        let b = borner(&texte);
        assert_eq!(b, format!("{para}…"));
        assert_eq!(borner("court"), "court");
    }

    #[test]
    fn sans_langue_ni_magasin_rien_n_est_retenu() {
        let t = TexteEditorial {
            texte: "Texte.".into(),
            langue: None,
            url: None,
        };
        assert_eq!(retenir(t.clone(), None, "qobuz"), None);
        let r = retenir(t, Some("de"), "qobuz").expect("langue du magasin");
        assert_eq!(r.langue, "de");
        assert_eq!(r.source, "qobuz");
    }
}
