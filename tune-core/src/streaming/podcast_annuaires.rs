//! Le flux RSS d'un podcast qu'Apple connaît sans en publier l'adresse
//! (#7152).
//!
//! La recherche iTunes rend pour beaucoup d'émissions de radio publique une
//! fiche SANS `feedUrl` (Affaires sensibles, Les Pieds sur terre, Le Masque et
//! la Plume… : 11 émissions de radio publique françaises sur 20 dans la mesure
//! du 26/10). Ces fiches étaient jetées. Elles sont maintenant résolues ici,
//! par trois annuaires interrogés dans cet ordre :
//!
//! 1. gpodder.net (`/search.json`, sans clé) ;
//! 2. fyyd (`/0.2/search/podcast`, sans clé) ;
//! 3. Podcast Index, par le relais de mozaiklabs.fr qui garde la clé
//!    (`/api/v1/podcastindex/…`, jeton OAuth du compte relié). Sans compte
//!    relié, ou relais sans clé (503), cette source est simplement sautée.
//!
//! # Concordance stricte, jamais « au plus proche »
//!
//! Un candidat n'est retenu que si :
//!
//! - son titre normalisé ([`normaliser`]) est ÉGAL à celui de la fiche Apple ;
//! - son auteur, s'il en porte un, concorde ([`auteurs_concordent`]) ;
//! - l'EN-TÊTE du flux lui-même (les premiers Kio, jamais un épisode) se
//!   charge, et son `<title>` comme son `<itunes:author>` concordent à leur
//!   tour. Un flux mort, renommé ou d'un homonyme est refusé.
//!
//! Aucun candidat ne passe : la fiche n'est pas proposée (voir
//! `PodcastService::search`).
//!
//! # Cache
//!
//! Le résultat (flux trouvé, ou absence constatée sans panne) est gardé en
//! mémoire : [`TTL_TROUVE`] pour un flux venu de gpodder ou fyyd,
//! [`TTL_PODCAST_INDEX`] pour un flux venu de Podcast Index (ses conditions
//! interdisent de garder une copie au-delà de son en-tête de cache),
//! [`TTL_ABSENT`] pour une absence. Une panne réseau ne se met pas en cache.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

/// Racine de gpodder.net.
pub const GPODDER_BASE: &str = "https://gpodder.net";
/// Racine de l'API fyyd.
pub const FYYD_BASE: &str = "https://api.fyyd.de/0.2";
/// Chemin du relais Podcast Index sur le site (site-mozaiklabs).
pub const CHEMIN_RELAIS: &str = "/api/v1/podcastindex";

const USER_AGENT: &str = "Tune/2.0 (https://mozaiklabs.fr)";
/// Délai d'une requête vers un annuaire ou un en-tête de flux.
const DELAI_REQUETE: Duration = Duration::from_secs(6);
/// Octets lus au plus dans un flux pour en lire l'en-tête.
pub const ENTETE_MAX: usize = 64 * 1024;
/// Candidats vérifiés au plus par source.
const CANDIDATS_MAX: usize = 3;

pub const TTL_TROUVE: Duration = Duration::from_secs(7 * 24 * 3600);
pub const TTL_PODCAST_INDEX: Duration = Duration::from_secs(3600);
pub const TTL_ABSENT: Duration = Duration::from_secs(3600);
const CACHE_MAX: usize = 5_000;
/// Après un relais inactif (503) ou injoignable : pas de nouvel appel avant.
const PAUSE_RELAIS: Duration = Duration::from_secs(600);

/// D'où vient le flux résolu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceFlux {
    Gpodder,
    Fyyd,
    PodcastIndex,
}

impl SourceFlux {
    pub fn cle(self) -> &'static str {
        match self {
            Self::Gpodder => "gpodder",
            Self::Fyyd => "fyyd",
            Self::PodcastIndex => "podcastindex",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FluxResolu {
    pub url: String,
    pub source: SourceFlux,
}

/// Une fiche d'annuaire : titre, auteur (vide s'il n'en porte pas), flux.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidat {
    pub titre: String,
    pub auteur: String,
    pub flux: String,
}

/// Titre ou auteur ramené à une forme comparable : entités HTML décodées
/// (fyyd rend `Sommar &amp; Vinter`), accents retirés, minuscules, toute
/// ponctuation ramenée à une espace, espaces fusionnées. `&` et `et`/`and`
/// ne sont PAS rapprochés : la concordance reste littérale.
pub fn normaliser(brut: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    let decode = quick_xml::escape::unescape(brut)
        .map(|c| c.into_owned())
        .unwrap_or_else(|_| brut.to_string());
    let mut plie = String::with_capacity(decode.len());
    for c in decode.nfkd() {
        if unicode_normalization::char::is_combining_mark(c) {
            continue;
        }
        match c {
            'œ' | 'Œ' => plie.push_str("oe"),
            'æ' | 'Æ' => plie.push_str("ae"),
            'ß' => plie.push_str("ss"),
            'ø' | 'Ø' => plie.push('o'),
            '&' => plie.push_str(" & "),
            c if c.is_alphanumeric() => plie.extend(c.to_lowercase()),
            _ => plie.push(' '),
        }
    }
    plie.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// L'auteur sans ses parenthèses ni ses adresses de courriel : gpodder écrit
/// `France Inter (podcast@radiofrance.com)` là où Apple écrit `France Inter`.
pub fn normaliser_auteur(brut: &str) -> String {
    let mut sans = String::with_capacity(brut.len());
    let mut profondeur = 0usize;
    for c in brut.chars() {
        match c {
            '(' | '[' => profondeur += 1,
            ')' | ']' => profondeur = profondeur.saturating_sub(1),
            _ if profondeur == 0 => sans.push(c),
            _ => {}
        }
    }
    let sans: String = sans
        .split_whitespace()
        .filter(|mot| !mot.contains('@'))
        .collect::<Vec<_>>()
        .join(" ");
    normaliser(&sans)
}

/// Deux titres concordent s'ils sont ÉGAUX une fois normalisés, et non vides.
pub fn titres_concordent(a: &str, b: &str) -> bool {
    let (a, b) = (normaliser(a), normaliser(b));
    !a.is_empty() && a == b
}

/// Deux auteurs concordent s'ils sont ÉGAUX une fois normalisés
/// ([`normaliser_auteur`]), et non vides.
pub fn auteurs_concordent(a: &str, b: &str) -> bool {
    let (a, b) = (normaliser_auteur(a), normaliser_auteur(b));
    !a.is_empty() && a == b
}

/// Une adresse de flux sans schéma, casse ni `/` final, pour dédoublonner
/// `http://…` et `https://…` d'un même flux.
fn flux_cle(url: &str) -> String {
    crate::radio_metadata_relais::flux_normalise(url)
}

/// Les candidats d'une source que la fiche Apple peut accepter : titre égal,
/// auteur concordant s'il est donné, un par flux (la forme `https` gagne), au
/// plus [`CANDIDATS_MAX`].
pub fn retenir(candidats: Vec<Candidat>, titre: &str, auteur: &str) -> Vec<Candidat> {
    let mut retenus: Vec<Candidat> = Vec::new();
    for c in candidats {
        if c.flux.trim().is_empty() || !titres_concordent(&c.titre, titre) {
            continue;
        }
        if !c.auteur.trim().is_empty() && !auteurs_concordent(&c.auteur, auteur) {
            continue;
        }
        let cle = flux_cle(&c.flux);
        if let Some(deja) = retenus.iter_mut().find(|r| flux_cle(&r.flux) == cle) {
            if c.flux.starts_with("https://") && !deja.flux.starts_with("https://") {
                deja.flux = c.flux.clone();
            }
            continue;
        }
        retenus.push(c);
    }
    retenus.truncate(CANDIDATS_MAX);
    retenus
}

/// Le titre et l'auteur de la CHAÎNE, lus dans le début d'un flux (avant le
/// premier `<item`). `None` si le texte n'a pas de `<channel>` ni de titre.
pub fn entete_de_chaine(xml: &str) -> Option<(String, String)> {
    let debut = xml.find("<channel")?;
    let tete = &xml[debut..];
    let tete = match tete.find("<item") {
        Some(fin) => &tete[..fin],
        None => tete,
    };
    let titre = super::podcasts::extraire_balise(tete, "title")?;
    let auteur = ["itunes:author", "googleplay:author", "author"]
        .iter()
        .find_map(|b| super::podcasts::extraire_balise(tete, b))
        .unwrap_or_default();
    Some((titre, auteur))
}

/// Le relais Podcast Index de mozaiklabs.fr, au nom du compte relié. Le jeton
/// n'est jamais journalisé (`Debug` le masque).
#[derive(Clone)]
pub struct RelaisPodcastIndex {
    pub base: String,
    jeton: String,
}

impl std::fmt::Debug for RelaisPodcastIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelaisPodcastIndex")
            .field("base", &self.base)
            .field("jeton", &"<masqué>")
            .finish()
    }
}

impl RelaisPodcastIndex {
    pub fn new(base: &str, jeton: &str) -> Self {
        Self {
            base: base.trim_end_matches('/').to_string(),
            jeton: jeton.to_string(),
        }
    }

    /// Le relais du compte relié à ce serveur, ou `None` sans compte.
    pub fn du_compte(backend: &std::sync::Arc<dyn DbBackend>) -> Option<Self> {
        let settings = SettingsRepo::with_backend(backend.clone());
        let compte = crate::cloud::sauvegarde_config::compte(&settings).ok()?;
        Some(Self::new(&compte.base, &compte.acces))
    }
}

/// Fin de la pause du relais (503 ou panne), par racine.
static PAUSE_DU_RELAIS: LazyLock<Mutex<HashMap<String, Instant>>> = LazyLock::new(Default::default);

type Cache = HashMap<String, (Instant, Duration, Option<FluxResolu>)>;
static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(Default::default);

/// Les trois annuaires. Les racines sont fixes en production ; un banc les
/// pointe vers un faux serveur local.
#[derive(Clone, Debug)]
pub struct Annuaires {
    client: reqwest::Client,
    gpodder: String,
    fyyd: String,
    relais: Option<RelaisPodcastIndex>,
}

/// Une source n'a pas répondu (réseau, délai, réponse illisible) : rien ne
/// se met en cache, la prochaine recherche réessaiera.
#[derive(Debug)]
struct Panne;

impl Annuaires {
    pub fn new(client: reqwest::Client) -> Self {
        Self::avec_bases(client, GPODDER_BASE, FYYD_BASE)
    }

    pub fn avec_bases(client: reqwest::Client, gpodder: &str, fyyd: &str) -> Self {
        Self {
            client,
            gpodder: gpodder.trim_end_matches('/').to_string(),
            fyyd: fyyd.trim_end_matches('/').to_string(),
            relais: None,
        }
    }

    pub fn avec_relais(mut self, relais: Option<RelaisPodcastIndex>) -> Self {
        self.relais = relais;
        self
    }

    fn get(&self, url: &str) -> reqwest::RequestBuilder {
        self.client
            .get(url)
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .timeout(DELAI_REQUETE)
    }

    async fn json(&self, req: reqwest::RequestBuilder) -> Result<Option<Value>, Panne> {
        let r = req.send().await.map_err(|_| Panne)?;
        if !r.status().is_success() {
            return Ok(None);
        }
        r.json::<Value>().await.map(Some).map_err(|_| Panne)
    }

    async fn gpodder(&self, titre: &str) -> Result<Vec<Candidat>, Panne> {
        let url = format!(
            "{}/search.json?q={}",
            self.gpodder,
            urlencoding::encode(titre)
        );
        let Some(v) = self.json(self.get(&url)).await? else {
            return Err(Panne);
        };
        Ok(v.as_array()
            .map(|a| {
                a.iter()
                    .map(|r| Candidat {
                        titre: r["title"].as_str().unwrap_or("").into(),
                        auteur: r["author"].as_str().unwrap_or("").into(),
                        flux: r["url"].as_str().unwrap_or("").into(),
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn fyyd(&self, titre: &str) -> Result<Vec<Candidat>, Panne> {
        let url = format!(
            "{}/search/podcast?title={}",
            self.fyyd,
            urlencoding::encode(titre)
        );
        let Some(v) = self.json(self.get(&url)).await? else {
            return Err(Panne);
        };
        Ok(v["data"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|r| Candidat {
                        titre: r["title"].as_str().unwrap_or("").into(),
                        auteur: r["author"].as_str().unwrap_or("").into(),
                        flux: r["xmlURL"].as_str().unwrap_or("").into(),
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Podcast Index par le relais : d'abord par identifiant Apple (le pont
    /// le plus sûr), puis par titre. Relais absent, en pause ou inactif :
    /// aucun candidat, sans panne.
    async fn podcast_index(
        &self,
        apple_id: Option<&str>,
        titre: &str,
    ) -> Result<Vec<Candidat>, Panne> {
        let Some(relais) = &self.relais else {
            return Ok(Vec::new());
        };
        if let Ok(p) = PAUSE_DU_RELAIS.lock()
            && p.get(&relais.base).is_some_and(|fin| Instant::now() < *fin)
        {
            return Ok(Vec::new());
        }
        let mut chemins = Vec::new();
        if let Some(id) =
            apple_id.filter(|i| !i.is_empty() && i.bytes().all(|b| b.is_ascii_digit()))
        {
            chemins.push(format!("{}{CHEMIN_RELAIS}/byitunesid?id={id}", relais.base));
        }
        chemins.push(format!(
            "{}{CHEMIN_RELAIS}/search?q={}",
            relais.base,
            urlencoding::encode(titre)
        ));
        let mut candidats = Vec::new();
        for url in chemins {
            let req = self
                .get(&url)
                .bearer_auth(&relais.jeton)
                .header(reqwest::header::ACCEPT, "application/json");
            let r = match req.send().await {
                Ok(r) => r,
                Err(_) => {
                    self.mettre_le_relais_en_pause(relais);
                    return Err(Panne);
                }
            };
            let statut = r.status();
            if statut == reqwest::StatusCode::SERVICE_UNAVAILABLE {
                // Relais sans clé : Tune continue sans cette source.
                info!("podcast_index_relais_inactif");
                self.mettre_le_relais_en_pause(relais);
                return Ok(Vec::new());
            }
            if !statut.is_success() {
                // Un refus de l'une des deux demandes ne jette pas ce que
                // l'autre a déjà rendu.
                debug!(statut = statut.as_u16(), "podcast_index_relais_refus");
                continue;
            }
            let v: Value = r.json().await.map_err(|_| Panne)?;
            if let Some(a) = v["feeds"].as_array() {
                candidats.extend(a.iter().map(|f| Candidat {
                    titre: f["title"].as_str().unwrap_or("").into(),
                    auteur: f["author"].as_str().unwrap_or("").into(),
                    flux: f["url"].as_str().unwrap_or("").into(),
                }));
            }
        }
        Ok(candidats)
    }

    fn mettre_le_relais_en_pause(&self, relais: &RelaisPodcastIndex) {
        if let Ok(mut p) = PAUSE_DU_RELAIS.lock() {
            p.insert(relais.base.clone(), Instant::now() + PAUSE_RELAIS);
        }
    }

    /// Les premiers octets d'un flux (au plus [`ENTETE_MAX`], arrêt au premier
    /// `<item`) : jamais un épisode. `Ok(None)` : le flux ne répond pas 2xx.
    async fn lire_entete(&self, url: &str) -> Result<Option<String>, Panne> {
        let r = self
            .get(url)
            .header(
                reqwest::header::RANGE,
                format!("bytes=0-{}", ENTETE_MAX - 1),
            )
            .header(
                reqwest::header::ACCEPT,
                "application/rss+xml, application/xml, text/xml, */*",
            )
            .send()
            .await
            .map_err(|_| Panne)?;
        if !r.status().is_success() {
            return Ok(None);
        }
        let mut octets: Vec<u8> = Vec::new();
        let mut flux = r.bytes_stream();
        while let Some(morceau) = flux.next().await {
            let morceau = morceau.map_err(|_| Panne)?;
            octets.extend_from_slice(&morceau);
            if octets.len() >= ENTETE_MAX || String::from_utf8_lossy(&octets).contains("<item") {
                break;
            }
        }
        octets.truncate(ENTETE_MAX);
        Ok(Some(String::from_utf8_lossy(&octets).into_owned()))
    }

    /// Le candidat est-il bien le podcast de la fiche Apple ? Vérifié sur
    /// l'en-tête du flux lui-même.
    async fn verifier(&self, c: &Candidat, titre: &str, auteur: &str) -> Result<bool, Panne> {
        let Some(xml) = self.lire_entete(&c.flux).await? else {
            debug!(flux = %c.flux, "podcast_candidat_flux_mort");
            return Ok(false);
        };
        let Some((titre_flux, auteur_flux)) = entete_de_chaine(&xml) else {
            return Ok(false);
        };
        if !titres_concordent(&titre_flux, titre) {
            return Ok(false);
        }
        let auteur_ok = if auteur_flux.trim().is_empty() {
            auteurs_concordent(&c.auteur, auteur)
        } else {
            auteurs_concordent(&auteur_flux, auteur)
        };
        Ok(auteur_ok)
    }

    fn cle_cache(&self, apple_id: Option<&str>, titre: &str, auteur: &str) -> String {
        let ident = match apple_id.filter(|i| !i.is_empty()) {
            Some(id) => format!("apple:{id}"),
            None => format!("t:{}|{}", normaliser(titre), normaliser_auteur(auteur)),
        };
        // La racine fait partie de la clé : deux bancs ne se répondent pas.
        format!("{}|{}|{ident}", self.gpodder, self.relais.is_some())
    }

    /// Le flux d'une fiche Apple sans `feedUrl`, ou `None` (aucune
    /// concordance stricte). Voir le commentaire du module.
    pub async fn resoudre(
        &self,
        apple_id: Option<&str>,
        titre: &str,
        auteur: &str,
    ) -> Option<FluxResolu> {
        if normaliser(titre).is_empty() || normaliser_auteur(auteur).is_empty() {
            return None;
        }
        let cle = self.cle_cache(apple_id, titre, auteur);
        if let Ok(c) = CACHE.lock()
            && let Some((quand, ttl, valeur)) = c.get(&cle)
            && quand.elapsed() < *ttl
        {
            return valeur.clone();
        }
        let mut panne = false;
        for source in [
            SourceFlux::Gpodder,
            SourceFlux::Fyyd,
            SourceFlux::PodcastIndex,
        ] {
            let candidats = match source {
                SourceFlux::Gpodder => self.gpodder(titre).await,
                SourceFlux::Fyyd => self.fyyd(titre).await,
                SourceFlux::PodcastIndex => self.podcast_index(apple_id, titre).await,
            };
            let candidats = match candidats {
                Ok(c) => c,
                Err(Panne) => {
                    debug!(source = source.cle(), "podcast_annuaire_panne");
                    panne = true;
                    continue;
                }
            };
            for c in retenir(candidats, titre, auteur) {
                match self.verifier(&c, titre, auteur).await {
                    Ok(true) => {
                        let trouve = FluxResolu {
                            url: c.flux.clone(),
                            source,
                        };
                        let ttl = if source == SourceFlux::PodcastIndex {
                            TTL_PODCAST_INDEX
                        } else {
                            TTL_TROUVE
                        };
                        mettre_en_cache(cle, ttl, Some(trouve.clone()));
                        info!(titre, source = source.cle(), flux = %trouve.url, "podcast_flux_resolu");
                        return Some(trouve);
                    }
                    Ok(false) => {}
                    Err(Panne) => panne = true,
                }
            }
        }
        if panne {
            warn!(titre, "podcast_flux_non_resolu_panne");
        } else {
            debug!(titre, "podcast_flux_introuvable");
            mettre_en_cache(cle, TTL_ABSENT, None);
        }
        None
    }
}

fn mettre_en_cache(cle: String, ttl: Duration, valeur: Option<FluxResolu>) {
    if let Ok(mut c) = CACHE.lock() {
        if c.len() >= CACHE_MAX {
            c.retain(|_, (quand, ttl, _)| quand.elapsed() < *ttl);
            if c.len() >= CACHE_MAX {
                c.clear();
            }
        }
        c.insert(cle, (Instant::now(), ttl, valeur));
    }
}

#[cfg(test)]
#[path = "podcast_annuaires_tests.rs"]
mod tests;
