//! #7089 — le logo d'un label, cherché sur le web ouvert, à côté de Qobuz.
//!
//! Chaque serveur Tune récupère LUI-MÊME le logo, pour son usage local
//! d'identification du label, dans son cache des pochettes. Mozaik Labs ne
//! redistribue rien.
//!
//! # Le rapprochement d'abord : jamais approximatif
//!
//! 1. **Par la release.** Un album local du label porte un
//!    `musicbrainz_release_id` : sa `label-info` donne le MBID du label, à
//!    condition que le nom concorde ([`noms_concordent`], après la
//!    normalisation prudente de [`super::noms_de_labels`]). La release est
//!    d'abord lue dans `musicbrainz_release_cache` : souvent, aucune requête.
//! 2. **Par le nom.** Sinon `label?query=label:"…"`, et seul un candidat dont
//!    le nom (ou un alias) concorde EXACTEMENT compte. Il est retenu s'il est
//!    seul, ou s'il devance le deuxième d'au moins [`ECART_MIN_DE_SCORE`]
//!    points. Sinon : rien — « BMG », « Columbia », « Domino » ont plusieurs
//!    homonymes au même nom, et un faux logo est pire qu'un monogramme.
//!
//! # Puis les sources, dans l'ordre de préférence (décision du 26/10/2026)
//!
//! 1. **Wikimedia Commons, libre** : le fichier P154 (« logo ») de l'élément
//!    Wikidata du label, ou une relation MusicBrainz « logo » qui pointe sur
//!    Commons. Seulement si la page du fichier le dit libre : domaine public,
//!    CC0, CC BY, CC BY-SA ([`licence_libre`]).
//! 2. **Wikipédia** : l'image de l'article anglais du label (son infobox),
//!    par Wikidata — y compris un logo NON LIBRE (« fair use »), marqué
//!    comme tel : usage d'identification.
//! 3. **Le site officiel du label** (relation MusicBrainz « official site ») :
//!    `apple-touch-icon`, icône haute définition, image « logo » explicite,
//!    `og:image`. Seule une image carrée ou presque, d'au moins
//!    [`COTE_MIN_SITE`] px, est gardée : une bannière ou une photo, non.
//! 4. **Discogs** : l'image PRINCIPALE (`primary`) de la fiche du label, par
//!    l'API publique sans jeton, à 25 requêtes par minute au plus. Les images
//!    secondaires — souvent des photos de rondelles — ne sont jamais prises.
//!
//! Qobuz vient après (son propre index, [`super::logos_labels`]).
//!
//! Chaque logo garde sa source et ce qu'on sait de sa licence
//! ([`StatutDeLicence`]) : libre, non libre en usage d'identification, ou
//! inconnue. La liste des labels l'expose ; le client l'affiche au survol.
//! Un logo de marque déclaré « œuvre personnelle » sur Commons (`PD-self`,
//! `Self-published work`) est marqué DOUTEUX.
//!
//! # Pas de rafale, un limiteur par source
//!
//! MusicBrainz : le limiteur PARTAGÉ du dépôt
//! ([`crate::http::fetch::MUSICBRAINZ`], une requête par seconde, même clé
//! que les autres passes). Wikidata, Commons, Wikipédia : le limiteur
//! Wikimedia partagé, une clé par site. Discogs sans jeton :
//! [`LIMITEUR_DISCOGS_SANS_JETON`], une requête toutes les 2,5 s. Sites
//! officiels : [`LIMITEUR_SITES`], une par seconde et par hôte. La passe reste
//! celle de l'index : paresseuse, bornée à `PASSE_MAX` labels.
//!
//! # Garde réseau
//!
//! Une page ou une image d'un site tiers n'est lue que sur un nom d'hôte
//! public : jamais une adresse IP littérale, jamais `localhost` ni `.local`
//! ([`hote_public`]), redirections comprises ; et chaque connexion ne vise
//! qu'une adresse publique vérifiée à la résolution ([`ResolveurPublic`]). Les images d'un site ne sont
//! lues qu'en HTTPS, plafonnées à `TAILLE_MAX_IMAGE`.

use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use serde_json::Value;

use super::logos_labels::{
    Attribution, CatalogueDeLabels, LabelDuCatalogue, SourceDuLogo, StatutDeLicence,
    TAILLE_MAX_IMAGE, noms_concordent,
};
use crate::db::backend::DbBackend;
use crate::http::fetch::RateLimiter;

/// L'écart de score minimal entre le premier et le deuxième homonyme exact.
pub const ECART_MIN_DE_SCORE: i64 = 15;

/// La largeur de la vignette demandée à Commons et à Wikipédia.
pub const LARGEUR_VIGNETTE: u32 = 250;

/// Le nom de l'index de ces sources, dans le dossier du cache des pochettes.
pub const FICHIER_INDEX_WEB: &str = "logos_labels_web.json";

/// La racine d'essai de ces sources pour
/// [`crate::services_factices::substituer_base`].
pub const BASE_FACTICE: &str = "logos-web";

/// Le plus petit côté accepté d'une image de site officiel.
pub const COTE_MIN_SITE: u32 = 120;
/// Le rapport largeur / hauteur d'une image de site officiel : carrée ou
/// presque.
pub const RAPPORT_SITE: (f64, f64) = (0.75, 1.34);
/// Le plus petit côté accepté de l'image principale Discogs.
pub const COTE_MIN_DISCOGS: u64 = 100;
/// Le rapport largeur / hauteur de l'image principale Discogs : un logo
/// peut être large (ECM : 240 × 151), pas une bande.
pub const RAPPORT_DISCOGS: (f64, f64) = (0.4, 2.5);
/// Une page de site officiel n'est pas lue au-delà.
pub const TAILLE_MAX_PAGE: usize = 512 * 1024;
/// Au plus tant d'images candidates essayées par site.
pub const CANDIDATS_MAX_SITE: usize = 3;

/// Discogs sans jeton : 25 requêtes par minute par IP. Une toutes les 2,5 s
/// en tient 24.
pub static LIMITEUR_DISCOGS_SANS_JETON: LazyLock<RateLimiter> =
    LazyLock::new(|| RateLimiter::with_interval(Duration::from_millis(2500)));
pub const CLE_DISCOGS_SANS_JETON: &str = "discogs-sans-jeton";

/// Les sites officiels : une requête par seconde et par hôte.
pub static LIMITEUR_SITES: LazyLock<RateLimiter> = LazyLock::new(|| RateLimiter::per_second(1.0));

/// Les catégories Commons qui rendent une licence de logo douteuse.
const CATEGORIES_DOUTEUSES: &[&str] = &["Category:PD-self", "Category:Self-published work"];

/// Les clés du limiteur Wikimedia, une par site.
const CLE_WIKIDATA: &str = crate::http::fetch::CLE_WIKIDATA;
const CLE_COMMONS: &str = "commons";
const CLE_WIKIPEDIA: &str = "wikipedia";

/// Le nom d'hôte est-il public ? Ni IP littérale, ni `localhost`, ni
/// `.local`/`.localhost`/`.internal`, et un point au moins.
pub fn hote_public(url: &reqwest::Url) -> bool {
    if !matches!(url.scheme(), "https" | "http") || !url.username().is_empty() {
        return false;
    }
    let Some(d) = url.host_str() else {
        return false;
    };
    let d = d.trim_end_matches('.').to_ascii_lowercase();
    if d.starts_with('[') || d.parse::<std::net::IpAddr>().is_ok() {
        return false;
    }
    // Une adresse IPv4 écrite autrement (`0x7f.1`, `2130706433`) : le
    // dernier libellé d'un vrai nom de domaine porte au moins une lettre.
    if !d
        .rsplit('.')
        .next()
        .is_some_and(|tld| tld.chars().any(|c| c.is_ascii_alphabetic()))
    {
        return false;
    }
    d.contains('.')
        && d != "localhost"
        && ![".localhost", ".local", ".internal", ".lan", ".home.arpa"]
            .iter()
            .any(|s| d.ends_with(s))
}

/// Une adresse joignable sur Internet ? Ni privée, ni de bouclage, ni
/// lien-local, ni ULA, ni non spécifiée, ni multicast, ni réservée.
pub fn adresse_publique(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, c, _] = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_multicast()
                || v4.is_broadcast()
                || v4.is_documentation()
                || a == 0
                || a >= 240
                || (a == 100 && (64..128).contains(&b))
                || (a == 192 && b == 0 && c == 0)
                || (a == 198 && (18..20).contains(&b)))
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return adresse_publique(IpAddr::V4(v4));
            }
            let s = v6.segments();
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00
                || (s[0] & 0xffc0) == 0xfe80
                || (s[0] & 0xffc0) == 0xfec0
                || (s[0] == 0x2001 && s[1] == 0x0db8)
                || (s[0] == 0x0064 && s[1] == 0xff9b)
                || s[..6].iter().all(|x| *x == 0))
        }
    }
}

/// Le résolveur du client des sites : il résout le nom, ne garde QUE les
/// adresses publiques ([`adresse_publique`]) et les rend au client, qui s'y
/// connecte directement — sans seconde résolution. Un nom sans adresse
/// publique est une erreur. Vaut pour chaque connexion, redirections
/// comprises.
#[derive(Debug, Default, Clone, Copy)]
pub struct ResolveurPublic;

impl reqwest::dns::Resolve for ResolveurPublic {
    fn resolve(&self, nom: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let hote = nom.as_str().to_string();
        Box::pin(async move {
            let adresses: Vec<std::net::SocketAddr> = tokio::net::lookup_host((hote.as_str(), 0))
                .await?
                .filter(|a| adresse_publique(a.ip()))
                .collect();
            if adresses.is_empty() {
                return Err(format!("{hote} : aucune adresse publique").into());
            }
            Ok(Box::new(adresses.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// Le client des sites officiels : ses connexions ne visent que des adresses
/// publiques vérifiées ([`ResolveurPublic`]), et il ne suit une redirection
/// que vers un hôte public, cinq fois au plus. `None` s'il ne peut être
/// construit : la source « site officiel » est alors coupée, jamais servie
/// par un client sans ces gardes.
fn client_des_sites() -> Option<reqwest::Client> {
    crate::http::client::builder()
        .dns_resolver(std::sync::Arc::new(ResolveurPublic))
        .redirect(reqwest::redirect::Policy::custom(|essai| {
            if essai.previous().len() >= 5 {
                essai.error("trop de redirections")
            } else if hote_public(essai.url()) {
                essai.follow()
            } else {
                essai.stop()
            }
        }))
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| tracing::warn!(erreur = %e, "logo_label_client_des_sites_indisponible"))
        .ok()
}

/// Où sont les services, et sous quels limiteurs. La production :
/// [`Transport::production`] ; les essais : [`Transport::sous_base`].
pub struct Transport {
    pub client: reqwest::Client,
    /// Le client des pages et images des sites officiels : adresses
    /// vérifiées à la résolution, redirections gardées. `None` : source
    /// coupée.
    pub client_sites: Option<reqwest::Client>,
    pub musicbrainz: String,
    pub wikidata: String,
    pub commons: String,
    pub wikipedia: String,
    pub discogs_api: String,
    pub discogs_site: String,
    /// Préfixes d'images admis sans la garde d'hôte public — vide en
    /// production, la racine du faux serveur dans un banc.
    pub prefixes_d_essai: Vec<String>,
    pub limiteur_mb: &'static RateLimiter,
    pub cle_mb: &'static str,
    pub limiteur_wikimedia: &'static RateLimiter,
    pub limiteur_discogs: &'static RateLimiter,
    pub limiteur_sites: &'static RateLimiter,
}

/// Sous quel limiteur part une requête.
#[derive(Clone, Copy)]
enum Limite<'a> {
    MusicBrainz,
    Wikimedia(&'static str),
    Discogs,
    Site(&'a str),
}

impl Transport {
    pub fn production(client: reqwest::Client) -> Self {
        Self {
            client,
            client_sites: client_des_sites(),
            musicbrainz: String::from("https://musicbrainz.org/ws/2"),
            wikidata: String::from("https://www.wikidata.org/w/api.php"),
            commons: String::from("https://commons.wikimedia.org/w/api.php"),
            wikipedia: String::from("https://en.wikipedia.org/w/api.php"),
            discogs_api: String::from("https://api.discogs.com"),
            discogs_site: String::from("https://www.discogs.com"),
            prefixes_d_essai: Vec::new(),
            limiteur_mb: &crate::http::fetch::MUSICBRAINZ,
            cle_mb: crate::metadata::musicbrainz_release::CLE_LIMITEUR_MUSICBRAINZ,
            limiteur_wikimedia: &crate::http::fetch::WIKIDATA,
            limiteur_discogs: &LIMITEUR_DISCOGS_SANS_JETON,
            limiteur_sites: &LIMITEUR_SITES,
        }
    }

    /// Tous les services sous une même racine d'essai : `{base}/mb`,
    /// `{base}/wikidata`, `{base}/commons`, `{base}/wikipedia`,
    /// `{base}/discogs`. Limiteurs de production.
    pub fn sous_base(client: reqwest::Client, base: &str) -> Self {
        let base = base.trim_end_matches('/');
        Self {
            client_sites: Some(client.clone()),
            musicbrainz: format!("{base}/mb"),
            wikidata: format!("{base}/wikidata"),
            commons: format!("{base}/commons"),
            wikipedia: format!("{base}/wikipedia"),
            discogs_api: format!("{base}/discogs"),
            discogs_site: format!("{base}/discogs-site"),
            prefixes_d_essai: vec![format!("{base}/")],
            ..Self::production(client)
        }
    }

    fn limiteur(&self, limite: Limite<'_>) -> (&'static RateLimiter, String) {
        match limite {
            Limite::MusicBrainz => (self.limiteur_mb, self.cle_mb.to_string()),
            Limite::Wikimedia(cle) => (self.limiteur_wikimedia, cle.to_string()),
            Limite::Discogs => (self.limiteur_discogs, CLE_DISCOGS_SANS_JETON.to_string()),
            Limite::Site(hote) => (self.limiteur_sites, format!("site:{hote}")),
        }
    }

    /// Une adresse de banc (racine du faux serveur) ?
    fn d_essai(&self, url: &str) -> bool {
        self.prefixes_d_essai.iter().any(|p| url.starts_with(p))
    }

    /// Une lecture JSON. `Ok(None)` : la ressource n'existe pas (404, 400).
    async fn json(
        &self,
        url: &str,
        params: &[(&str, &str)],
        limite: Limite<'_>,
    ) -> Result<Option<Value>, String> {
        let (limiteur, cle) = self.limiteur(limite);
        limiteur.acquire(&cle).await;
        let reponse = self
            .client
            .get(url)
            .query(params)
            .header(
                reqwest::header::USER_AGENT,
                crate::metadata::musicbrainz_release::MB_UA,
            )
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        limiteur.constater_reponse(&cle, &reponse);
        match reponse.status().as_u16() {
            400 | 404 => return Ok(None),
            s if !(200..300).contains(&s) => return Err(format!("HTTP {s}")),
            _ => {}
        }
        reponse.json().await.map(Some).map_err(|e| e.to_string())
    }

    /// Des octets, au plus `max`. `tronquer` : une page trop longue est
    /// coupée ; sinon (une image) elle est refusée. Rend aussi l'adresse
    /// finale, après redirections.
    async fn octets(
        &self,
        client: &reqwest::Client,
        url: &str,
        limite: Limite<'_>,
        max: usize,
        tronquer: bool,
    ) -> Result<(Vec<u8>, String), String> {
        let (limiteur, cle) = self.limiteur(limite);
        limiteur.acquire(&cle).await;
        let mut reponse = client
            .get(url)
            .header(
                reqwest::header::USER_AGENT,
                crate::metadata::musicbrainz_release::MB_UA,
            )
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        limiteur.constater_reponse(&cle, &reponse);
        if !reponse.status().is_success() {
            return Err(format!("HTTP {}", reponse.status()));
        }
        if !tronquer && reponse.content_length().is_some_and(|n| n as usize > max) {
            return Err(String::from("trop lourd"));
        }
        let finale = reponse.url().to_string();
        let mut corps = Vec::new();
        while let Some(morceau) = reponse.chunk().await.map_err(|e| e.to_string())? {
            corps.extend_from_slice(&morceau);
            if corps.len() > max {
                if tronquer {
                    corps.truncate(max);
                    break;
                }
                return Err(String::from("trop lourd"));
            }
        }
        Ok((corps, finale))
    }
}

/// Le transport du serveur. En production : les vrais services, toujours.
/// Dans une construction de tests (`services-factices`) : SEULEMENT si un
/// banc a déclaré une racine d'essai sous [`BASE_FACTICE`] — sinon aucun
/// transport, donc aucune passe et aucun appel réseau depuis un test qui
/// ouvre simplement l'onglet Labels.
pub fn transport_du_serveur(client: reqwest::Client) -> Option<Transport> {
    #[cfg(any(test, feature = "services-factices"))]
    {
        let base = crate::services_factices::base_substituee(BASE_FACTICE)?;
        Some(Transport::sous_base(client, &base))
    }
    #[cfg(not(any(test, feature = "services-factices")))]
    {
        Some(Transport::production(client))
    }
}

/// Le catalogue des logos du web : un [`CatalogueDeLabels`] sans compte.
pub struct CatalogueLogosWeb {
    transport: Transport,
    /// La base, pour lire les releases déjà gardées. `None` dans les essais.
    backend: Option<Arc<dyn DbBackend>>,
    /// La dernière image déjà téléchargée pour en mesurer la taille (site,
    /// Discogs) : [`CatalogueDeLabels::telecharger_image`] la rend sans
    /// second téléchargement.
    derniere_image: Mutex<Option<(String, Vec<u8>)>>,
}

impl CatalogueLogosWeb {
    pub fn new(transport: Transport, backend: Option<Arc<dyn DbBackend>>) -> Self {
        Self {
            transport,
            backend,
            derniere_image: Mutex::new(None),
        }
    }
}

// ── Rapprochement ───────────────────────────────────────────────────────────

/// Le nom à comparer : celui de la balise, débarrassé d'une ligne de
/// copyright ([`super::noms_de_labels`]).
fn nom_compare(nom: &str) -> String {
    super::noms_de_labels::nom_de_label_normalise(nom).into_owned()
}

/// Le label de la `label-info` d'une release dont le nom concorde.
pub fn label_de_la_release(release: &Value, nom: &str) -> Option<(String, String)> {
    let voulu = nom_compare(nom);
    release["label-info"].as_array()?.iter().find_map(|li| {
        let label = &li["label"];
        let n = label["name"].as_str()?;
        let id = label["id"].as_str().filter(|s| !s.is_empty())?;
        (noms_concordent(n, &voulu) || noms_concordent(n, nom))
            .then(|| (id.to_string(), n.to_string()))
    })
}

/// Le label d'une recherche `label?query=` : seul un candidat au nom exact
/// (ou à l'alias exact) compte ; il est retenu s'il est SEUL, ou s'il devance
/// le deuxième d'au moins [`ECART_MIN_DE_SCORE`] points.
pub fn label_de_la_recherche(reponse: &Value, nom: &str) -> Option<(String, String)> {
    let voulu = nom_compare(nom);
    let mut exacts: Vec<(i64, String, String)> = reponse["labels"]
        .as_array()?
        .iter()
        .filter_map(|l| {
            let n = l["name"].as_str()?;
            let id = l["id"].as_str().filter(|s| !s.is_empty())?;
            let alias = l["aliases"].as_array().into_iter().flatten();
            let exact = std::iter::once(n)
                .chain(alias.filter_map(|a| a["name"].as_str()))
                .any(|x| noms_concordent(x, &voulu));
            exact.then(|| {
                (
                    l["score"].as_i64().unwrap_or(0),
                    id.to_string(),
                    n.to_string(),
                )
            })
        })
        .collect();
    exacts.sort_by_key(|e| std::cmp::Reverse(e.0));
    match exacts.as_slice() {
        [] => None,
        [seul] => Some((seul.1.clone(), seul.2.clone())),
        [premier, deuxieme, ..] => (premier.0 - deuxieme.0 >= ECART_MIN_DE_SCORE)
            .then(|| (premier.1.clone(), premier.2.clone())),
    }
}

// ── Relations du label ──────────────────────────────────────────────────────

/// Le nom d'un fichier Commons dans une adresse : `…/wiki/File:X` ou
/// `upload.wikimedia.org/wikipedia/commons/…/X`. Toute autre adresse (Wikia,
/// tinypic, un site de logos, la Wikipédia anglaise…) : `None`.
pub fn fichier_commons(url: &str) -> Option<String> {
    let sans_schema = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let (hote, chemin) = sans_schema.split_once('/')?;
    let brut = if hote == "commons.wikimedia.org" {
        chemin
            .strip_prefix("wiki/File:")
            .or_else(|| chemin.strip_prefix("wiki/Fichier:"))?
    } else if hote == "upload.wikimedia.org" && chemin.starts_with("wikipedia/commons/") {
        // Une vignette : `…/thumb/a/ab/X.svg/250px-X.svg.png` → `X.svg`.
        let parties: Vec<&str> = chemin.split('/').collect();
        if parties.get(2) == Some(&"thumb") {
            *parties.get(5)?
        } else {
            *parties.last()?
        }
    } else {
        return None;
    };
    let brut = brut.split(['?', '#']).next()?;
    let nom = urlencoding::decode(brut).ok()?.replace('_', " ");
    (!nom.trim().is_empty()).then(|| nom.trim().to_string())
}

/// Ce que les relations d'un label MusicBrainz apportent.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Relations {
    pub qid: Option<String>,
    /// Les fichiers Commons des relations « logo ».
    pub fichiers_commons: Vec<String>,
    /// Le site officiel.
    pub site: Option<String>,
    /// L'identifiant Discogs du label.
    pub discogs: Option<String>,
}

/// `essai` : préfixes d'un banc, admis sans la garde d'hôte public.
pub fn relations_du_label(label: &Value, essai: &[String]) -> Relations {
    let mut r = Relations::default();
    for rel in label["relations"].as_array().into_iter().flatten() {
        let Some(url) = rel["url"]["resource"].as_str() else {
            continue;
        };
        match rel["type"].as_str() {
            Some("wikidata") if r.qid.is_none() => {
                r.qid = url
                    .rsplit('/')
                    .next()
                    .filter(|q| q.starts_with('Q') && q[1..].chars().all(|c| c.is_ascii_digit()))
                    .map(String::from);
            }
            Some("logo") => {
                if let Some(f) = fichier_commons(url)
                    && !r.fichiers_commons.contains(&f)
                {
                    r.fichiers_commons.push(f);
                }
            }
            Some("official site" | "official homepage") if r.site.is_none() => {
                if essai.iter().any(|p| url.starts_with(p))
                    || reqwest::Url::parse(url).is_ok_and(|u| hote_public(&u))
                {
                    r.site = Some(url.to_string());
                }
            }
            Some("discogs") if r.discogs.is_none() => {
                r.discogs = url
                    .split("/label/")
                    .nth(1)
                    .map(|s| {
                        s.chars()
                            .take_while(char::is_ascii_digit)
                            .collect::<String>()
                    })
                    .filter(|s| !s.is_empty());
            }
            _ => {}
        }
    }
    r
}

// ── Wikidata, Commons, Wikipédia ────────────────────────────────────────────

/// Le logo P154 d'un élément Wikidata : celui de rang « preferred », sinon le
/// plus récent (qualificatif P580, date de début), sinon le premier. Les
/// logos de rang « deprecated » ne comptent pas.
pub fn logo_p154(entites: &Value, qid: &str) -> Option<String> {
    let revendications = entites["entities"][qid]["claims"]["P154"].as_array()?;
    let valeur = |c: &Value| {
        c["mainsnak"]["datavalue"]["value"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.trim().to_string())
    };
    let debut = |c: &Value| {
        c["qualifiers"]["P580"][0]["datavalue"]["value"]["time"]
            .as_str()
            .map(String::from)
    };
    let valides: Vec<&Value> = revendications
        .iter()
        .filter(|c| c["rank"].as_str() != Some("deprecated") && valeur(c).is_some())
        .collect();
    if let Some(c) = valides
        .iter()
        .find(|c| c["rank"].as_str() == Some("preferred"))
    {
        return valeur(c);
    }
    // Les dates Wikidata (`+1998-00-00T00:00:00Z`) se comparent comme du texte
    // tant qu'elles ont le même signe et quatre chiffres d'année.
    if let Some(c) = valides
        .iter()
        .filter(|c| debut(c).is_some())
        .max_by_key(|c| debut(c))
    {
        return valeur(c);
    }
    valides.first().and_then(|c| valeur(c))
}

/// Le titre de l'article de la Wikipédia anglaise d'un élément Wikidata.
pub fn article_enwiki(entites: &Value, qid: &str) -> Option<String> {
    entites["entities"][qid]["sitelinks"]["enwiki"]["title"]
        .as_str()
        .filter(|t| !t.trim().is_empty())
        .map(String::from)
}

/// Un champ `extmetadata`, sans balises HTML, espaces resserrés.
fn champ(meta: &Value, cle: &str) -> Option<String> {
    let brut = meta[cle]["value"].as_str()?;
    let mut texte = String::with_capacity(brut.len());
    let mut dans_balise = false;
    for c in brut.chars() {
        match c {
            '<' => dans_balise = true,
            '>' => dans_balise = false,
            c if !dans_balise => texte.push(c),
            _ => {}
        }
    }
    let texte = desechapper(&texte);
    let texte = texte.split_whitespace().collect::<Vec<_>>().join(" ");
    let texte: String = texte.chars().take(200).collect();
    (!texte.is_empty()).then_some(texte)
}

fn desechapper(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#039;", "'")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ")
}

/// Une licence est-elle LIBRE, au sens de Tune ? Domaine public, CC0, CC BY
/// et CC BY-SA (toutes versions). Jamais NC ni ND, jamais « fair use ».
pub fn licence_libre(identifiant: &str, nom_court: &str) -> bool {
    let id = identifiant.trim().to_ascii_lowercase();
    let court = nom_court.trim().to_ascii_lowercase();
    if id.contains("-nc") || id.contains("-nd") || court.contains("fair use") {
        return false;
    }
    id == "pd"
        || id.starts_with("pd-")
        || id == "cc0"
        || id == "cc-zero"
        || id == "cc-by"
        || id.starts_with("cc-by-")
        || (id.is_empty() && (court == "public domain" || court == "cc0"))
}

/// Une vignette que le cache des pochettes sait servir (pas de GIF ni de
/// SVG brut).
fn vignette_servable(url: &str) -> bool {
    let chemin = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
    [".png", ".jpg", ".jpeg"]
        .iter()
        .any(|e| chemin.ends_with(e))
}

/// L'attribution d'un fichier d'après une réponse `imageinfo`
/// (`formatversion=2`) de Commons ou de Wikipédia, et l'adresse de sa
/// vignette. `exiger_libre` : Commons — un fichier non libre est écarté.
/// Sinon (Wikipédia) il est gardé, marqué non libre.
pub fn attribution_du_fichier(
    reponse: &Value,
    source: SourceDuLogo,
    exiger_libre: bool,
    prefixe_essai: &[String],
) -> Option<(String, Attribution)> {
    let page = reponse["query"]["pages"].as_array()?.first()?;
    if page["missing"].as_bool() == Some(true) || page["invalid"].as_bool() == Some(true) {
        return None;
    }
    let info = page["imageinfo"].as_array()?.first()?;
    let meta = &info["extmetadata"];
    let identifiant = champ(meta, "License").unwrap_or_default();
    let nom_court = champ(meta, "LicenseShortName").unwrap_or_default();
    let non_libre_declare = meta["NonFree"]["value"]
        .as_str()
        .is_some_and(|v| !v.trim().is_empty() && v.trim() != "false");
    let libre = !non_libre_declare && licence_libre(&identifiant, &nom_court);
    if exiger_libre && !libre {
        return None;
    }
    let vignette = info["thumburl"]
        .as_str()
        .filter(|u| {
            u.starts_with("https://upload.wikimedia.org/")
                || prefixe_essai.iter().any(|p| u.starts_with(p))
        })
        .filter(|u| vignette_servable(u))?
        .to_string();
    let fichier = page["title"]
        .as_str()?
        .trim_start_matches("File:")
        .to_string();
    let douteux = page["categories"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["title"].as_str())
        .any(|t| CATEGORIES_DOUTEUSES.contains(&t.replace('_', " ").as_str()));
    let source_url = info["descriptionurl"]
        .as_str()
        .filter(|u| u.starts_with("https://"))
        .map(String::from)
        .unwrap_or_else(|| {
            format!(
                "https://commons.wikimedia.org/wiki/File:{}",
                urlencoding::encode(&fichier.replace(' ', "_"))
            )
        });
    let licence = if nom_court.is_empty() {
        identifiant
    } else {
        nom_court
    };
    Some((
        vignette,
        Attribution {
            source,
            statut_licence: if libre {
                StatutDeLicence::Libre
            } else {
                StatutDeLicence::NonLibreIdentification
            },
            fournisseur: match source {
                SourceDuLogo::Wikipedia => String::from("Wikipedia"),
                _ => String::from("Wikimedia Commons"),
            },
            licence: (!licence.is_empty()).then_some(licence),
            licence_url: champ(meta, "LicenseUrl").filter(|u| u.starts_with("http")),
            auteur: champ(meta, "Artist"),
            source_url,
            fichier: Some(fichier),
            douteux,
        },
    ))
}

// ── Site officiel ───────────────────────────────────────────────────────────

/// Un attribut d'une balise HTML, désechappé.
fn attribut(balise: &str, nom: &str) -> Option<String> {
    static MOTIFS: LazyLock<Mutex<std::collections::HashMap<String, regex::Regex>>> =
        LazyLock::new(Default::default);
    let motif = {
        let mut m = MOTIFS.lock().unwrap_or_else(|e| e.into_inner());
        m.entry(nom.to_string())
            .or_insert_with(|| {
                regex::Regex::new(&format!(r#"(?i)(?:^|\s){nom}\s*=\s*["']([^"']*)["']"#))
                    .expect("motif d'attribut")
            })
            .clone()
    };
    motif
        .captures(balise)
        .map(|c| desechapper(c[1].trim()))
        .filter(|v| !v.is_empty())
}

/// Les images candidates d'une page d'accueil, de la plus probable à la
/// moins probable : `apple-touch-icon`, icône déclarée d'au moins
/// [`COTE_MIN_SITE`] px, `<img>` « logo », `og:image`. Adresses absolues en
/// HTTPS seulement, ni SVG ni `data:`.
pub fn candidats_du_site(page: &str, base: &str, essai: &[String]) -> Vec<String> {
    static BALISES: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(?is)<(meta|link|img)\b[^>]*>").expect("balises"));
    let Ok(base) = reqwest::Url::parse(base) else {
        return Vec::new();
    };
    let mut trouves: Vec<(u8, String)> = Vec::new();
    for m in BALISES.captures_iter(page) {
        let balise = &m[0];
        let (rang, adresse) = match m[1].to_ascii_lowercase().as_str() {
            "meta" => {
                let p = attribut(balise, "property")
                    .or_else(|| attribut(balise, "name"))
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                if !matches!(
                    p.as_str(),
                    "og:image" | "og:image:url" | "og:image:secure_url"
                ) {
                    continue;
                }
                (3, attribut(balise, "content"))
            }
            "link" => {
                let rel = attribut(balise, "rel")
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                if rel.contains("apple-touch-icon") {
                    (0, attribut(balise, "href"))
                } else if rel.split_whitespace().any(|r| r == "icon") {
                    let assez = attribut(balise, "sizes").is_some_and(|s| {
                        s.split_whitespace().any(|t| {
                            t.split_once(['x', 'X']).is_some_and(|(w, h)| {
                                w == h && w.parse::<u32>().is_ok_and(|n| n >= COTE_MIN_SITE)
                            })
                        })
                    });
                    if !assez {
                        continue;
                    }
                    (1, attribut(balise, "href"))
                } else {
                    continue;
                }
            }
            _ => {
                let indices = ["class", "id", "alt", "src"]
                    .iter()
                    .filter_map(|a| attribut(balise, a))
                    .collect::<Vec<_>>()
                    .join(" ")
                    .to_ascii_lowercase();
                if !indices.contains("logo") {
                    continue;
                }
                (2, attribut(balise, "src"))
            }
        };
        let Some(adresse) = adresse else { continue };
        let Ok(absolue) = base.join(&adresse) else {
            continue;
        };
        let chemin = absolue.path().to_ascii_lowercase();
        let absolue_texte = absolue.to_string();
        let d_essai = essai.iter().any(|p| absolue_texte.starts_with(p));
        if chemin.ends_with(".svg")
            || (!d_essai && (absolue.scheme() != "https" || !hote_public(&absolue)))
        {
            continue;
        }
        let absolue = absolue_texte;
        if !trouves.iter().any(|(_, a)| a == &absolue) {
            trouves.push((rang, absolue));
        }
    }
    trouves.sort_by_key(|(rang, _)| *rang);
    trouves.into_iter().map(|(_, a)| a).collect()
}

/// Les dimensions d'une image PNG ou JPEG.
pub fn dimensions(octets: &[u8]) -> Option<(u32, u32)> {
    image::ImageReader::new(std::io::Cursor::new(octets))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

/// Une image de site officiel acceptable : carrée ou presque, assez grande.
pub fn image_de_site_acceptable(largeur: u32, hauteur: u32) -> bool {
    if largeur.min(hauteur) < COTE_MIN_SITE {
        return false;
    }
    let r = f64::from(largeur) / f64::from(hauteur);
    (RAPPORT_SITE.0..=RAPPORT_SITE.1).contains(&r)
}

// ── Discogs ─────────────────────────────────────────────────────────────────

/// L'image PRINCIPALE d'une fiche Discogs de label, si elle a la forme d'un
/// logo. Une image secondaire — souvent la photo d'une rondelle — jamais.
pub fn image_principale_discogs(fiche: &Value) -> Option<String> {
    let image = fiche["images"]
        .as_array()?
        .iter()
        .find(|i| i["type"].as_str() == Some("primary"))?;
    let (l, h) = (image["width"].as_u64()?, image["height"].as_u64()?);
    if l.min(h) < COTE_MIN_DISCOGS {
        return None;
    }
    let r = l as f64 / h as f64;
    if !(RAPPORT_DISCOGS.0..=RAPPORT_DISCOGS.1).contains(&r) {
        return None;
    }
    image["uri"]
        .as_str()
        .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
        .map(String::from)
}

// ── La recherche ────────────────────────────────────────────────────────────

impl CatalogueLogosWeb {
    fn mb(&self, chemin: &str) -> String {
        format!("{}/{chemin}", self.transport.musicbrainz)
    }

    /// Le MBID du label, par la release d'abord, par le nom ensuite.
    async fn mbid_du_label(
        &self,
        nom: &str,
        release: Option<&str>,
    ) -> Result<Option<(String, String)>, String> {
        if let Some(release) = release.map(str::trim).filter(|r| !r.is_empty()) {
            let gardee = self.backend.as_ref().and_then(|b| {
                crate::metadata::musicbrainz_release_cache::lire(
                    b,
                    release,
                    "labels",
                    chrono::Utc::now(),
                )
            });
            let lue = match gardee {
                Some(v) => Some(v),
                None => {
                    self.transport
                        .json(
                            &self.mb(&format!("release/{release}")),
                            &[("inc", "labels"), ("fmt", "json")],
                            Limite::MusicBrainz,
                        )
                        .await?
                }
            };
            if let Some(trouve) = lue.as_ref().and_then(|v| label_de_la_release(v, nom)) {
                return Ok(Some(trouve));
            }
        }
        let voulu = nom_compare(nom);
        if !voulu.chars().any(char::is_alphanumeric) {
            return Ok(None);
        }
        let requete = format!(
            "label:\"{}\"",
            crate::metadata::musicbrainz_release::echapper_phrase_lucene(&voulu)
        );
        let reponse = self
            .transport
            .json(
                &self.mb("label"),
                &[("query", &requete), ("limit", "10"), ("fmt", "json")],
                Limite::MusicBrainz,
            )
            .await?;
        Ok(reponse.as_ref().and_then(|r| label_de_la_recherche(r, nom)))
    }

    /// Une requête `imageinfo` sur l'API `api` (Commons ou Wikipédia).
    async fn imageinfo(
        &self,
        api: &str,
        cle: &'static str,
        fichier: &str,
    ) -> Result<Option<Value>, String> {
        let titre = format!("File:{fichier}");
        let largeur = LARGEUR_VIGNETTE.to_string();
        self.transport
            .json(
                api,
                &[
                    ("action", "query"),
                    ("format", "json"),
                    ("formatversion", "2"),
                    ("prop", "imageinfo|categories"),
                    ("iiprop", "url|mime|extmetadata"),
                    ("iiurlwidth", &largeur),
                    (
                        "iiextmetadatafilter",
                        "License|LicenseShortName|LicenseUrl|Artist|NonFree",
                    ),
                    (
                        "clcategories",
                        "Category:PD-self|Category:Self-published work",
                    ),
                    ("titles", &titre),
                ],
                Limite::Wikimedia(cle),
            )
            .await
    }

    /// Source 1 : le premier fichier LIBRE de Commons parmi `fichiers`.
    async fn par_commons(
        &self,
        fichiers: &[String],
    ) -> Result<Option<(String, Attribution)>, String> {
        for fichier in fichiers.iter().take(2) {
            let reponse = self
                .imageinfo(&self.transport.commons, CLE_COMMONS, fichier)
                .await?;
            if let Some(trouve) = reponse.as_ref().and_then(|r| {
                attribution_du_fichier(
                    r,
                    SourceDuLogo::Commons,
                    true,
                    &self.transport.prefixes_d_essai,
                )
            }) {
                if trouve.1.douteux {
                    tracing::info!(fichier = ?trouve.1.fichier, licence = ?trouve.1.licence,
                        "logo_label_licence_douteuse");
                }
                return Ok(Some(trouve));
            }
            tracing::debug!(fichier = %fichier, "logo_label_commons_non_libre_ou_absent");
        }
        Ok(None)
    }

    /// Source 2 : l'image de l'article de la Wikipédia anglaise, libre ou non.
    async fn par_wikipedia(&self, article: &str) -> Result<Option<(String, Attribution)>, String> {
        let page = self
            .transport
            .json(
                &self.transport.wikipedia,
                &[
                    ("action", "query"),
                    ("format", "json"),
                    ("formatversion", "2"),
                    ("redirects", "1"),
                    ("prop", "pageprops"),
                    ("ppprop", "page_image|page_image_free"),
                    ("titles", article),
                ],
                Limite::Wikimedia(CLE_WIKIPEDIA),
            )
            .await?;
        let proprietes = page
            .as_ref()
            .and_then(|p| p["query"]["pages"].as_array()?.first().cloned())
            .map(|p| p["pageprops"].clone())
            .unwrap_or(Value::Null);
        let Some(fichier) = proprietes["page_image_free"]
            .as_str()
            .or_else(|| proprietes["page_image"].as_str())
            .map(|f| f.replace('_', " "))
        else {
            return Ok(None);
        };
        let reponse = self
            .imageinfo(&self.transport.wikipedia, CLE_WIKIPEDIA, &fichier)
            .await?;
        Ok(reponse.as_ref().and_then(|r| {
            attribution_du_fichier(
                r,
                SourceDuLogo::Wikipedia,
                false,
                &self.transport.prefixes_d_essai,
            )
        }))
    }

    /// Une adresse d'image qu'on peut lire : hôte public en HTTPS, ou banc.
    fn image_autorisee(&self, url: &str) -> bool {
        self.transport.d_essai(url)
            || reqwest::Url::parse(url).is_ok_and(|u| u.scheme() == "https" && hote_public(&u))
    }

    /// Source 3 : le logo déclaré par le site officiel. Une panne du site
    /// n'est pas une erreur de la passe : le site est peut-être mort pour de
    /// bon.
    async fn par_le_site(&self, site: &str) -> Option<(String, Attribution)> {
        let hote = reqwest::Url::parse(site).ok()?.host_str()?.to_string();
        let client = self.transport.client_sites.as_ref()?;
        let (page, finale) = self
            .transport
            .octets(client, site, Limite::Site(&hote), TAILLE_MAX_PAGE, true)
            .await
            .map_err(|e| tracing::debug!(site, erreur = %e, "logo_label_site_injoignable"))
            .ok()?;
        let page = String::from_utf8_lossy(&page);
        let candidats = candidats_du_site(&page, &finale, &self.transport.prefixes_d_essai);
        for url in candidats.iter().take(CANDIDATS_MAX_SITE) {
            if !self.image_autorisee(url) {
                continue;
            }
            let hote_image = reqwest::Url::parse(url)
                .ok()
                .and_then(|u| u.host_str().map(String::from))
                .unwrap_or_default();
            let Ok((octets, _)) = self
                .transport
                .octets(
                    client,
                    url,
                    Limite::Site(&hote_image),
                    TAILLE_MAX_IMAGE,
                    false,
                )
                .await
            else {
                continue;
            };
            let acceptable = crate::library::artwork::sniff_image_ext(&octets).is_some()
                && dimensions(&octets).is_some_and(|(l, h)| image_de_site_acceptable(l, h));
            if !acceptable {
                tracing::debug!(url = %url, "logo_label_site_image_ecartee");
                continue;
            }
            *self
                .derniere_image
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some((url.clone(), octets));
            return Some((
                url.clone(),
                Attribution {
                    source: SourceDuLogo::SiteOfficiel,
                    statut_licence: StatutDeLicence::Inconnue,
                    fournisseur: hote.trim_start_matches("www.").to_string(),
                    licence: None,
                    licence_url: None,
                    auteur: None,
                    source_url: site.to_string(),
                    fichier: None,
                    douteux: false,
                },
            ));
        }
        None
    }

    /// Source 4 : l'image principale de la fiche Discogs du label.
    async fn par_discogs(&self, id: &str) -> Result<Option<(String, Attribution)>, String> {
        let fiche = self
            .transport
            .json(
                &format!("{}/labels/{id}", self.transport.discogs_api),
                &[],
                Limite::Discogs,
            )
            .await?;
        Ok(fiche
            .as_ref()
            .and_then(image_principale_discogs)
            .filter(|u| self.image_autorisee(u))
            .map(|url| {
                (
                    url,
                    Attribution {
                        source: SourceDuLogo::Discogs,
                        statut_licence: StatutDeLicence::Inconnue,
                        fournisseur: String::from("Discogs"),
                        licence: None,
                        licence_url: None,
                        auteur: None,
                        source_url: format!("{}/label/{id}", self.transport.discogs_site),
                        fichier: None,
                        douteux: false,
                    },
                )
            }))
    }
}

#[async_trait::async_trait]
impl CatalogueDeLabels for CatalogueLogosWeb {
    /// `album_du_catalogue` est ici le MBID de la release d'un album local.
    async fn trouver_label(
        &self,
        nom: &str,
        album_du_catalogue: Option<&str>,
    ) -> Result<Option<LabelDuCatalogue>, String> {
        let Some((mbid, nom_mb)) = self.mbid_du_label(nom, album_du_catalogue).await? else {
            return Ok(None);
        };
        let Some(label) = self
            .transport
            .json(
                &self.mb(&format!("label/{mbid}")),
                &[("inc", "url-rels"), ("fmt", "json")],
                Limite::MusicBrainz,
            )
            .await?
        else {
            return Ok(None);
        };
        let mut rel = relations_du_label(&label, &self.transport.prefixes_d_essai);
        let mut article = None;
        if let Some(qid) = rel.qid.clone() {
            let entites = self
                .transport
                .json(
                    &self.transport.wikidata,
                    &[
                        ("action", "wbgetentities"),
                        ("format", "json"),
                        ("props", "claims|sitelinks"),
                        ("sitefilter", "enwiki"),
                        ("ids", &qid),
                    ],
                    Limite::Wikimedia(CLE_WIKIDATA),
                )
                .await?;
            if let Some(entites) = entites.as_ref() {
                if let Some(p154) = logo_p154(entites, &qid) {
                    rel.fichiers_commons.retain(|f| f != &p154);
                    rel.fichiers_commons.insert(0, p154);
                }
                article = article_enwiki(entites, &qid);
            }
        }
        let mut trouve = self.par_commons(&rel.fichiers_commons).await?;
        if trouve.is_none()
            && let Some(article) = article.as_deref()
        {
            trouve = self.par_wikipedia(article).await?;
        }
        if trouve.is_none()
            && let Some(site) = rel.site.as_deref()
        {
            trouve = self.par_le_site(site).await;
        }
        if trouve.is_none()
            && let Some(id) = rel.discogs.as_deref()
        {
            trouve = self.par_discogs(id).await?;
        }
        let (image_url, attribution) = match trouve {
            Some((url, a)) => (Some(url), Some(a)),
            None => (None, None),
        };
        Ok(Some(LabelDuCatalogue {
            id: mbid,
            nom: nom_mb,
            image_url,
            attribution,
        }))
    }

    async fn telecharger_image(&self, url: &str) -> Result<Vec<u8>, String> {
        if let Some((deja, octets)) = self
            .derniere_image
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            && deja == url
        {
            return Ok(octets);
        }
        if !self.image_autorisee(url) {
            return Err(format!("adresse refusée : {url}"));
        }
        let hote = reqwest::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(String::from))
            .unwrap_or_default();
        let limite = if hote == "upload.wikimedia.org" {
            Limite::Wikimedia(CLE_COMMONS)
        } else if hote.ends_with("discogs.com") {
            Limite::Discogs
        } else {
            Limite::Site(&hote)
        };
        let client = match limite {
            Limite::Site(_) => self
                .transport
                .client_sites
                .as_ref()
                .ok_or_else(|| String::from("client des sites indisponible"))?,
            _ => &self.transport.client,
        };
        self.transport
            .octets(client, url, limite, TAILLE_MAX_IMAGE, false)
            .await
            .map(|(o, _)| o)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::logos_labels::{CacheLogosLabels, Demande, PASSE_MAX, Statut};
    use axum::extract::{Path as Chemin, Query};
    use axum::routing::get;
    use axum::{Json, Router};
    use serde_json::json;
    use std::collections::HashMap as Carte;
    use std::time::{Duration, Instant};

    const ESSAI: &[String] = &[];

    // ── Rapprochement ───────────────────────────────────────────────────────

    #[test]
    fn la_release_donne_le_label_dont_le_nom_concorde_meme_en_ligne_de_copyright() {
        let release = json!({"label-info": [
            {"catalog-number": "x", "label": {"id": "mb-virgin", "name": "Virgin France S.A."}},
            {"catalog-number": "y", "label": {"id": "mb-source", "name": "Source"}},
        ]});
        assert_eq!(
            label_de_la_release(&release, "source"),
            Some(("mb-source".into(), "Source".into()))
        );
        let emi = json!({"label-info": [{"label": {"id": "mb-emi", "name": "EMI Records Ltd."}}]});
        assert_eq!(
            label_de_la_release(&emi, "2003 EMI Records Ltd.").map(|t| t.0),
            Some("mb-emi".into())
        );
        // Un album mal étiqueté ne donne pas son label à un autre.
        assert_eq!(label_de_la_release(&release, "Rhino"), None);
        assert_eq!(label_de_la_release(&json!({}), "Source"), None);
    }

    fn candidat(id: &str, nom: &str, score: i64) -> Value {
        json!({"id": id, "name": nom, "score": score})
    }

    #[test]
    fn la_recherche_ne_retient_qu_un_nom_exact_seul_ou_nettement_devant() {
        let r = json!({"labels": [candidat("1", "Warp Records", 100), candidat("2", "Warp", 90)]});
        assert_eq!(
            label_de_la_recherche(&r, "warp"),
            Some(("2".into(), "Warp".into()))
        );
        let bmg = json!({"labels": [candidat("a", "BMG", 100), candidat("b", "BMG", 92)]});
        assert_eq!(label_de_la_recherche(&bmg, "BMG"), None);
        let ok = json!({"labels": [candidat("a", "Echo", 85), candidat("b", "Echo", 100)]});
        assert_eq!(
            label_de_la_recherche(&ok, "Echo").map(|t| t.0),
            Some("b".into())
        );
        let alias = json!({"labels": [{"id": "m", "name": "Mercury Records", "score": 100,
            "aliases": [{"name": "Mercury"}]}]});
        assert_eq!(
            label_de_la_recherche(&alias, "MERCURY").map(|t| t.0),
            Some("m".into())
        );
        let jazz = json!({"labels": [candidat("c", "Concord Jazz", 100)]});
        assert_eq!(label_de_la_recherche(&jazz, "Jazz"), None);
    }

    // ── Relations, garde réseau ─────────────────────────────────────────────

    #[test]
    fn seuls_les_fichiers_de_commons_sont_lus_comme_fichiers_commons() {
        assert_eq!(
            fichier_commons("https://commons.wikimedia.org/wiki/File:Blue_Note_Records.svg")
                .as_deref(),
            Some("Blue Note Records.svg")
        );
        assert_eq!(
            fichier_commons(
                "https://upload.wikimedia.org/wikipedia/commons/4/4b/Na%C3%AFve_logo.png"
            )
            .as_deref(),
            Some("Naïve logo.png")
        );
        assert_eq!(
            fichier_commons(
                "https://upload.wikimedia.org/wikipedia/commons/thumb/a/ab/ECM_Records.svg/250px-ECM_Records.svg.png"
            )
            .as_deref(),
            Some("ECM Records.svg")
        );
        for ailleurs in [
            "https://upload.wikimedia.org/wikipedia/en/a/ab/Fair_use_logo.png",
            "https://images1.wikia.nocookie.net/x/logo.png",
            "http://i61.tinypic.com/abc.jpg",
            "javascript:alert(1)",
        ] {
            assert_eq!(fichier_commons(ailleurs), None, "{ailleurs}");
        }
    }

    #[test]
    fn les_relations_donnent_wikidata_commons_site_et_discogs() {
        let label = json!({"relations": [
            {"type": "logo", "url": {"resource": "https://seeklogo.com/x.png"}},
            {"type": "logo", "url": {"resource": "https://commons.wikimedia.org/wiki/File:A.svg"}},
            {"type": "wikidata", "url": {"resource": "https://www.wikidata.org/wiki/Q885833"}},
            {"type": "official site", "url": {"resource": "http://192.168.1.18/"}},
            {"type": "official site", "url": {"resource": "https://www.bluenote.com/"}},
            {"type": "discogs", "url": {"resource": "https://www.discogs.com/label/281-Blue-Note"}},
        ]});
        assert_eq!(
            relations_du_label(&label, ESSAI),
            Relations {
                qid: Some("Q885833".into()),
                fichiers_commons: vec!["A.svg".into()],
                site: Some("https://www.bluenote.com/".into()),
                discogs: Some("281".into()),
            },
            "un site sur une adresse IP n'est jamais retenu"
        );
    }

    #[test]
    fn la_garde_reseau_refuse_ip_localhost_et_reseau_local() {
        let public = |u: &str| hote_public(&reqwest::Url::parse(u).unwrap());
        assert!(public("https://www.bluenote.com/"));
        assert!(public("http://ecmrecords.com/logo.png"));
        for interdit in [
            "http://127.0.0.1/",
            "https://192.168.1.18:8888/",
            "http://[::1]/",
            "http://localhost/",
            "http://nas.local/",
            "http://box.lan/",
            "http://0x7f.1/",
            "http://2130706433/",
            "http://user:mdp@exemple.org/",
            "ftp://exemple.org/",
            "file:///etc/passwd",
        ] {
            assert!(!public(interdit), "{interdit} devait être refusé");
        }
    }

    #[test]
    fn seules_les_adresses_publiques_passent_la_resolution() {
        let publique = |a: &str| adresse_publique(a.parse().unwrap());
        for ok in [
            "93.184.216.34",
            "151.101.1.69",
            "2606:4700::6810:84e5",
            "2a00:1450:4007:80e::200e",
        ] {
            assert!(publique(ok), "{ok} est publique");
        }
        for interdite in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.18",
            "169.254.169.254",
            "0.0.0.0",
            "100.64.0.1",
            "224.0.0.1",
            "255.255.255.255",
            "198.18.0.1",
            "192.0.0.8",
            "240.0.0.1",
            "::1",
            "::",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "fec0::1",
            "ff02::1",
            "::ffff:192.168.1.18",
            "::ffff:127.0.0.1",
            "64:ff9b::a00:1",
            "2001:db8::1",
            "::7f00:1",
        ] {
            assert!(!publique(interdite), "{interdite} devait être refusée");
        }
    }

    /// Le client de PRODUCTION des sites refuse de se connecter à un nom
    /// dont l'adresse n'est pas publique — ici `localhost`, qui résout en
    /// 127.0.0.1 — alors qu'un client ordinaire y arrive.
    #[tokio::test]
    async fn le_client_des_sites_ne_se_connecte_qu_a_une_adresse_publique() {
        let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = ecoute.local_addr().unwrap().port();
        let app = Router::new().route("/", get(|| async { "page" }));
        tokio::spawn(async move {
            axum::serve(ecoute, app).await.unwrap();
        });
        let url = format!("http://localhost:{port}/");
        assert!(
            crate::http::client::builder()
                .build()
                .expect("client HTTP partagé")
                .get(&url)
                .send()
                .await
                .is_ok(),
            "le témoin : le faux site répond à un client ordinaire"
        );
        let client = client_des_sites().expect("client des sites");
        let refus = client.get(&url).send().await;
        assert!(
            refus.is_err(),
            "connexion acceptée vers une adresse de bouclage : {refus:?}"
        );
        let resolu =
            reqwest::dns::Resolve::resolve(&ResolveurPublic, "localhost".parse().unwrap()).await;
        assert!(resolu.is_err(), "localhost ne doit donner aucune adresse");
    }

    // ── Wikidata, Commons, Wikipédia ────────────────────────────────────────

    fn revendication(fichier: &str, rang: &str, debut: Option<&str>) -> Value {
        let mut c = json!({"rank": rang, "mainsnak": {"datavalue": {"value": fichier}}});
        if let Some(d) = debut {
            c["qualifiers"] = json!({"P580": [{"datavalue": {"value": {"time": d}}}]});
        }
        c
    }

    #[test]
    fn le_logo_p154_prefere_le_rang_preferred_puis_le_plus_recent() {
        let entites = |claims: Vec<Value>| {
            json!({"entities": {"Q1": {"claims": {"P154": claims},
            "sitelinks": {"enwiki": {"title": "Blue Note Records"}}}}})
        };
        assert_eq!(
            logo_p154(
                &entites(vec![
                    revendication("Ancien.svg", "normal", Some("+1960-00-00T00:00:00Z")),
                    revendication("Choisi.svg", "preferred", None),
                ]),
                "Q1"
            )
            .as_deref(),
            Some("Choisi.svg")
        );
        assert_eq!(
            logo_p154(
                &entites(vec![
                    revendication("1960.svg", "normal", Some("+1960-00-00T00:00:00Z")),
                    revendication("2019.svg", "normal", Some("+2019-01-01T00:00:00Z")),
                    revendication("Retire.svg", "deprecated", Some("+2024-01-01T00:00:00Z")),
                ]),
                "Q1"
            )
            .as_deref(),
            Some("2019.svg")
        );
        assert_eq!(logo_p154(&entites(vec![]), "Q1"), None);
        assert_eq!(
            article_enwiki(&entites(vec![]), "Q1").as_deref(),
            Some("Blue Note Records")
        );
        assert_eq!(article_enwiki(&json!({}), "Q1"), None);
    }

    const VIGNETTE: &str =
        "https://upload.wikimedia.org/wikipedia/commons/thumb/a/ab/B.svg/250px-B.svg.png";

    fn page_fichier(licence: &str, court: &str, categories: &[&str], vignette: &str) -> Value {
        json!({"query": {"pages": [{
            "title": "File:Blue Note Records.svg",
            "categories": categories.iter().map(|c| json!({"title": c})).collect::<Vec<_>>(),
            "imageinfo": [{
                "thumburl": vignette,
                "descriptionurl": "https://commons.wikimedia.org/wiki/File:Blue_Note_Records.svg",
                "extmetadata": {
                    "License": {"value": licence},
                    "LicenseShortName": {"value": court},
                    "Artist": {"value": "<a href=\"//x\">Blue Note</a> &amp; Co"},
                }
            }]
        }]}})
    }

    fn commons(p: &Value) -> Option<(String, Attribution)> {
        attribution_du_fichier(p, SourceDuLogo::Commons, true, ESSAI)
    }

    #[test]
    fn commons_ne_garde_que_les_licences_libres_avec_leur_attribution() {
        let (vignette, a) =
            commons(&page_fichier("pd", "Public domain", &[], VIGNETTE)).expect("PD");
        assert_eq!(vignette, VIGNETTE);
        assert_eq!(a.source, SourceDuLogo::Commons);
        assert_eq!(a.statut_licence, StatutDeLicence::Libre);
        assert_eq!(a.licence.as_deref(), Some("Public domain"));
        assert_eq!(a.auteur.as_deref(), Some("Blue Note & Co"));
        assert_eq!(a.fichier.as_deref(), Some("Blue Note Records.svg"));
        assert_eq!(
            a.source_url,
            "https://commons.wikimedia.org/wiki/File:Blue_Note_Records.svg"
        );
        assert!(!a.douteux);
        for (id, court) in [
            ("cc0", "CC0"),
            ("cc-by-4.0", "CC BY 4.0"),
            ("cc-by-sa-3.0", "CC BY-SA 3.0"),
        ] {
            assert!(
                commons(&page_fichier(id, court, &[], VIGNETTE)).is_some(),
                "{id} est libre"
            );
        }
        for (id, court) in [
            ("cc-by-nc-sa-4.0", "CC BY-NC-SA 4.0"),
            ("cc-by-nd-4.0", "CC BY-ND 4.0"),
            ("", "Fair use"),
            ("arr", "All rights reserved"),
            ("", ""),
        ] {
            assert!(
                commons(&page_fichier(id, court, &[], VIGNETTE)).is_none(),
                "« {court} » n'est pas libre"
            );
        }
        let mut non_libre = page_fichier("pd", "Public domain", &[], VIGNETTE);
        non_libre["query"]["pages"][0]["imageinfo"][0]["extmetadata"]["NonFree"] =
            json!({"value": "true"});
        assert!(commons(&non_libre).is_none());
        assert!(
            commons(&page_fichier(
                "pd",
                "Public domain",
                &[],
                "https://exemple.org/b.png"
            ))
            .is_none()
        );
        assert!(
            commons(&page_fichier(
                "pd",
                "Public domain",
                &[],
                "https://upload.wikimedia.org/x/Anti.gif"
            ))
            .is_none(),
            "une vignette GIF ne se sert pas"
        );
        assert!(
            commons(&json!({"query": {"pages": [{"title": "File:X", "missing": true}]}})).is_none()
        );
    }

    #[test]
    fn wikipedia_garde_un_logo_non_libre_marque_usage_d_identification() {
        let p = page_fichier(
            "",
            "Fair use",
            &[],
            "https://upload.wikimedia.org/wikipedia/en/thumb/e/ec/ECM.svg/250px-ECM.svg.png",
        );
        let (_, a) =
            attribution_du_fichier(&p, SourceDuLogo::Wikipedia, false, ESSAI).expect("gardé");
        assert_eq!(a.source, SourceDuLogo::Wikipedia);
        assert_eq!(a.statut_licence, StatutDeLicence::NonLibreIdentification);
        assert_eq!(a.licence.as_deref(), Some("Fair use"));
        assert_eq!(a.fournisseur, "Wikipedia");
        let (_, libre) = attribution_du_fichier(
            &page_fichier("pd", "Public domain", &[], VIGNETTE),
            SourceDuLogo::Wikipedia,
            false,
            ESSAI,
        )
        .unwrap();
        assert_eq!(libre.statut_licence, StatutDeLicence::Libre);
    }

    #[test]
    fn un_logo_declare_oeuvre_personnelle_est_marque_douteux() {
        // Mesuré sur Commons le 26/10 : « BEATLES MANZANA.PNG » (Apple
        // Records), PD-self déclaré par un tiers.
        let (_, a) = commons(&page_fichier(
            "pd",
            "Public domain",
            &["Category:PD-self", "Category:Self-published work"],
            VIGNETTE,
        ))
        .unwrap();
        assert!(a.douteux);
        let (_, a) = commons(&page_fichier(
            "cc-by-sa-4.0",
            "CC BY-SA 4.0",
            &["Category:Self-published_work"],
            VIGNETTE,
        ))
        .unwrap();
        assert!(a.douteux);
    }

    // ── Site officiel, Discogs ──────────────────────────────────────────────

    #[test]
    fn le_site_propose_ses_icones_avant_son_og_image() {
        let page = r#"<html><head>
            <meta property="og:image" content="https://www.label.com/banniere.jpg">
            <link rel="icon" sizes="32x32" href="/favicon-32.png">
            <link rel="icon" sizes="192x192" href="/icone-192.png">
            <link rel="apple-touch-icon" href="/apple-touch-icon.png">
            <link rel="stylesheet" href="/x.css">
            </head><body>
            <img class="site-logo" src="logo.svg"><img alt="Logo du label" src="/img/logo.png">
            <img src="http://www.label.com/photo.jpg">
            <img src="https://192.168.1.1/logo.png">
            </body></html>"#;
        assert_eq!(
            candidats_du_site(page, "https://www.label.com/accueil/", ESSAI),
            vec![
                "https://www.label.com/apple-touch-icon.png",
                "https://www.label.com/icone-192.png",
                "https://www.label.com/img/logo.png",
                "https://www.label.com/banniere.jpg",
            ],
            "ni petite icône, ni SVG, ni photo sans « logo », ni adresse IP"
        );
    }

    #[test]
    fn une_image_de_site_doit_etre_carree_ou_presque_et_assez_grande() {
        assert!(image_de_site_acceptable(180, 180));
        assert!(image_de_site_acceptable(512, 400));
        assert!(
            !image_de_site_acceptable(1200, 630),
            "une bannière og:image"
        );
        assert!(!image_de_site_acceptable(64, 64), "trop petite");
        assert!(!image_de_site_acceptable(300, 600));
        assert_eq!(dimensions(&png(180, 120)), Some((180, 120)));
        assert_eq!(dimensions(b"<html>"), None);
    }

    #[test]
    fn discogs_ne_prend_que_l_image_principale_en_forme_de_logo() {
        let fiche = |images: Value| json!({"name": "Blue Note", "images": images});
        let img = |t: &str, w: u64, h: u64, u: &str| json!({"type": t, "width": w, "height": h, "uri": u});
        assert_eq!(
            image_principale_discogs(&fiche(json!([
                img("secondary", 600, 600, "https://i.discogs.com/rondelle.jpg"),
                img("primary", 240, 151, "https://i.discogs.com/ecm.jpg"),
            ])))
            .as_deref(),
            Some("https://i.discogs.com/ecm.jpg")
        );
        assert_eq!(
            image_principale_discogs(&fiche(json!([img(
                "secondary",
                600,
                600,
                "https://i.discogs.com/r.jpg"
            )]))),
            None,
            "jamais une image secondaire"
        );
        assert_eq!(
            image_principale_discogs(&fiche(json!([img("primary", 600, 136, "https://i/x")]))),
            None
        );
        assert_eq!(
            image_principale_discogs(&fiche(json!([img("primary", 80, 80, "https://i/x")]))),
            None
        );
        assert_eq!(
            image_principale_discogs(&fiche(json!([img("primary", 300, 300, "")]))),
            None
        );
        assert_eq!(image_principale_discogs(&json!({})), None);
    }

    // ── La passe, contre de faux services ───────────────────────────────────

    fn png(largeur: u32, hauteur: u32) -> Vec<u8> {
        let mut o = std::io::Cursor::new(Vec::new());
        image::RgbaImage::new(largeur, hauteur)
            .write_to(&mut o, image::ImageFormat::Png)
            .unwrap();
        o.into_inner()
    }

    #[derive(Default)]
    struct Journal {
        mb: Vec<(Instant, String)>,
        autres: Vec<String>,
    }

    /// De faux MusicBrainz, Wikidata, Commons, Wikipédia, Discogs et site,
    /// sur 127.0.0.1, sous une seule racine. Ils connaissent :
    /// - « Blue Note » par sa release, logo libre sur Commons ;
    /// - « ECM Records » par le nom, logo Commons non libre → Wikipédia
    ///   (fair use) ;
    /// - « Site Records » : ni Wikidata ni Commons, un site officiel avec une
    ///   bannière og:image et un apple-touch-icon ;
    /// - « Disco Records » : seulement Discogs ;
    /// - deux homonymes « BMG ».
    async fn services_simules() -> (String, Arc<Mutex<Journal>>) {
        let journal = Arc::new(Mutex::new(Journal::default()));
        let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let racine = format!("http://{}", ecoute.local_addr().unwrap());
        let j = journal.clone();
        let mb = move |chemin: String| {
            j.lock().unwrap().mb.push((Instant::now(), chemin));
        };
        let (m1, m2, m3) = (mb.clone(), mb.clone(), mb);
        let ja = journal.clone();
        let note = move |s: String| ja.lock().unwrap().autres.push(s);
        let (n1, n2, n3, n4, n5, n6) = (
            note.clone(),
            note.clone(),
            note.clone(),
            note.clone(),
            note.clone(),
            note,
        );
        let r1 = racine.clone();
        let r2 = racine.clone();
        let r3 = racine.clone();
        let r4 = racine.clone();
        let r5 = racine.clone();
        let app = Router::new()
            .route(
                "/mb/release/{id}",
                get(move |Chemin(id): Chemin<String>| {
                    m1(format!("release/{id}"));
                    async move {
                        Json(json!({"id": id, "label-info": [
                            {"label": {"id": "mb-bluenote", "name": "Blue Note"}}]}))
                    }
                }),
            )
            .route(
                "/mb/label",
                get(move |Query(q): Query<Carte<String, String>>| {
                    m2(format!("label?{}", q["query"]));
                    async move {
                        let l = |id: &str, n: &str, s: i64| json!({"id": id, "name": n, "score": s});
                        let labels = if q["query"].contains("ECM") {
                            json!([l("mb-ecm", "ECM Records", 100), l("mb-ecm2", "ECM Records Ltd", 80)])
                        } else if q["query"].contains("BMG") {
                            json!([l("b1", "BMG", 100), l("b2", "BMG", 95)])
                        } else if q["query"].contains("Site Records") {
                            json!([l("mb-site", "Site Records", 100)])
                        } else if q["query"].contains("Disco Records") {
                            json!([l("mb-disco", "Disco Records", 100)])
                        } else {
                            json!([])
                        };
                        Json(json!({"labels": labels}))
                    }
                }),
            )
            .route(
                "/mb/label/{id}",
                get(move |Chemin(id): Chemin<String>| {
                    m3(format!("label/{id}"));
                    let r = r1.clone();
                    async move {
                        let rel = |t: &str, u: &str| json!({"type": t, "url": {"resource": u}});
                        let relations = match id.as_str() {
                            "mb-bluenote" => json!([rel("wikidata", "https://www.wikidata.org/wiki/Q885833"),
                                rel("logo", "http://i61.tinypic.com/x.jpg")]),
                            "mb-ecm" => json!([rel("wikidata", "https://www.wikidata.org/wiki/Q311439"),
                                rel("discogs", "https://www.discogs.com/label/5-ECM-Records")]),
                            "mb-site" => json!([rel("official site", &format!("{r}/site/")),
                                rel("discogs", "https://www.discogs.com/label/77-Site")]),
                            "mb-disco" => json!([rel("discogs", "https://www.discogs.com/label/281-Disco")]),
                            _ => json!([]),
                        };
                        Json(json!({"id": id, "relations": relations}))
                    }
                }),
            )
            .route(
                "/wikidata",
                get(move |Query(q): Query<Carte<String, String>>| {
                    n1(format!("wikidata {}", q["ids"]));
                    async move {
                        let ids = q["ids"].clone();
                        let (fichier, article) = if ids == "Q885833" {
                            ("Blue Note Records.svg", "Blue Note Records")
                        } else {
                            ("ECM non libre.png", "ECM Records")
                        };
                        let mut entites = serde_json::Map::new();
                        entites.insert(
                            ids,
                            json!({"claims": {"P154": [
                                {"rank": "normal", "mainsnak": {"datavalue": {"value": fichier}}}]},
                                "sitelinks": {"enwiki": {"title": article}}}),
                        );
                        Json(json!({ "entities": entites }))
                    }
                }),
            )
            .route(
                "/commons",
                get(move |Query(q): Query<Carte<String, String>>| {
                    n2(format!("commons {}", q["titles"]));
                    let r = r2.clone();
                    async move {
                        let libre = q["titles"].contains("Blue Note");
                        Json(json!({"query": {"pages": [{
                            "title": q["titles"],
                            "imageinfo": [{
                                "thumburl": format!("{r}/images/bn.png"),
                                "descriptionurl": "https://commons.wikimedia.org/wiki/File:Blue_Note_Records.svg",
                                "extmetadata": {
                                    "License": {"value": if libre { "pd" } else { "" }},
                                    "LicenseShortName": {"value": if libre { "Public domain" } else { "Fair use" }},
                                    "Artist": {"value": "Reid Miles"}}}]}]}}))
                    }
                }),
            )
            .route(
                "/wikipedia",
                get(move |Query(q): Query<Carte<String, String>>| {
                    n3(format!("wikipedia {}", q["titles"]));
                    let r = r3.clone();
                    async move {
                        if q.get("prop").map(String::as_str) == Some("pageprops") {
                            Json(json!({"query": {"pages": [{"title": q["titles"],
                                "pageprops": {"page_image": "ECM_Records_logo.png"}}]}}))
                        } else {
                            Json(json!({"query": {"pages": [{
                                "title": "File:ECM Records logo.png",
                                "imageinfo": [{
                                    "thumburl": format!("{r}/images/ecm.png"),
                                    "descriptionurl": "https://en.wikipedia.org/wiki/File:ECM_Records_logo.png",
                                    "extmetadata": {"LicenseShortName": {"value": "Fair use"},
                                                    "NonFree": {"value": "true"}}}]}]}}))
                        }
                    }
                }),
            )
            .route(
                "/site/",
                get(move || {
                    n4("site".into());
                    let r = r4.clone();
                    async move {
                        axum::response::Html(format!(
                            r#"<head><meta property="og:image" content="{r}/site/banniere.png">
                               <link rel="apple-touch-icon" href="/site/icone.png"></head>"#
                        ))
                    }
                }),
            )
            .route("/site/banniere.png", get(|| async { png(1200, 630) }))
            .route("/site/icone.png", get(|| async { png(180, 180) }))
            .route(
                "/discogs/labels/{id}",
                get(move |Chemin(id): Chemin<String>| {
                    n5(format!("discogs {id}"));
                    let r = r5.clone();
                    async move {
                        Json(json!({"id": id, "images": [
                            {"type": "secondary", "width": 600, "height": 600, "uri": format!("{r}/images/rondelle.png")},
                            {"type": "primary", "width": 300, "height": 300, "uri": format!("{r}/images/disco.png")}]}))
                    }
                }),
            )
            .route(
                "/images/{nom}",
                get(move |Chemin(nom): Chemin<String>| {
                    n6(format!("image {nom}"));
                    async { png(250, 250) }
                }),
            );
        tokio::spawn(async move {
            axum::serve(ecoute, app).await.unwrap();
        });
        (racine, journal)
    }

    fn catalogue(racine: &str, limiteur_mb: &'static RateLimiter) -> CatalogueLogosWeb {
        let rapide: &'static RateLimiter = Box::leak(Box::new(RateLimiter::per_second(1000.0)));
        let mut t = Transport::sous_base(
            crate::http::client::builder()
                .build()
                .expect("client HTTP partagé"),
            racine,
        );
        t.limiteur_mb = limiteur_mb;
        t.cle_mb = "mb-essai";
        t.limiteur_wikimedia = rapide;
        t.limiteur_discogs = rapide;
        t.limiteur_sites = rapide;
        CatalogueLogosWeb::new(t, None)
    }

    fn rapide() -> &'static RateLimiter {
        Box::leak(Box::new(RateLimiter::per_second(1000.0)))
    }

    #[tokio::test]
    async fn la_release_puis_wikidata_puis_commons_donnent_un_logo_libre_attribue() {
        let (racine, journal) = services_simules().await;
        let dossier = crate::test_scratch::scratch_dir("logos-web-commons");
        let cache = CacheLogosLabels::ouvrir_index(dossier.path(), FICHIER_INDEX_WEB);
        let cat = catalogue(&racine, rapide());

        let n = cache
            .resoudre(
                &cat,
                vec![Demande {
                    nom: "Blue Note".into(),
                    album_du_catalogue: Some("rel-1".into()),
                }],
                || 1_000,
                Duration::ZERO,
            )
            .await;

        assert_eq!(n, 1, "le logo libre est trouvé");
        let entree = cache.entree("blue note").unwrap();
        assert_eq!(entree.statut, Statut::Trouve);
        assert_eq!(entree.mb_label_id.as_deref(), Some("mb-bluenote"));
        assert_eq!(
            entree.qobuz_label_id, None,
            "un MBID n'est pas un identifiant Qobuz"
        );
        let a = entree.attribution.expect("attribution gardée");
        assert_eq!(
            (a.source, a.statut_licence),
            (SourceDuLogo::Commons, StatutDeLicence::Libre)
        );
        assert_eq!(a.auteur.as_deref(), Some("Reid Miles"));
        assert!(
            cache.image("Blue Note").is_some(),
            "vignette dans le cache des pochettes"
        );
        let j = journal.lock().unwrap();
        assert_eq!(
            j.mb.iter().map(|m| m.1.as_str()).collect::<Vec<_>>(),
            vec!["release/rel-1", "label/mb-bluenote"],
            "par la release : aucune recherche par le nom"
        );
        assert!(
            !j.autres
                .iter()
                .any(|a| a.starts_with("wikipedia") || a == "site" || a.starts_with("discogs")),
            "Commons a suffi : aucune autre source interrogée ({:?})",
            j.autres
        );
        let relu = CacheLogosLabels::ouvrir_index(dossier.path(), FICHIER_INDEX_WEB);
        assert_eq!(
            relu.entree("Blue Note")
                .unwrap()
                .attribution
                .unwrap()
                .source,
            SourceDuLogo::Commons
        );
    }

    #[tokio::test]
    async fn les_sources_se_relaient_dans_l_ordre_wikipedia_site_discogs() {
        let (racine, journal) = services_simules().await;
        let dossier = crate::test_scratch::scratch_dir("logos-web-relais");
        let cache = CacheLogosLabels::ouvrir_index(dossier.path(), FICHIER_INDEX_WEB);
        let cat = catalogue(&racine, rapide());
        let demandes = ["ECM Records", "Site Records", "Disco Records", "BMG"]
            .map(|n| Demande {
                nom: n.into(),
                album_du_catalogue: None,
            })
            .to_vec();

        let n = cache
            .resoudre(&cat, demandes, || 1_000, Duration::ZERO)
            .await;

        assert_eq!(n, 3);
        let source = |nom: &str| {
            let a = cache.entree(nom).unwrap().attribution.unwrap();
            (a.source, a.statut_licence, a.fournisseur, a.source_url)
        };
        let (s, l, f, _) = source("ECM Records");
        assert_eq!(
            (s, l, f.as_str()),
            (
                SourceDuLogo::Wikipedia,
                StatutDeLicence::NonLibreIdentification,
                "Wikipedia"
            ),
            "Commons non libre → l'infobox de Wikipédia, marquée non libre"
        );
        let (s, l, _, u) = source("Site Records");
        assert_eq!(
            (s, l),
            (SourceDuLogo::SiteOfficiel, StatutDeLicence::Inconnue)
        );
        assert_eq!(u, format!("{racine}/site/"));
        let (s, l, f, u) = source("Disco Records");
        assert_eq!(
            (s, l, f.as_str()),
            (SourceDuLogo::Discogs, StatutDeLicence::Inconnue, "Discogs")
        );
        assert!(u.ends_with("/label/281"));
        assert_eq!(cache.entree("BMG").unwrap().statut, Statut::Absent);
        assert_eq!(
            cache.entree("BMG").unwrap().mb_label_id,
            None,
            "homonymes : aucun label retenu"
        );
        let j = journal.lock().unwrap();
        assert!(
            !j.autres.contains(&"discogs 77".to_string()),
            "le site a suffi : Discogs n'est pas lu"
        );
        assert!(
            !j.autres.contains(&"discogs 5".to_string()),
            "Wikipédia a suffi pour ECM"
        );
        // L'icône du site, pas la bannière ; et l'image du site n'est pas
        // téléchargée deux fois.
        let site = cache.image("Site Records").unwrap();
        let (chemin, _) = crate::library::artwork::find_cached(dossier.path(), &site).unwrap();
        assert_eq!(
            dimensions(&std::fs::read(chemin).unwrap()),
            Some((180, 180))
        );
    }

    /// 🔴 Pas de rafale : toutes les requêtes MusicBrainz d'une passe passent
    /// par le limiteur partagé — ici réglé à une toutes les 100 ms pour
    /// l'essai —, et une passe sur 30 labels n'en traite que `PASSE_MAX`, à
    /// trois requêtes MusicBrainz au plus chacun.
    #[tokio::test]
    async fn une_passe_est_bornee_et_espace_ses_requetes_musicbrainz() {
        let (racine, journal) = services_simules().await;
        let dossier = crate::test_scratch::scratch_dir("logos-web-rafale");
        let cache = CacheLogosLabels::ouvrir_index(dossier.path(), FICHIER_INDEX_WEB);
        let intervalle = Duration::from_millis(100);
        let cat = catalogue(
            &racine,
            Box::leak(Box::new(RateLimiter::with_interval(intervalle))),
        );
        let demandes: Vec<Demande> = (0..30)
            .map(|i| Demande {
                nom: format!("Label {i}"),
                album_du_catalogue: Some(format!("rel-{i}")),
            })
            .collect();

        let a_faire = cache.a_resoudre(&demandes, 1_000, PASSE_MAX);
        assert_eq!(a_faire.len(), PASSE_MAX);
        cache
            .resoudre(&cat, a_faire, || 1_000, Duration::ZERO)
            .await;

        let j = journal.lock().unwrap();
        assert!(!j.mb.is_empty());
        assert!(
            j.mb.len() <= 3 * PASSE_MAX,
            "{} requêtes MusicBrainz pour {PASSE_MAX} labels",
            j.mb.len()
        );
        // Mesuré à l'ARRIVÉE sur le faux serveur : le réseau local ajoute
        // quelques millisecondes de gigue. Une rafale, elle, arriverait en
        // moins d'une milliseconde. D'où deux bornes : aucun écart sous la
        // moitié de l'intervalle, et un débit moyen tenu à 90 %.
        for paire in j.mb.windows(2) {
            let ecart = paire[1].0.duration_since(paire[0].0);
            assert!(
                ecart >= intervalle / 2,
                "rafale : {:?} entre « {} » et « {} »",
                ecart,
                paire[0].1,
                paire[1].1
            );
        }
        let duree = j.mb.last().unwrap().0.duration_since(j.mb[0].0);
        let attendu = intervalle * (j.mb.len() as u32 - 1);
        assert!(
            duree >= attendu * 9 / 10,
            "{} requêtes MusicBrainz en {:?} : le limiteur n'espace pas (attendu {:?})",
            j.mb.len(),
            duree,
            attendu
        );
        let traites: std::collections::HashSet<&str> =
            j.mb.iter()
                .filter_map(|m| m.1.strip_prefix("release/"))
                .collect();
        assert_eq!(
            traites.len(),
            PASSE_MAX,
            "seuls PASSE_MAX labels sont demandés"
        );
    }

    #[test]
    fn la_production_a_un_limiteur_par_source() {
        let t = Transport::production(
            crate::http::client::builder()
                .build()
                .expect("client HTTP partagé"),
        );
        assert!(std::ptr::eq(
            t.limiteur_mb,
            &*crate::http::fetch::MUSICBRAINZ
        ));
        assert_eq!(
            t.cle_mb,
            crate::metadata::musicbrainz_release::CLE_LIMITEUR_MUSICBRAINZ
        );
        assert!(std::ptr::eq(
            t.limiteur_wikimedia,
            &*crate::http::fetch::WIKIDATA
        ));
        assert!(std::ptr::eq(
            t.limiteur_discogs,
            &*LIMITEUR_DISCOGS_SANS_JETON
        ));
        assert!(
            LIMITEUR_DISCOGS_SANS_JETON.intervalle_courant(CLE_DISCOGS_SANS_JETON)
                >= Duration::from_millis(2400),
            "Discogs sans jeton : 25 requêtes par minute au plus"
        );
        assert!(std::ptr::eq(t.limiteur_sites, &*LIMITEUR_SITES));
        assert!(
            t.prefixes_d_essai.is_empty(),
            "aucune exemption de la garde réseau en production"
        );
        assert!(t.musicbrainz.starts_with("https://musicbrainz.org/"));
        assert!(crate::metadata::musicbrainz_release::MB_UA.contains("mozaiklabs.fr"));
    }
}
