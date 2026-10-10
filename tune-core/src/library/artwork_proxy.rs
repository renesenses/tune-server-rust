//! Relais de pochettes distantes (`GET /library/artwork/proxy?url=…`) — la
//! fermeture du relais ouvert de #4260.
//!
//! ## Le fait
//!
//! Le gestionnaire prenait l'URL du paramètre `url` et la donnait telle quelle
//! à `reqwest` : le serveur allait la chercher, redirections comprises, et
//! rendait le corps. Jusqu'à #4061 seul le client web authentifié y arrivait ;
//! #4061 a mis cette route dans le `<upnp:albumArtURI>` du dossier Radio de la
//! DIDL, donc l'a publiée à tout le LAN sans jeton (#3933 exempte les
//! ressources de la DIDL de l'authentification). N'importe quel appareil du
//! réseau pouvait faire faire à Tune une requête vers `169.254.169.254`,
//! `localhost:8888/api/…` ou un hôte interne, et lire la réponse.
//!
//! ## Les deux couches
//!
//! 1. **Signature** : les URL que le serveur produit lui-même (le logo de
//!    station de la DIDL, [`url_relais_signee`]) portent un jeton HMAC-SHA256
//!    de l'URL distante, clé = secret d'instance persistant dans `settings`
//!    ([`CLE_SECRET`], créé au premier usage). Une signature présente mais
//!    fausse est refusée (400) ; une URL non signée est refusée (400) quand
//!    l'appel arrive par l'exemption DIDL (`signature_exigee`), et retombe
//!    sinon sur la couche 2 — le client web (`api.ts::artworkUrl`) bâtit ses
//!    URL de relais lui-même, sans signature, depuis toujours.
//! 2. **Liste d'hôtes** ([`HOTES_AUTORISES`], complétée par le réglage
//!    [`CLE_HOTES_SUPPLEMENTAIRES`]) pour toute URL non signée, et, signée ou
//!    non, refus (403) de toute adresse de boucle locale, privée, lien-local,
//!    multidiffusion ou non spécifiée — **après résolution DNS** : le
//!    résolveur du client est [`ResolveurGarde`], qui refuse chaque adresse
//!    rendue, pas seulement la chaîne. Les adresses littérales ne passent pas
//!    par le résolveur (hyper les court-circuite) : elles sont jugées avant la
//!    requête. Les redirections ne sont jamais suivies par `reqwest`
//!    (`Policy::none`) ; chaque saut repasse par la même garde.
//!
//! Un seul relais, [`Relais`], porte le client HTTP et la politique d'adresse.
//! La production le construit par [`Relais::production`] ; les bancs d'essai
//! par [`Relais::avec`], avec un résolveur factice et une politique qui admet
//! la boucle locale — c'est le SEUL moyen d'éprouver le chemin « accepté »
//! sans sortir sur Internet, et il n'existe que sous forme de constructeur :
//! aucune variable d'environnement n'ouvre la garde en production.

use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

/// Réglage qui porte le secret de signature (64 hexadécimaux, créé au
/// premier usage par [`secret`]).
pub const CLE_SECRET: &str = "artwork_proxy_secret";

/// Réglage optionnel : hôtes supplémentaires admis sans signature, séparés
/// par des virgules, des espaces ou des retours à la ligne (`radio.exemple.fr,
/// cdn.autre.net`). Un suffixe de domaine, comme la liste bâtie.
pub const CLE_HOTES_SUPPLEMENTAIRES: &str = "artwork_proxy_hosts";

/// Les hôtes qu'une pochette distante peut porter en production, relevés dans
/// ce qui est réellement relayé : les CDN des services de diffusion, les
/// sources de pochettes et d'images d'artistes, l'annuaire de radios de
/// mozaiklabs.fr et les diffuseurs dont Tune lit les métadonnées « en cours »
/// (Radio Paradise, SomaFM, Radio France, BBC). Suffixes de domaine :
/// `static.qobuz.com` est admis par `qobuz.com`.
pub const HOTES_AUTORISES: &[&str] = &[
    // Mozaiklabs — annuaire de radios (`refresh_radio_logos`), communauté.
    "mozaiklabs.fr",
    // MusicBrainz / Cover Art Archive (redirige vers archive.org).
    "coverartarchive.org",
    "archive.org",
    "musicbrainz.org",
    // Services de diffusion.
    "qobuz.com",
    "tidal.com",
    "deezer.com",
    "dzcdn.net",
    "spotify.com",
    "scdn.co",
    "spotifycdn.com",
    "bandcamp.com",
    "bcbits.com",
    "youtube.com",
    "ytimg.com",
    "ggpht.com",
    "googleusercontent.com",
    // Sources de pochettes et d'images d'artistes.
    "discogs.com",
    "last.fm",
    "lastfm.freetls.fastly.net",
    "theaudiodb.com",
    "fanart.tv",
    "wikimedia.org",
    // Apple (podcasts, recherche iTunes).
    "mzstatic.com",
    "apple.com",
    // Diffuseurs dont Tune lit les métadonnées « en cours ».
    "radioparadise.com",
    "somafm.com",
    "radiofrance.fr",
    "radiofrance-podcast.net",
    "bbci.co.uk",
    "bbc.co.uk",
    "radio-browser.info",
];

/// Nombre maximal de redirections suivies à la main.
const REDIRECTIONS_MAX: usize = 5;

// ---------------------------------------------------------------------------
// Couche 1 — signature
// ---------------------------------------------------------------------------

/// Le secret de signature de cette instance, créé au premier usage.
///
/// Distinct de `jwt_secret` : celui-ci n'existe que quand l'authentification
/// est activée, et l'utilisateur peut le changer depuis l'API — un secret qui
/// change invalide toutes les URL que les points de contrôle ont mémorisées.
pub fn secret(backend: &Arc<dyn DbBackend>) -> String {
    let settings = SettingsRepo::with_backend(backend.clone());
    if let Ok(Some(s)) = settings.get(CLE_SECRET) {
        if !s.trim().is_empty() {
            return s;
        }
    }
    let mut octets = [0u8; 32];
    getrandom::getrandom(&mut octets).expect("OS RNG unavailable");
    let neuf = hex(&octets);
    if let Err(e) = settings.set(CLE_SECRET, &neuf) {
        tracing::warn!(erreur = %e, "artwork_proxy_secret_non_persiste");
    }
    neuf
}

fn hex(octets: &[u8]) -> String {
    use std::fmt::Write;
    octets
        .iter()
        .fold(String::with_capacity(octets.len() * 2), |mut s, o| {
            let _ = write!(s, "{o:02x}");
            s
        })
}

/// HMAC-SHA256 (RFC 2104) écrit sur `sha2` seul : la caisse `hmac` n'est pas
/// une dépendance directe de l'espace de travail.
fn hmac_sha256(cle: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOC: usize = 64;
    let mut k = [0u8; BLOC];
    if cle.len() > BLOC {
        k[..32].copy_from_slice(&Sha256::digest(cle));
    } else {
        k[..cle.len()].copy_from_slice(cle);
    }
    let mut interne = Sha256::new();
    interne.update(k.iter().map(|b| b ^ 0x36).collect::<Vec<u8>>());
    interne.update(message);
    let condensat_interne = interne.finalize();
    let mut externe = Sha256::new();
    externe.update(k.iter().map(|b| b ^ 0x5c).collect::<Vec<u8>>());
    externe.update(condensat_interne);
    externe.finalize().into()
}

/// La signature d'une URL distante : 64 hexadécimaux.
pub fn signature(secret: &str, url: &str) -> String {
    hex(&hmac_sha256(secret.as_bytes(), url.as_bytes()))
}

/// Vrai si `sig` est la signature de `url` — comparaison en temps constant.
pub fn signature_valide(secret: &str, url: &str, sig: &str) -> bool {
    let attendue = signature(secret, url);
    let (a, b) = (attendue.as_bytes(), sig.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// L'URL de relais signée d'une pochette distante, telle que le serveur la
/// publie (`{base}/api/v1/library/artwork/proxy?url=…&sig=…`).
pub fn url_relais_signee(base_url: &str, secret: &str, distante: &str) -> String {
    format!(
        "{base_url}{}/library/artwork/proxy?url={}&sig={}",
        crate::upnp_server::API_PATH,
        urlencoding::encode(distante),
        signature(secret, distante)
    )
}

// ---------------------------------------------------------------------------
// Couche 2 — hôtes et adresses
// ---------------------------------------------------------------------------

/// Vrai si `hote` est l'un des [`HOTES_AUTORISES`] (ou de `supplementaires`),
/// ou un sous-domaine de l'un d'eux.
pub fn hote_autorise(hote: &str, supplementaires: &[String]) -> bool {
    let hote = hote.trim().trim_end_matches('.').to_ascii_lowercase();
    if hote.is_empty() {
        return false;
    }
    let admis = |suffixe: &str| {
        let suffixe = suffixe.trim().trim_end_matches('.').to_ascii_lowercase();
        !suffixe.is_empty()
            && (hote == suffixe
                || hote
                    .strip_suffix(&suffixe)
                    .is_some_and(|reste| reste.ends_with('.')))
    };
    HOTES_AUTORISES.iter().any(|s| admis(s)) || supplementaires.iter().any(|s| admis(s))
}

/// Découpe le réglage [`CLE_HOTES_SUPPLEMENTAIRES`].
pub fn hotes_supplementaires(backend: &Arc<dyn DbBackend>) -> Vec<String> {
    SettingsRepo::with_backend(backend.clone())
        .get(CLE_HOTES_SUPPLEMENTAIRES)
        .ok()
        .flatten()
        .map(|v| {
            v.split(|c: char| c == ',' || c.is_whitespace())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// Boucle locale, plages privées, lien-local, multidiffusion, non spécifiée,
/// réservées — et leurs équivalents IPv6, y compris l'IPv4 mappée
/// (`::ffff:127.0.0.1`).
///
/// Les masques IPv6 sont écrits à la main : `Ipv6Addr::is_unique_local` et
/// `is_unicast_link_local` sont encore instables (fc00::/7 et fe80::/10).
pub fn adresse_interdite(ip: IpAddr) -> bool {
    fn v4(a: std::net::Ipv4Addr) -> bool {
        a.is_loopback()
            || a.is_private()
            || a.is_link_local()
            || a.is_broadcast()
            || a.is_multicast()
            || a.is_unspecified()
            || a.is_documentation()
            // 100.64.0.0/10 (RFC 6598, CGNAT) et 0.0.0.0/8.
            || (a.octets()[0] == 100 && (a.octets()[1] & 0xc0) == 64)
            || a.octets()[0] == 0
            // 240.0.0.0/4 réservé.
            || a.octets()[0] >= 240
    }
    match ip {
        IpAddr::V4(a) => v4(a),
        IpAddr::V6(a) => {
            if let Some(mappee) = a.to_ipv4_mapped() {
                return v4(mappee);
            }
            let seg = a.segments();
            a.is_loopback()
                || a.is_unspecified()
                || a.is_multicast()
                || (seg[0] & 0xfe00) == 0xfc00
                || (seg[0] & 0xffc0) == 0xfe80
                // 2001:db8::/32 documentation.
                || (seg[0] == 0x2001 && seg[1] == 0x0db8)
        }
    }
}

/// Une adresse du RÉSEAU LOCAL : plages privées IPv4 (10/8, 172.16/12,
/// 192.168/16), leur forme IPv6 mappée, et les adresses uniques locales IPv6
/// (fc00::/7).
///
/// Sous-ensemble STRICT de [`adresse_interdite`] : la boucle locale, le
/// lien-local (169.254.x.x — dont les métadonnées d'un nuage), la
/// multidiffusion et les réservées n'en font pas partie et restent refusées
/// par [`Relais::reseau_local`]. Voir [`pochette_de_bibliotheque`].
pub fn adresse_reseau_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => a.is_private(),
        IpAddr::V6(a) => match a.to_ipv4_mapped() {
            Some(v4) => v4.is_private(),
            None => (a.segments()[0] & 0xfe00) == 0xfc00,
        },
    }
}

/// L'URL est-elle la pochette ENREGISTRÉE d'un album de la bibliothèque ?
///
/// ## Pourquoi cette exception existe — bug du .18, 17/09/2026
///
/// Un serveur UPnP intégré à la bibliothèque (#4201) donne à ses albums une
/// pochette sur SON adresse de réseau local —
/// `http://192.168.1.41:26125/aa/334872490115648/cover.jpg` pour « Kino
/// Music ». Le client la demande à ce relais, qui depuis #4260 refuse toute
/// adresse privée : les 39 pochettes UPnP du .18 étaient grises, et le journal
/// en répétait `artwork_proxy_hote_refuse` à chaque vignette.
///
/// ## Pourquoi elle ne rouvre pas le relais
///
/// L'adresse n'est pas celle que le CLIENT fournit, mais celle que la
/// bibliothèque a ÉCRITE : la synchronisation d'une source UPnP que
/// l'utilisateur a lui-même intégrée. Un appareil du réseau ne peut pas faire
/// relayer une adresse de son choix : il lui faudrait d'abord la faire entrer
/// en base comme pochette d'album. Et même alors, [`Relais::reseau_local`]
/// n'admet que le réseau local — ni la boucle locale, ni le lien-local.
pub fn pochette_de_bibliotheque(backend: &Arc<dyn DbBackend>, url: &str) -> bool {
    let url = url.to_string();
    backend
        .query_one(
            "SELECT 1 FROM albums WHERE cover_path = $1 LIMIT 1",
            &[&url as &dyn crate::db::backend::ToSqlValue],
        )
        .ok()
        .flatten()
        .is_some()
}

/// L'hôte et le port d'une URL qui est EXACTEMENT la route de pochette d'un
/// serveur Tune — `http(s)://<hôte>:<port>/api/v1/library/artwork/<condensat>`,
/// sans requête, sans fragment, sans identifiants — ou `None`.
///
/// ## #4954 — les pochettes d'un serveur Tune vu dans « Serveurs multimédia »
///
/// Le client web lit le catalogue d'un serveur Tune découvert par son API REST
/// et bâtit, pour chaque album, cette adresse sur l'IP de réseau local du
/// serveur (`tuneRemote.ts::pochetteDistante`). En 0.9.163, il l'envoyait à ce
/// relais, qui refuse l'adresse privée : toutes les tuiles grises, le serveur
/// Tune lui-même compris (Jean Valjean, 2 809 albums).
///
/// Le relais ne l'admet que si l'hôte:port est AUSSI un serveur multimédia
/// découvert (registre SSDP, vérifié par l'appelant). Le chemin est figé, et le
/// condensat a la forme de `albums.cover_path` (32 ou 64 hexadécimaux) : ni
/// l'API du serveur visé, ni sa route de relais, ni une autre ressource.
pub fn pochette_d_un_serveur_tune(url: &str) -> Option<(String, u16)> {
    let u = reqwest::Url::parse(url).ok()?;
    if !matches!(u.scheme(), "http" | "https")
        || u.query().is_some()
        || u.fragment().is_some()
        || !u.username().is_empty()
        || u.password().is_some()
    {
        return None;
    }
    let condensat = u.path().strip_prefix("/api/v1/library/artwork/")?;
    let condensat_valide =
        matches!(condensat.len(), 32 | 64) && condensat.bytes().all(|b| b.is_ascii_hexdigit());
    if !condensat_valide {
        return None;
    }
    let hote = u
        .host_str()?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    Some((hote, u.port_or_known_default()?))
}

/// Taille maximale d'une pochette relayée depuis un serveur multimédia du
/// réseau local ([`Relais::relayer_pochette_de_serveur_multimedia`]) — la
/// borne des vignettes de podcast.
pub const POCHETTE_SERVEUR_MULTIMEDIA_TAILLE_MAX: usize = 10 * 1024 * 1024;

/// L'hôte et le port d'une URL candidate au relais des pochettes de serveurs
/// multimédia du réseau local, ou `None`.
///
/// ## #4895 — les pochettes d'un serveur multimédia tiers (UPnP)
///
/// Un serveur multimédia du réseau (une box, un NAS…) donne à ses albums une
/// pochette sur SON adresse de réseau local. Hors bibliothèque — lecture en
/// cours, file d'attente — le relais refusait l'adresse privée. Il l'admet
/// désormais si l'hôte:port est EXACTEMENT celui d'un serveur présent au
/// registre SSDP (vérifié par l'appelant, à chaque saut).
///
/// Refusé ici, avant le registre : un schéma autre que `http`/`https`, des
/// identifiants dans l'URL, et tout chemin sous `/api/` — l'API d'un serveur
/// Tune découvert (le nôtre compris, #3786) n'est pas une pochette ; la
/// pochette d'un serveur Tune passe par [`pochette_d_un_serveur_tune`].
pub fn pochette_de_serveur_multimedia(url: &reqwest::Url) -> Option<(String, u16)> {
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    let chemin = url.path().to_ascii_lowercase();
    if chemin == "/api" || chemin.starts_with("/api/") {
        return None;
    }
    let hote = url
        .host_str()?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    Some((hote, url.port_or_known_default()?))
}

/// Lit le corps d'une réponse d'image : `Content-Type` image exigé (ou, si
/// `octet_stream_admis`, `application/octet-stream` ; absent toléré seulement
/// dans ce cas), au plus `taille_max` octets.
async fn lire_image(
    mut reponse: reqwest::Response,
    taille_max: usize,
    octet_stream_admis: bool,
) -> Result<Relaye, EchecTelechargement> {
    let content_type = reponse
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_ascii_lowercase());
    match content_type.as_deref() {
        Some(ct) => {
            let essence = ct.split(';').next().unwrap_or("").trim();
            let admis = essence.starts_with("image/")
                || (octet_stream_admis && essence == "application/octet-stream");
            if !admis {
                return Err(EchecTelechargement::TypeRefuse(essence.to_string()));
            }
        }
        None if !octet_stream_admis => {
            return Err(EchecTelechargement::TypeRefuse(String::new()));
        }
        None => {}
    }
    if let Some(annoncee) = reponse.content_length()
        && annoncee > taille_max as u64
    {
        return Err(EchecTelechargement::TropVolumineuse(taille_max));
    }
    let mut octets: Vec<u8> = Vec::new();
    while let Some(morceau) = reponse
        .chunk()
        .await
        .map_err(|e| EchecTelechargement::Amont(e.to_string()))?
    {
        if octets.len() + morceau.len() > taille_max {
            return Err(EchecTelechargement::TropVolumineuse(taille_max));
        }
        octets.extend_from_slice(&morceau);
    }
    Ok(Relaye {
        content_type: content_type.unwrap_or_else(|| "image/jpeg".into()),
        octets,
    })
}

// ---------------------------------------------------------------------------
// Résolution DNS gardée
// ---------------------------------------------------------------------------

/// Ce que le résolveur rend pour un nom.
pub type Resolution = Pin<Box<dyn Future<Output = std::io::Result<Vec<IpAddr>>> + Send>>;

/// Un résolveur de noms — le système en production, une table en banc.
pub trait Resolveur: Send + Sync {
    fn resoudre(&self, nom: &str) -> Resolution;
}

/// Le résolveur du système (`tokio::net::lookup_host`).
pub struct ResolveurSysteme;

impl Resolveur for ResolveurSysteme {
    fn resoudre(&self, nom: &str) -> Resolution {
        let nom = nom.to_string();
        Box::pin(async move {
            let adresses = tokio::net::lookup_host((nom.as_str(), 0)).await?;
            Ok(adresses.map(|s| s.ip()).collect())
        })
    }
}

/// L'erreur que le résolveur gardé lève quand un nom rend une adresse
/// interdite. Retrouvée dans la chaîne des causes de l'erreur `reqwest` pour
/// rendre un 403 qui nomme l'adresse, plutôt qu'un 502 muet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdresseRefusee {
    pub hote: String,
    pub ip: IpAddr,
}

impl std::fmt::Display for AdresseRefusee {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} résout en {}, adresse interdite", self.hote, self.ip)
    }
}

impl std::error::Error for AdresseRefusee {}

type PolitiqueAdresse = Arc<dyn Fn(IpAddr) -> bool + Send + Sync>;

/// Le résolveur que le client `reqwest` utilise : il résout par `interne` et
/// refuse le nom entier dès qu'UNE de ses adresses est interdite — il ne
/// filtre pas, il refuse, sinon un nom mêlant une adresse publique et une
/// privée passerait selon l'ordre de la réponse DNS.
struct ResolveurGarde {
    interne: Arc<dyn Resolveur>,
    adresse_admise: PolitiqueAdresse,
}

impl reqwest::dns::Resolve for ResolveurGarde {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let hote = name.as_str().to_string();
        let interne = self.interne.clone();
        let admise = self.adresse_admise.clone();
        Box::pin(async move {
            let adresses = interne.resoudre(&hote).await?;
            if let Some(ip) = adresses.iter().copied().find(|ip| !admise(*ip)) {
                let refus = AdresseRefusee { hote, ip };
                return Err(Box::new(refus) as Box<dyn std::error::Error + Send + Sync>);
            }
            if adresses.is_empty() {
                return Err(Box::new(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("{hote} : aucune adresse"),
                ))
                    as Box<dyn std::error::Error + Send + Sync>);
            }
            let it: reqwest::dns::Addrs = Box::new(
                adresses
                    .into_iter()
                    .map(|ip| std::net::SocketAddr::new(ip, 0)),
            );
            Ok(it)
        })
    }
}

// ---------------------------------------------------------------------------
// Le relais
// ---------------------------------------------------------------------------

/// Pourquoi une demande de relais est refusée avant, ou sans, toucher
/// l'amont.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refus {
    /// `?url=` illisible ou d'un schéma autre que `http`/`https` → 400.
    UrlInvalide(String),
    /// L'appel arrive par l'exemption DIDL et ne porte pas de `sig` → 400.
    SignatureRequise,
    /// `sig` présent mais faux → 400.
    SignatureInvalide,
    /// URL non signée vers un hôte hors liste → 403.
    HoteRefuse(String),
    /// Adresse littérale ou résolue interdite → 403.
    AdresseInterdite { hote: String, ip: IpAddr },
    /// Redirection vers une URL que la garde refuse → 403.
    RedirectionRefusee(String),
    /// Plus de [`REDIRECTIONS_MAX`] sauts → 403.
    TropDeRedirections,
}

impl Refus {
    /// Le code HTTP à rendre.
    pub fn statut(&self) -> u16 {
        match self {
            Refus::UrlInvalide(_) | Refus::SignatureRequise | Refus::SignatureInvalide => 400,
            Refus::HoteRefuse(_)
            | Refus::AdresseInterdite { .. }
            | Refus::RedirectionRefusee(_)
            | Refus::TropDeRedirections => 403,
        }
    }

    /// Le motif journalisé.
    pub fn motif(&self) -> &'static str {
        match self {
            Refus::UrlInvalide(_) => "artwork_proxy_url_invalide",
            Refus::SignatureRequise => "artwork_proxy_signature_requise",
            Refus::SignatureInvalide => "artwork_proxy_signature_invalide",
            Refus::HoteRefuse(_)
            | Refus::AdresseInterdite { .. }
            | Refus::RedirectionRefusee(_)
            | Refus::TropDeRedirections => "artwork_proxy_hote_refuse",
        }
    }
}

impl std::fmt::Display for Refus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refus::UrlInvalide(u) => write!(f, "URL invalide : {u}"),
            Refus::SignatureRequise => write!(f, "signature requise"),
            Refus::SignatureInvalide => write!(f, "signature invalide"),
            Refus::HoteRefuse(h) => write!(f, "hôte hors liste : {h}"),
            Refus::AdresseInterdite { hote, ip } => {
                write!(f, "adresse interdite : {hote} → {ip}")
            }
            Refus::RedirectionRefusee(u) => write!(f, "redirection refusée : {u}"),
            Refus::TropDeRedirections => write!(f, "trop de redirections"),
        }
    }
}

/// Ce que rend le relais : un refus, une panne d'amont (→ 502) ou l'image.
#[derive(Debug)]
pub enum Echec {
    Refus(Refus),
    Amont(String),
}

impl From<Refus> for Echec {
    fn from(r: Refus) -> Self {
        Echec::Refus(r)
    }
}

/// L'image relayée.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relaye {
    pub content_type: String,
    pub octets: Vec<u8>,
}

/// Une demande de relais, telle que le gestionnaire la reçoit.
pub struct Demande<'a> {
    /// Le paramètre `url`, décodé.
    pub url: &'a str,
    /// Le paramètre `sig`, s'il est là.
    pub sig: Option<&'a str>,
    /// Le secret d'instance ([`secret`]).
    pub secret: &'a str,
    /// Vrai quand l'appel est arrivé par l'exemption DIDL (#3933) : une URL
    /// non signée est alors refusée au lieu de retomber sur la liste d'hôtes.
    pub signature_exigee: bool,
    /// Le réglage [`CLE_HOTES_SUPPLEMENTAIRES`], déjà découpé.
    pub hotes_supplementaires: &'a [String],
}

/// Le relais : un client `reqwest` sans redirection automatique, dont le
/// résolveur refuse les adresses interdites, et la politique d'adresse
/// appliquée aux littéraux.
#[derive(Clone)]
pub struct Relais {
    client: reqwest::Client,
    adresse_admise: PolitiqueAdresse,
}

impl Relais {
    /// Le relais de production : résolveur système, [`adresse_interdite`].
    pub fn production() -> Self {
        Self::avec(Arc::new(ResolveurSysteme), |ip| !adresse_interdite(ip))
    }

    /// Le relais des pochettes de bibliothèque ([`pochette_de_bibliotheque`]) :
    /// la politique de production, plus le RÉSEAU LOCAL
    /// ([`adresse_reseau_local`]). La boucle locale et le lien-local restent
    /// refusés.
    pub fn reseau_local() -> Self {
        Self::avec(Arc::new(ResolveurSysteme), |ip| {
            !adresse_interdite(ip) || adresse_reseau_local(ip)
        })
    }

    /// Un relais avec un résolveur et une politique d'adresse donnés — pour
    /// les bancs d'essai, qui ne sortent pas sur Internet.
    pub fn avec<F>(resolveur: Arc<dyn Resolveur>, adresse_admise: F) -> Self
    where
        F: Fn(IpAddr) -> bool + Send + Sync + 'static,
    {
        let adresse_admise: PolitiqueAdresse = Arc::new(adresse_admise);
        let client = crate::http::client::builder()
            .redirect(reqwest::redirect::Policy::none())
            // Un mandataire système résoudrait les noms à notre place et
            // contournerait la garde.
            .no_proxy()
            .dns_resolver(Arc::new(ResolveurGarde {
                interne: resolveur,
                adresse_admise: adresse_admise.clone(),
            }))
            .timeout(std::time::Duration::from_secs(20))
            .user_agent("Tune/2.0 (https://mozaiklabs.fr)")
            .build()
            .expect("client de relais de pochettes");
        Self {
            client,
            adresse_admise,
        }
    }

    /// Juge une URL (première ou redirection) : schéma, adresse littérale,
    /// hôte. `hote_origine` est l'hôte de l'URL signée d'origine : une
    /// redirection vers lui-même reste admise, hors liste ou non.
    fn juger(
        &self,
        url: &reqwest::Url,
        signee: bool,
        hote_origine: Option<&str>,
        supplementaires: &[String],
    ) -> Result<String, Refus> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(Refus::UrlInvalide(url.to_string()));
        }
        let Some(brut) = url.host_str() else {
            return Err(Refus::UrlInvalide(url.to_string()));
        };
        // `Url` normalise les littéraux (`2130706433` → `127.0.0.1`,
        // `0x7f.0.0.1` → `127.0.0.1`) et met l'IPv6 entre crochets.
        let hote = match brut
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
        {
            Ok(ip) => {
                if !(self.adresse_admise)(ip) {
                    return Err(Refus::AdresseInterdite {
                        hote: ip.to_string(),
                        ip,
                    });
                }
                ip.to_string()
            }
            Err(_) => brut.trim_end_matches('.').to_ascii_lowercase(),
        };
        let meme_origine = hote_origine.is_some_and(|o| o.eq_ignore_ascii_case(&hote));
        if !signee && !meme_origine && !hote_autorise(&hote, supplementaires) {
            return Err(Refus::HoteRefuse(hote));
        }
        Ok(hote)
    }

    /// Relaie `demande.url` — ou dit pourquoi non.
    pub async fn relayer(&self, demande: &Demande<'_>) -> Result<Relaye, Echec> {
        let signee = match demande.sig {
            Some(sig) => {
                if !signature_valide(demande.secret, demande.url, sig) {
                    return Err(Refus::SignatureInvalide.into());
                }
                true
            }
            None if demande.signature_exigee => return Err(Refus::SignatureRequise.into()),
            None => false,
        };

        let premiere = reqwest::Url::parse(demande.url)
            .map_err(|_| Refus::UrlInvalide(demande.url.to_string()))?;
        let hote_origine = self.juger(&premiere, signee, None, demande.hotes_supplementaires)?;

        let mut courante = premiere;
        for _ in 0..=REDIRECTIONS_MAX {
            let reponse =
                self.client.get(courante.clone()).send().await.map_err(
                    |e| match adresse_refusee(&e) {
                        Some(r) => Echec::Refus(Refus::AdresseInterdite {
                            hote: r.hote,
                            ip: r.ip,
                        }),
                        None => Echec::Amont(e.to_string()),
                    },
                )?;
            let statut = reponse.status();
            if statut.is_redirection() {
                let Some(cible) = reponse
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|l| courante.join(l).ok())
                else {
                    return Err(Echec::Amont(format!(
                        "redirection sans Location ({statut})"
                    )));
                };
                // Une redirection est jugée comme une URL NON signée (la
                // signature couvre l'URL d'origine, pas ce que l'amont y
                // substitue), avec pour seule tolérance l'hôte d'origine.
                self.juger(
                    &cible,
                    false,
                    Some(&hote_origine),
                    demande.hotes_supplementaires,
                )
                .map_err(|r| match r {
                    Refus::AdresseInterdite { .. } => r,
                    _ => Refus::RedirectionRefusee(cible.to_string()),
                })?;
                courante = cible;
                continue;
            }
            if !statut.is_success() {
                return Err(Echec::Amont(format!("amont : {statut}")));
            }
            let content_type = reponse
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("image/jpeg")
                .to_string();
            let octets = reponse
                .bytes()
                .await
                .map_err(|e| Echec::Amont(e.to_string()))?
                .to_vec();
            return Ok(Relaye {
                content_type,
                octets,
            });
        }
        Err(Refus::TropDeRedirections.into())
    }

    /// Télécharge une image que le SERVEUR a choisie (#5214), pour la mettre en
    /// cache — jamais une URL reçue d'un client pour être relayée.
    ///
    /// C'est le chemin de la vignette d'un podcast auquel l'utilisateur s'est
    /// abonné : l'adresse vient du flux RSS qu'il a choisi, et les hébergeurs
    /// de flux sont en nombre illimité. La liste d'hôtes ne s'y applique donc
    /// pas — comme pour une URL signée —, et elle n'est pas élargie : le
    /// relais `/library/artwork/proxy` reste exactement ce qu'il était.
    ///
    /// Tout le reste de la garde s'applique, à chaque saut : schéma
    /// `http`/`https`, adresse littérale jugée par la politique du relais,
    /// résolution DNS par [`ResolveurGarde`], redirections suivies à la main
    /// (au plus [`REDIRECTIONS_MAX`]) et jugées comme la première URL. Et deux
    /// bornes propres à une mise en cache :
    ///
    /// - `Content-Type` : une réponse qui en annonce un doit dire `image/…`
    ///   (ou `application/octet-stream`, que des CDN posent sur tout) ;
    /// - taille : au plus `taille_max` octets, jugés sur `Content-Length`
    ///   quand il est là ET pendant la lecture, qui s'arrête au dépassement.
    pub async fn telecharger_image(
        &self,
        url: &str,
        taille_max: usize,
    ) -> Result<Relaye, EchecTelechargement> {
        let premiere = reqwest::Url::parse(url).map_err(|_| Refus::UrlInvalide(url.to_string()))?;
        self.juger(&premiere, true, None, &[])?;
        let mut courante = premiere;
        for _ in 0..=REDIRECTIONS_MAX {
            let reponse =
                self.client.get(courante.clone()).send().await.map_err(
                    |e| match adresse_refusee(&e) {
                        Some(r) => EchecTelechargement::Refus(Refus::AdresseInterdite {
                            hote: r.hote,
                            ip: r.ip,
                        }),
                        None => EchecTelechargement::Amont(e.to_string()),
                    },
                )?;
            let statut = reponse.status();
            if statut.is_redirection() {
                let Some(cible) = reponse
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|l| courante.join(l).ok())
                else {
                    return Err(EchecTelechargement::Amont(format!(
                        "redirection sans Location ({statut})"
                    )));
                };
                self.juger(&cible, true, None, &[]).map_err(|r| match r {
                    Refus::AdresseInterdite { .. } => r,
                    _ => Refus::RedirectionRefusee(cible.to_string()),
                })?;
                courante = cible;
                continue;
            }
            if !statut.is_success() {
                return Err(EchecTelechargement::Amont(format!("amont : {statut}")));
            }
            return lire_image(reponse, taille_max, true).await;
        }
        Err(Refus::TropDeRedirections.into())
    }

    /// #4895 — relaie la pochette d'un serveur multimédia du réseau local.
    ///
    /// `serveurs` est le registre SSDP courant, en couples hôte:port (hôte en
    /// minuscules, sans crochets). À CHAQUE saut — l'URL d'origine comme
    /// chaque redirection — l'hôte:port doit y figurer exactement
    /// ([`pochette_de_serveur_multimedia`]) : ni un autre port du même hôte, ni
    /// une autre adresse du sous-réseau. Le reste de la garde s'applique
    /// aussi : schéma `http`/`https`, politique d'adresse du relais (le relais
    /// [`Relais::reseau_local`] refuse boucle locale et lien-local),
    /// résolution DNS gardée, GET seul, au plus [`REDIRECTIONS_MAX`] sauts, un
    /// délai borné (celui du client), un `Content-Type` `image/…` exigé et au
    /// plus `taille_max` octets.
    pub async fn relayer_pochette_de_serveur_multimedia(
        &self,
        url: &str,
        serveurs: &[(String, u16)],
        taille_max: usize,
    ) -> Result<Relaye, EchecTelechargement> {
        let au_registre = |u: &reqwest::Url| {
            pochette_de_serveur_multimedia(u).is_some_and(|(hote, port)| {
                serveurs
                    .iter()
                    .any(|(h, p)| *p == port && h.eq_ignore_ascii_case(&hote))
            })
        };
        let premiere = reqwest::Url::parse(url).map_err(|_| Refus::UrlInvalide(url.to_string()))?;
        let hote_origine = self.juger(&premiere, true, None, &[])?;
        if !au_registre(&premiere) {
            return Err(Refus::HoteRefuse(hote_origine).into());
        }
        let mut courante = premiere;
        for _ in 0..=REDIRECTIONS_MAX {
            let reponse =
                self.client.get(courante.clone()).send().await.map_err(
                    |e| match adresse_refusee(&e) {
                        Some(r) => EchecTelechargement::Refus(Refus::AdresseInterdite {
                            hote: r.hote,
                            ip: r.ip,
                        }),
                        None => EchecTelechargement::Amont(e.to_string()),
                    },
                )?;
            let statut = reponse.status();
            if statut.is_redirection() {
                let Some(cible) = reponse
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|l| courante.join(l).ok())
                else {
                    return Err(EchecTelechargement::Amont(format!(
                        "redirection sans Location ({statut})"
                    )));
                };
                self.juger(&cible, true, None, &[]).map_err(|r| match r {
                    Refus::AdresseInterdite { .. } => r,
                    _ => Refus::RedirectionRefusee(cible.to_string()),
                })?;
                if !au_registre(&cible) {
                    return Err(Refus::RedirectionRefusee(cible.to_string()).into());
                }
                courante = cible;
                continue;
            }
            if !statut.is_success() {
                return Err(EchecTelechargement::Amont(format!("amont : {statut}")));
            }
            return lire_image(reponse, taille_max, false).await;
        }
        Err(Refus::TropDeRedirections.into())
    }
}

/// Pourquoi [`Relais::telecharger_image`] n'a pas rendu d'image.
#[derive(Debug)]
pub enum EchecTelechargement {
    /// La garde d'adresse ou de schéma a refusé (première URL ou redirection).
    Refus(Refus),
    /// L'amont a répondu autre chose qu'une image, ou n'a pas répondu.
    Amont(String),
    /// `Content-Type` qui n'est pas une image.
    TypeRefuse(String),
    /// Plus de N octets.
    TropVolumineuse(usize),
}

impl From<Refus> for EchecTelechargement {
    fn from(r: Refus) -> Self {
        EchecTelechargement::Refus(r)
    }
}

impl std::fmt::Display for EchecTelechargement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EchecTelechargement::Refus(r) => write!(f, "{r}"),
            EchecTelechargement::Amont(e) => write!(f, "{e}"),
            EchecTelechargement::TypeRefuse(t) => write!(f, "type refusé : {t}"),
            EchecTelechargement::TropVolumineuse(n) => write!(f, "plus de {n} octets"),
        }
    }
}

/// Retrouve un [`AdresseRefusee`] dans la chaîne des causes d'une erreur
/// `reqwest` (hyper l'enveloppe dans son erreur de connexion).
fn adresse_refusee(e: &reqwest::Error) -> Option<AdresseRefusee> {
    let mut cause: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(e);
    while let Some(c) = cause {
        if let Some(r) = c.downcast_ref::<AdresseRefusee>() {
            return Some(r.clone());
        }
        cause = c.source();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Le RÉSEAU LOCAL admis pour une pochette de bibliothèque : les plages
    /// privées et l'IPv6 unique locale — jamais la boucle locale, le
    /// lien-local (métadonnées d'un nuage) ni une adresse publique.
    #[test]
    fn le_reseau_local_est_un_sous_ensemble_strict_des_adresses_interdites() {
        let admises = [
            "192.168.1.41",
            "10.0.0.7",
            "172.16.5.1",
            "fd12::1",
            "::ffff:192.168.0.9",
        ];
        for a in admises {
            let ip: IpAddr = a.parse().unwrap();
            assert!(adresse_reseau_local(ip), "{a} est du réseau local");
            assert!(
                adresse_interdite(ip),
                "{a} reste interdite pour le relais général"
            );
        }
        let refusees = [
            "127.0.0.1",
            "169.254.169.254",
            "::1",
            "fe80::1",
            "224.0.0.1",
            "0.0.0.0",
            "100.64.0.1",
        ];
        for a in refusees {
            let ip: IpAddr = a.parse().unwrap();
            assert!(
                !adresse_reseau_local(ip),
                "{a} n'est PAS admise comme réseau local"
            );
        }
        assert!(!adresse_reseau_local("203.0.113.7".parse().unwrap()));
    }
    use std::collections::HashMap;

    /// Vecteur RFC 4231, cas 2 : clé `Jefe`, message
    /// `what do ya want for nothing?`.
    #[test]
    fn hmac_sha256_suit_la_rfc_4231() {
        assert_eq!(
            signature("Jefe", "what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn une_signature_alteree_ne_passe_pas() {
        let s = signature("secret", "https://mozaiklabs.fr/a.png");
        assert!(signature_valide(
            "secret",
            "https://mozaiklabs.fr/a.png",
            &s
        ));
        let mut alteree = s.clone();
        alteree.replace_range(0..1, if s.starts_with('0') { "1" } else { "0" });
        assert!(!signature_valide(
            "secret",
            "https://mozaiklabs.fr/a.png",
            &alteree
        ));
        assert!(!signature_valide(
            "secret",
            "https://mozaiklabs.fr/b.png",
            &s
        ));
        assert!(!signature_valide(
            "autre",
            "https://mozaiklabs.fr/a.png",
            &s
        ));
        assert!(!signature_valide(
            "secret",
            "https://mozaiklabs.fr/a.png",
            ""
        ));
    }

    #[test]
    fn la_liste_admet_les_sous_domaines_et_rien_d_autre() {
        assert!(hote_autorise("static.qobuz.com", &[]));
        assert!(hote_autorise("MOZAIKLABS.FR.", &[]));
        assert!(hote_autorise("ia800300.us.archive.org", &[]));
        assert!(hote_autorise("lastfm.freetls.fastly.net", &[]));
        assert!(!hote_autorise("fastly.net", &[]));
        assert!(!hote_autorise("qobuz.com.evil.example", &[]));
        assert!(!hote_autorise("notqobuz.com", &[]));
        assert!(!hote_autorise("localhost", &[]));
        assert!(!hote_autorise("", &[]));
        assert!(hote_autorise(
            "logos.radio.exemple",
            &["radio.exemple".to_string()]
        ));
    }

    #[test]
    fn les_adresses_internes_sont_interdites() {
        for a in [
            "127.0.0.1",
            "127.1.2.3",
            "10.0.0.1",
            "172.16.5.5",
            "192.168.1.18",
            "169.254.169.254",
            "0.0.0.0",
            "100.64.0.1",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "::",
            "fe80::1",
            "fc00::1",
            "fd12::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "ff02::1",
        ] {
            let ip: IpAddr = a.parse().unwrap();
            assert!(adresse_interdite(ip), "{a} devrait être interdite");
        }
        for a in ["8.8.8.8", "93.184.216.34", "2606:4700::1111"] {
            let ip: IpAddr = a.parse().unwrap();
            assert!(!adresse_interdite(ip), "{a} est publique");
        }
    }

    /// Un résolveur de banc : une table nom → adresses.
    struct Table(HashMap<&'static str, Vec<IpAddr>>);

    impl Resolveur for Table {
        fn resoudre(&self, nom: &str) -> Resolution {
            let r = self.0.get(nom).cloned();
            Box::pin(async move {
                r.ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "inconnu"))
            })
        }
    }

    /// LE témoin de la résolution DNS : un nom PUBLIC de la liste, mais qui
    /// résout en 127.0.0.1 (rebinding, /etc/hosts empoisonné, split DNS), est
    /// refusé par le résolveur du client — pas sur la chaîne, sur l'adresse.
    #[tokio::test]
    async fn un_nom_de_la_liste_qui_resout_en_prive_est_refuse() {
        let mut table = HashMap::new();
        table.insert(
            "static.qobuz.com",
            vec![
                "93.184.216.34".parse().unwrap(),
                "127.0.0.1".parse().unwrap(),
            ],
        );
        let relais = Relais::avec(Arc::new(Table(table)), |ip| !adresse_interdite(ip));
        let demande = Demande {
            url: "https://static.qobuz.com/images/cover.jpg",
            sig: None,
            secret: "s",
            signature_exigee: false,
            hotes_supplementaires: &[],
        };
        match relais.relayer(&demande).await {
            Err(Echec::Refus(Refus::AdresseInterdite { hote, ip })) => {
                assert_eq!(hote, "static.qobuz.com");
                assert_eq!(ip, "127.0.0.1".parse::<IpAddr>().unwrap());
            }
            autre => panic!("attendu un refus d'adresse, obtenu {autre:?}"),
        }
    }

    #[tokio::test]
    async fn les_litteraux_internes_sont_refuses_avant_toute_requete() {
        // Résolveur qui ne connaît RIEN : si une requête partait, ce serait
        // par un littéral, donc sans lui — c'est justement la garde éprouvée.
        let relais = Relais::avec(Arc::new(Table(HashMap::new())), |ip| !adresse_interdite(ip));
        for url in [
            "http://127.0.0.1:8888/api/v1/settings",
            "http://169.254.169.254/latest/meta-data/",
            "http://10.0.0.1/",
            "http://[::1]/",
            "http://[::ffff:127.0.0.1]/",
            // 2130706433 = 127.0.0.1 en décimal ; `url` le normalise.
            "http://2130706433/",
            "http://0x7f.0.0.1/",
        ] {
            let demande = Demande {
                url,
                sig: None,
                secret: "s",
                signature_exigee: false,
                hotes_supplementaires: &[],
            };
            match relais.relayer(&demande).await {
                Err(Echec::Refus(Refus::AdresseInterdite { .. })) => {}
                autre => panic!("{url} : attendu un refus d'adresse, obtenu {autre:?}"),
            }
        }
    }

    #[tokio::test]
    async fn un_hote_hors_liste_non_signe_est_refuse_sans_resolution() {
        let relais = Relais::avec(Arc::new(Table(HashMap::new())), |ip| !adresse_interdite(ip));
        let demande = Demande {
            url: "https://cdn.inconnu.example/a.png",
            sig: None,
            secret: "s",
            signature_exigee: false,
            hotes_supplementaires: &[],
        };
        assert!(matches!(
            relais.relayer(&demande).await,
            Err(Echec::Refus(Refus::HoteRefuse(h))) if h == "cdn.inconnu.example"
        ));
    }

    #[tokio::test]
    async fn seuls_http_et_https_passent() {
        let relais = Relais::avec(Arc::new(Table(HashMap::new())), |_| true);
        for url in ["file:///etc/passwd", "ftp://mozaiklabs.fr/a", "gopher://x/"] {
            let demande = Demande {
                url,
                sig: None,
                secret: "s",
                signature_exigee: false,
                hotes_supplementaires: &[],
            };
            assert!(
                matches!(
                    relais.relayer(&demande).await,
                    Err(Echec::Refus(Refus::UrlInvalide(_)))
                ),
                "{url}"
            );
        }
    }
}
