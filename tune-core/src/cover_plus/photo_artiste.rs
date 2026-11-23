//! Cover+ (#8452) — repli quand la règle franche échoue : une PHOTO de
//! l'artiste, jamais ses albums (spécification, § 3.6).
//!
//! Portage de `photo_artiste.py` (radio-metadata.fr, 11/11/2026).
//!
//! L'API iTunes publique ne donne aucune image d'artiste (`entity=musicArtist`
//! n'en porte pas). On passe donc par la page de l'artiste sur
//! music.apple.com, dont la balise `og:image` est sa photo. **C'est la
//! lecture d'une page web, pas une API** : si Apple change sa page, ce repli
//! tombe en silence. Il se coupe par le réglage `cover_plus.photo_artiste`
//! ou la variable `TUNE_COVER_PLUS_PHOTO_ARTISTE=0` (voir
//! [`super::photo_artiste_active`]).
//!
//! Étapes, pour un nom d'artiste :
//! 1. iTunes `search?entity=musicArtist` sur le nom, boutique de la station ;
//! 2. on ne garde qu'une égalité stricte après normalisation, article en tête
//!    et espaces ignorés, mais jamais de préfixe : « Tank » ne vaut pas
//!    « Tank And The Bangas » ;
//! 3. à défaut, même chose avec le premier nom d'une collaboration
//!    (« MOGWAI [+] IGGY POP » → « MOGWAI », « X Feat. Y » → « X ») ;
//! 4. lecture de `og:image` sur la page de l'artiste ; le logo générique
//!    d'Apple Music (`/assets/meta/apple-music.png`) veut dire « pas de
//!    photo ».
//!
//! L'image `og:image` est un bandeau 1200x630 ; `600x600cc.jpg` la recadre au
//! carré.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use super::Source;
use super::choix::{meme_nom, norme};

/// L'agent du code de référence : la page d'artiste est servie à un
/// navigateur.
pub const AGENT_PAGE: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.1.15";
/// Le logo générique d'Apple Music : « pas de photo ».
pub const GENERIQUE: &str = "/assets/meta/apple-music.png";
/// Ce qu'on lit au plus d'une page d'artiste.
const PAGE_MAX: usize = 4 * 1024 * 1024;

static COLLAB: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\s+(?:\[\+\]|&|feat\.?|ft\.?|featuring|x|vs\.?|with|avec|y|e|and|et)\s+|\s*[,/;]\s*",
    )
    .expect("motif Cover+ valide")
});
static CARRE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"/\d+x\d+[a-z]*\.(?:png|jpg|webp)$").expect("motif Cover+ valide")
});
static OG_IMAGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<meta property="og:image" content="([^"]+)""#).expect("motif Cover+ valide")
});

/// Le premier nom d'une collaboration.
pub fn premier_nom(nom: &str) -> String {
    COLLAB
        .splitn(nom, 2)
        .next()
        .unwrap_or(nom)
        .trim()
        .to_string()
}

/// Le bandeau `og:image` recadré au carré : `…/1200x630cw.png` →
/// `…/600x600cc.jpg`. Une adresse d'une autre forme est rendue telle quelle.
pub fn carre(url: &str, taille: u32) -> String {
    CARRE
        .replace(url, format!("/{taille}x{taille}cc.jpg").as_str())
        .into_owned()
}

/// La balise `og:image` d'une page.
pub fn og_image(html: &str) -> Option<String> {
    OG_IMAGE
        .captures(html)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

/// Le compte rendu d'une recherche de photo.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Photo {
    /// La photo carrée (600x600), quand elle est trouvée.
    pub url: Option<String>,
    /// Pourquoi il n'y en a pas.
    pub raison: &'static str,
}

impl Photo {
    pub fn trouvee(&self) -> bool {
        self.url.is_some()
    }

    fn sans(raison: &'static str) -> Self {
        Self { url: None, raison }
    }
}

async fn lire_la_page(source: Source<'_>, adresse: &str) -> Option<String> {
    let url = reqwest::Url::parse(adresse).ok()?;
    let mut reponse = super::obtenir(source, url, Some(AGENT_PAGE)).await?;
    let mut octets = Vec::new();
    while let Some(morceau) = reponse.chunk().await.ok().flatten() {
        octets.extend_from_slice(&morceau);
        if octets.len() >= PAGE_MAX {
            break;
        }
    }
    Some(String::from_utf8_lossy(&octets).into_owned())
}

/// La photo de l'artiste `nom`, dans la boutique `boutique`.
pub async fn photo_artiste(source: Source<'_>, nom: &str, boutique: &str) -> Photo {
    let nom = nom.trim();
    if nom.is_empty() {
        return Photo::sans("aucun nom d'artiste");
    }
    let mut noms = vec![nom.to_string()];
    let p = premier_nom(nom);
    if !p.is_empty() && norme(&p) != norme(nom) {
        noms.push(p);
    }
    for n in &noms {
        let Some(resultats) = super::appeler(
            source,
            "/search",
            &[
                ("term", n.as_str()),
                ("entity", "musicArtist"),
                ("country", boutique),
                ("limit", "10"),
            ],
        )
        .await
        else {
            return Photo::sans("iTunes en échec");
        };
        let Some(franc) = resultats.iter().find(|x| {
            meme_nom(
                x.get("artistName").and_then(Value::as_str).unwrap_or(""),
                n,
            )
        }) else {
            continue;
        };
        let Some(page) = franc.get("artistLinkUrl").and_then(Value::as_str) else {
            return Photo::sans("artiste sans page");
        };
        let Some(html) = lire_la_page(source, page).await else {
            return Photo::sans("page artiste illisible");
        };
        return match og_image(&html) {
            None => Photo::sans("page sans og:image"),
            Some(og) if og.contains(GENERIQUE) => {
                Photo::sans("pas de photo chez Apple (logo générique)")
            }
            Some(og) => Photo {
                url: Some(carre(&og, 600)),
                raison: "",
            },
        };
    }
    Photo::sans("aucun artiste de ce nom exact chez iTunes")
}
