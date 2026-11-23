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
//!
//! ## Si Apple change sa page
//!
//! La lecture est tolérante (ordre des attributs, guillemets, `name=` au lieu
//! de `property=`, entités `&amp;`) et n'accepte qu'une adresse `https://`.
//! Une page méconnaissable rend « pas de photo » : le logo de la station
//! reste affiché, rien ne panique, et la photo n'est pas redemandée pour cet
//! artiste. Après [`ECHECS_AVANT_SUSPENSION`] pages méconnaissables
//! d'affilée, le repli se suspend [`SUSPENSION`] : plus aucun appel ne part
//! vers une page qu'on ne sait plus lire.

use std::sync::LazyLock;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::Value;

use super::Source;
use super::choix::{meme_nom, norme};

/// L'agent du code de référence : la page d'artiste est servie à un
/// navigateur.
pub const AGENT_PAGE: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.1.15";
/// Les pages d'artiste ne se lisent que sous cette racine.
pub const PAGES_APPLE: &str = "https://music.apple.com/";
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
/// Une balise `<meta …>` qui désigne `og:image`, quels que soient l'ordre
/// de ses attributs et ses guillemets.
static META_OG_IMAGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<meta\b[^>]*?\b(?:property|name)\s*=\s*["']og:image["'][^>]*>"#)
        .expect("motif Cover+ valide")
});
static CONTENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)\bcontent\s*=\s*(?:"([^"]*)"|'([^']*)')"#).expect("motif Cover+ valide")
});

/// Pages méconnaissables d'affilée avant de suspendre le repli.
pub const ECHECS_AVANT_SUSPENSION: u32 = 5;
/// Durée de la suspension du repli après une série de pages méconnaissables.
pub const SUSPENSION: Duration = Duration::from_secs(6 * 3600);

/// La santé de la lecture des pages : échecs d'analyse d'affilée, et la
/// fin de la suspension en cours.
#[derive(Debug, Default)]
pub(crate) struct Sante {
    echecs: u32,
    suspendu_jusqu_a: Option<Instant>,
}

impl Sante {
    pub(crate) const fn new() -> Self {
        Self {
            echecs: 0,
            suspendu_jusqu_a: None,
        }
    }

    /// Le repli est-il suspendu à l'instant `maintenant` ?
    pub(crate) fn suspendu(&self, maintenant: Instant) -> bool {
        self.suspendu_jusqu_a.is_some_and(|t| maintenant < t)
    }

    /// Une page a été lue : reconnue (`true` : photo ou logo générique) ou
    /// méconnaissable.
    pub(crate) fn constater(&mut self, reconnue: bool, maintenant: Instant) {
        if reconnue {
            self.echecs = 0;
            self.suspendu_jusqu_a = None;
            return;
        }
        self.echecs = self.echecs.saturating_add(1);
        if self.echecs >= ECHECS_AVANT_SUSPENSION {
            self.echecs = 0;
            self.suspendu_jusqu_a = Some(maintenant + SUSPENSION);
            tracing::debug!("cover_plus_photo_artiste_suspendue");
        }
    }
}

static SANTE: Mutex<Sante> = Mutex::new(Sante::new());

fn sante() -> std::sync::MutexGuard<'static, Sante> {
    SANTE.lock().unwrap_or_else(|e| e.into_inner())
}

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

/// La balise `og:image` d'une page, si elle désigne une adresse `https://`.
/// Toute autre forme rend `None` : jamais de panique, jamais d'adresse
/// douteuse affichée.
pub fn og_image(html: &str) -> Option<String> {
    META_OG_IMAGE.find_iter(html).find_map(|balise| {
        let c = CONTENT.captures(balise.as_str())?;
        let brut = c.get(1).or_else(|| c.get(2))?.as_str().trim();
        let url = brut.replace("&amp;", "&");
        let valide = url.starts_with("https://")
            && url.len() <= 2048
            && !url
                .chars()
                .any(|c| c.is_whitespace() || c == '<' || c == '>');
        valide.then_some(url)
    })
}

/// Le compte rendu d'une recherche de photo.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Photo {
    /// La photo carrée (600x600), quand elle est trouvée.
    pub url: Option<String>,
    /// Pourquoi il n'y en a pas.
    pub raison: &'static str,
    /// L'échec tient à une panne (réseau, budget, suspension), pas à
    /// l'artiste : il ne doit pas être retenu pour cet artiste.
    pub passagere: bool,
}

impl Photo {
    pub fn trouvee(&self) -> bool {
        self.url.is_some()
    }

    fn sans(raison: &'static str) -> Self {
        Self {
            url: None,
            raison,
            passagere: false,
        }
    }

    fn panne(raison: &'static str) -> Self {
        Self {
            url: None,
            raison,
            passagere: true,
        }
    }
}

async fn lire_la_page(source: Source<'_>, adresse: &str) -> Option<String> {
    let url = reqwest::Url::parse(adresse).ok()?;
    let racine = racine_des_pages(source);
    let client = client_de_page(racine.clone())?;
    let mut reponse = super::obtenir(source, &client, url, Some(AGENT_PAGE)).await?;
    // Défense en profondeur : la page lue est bien sous la racine permise.
    if !reponse.url().as_str().starts_with(&racine) {
        return None;
    }
    let mut octets = Vec::new();
    loop {
        match reponse.chunk().await {
            Ok(Some(morceau)) => {
                octets.extend_from_slice(&morceau);
                if octets.len() >= PAGE_MAX {
                    break;
                }
            }
            Ok(None) => break,
            // Coupure en cours de lecture : ce qu'on a suffit souvent (la
            // balise est dans l'en-tête), sinon l'analyse le dira.
            Err(_) => break,
        }
    }
    Some(String::from_utf8_lossy(&octets).into_owned())
}

/// La page d'artiste ne se lit que sur `https://music.apple.com/`. Les
/// épreuves, qui dressent un faux service, lisent aussi sous sa racine.
fn page_admise(source: Source<'_>, page: &str) -> bool {
    page.starts_with(PAGES_APPLE)
        || source
            .racine_factice()
            .is_some_and(|r| page.starts_with(&format!("{r}/")))
}

/// La racine sous laquelle une page d'artiste peut être lue, redirections
/// comprises : music.apple.com, ou la racine d'un faux service d'épreuve.
fn racine_des_pages(source: Source<'_>) -> String {
    source
        .racine_factice()
        .map(|r| format!("{r}/"))
        .unwrap_or_else(|| PAGES_APPLE.to_string())
}

/// Un client pour la page d'artiste : il ne suit une redirection que si
/// elle reste sous `racine` (et 5 au plus). Une redirection qui en sort est
/// refusée : la réponse 3xx revient telle quelle, et le logo reste affiché.
/// Construit à la demande : une page au plus par artiste, et par titre.
fn client_de_page(racine: String) -> Option<reqwest::Client> {
    crate::http::client::builder()
        .timeout(super::DELAI_APPEL)
        .redirect(reqwest::redirect::Policy::custom(move |essai| {
            if essai.previous().len() < 5 && essai.url().as_str().starts_with(&racine) {
                essai.follow()
            } else {
                essai.stop()
            }
        }))
        .build()
        .ok()
}

/// La photo ne vient que du CDN d'Apple, `*.mzstatic.com`, en `https://`.
pub fn image_admise(adresse: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(adresse) else {
        return false;
    };
    let hote = url.host_str().unwrap_or_default().to_ascii_lowercase();
    url.scheme() == "https" && (hote == "mzstatic.com" || hote.ends_with(".mzstatic.com"))
}

/// La photo de l'artiste `nom`, dans la boutique `boutique`.
pub async fn photo_artiste(source: Source<'_>, nom: &str, boutique: &str) -> Photo {
    let nom = nom.trim();
    if nom.is_empty() {
        return Photo::sans("aucun nom d'artiste");
    }
    if sante().suspendu(Instant::now()) {
        return Photo::panne("repli suspendu : page d'Apple méconnaissable");
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
            return Photo::panne("iTunes en échec");
        };
        let Some(franc) = resultats
            .iter()
            .find(|x| meme_nom(x.get("artistName").and_then(Value::as_str).unwrap_or(""), n))
        else {
            continue;
        };
        let Some(page) = franc.get("artistLinkUrl").and_then(Value::as_str) else {
            return Photo::sans("artiste sans page");
        };
        if !page_admise(source, page) {
            return Photo::sans("page d'artiste hors de music.apple.com");
        }
        let Some(html) = lire_la_page(source, page).await else {
            return Photo::panne("page artiste illisible");
        };
        let og = og_image(&html);
        sante().constater(og.is_some(), Instant::now());
        return match og {
            None => Photo::sans("page sans og:image"),
            Some(og) if og.contains(GENERIQUE) => {
                Photo::sans("pas de photo chez Apple (logo générique)")
            }
            Some(og) if !image_admise(&og) => Photo::sans("photo hors de *.mzstatic.com"),
            Some(og) => Photo {
                url: Some(carre(&og, 600)),
                raison: "",
                passagere: false,
            },
        };
    }
    Photo::sans("aucun artiste de ce nom exact chez iTunes")
}
